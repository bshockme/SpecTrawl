//! The snapshot publisher: one thread that applies commands to the engine and publishes an immutable [`Snapshot`] 20 times a second.

mod build;
mod devices;
mod narrate;
pub mod status;

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use api::{Ack, Command, CommandReceiver, Handle, LogEntry, LogKind, Snapshot, command_channel};
use arc_swap::ArcSwap;
use sdr_core::Backend;

use crate::budget::SourceId;
use crate::engine::Engine;
use build::Watch;
use devices::Registry;

#[derive(Debug, Clone)]
pub struct PublisherConfig {
    /// Time between snapshots.
    pub period: Duration,
    /// The newest finished rows each receiver carries.
    pub rows_per_snapshot: usize,
    /// Acknowledgements kept (and copied into every snapshot).
    pub acks_kept: usize,
    /// Log lines kept (and copied into every snapshot).
    pub log_kept: usize,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        PublisherConfig { period: Duration::from_millis(50), rows_per_snapshot: 48, acks_kept: 64, log_kept: 500 }
    }
}

/// The newest `kept` log lines, numbered from 1.
pub(crate) struct Log {
    lines: VecDeque<Arc<LogEntry>>,
    kept: usize,
    n: u64,
}

impl Log {
    pub fn push(&mut self, receiver: Option<char>, kind: LogKind, text: String) {
        self.n += 1;
        log::info!("{text}");
        self.lines.push_back(Arc::new(LogEntry { n: self.n, receiver, kind, text }));
        while self.lines.len() > self.kept {
            self.lines.pop_front();
        }
    }
}

struct Publisher {
    engine: Engine,
    registry: Registry,
    cfg: PublisherConfig,
    acks: VecDeque<Ack>,
    log: Log,
    narrator: narrate::Narrator,
    catalogue: Arc<Vec<api::CatalogueEntry>>,
    band_plan: Arc<Vec<api::BandSpan>>,
    watch: Watch,
    version: u64,
    /// The systems view and the records view version it was built from.
    systems: (u64, Arc<api::SystemsView>),
    /// The channels view, likewise.
    channels: (u64, Arc<api::ChannelsView>),
    swap: Arc<ArcSwap<Snapshot>>,
}

/// Starts thread "snapshot"; the engine lives on it and is dropped when the last `Handle` is.
///
/// The first snapshot (version 1, devices enumerated, sources already in the engine adopted) is stored before this returns.
/// The thread is detached; [`spawn_with_guard`] also returns a guard to wait for it.
pub fn spawn(engine: Engine, backends: Vec<Box<dyn Backend>>, cfg: PublisherConfig) -> Handle {
    spawn_with_guard(engine, backends, cfg).0
}

/// [`spawn`], plus a guard that waits for the thread to finish once every `Handle` is gone.
pub fn spawn_with_guard(engine: Engine, backends: Vec<Box<dyn Backend>>, cfg: PublisherConfig) -> (Handle, PublisherGuard) {
    let (tx, rx) = command_channel();
    let swap = Arc::new(ArcSwap::from_pointee(Snapshot::empty()));
    let mut log = Log { lines: VecDeque::new(), kept: cfg.log_kept, n: 0 };
    let registry = Registry::new(&engine, backends, &mut log);
    let mut p = Publisher {
        engine,
        registry,
        cfg,
        acks: VecDeque::new(),
        log,
        narrator: narrate::Narrator::default(),
        catalogue: Arc::new(crate::catalogue::entries()),
        band_plan: Arc::new(crate::catalogue::band_plan()),
        watch: Watch::default(),
        version: 0,
        swap: swap.clone(),
        systems: (0, Arc::default()),
        channels: (0, Arc::default()),
    };
    p.publish();
    let (done_tx, done) = mpsc::channel::<()>();
    let thread = thread::Builder::new()
        .name("snapshot".into())
        .spawn(move || {
            // Dropped last, after `run` has stopped the sources and dropped the engine (or unwound): that ends the guard's wait.
            let _done = done_tx;
            p.run(rx);
        })
        .expect("start the snapshot thread");
    (Handle::new(swap, tx), PublisherGuard { thread, done })
}

/// Waits for the snapshot thread, which ends promptly once the last `Handle` is dropped.
#[derive(Debug)]
pub struct PublisherGuard {
    thread: thread::JoinHandle<()>,
    done: mpsc::Receiver<()>,
}

impl PublisherGuard {
    /// Waits up to `timeout` for the thread to stop the open sources, drop the engine and exit; `true` if it did.
    /// On `false` the thread is left running, detached. Call it after every `Handle` is dropped, or it waits out `timeout`.
    pub fn join(self, timeout: Duration) -> bool {
        if let Err(RecvTimeoutError::Timeout) = self.done.recv_timeout(timeout) {
            return false;
        }
        if self.thread.join().is_err() {
            log::error!("the snapshot thread panicked");
        }
        true
    }
}

