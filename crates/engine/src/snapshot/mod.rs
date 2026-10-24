//! The snapshot publisher: one thread that applies commands to the engine and publishes an immutable [`Snapshot`] 20 times a second.

mod build;
mod devices;
pub mod status;

use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use api::{Ack, Command, CommandReceiver, Handle, LogEntry, Snapshot, command_channel};
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
        PublisherConfig { period: Duration::from_millis(50), rows_per_snapshot: 16, acks_kept: 64, log_kept: 200 }
    }
}

/// The newest `kept` log lines, numbered from 1.
pub(crate) struct Log {
    lines: VecDeque<Arc<LogEntry>>,
    kept: usize,
    n: u64,
}

impl Log {
    pub fn push(&mut self, receiver: Option<char>, text: String) {
        self.n += 1;
        log::info!("{text}");
        self.lines.push_back(Arc::new(LogEntry { n: self.n, receiver, text }));
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
    watch: Watch,
    version: u64,
    swap: Arc<ArcSwap<Snapshot>>,
}

/// Starts thread "snapshot"; the engine lives on it and is dropped when the last `Handle` is.
///
/// The first snapshot (version 1, devices enumerated, sources already in the engine adopted) is stored before this returns.
pub fn spawn(engine: Engine, backends: Vec<Box<dyn Backend>>, cfg: PublisherConfig) -> Handle {
    let (tx, rx) = command_channel();
    let swap = Arc::new(ArcSwap::from_pointee(Snapshot::empty()));
    let mut log = Log { lines: VecDeque::new(), kept: cfg.log_kept, n: 0 };
    let registry = Registry::new(&engine, backends, &mut log);
    let mut p = Publisher { engine, registry, cfg, acks: VecDeque::new(), log, watch: Watch::default(), version: 0, swap: swap.clone() };
    p.publish();
    thread::Builder::new().name("snapshot".into()).spawn(move || p.run(rx)).expect("start the snapshot thread");
    Handle::new(swap, tx)
}

impl Publisher {
    /// Until every handle is gone: drain, apply, publish, wait out the period.
    fn run(mut self, rx: CommandReceiver) {
        loop {
            let next = Instant::now() + self.cfg.period;
            let Some(commands) = rx.drain() else { break };
            for (seq, c) in commands {
                let target = command_source(&c).and_then(|id| self.registry.letter(id));
                let result = self.registry.apply(&self.engine, c, &mut self.log);
                if let Err(e) = &result {
                    self.log.push(target, format!("command {seq} failed: {e}"));
                }
                self.acks.push_back(Ack { seq, result });
                while self.acks.len() > self.cfg.acks_kept {
                    self.acks.pop_front();
                }
            }
            self.publish();
            while let Some(left) = next.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
                thread::park_timeout(left);
            }
        }
        // Stop the open sources; the engine drops with this thread's stack.
        for id in self.registry.ids() {
            self.engine.remove_source(id);
        }
    }

    fn publish(&mut self) {
        let (receivers, bus) = build::receivers(&self.engine, &self.registry, self.cfg.rows_per_snapshot, &mut self.watch, &mut self.log);
        self.version += 1;
        self.swap.store(Arc::new(Snapshot {
            version: self.version,
            receivers,
            devices: self.registry.entries().to_vec(),
            bus,
            acks: self.acks.iter().cloned().collect(),
            log: self.log.lines.iter().cloned().collect(),
        }));
    }
}

/// The receiver a command targets, if any.
fn command_source(c: &Command) -> Option<SourceId> {
    match c {
        Command::Close(id) => Some(*id),
        Command::Tune { source, .. } | Command::SetGain { source, .. } | Command::SetMode { source, .. } => Some(*source),
        Command::Enumerate | Command::Open { .. } | Command::SetBudget { .. } => None,
    }
}