impl Publisher {
    /// Until every handle is gone: drain, apply, publish, wait for the next deadline (on a fixed grid, so the rate holds
    /// whatever each tick costs). The last handle dropping ends the wait at once.
    fn run(mut self, mut rx: CommandReceiver) {
        let period = self.cfg.period;
        let mut next = Instant::now() + period;
        'run: while let Some(commands) = rx.drain() {
            for (seq, c) in commands {
                self.apply(seq, c);
            }
            self.registry.update_lo_masks(&self.engine);
            self.publish();
            while let Some(left) = next.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
                if !rx.wait(left) {
                    break 'run;
                }
            }
            next += period;
            // More than a period behind (a slow command): start the grid again rather than publish a burst.
            let now = Instant::now();
            if next < now {
                next = now + period;
            }
        }
        // Stop the open sources; the engine drops with this thread's stack.
        for id in self.registry.ids() {
            self.engine.remove_source(id);
        }
    }

    /// Applies one command and records its acknowledgement; a panic (a driver's, say) is acknowledged as an error.
    fn apply(&mut self, seq: u64, c: Command) {
        let target = command_source(&c).and_then(|id| self.registry.letter(id));
        let name = command_name(&c);
        let (registry, engine, log) = (&mut self.registry, &self.engine, &mut self.log);
        let result = panic::catch_unwind(AssertUnwindSafe(|| match c {
            // Answered by the store into a later snapshot, under this command's number.
            Command::QueryRecords(q) => match engine.store() {
                Some(s) => s.query(seq, q).map(|()| api::Outcome::Queried),
                None => Err("history search needs the record store".into()),
            },
            Command::Play { target, mode, gain, to } => engine.play(target, mode, gain, to).map(api::Outcome::Playing),
            Command::SaveIq { record } => {
                engine.records().save_iq(record).map(|(path, pending)| api::Outcome::SavedIq { record, path, pending })
            }
            Command::StopPlay(id) => {
                if engine.stop_play(id) {
                    Ok(api::Outcome::Done)
                } else {
                    Err(format!("replay {} is not playing", id.0))
                }
            }
            Command::StartWorker { source, mode, center_hz } => {
                engine.start_worker(source, &mode, center_hz).map(api::Outcome::WorkerStarted)
            }
            Command::StopWorker(id) => {
                if engine.stop_worker(id) {
                    Ok(api::Outcome::Done)
                } else {
                    Err(format!("worker {} is not running", id.0))
                }
            }
            c => registry.apply(engine, c, log),
        }))
        .unwrap_or_else(|payload| Err(format!("{name} panicked: {}", panic_text(payload.as_ref()))));
        if let Err(e) = &result {
            self.log.push(target, LogKind::Error, format!("command {seq} failed: {e}"));
        }
        self.acks.push_back(Ack { seq, result });
        while self.acks.len() > self.cfg.acks_kept {
            self.acks.pop_front();
        }
    }

    fn publish(&mut self) {
        let (receivers, bus) = build::receivers(&self.engine, &self.registry, self.cfg.rows_per_snapshot, &mut self.watch, &mut self.log);
        let records = self.engine.records().view();
        let letter = |serial: &str| receivers.iter().find(|r| r.device.serial == serial).map(|r| r.letter);
        self.narrator.emitters(&records, letter, &mut self.log);
        self.narrator.bus(&bus, &mut self.log);
        if self.systems.0 != records.version {
            self.systems = (records.version, Arc::new(crate::decoders::systems(&records)));
        }
        if self.channels.0 != records.version {
            self.channels = (records.version, Arc::new(crate::decoders::airband::channels(&records)));
        }
        self.version += 1;
        self.swap.store(Arc::new(Snapshot {
            version: self.version,
            receivers,
            devices: self.registry.entries().to_vec(),
            bus,
            acks: self.acks.iter().cloned().collect(),
            log: self.log.lines.iter().cloned().collect(),
            records,
            store: self.engine.store().map(|s| s.view()).or_else(|| {
                self.engine.store_error().map(|e| api::StoreView {
                    path: Default::default(),
                    written: 0,
                    queued: 0,
                    dropped: 0,
                    error: Some(e.to_string()),
                })
            }),
            query: self.engine.store().and_then(|s| s.query_result()),
            audio: Arc::new(self.engine.audio_view()),
            classify: self.engine.classify_view(),
            decoders: self.engine.decoder_views(),
            systems: self.systems.1.clone(),
            channels: self.channels.1.clone(),
            workers: self.engine.worker_views(),
            worker_modes: self.engine.worker_modes(),
            catalogue: self.catalogue.clone(),
            band_plan: self.band_plan.clone(),
        }));
    }
}

/// What a command is, for an error message.
fn command_name(c: &Command) -> &'static str {
    match c {
        Command::Enumerate => "enumerate",
        Command::Open { .. } => "open",
        Command::Close(_) => "close",
        Command::Tune { .. } => "tune",
        Command::SetGain { .. } => "set gain",
        Command::SetMode { .. } => "set mode",
        Command::SetBudget { .. } => "set budget",
        Command::SetSweep { .. } => "set sweep",
        Command::Tag { .. } => "tag",
        Command::Untag { .. } => "untag",
        Command::Note { .. } => "note",
        Command::QueryRecords(_) => "query records",
        Command::Play { .. } => "play",
        Command::StopPlay(_) => "stop play",
        Command::SaveIq { .. } => "save IQ",
        Command::StartWorker { .. } => "start worker",
        Command::StopWorker(_) => "stop worker",
    }
}

/// A panic's message, when it has one.
fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("(no message)")
}

/// The receiver a command targets, if any.
fn command_source(c: &Command) -> Option<SourceId> {
    match c {
        Command::Close(id) => Some(*id),
        Command::Tune { source, .. }
        | Command::SetGain { source, .. }
        | Command::SetMode { source, .. }
        | Command::SetSweep { source, .. } => Some(*source),
        Command::Enumerate
        | Command::Open { .. }
        | Command::SetBudget { .. }
        | Command::Tag { .. }
        | Command::Untag { .. }
        | Command::Note { .. }
        | Command::QueryRecords(_)
        | Command::StopPlay(_)
        | Command::SaveIq { .. }
        | Command::StopWorker(_) => None,
        Command::StartWorker { source, .. } => Some(*source),
        Command::Play { target: api::PlayTarget::Region { source, .. }, .. } => Some(*source),
        Command::Play { .. } => None,
    }
}
