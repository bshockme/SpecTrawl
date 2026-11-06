//! The decoder bank (spec §4.6): decoders attached to confirmed emitters, as bus consumers. Native first: P25 Phase 1
//! control channels decode with `detect`. A decoder's identity reaches the user only as a claim on its emitter's open
//! record, so the record, the emitter's headline and the system row read the same value (spec §5).
//!
//! A manager thread watches the record book. An emitter whose records carry a `P25 control` decoder claim from the
//! bank (TSBKs decoded in a window) gets a [`P25Control`] on the bus at `Priority::TrunkControl`, back-filled from its
//! open record's start so its site appears without waiting for the next broadcast. A decoder detaches when its emitter
//! has had no open record for [`IDLE`], or its source goes.

pub mod p25;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use api::{Claim, DecoderView, EmitterId, Evidence, Identity, Lamp, RecordId, RecordsView, SignalRecord, SystemRow, SystemsView};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::channels::{ChannelBus, ChannelId, ChannelSpec, Consumer, Delivery, Flow, Priority, Start};
use crate::classify::Sources;
use crate::records::RecordBook;

pub use p25::P25Control;

/// What a decoder's claims are filed under in the book.
pub const DECODER: &str = "decoder";
/// A decoder whose emitter has had no open record this long is detached.
pub const IDLE: Duration = Duration::from_secs(30);
/// Decoders per source, at most.
pub const MAX_PER_SOURCE: usize = 4;
/// The manager's tick.
const TICK: Duration = Duration::from_millis(200);
/// A P25 control channel's window: 12.5 kHz at 48 kHz or more (`engine-old`'s monitor).
const P25_WIDTH_HZ: f64 = 12.5e3;
const P25_RATE: f64 = 48e3;

/// A decoder's state: the common block of spec §4.6 and what it has learned of who the signal is.
#[derive(Debug, Clone, PartialEq)]
pub struct Telemetry {
    pub lamp: Lamp,
    /// Frames that decoded, and frames tried that didn't.
    pub blocks_ok: u64,
    pub blocks_bad: u64,
    pub identity: Option<Identity>,
}

/// The decoder trait of spec §4.6. `feed` never blocks.
pub trait Decoder: Send {
    fn mode(&self) -> &'static str;
    fn configure(&mut self, rate: f64);
    fn feed(&mut self, block: &[Complex32]);
    fn reset(&mut self);
    fn telemetry(&self) -> Telemetry;
    /// One sentence.
    fn summary(&self) -> String;
    /// The claim the decoder makes about its signal, if it has locked.
    fn claim(&self) -> Option<Claim>;
}

/// One attached decoder, as the manager and the bus consumer share it.
struct Attached {
    source: SourceId,
    emitter: EmitterId,
    center_hz: f64,
    mode: &'static str,
    /// The emitter's newest open record (`u64::MAX`: none), which the decoder's claims go on.
    record: AtomicU64,
    /// The end of the emitter's newest open record when it last had one.
    last_open: Mutex<std::time::SystemTime>,
    stop: AtomicBool,
    ended: AtomicBool,
    view: Mutex<(Telemetry, String)>,
    channel: Mutex<Option<ChannelId>>,
}

struct Shared {
    stop: AtomicBool,
    attached: Mutex<Vec<Arc<Attached>>>,
}

pub struct DecoderBank {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl DecoderBank {
    /// Starts the manager (nothing when `enabled` is false).
    pub(crate) fn start(enabled: bool, book: Arc<RecordBook>, bus: Arc<ChannelBus>, sources: Sources) -> DecoderBank {
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), attached: Mutex::new(Vec::new()) });
        let thread = enabled.then(|| {
            let s = shared.clone();
            std::thread::Builder::new().name("decoders".into()).spawn(move || manage(s, book, bus, sources)).expect("spawn")
        });
        DecoderBank { shared, thread }
    }

    pub fn views(&self) -> Vec<DecoderView> {
        self.shared
            .attached
            .lock()
            .iter()
            .map(|a| {
                let (t, summary) = a.view.lock().clone();
                DecoderView {
                    source: a.source,
                    emitter: a.emitter,
                    mode: a.mode.to_string(),
                    center_hz: a.center_hz,
                    lamp: t.lamp,
                    summary,
                    blocks_ok: t.blocks_ok,
                    blocks_bad: t.blocks_bad,
                }
            })
            .collect()
    }

    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for a in self.shared.attached.lock().iter() {
            a.stop.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DecoderBank {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether the bank found this emitter to be a P25 control channel.
fn is_p25_control(c: &Claim) -> bool {
    matches!(&c.by, Evidence::Decoder { mode, .. } if mode == "P25 control")
}

fn manage(shared: Arc<Shared>, book: Arc<RecordBook>, bus: Arc<ChannelBus>, sources: Sources) {
    while !shared.stop.load(Ordering::Relaxed) {
        let view = book.view();
        // Each emitter's newest open record, and whether any of its records says P25 control.
        let mut open: HashMap<EmitterId, &Arc<SignalRecord>> = HashMap::new();
        let mut control: HashMap<EmitterId, &Arc<SignalRecord>> = HashMap::new();
        for r in view.records.iter() {
            let Some(em) = r.emitter else { continue };
            if r.open {
                open.entry(em).and_modify(|o| *o = if r.first_sample > o.first_sample { r } else { o }).or_insert(r);
            }
            if r.claims.iter().any(is_p25_control) {
                control.entry(em).or_insert(r);
            }
        }
        let mut attached = shared.attached.lock();
        // The newest time the book has seen: idleness is measured on the records' own clock.
        let now = view.records.iter().map(|r| r.t_end).max();
        // Keep each decoder pointed at its emitter's newest open record; detach the idle and the ended.
        attached.retain(|a| {
            let rec = open.get(&a.emitter);
            a.record.store(rec.map_or(u64::MAX, |r| r.id.0), Ordering::Relaxed);
            let mut last = a.last_open.lock();
            if let Some(r) = rec {
                *last = r.t_end;
            }
            let quiet = now.is_some_and(|now| now.duration_since(*last).unwrap_or_default() > IDLE);
            let keep = !a.ended.load(Ordering::Relaxed) && !quiet && sources.lock().contains_key(&a.source);
            if !keep {
                a.stop.store(true, Ordering::Relaxed);
                if let Some(ch) = a.channel.lock().take() {
                    bus.unsubscribe(ch);
                }
                log::info!("decoder {} at {:.4} MHz detached", a.mode, a.center_hz / 1e6);
            }
            keep
        });
        for (em, r) in control {
            if attached.iter().any(|a| a.emitter == em) || attached.iter().filter(|a| a.source == r.source).count() >= MAX_PER_SOURCE {
                continue;
            }
            let Some(src) = sources.lock().get(&r.source).cloned() else { continue };
            let Some(stage) = bus.stage(src.id()) else { continue };
            let center_hz = view.emitters.iter().find(|e| e.id == em).map_or(r.center_hz(), |e| e.center_hz);
            let newest = open.get(&em);
            let a = Arc::new(Attached {
                source: src.id(),
                emitter: em,
                center_hz,
                mode: "P25 control",
                record: AtomicU64::new(newest.map_or(u64::MAX, |o| o.id.0)),
                last_open: Mutex::new(newest.map_or(r.t_end, |o| o.t_end)),
                stop: AtomicBool::new(false),
                ended: AtomicBool::new(false),
                view: Mutex::new((
                    Telemetry { lamp: Lamp::Searching, blocks_ok: 0, blocks_bad: 0, identity: None },
                    format!("P25 control {:.4} MHz: searching", center_hz / 1e6),
                )),
                channel: Mutex::new(None),
            });
            let feed = || -> Box<dyn Consumer> {
                Box::new(Feed {
                    attached: a.clone(),
                    book: book.clone(),
                    decoder: Box::new(P25Control::new(center_hz)),
                    hop: stage.hop(),
                    input_rate: src.current_segment().sample_rate,
                    configured: false,
                    last: None,
                })
            };
            let spec = ChannelSpec { source: src.id(), center_hz, bandwidth_hz: P25_WIDTH_HZ, min_rate: P25_RATE, parent: None };
            // Back-filled from the open record's start, so the site shows from what the ring already holds; live if
            // that has gone (the control channel is on the air: it will say it again).
            let sub = match newest.map(|o| bus.subscribe(spec.clone(), Start::At(o.first_sample), Priority::TrunkControl, feed())) {
                Some(Ok(info)) => Ok(info),
                _ => bus.subscribe(spec, Start::Live, Priority::TrunkControl, feed()),
            };
            match sub {
                Ok(info) => {
                    *a.channel.lock() = Some(info.id);
                    log::info!("decoder P25 control attached at {:.4} MHz (emitter {})", center_hz / 1e6, em.0);
                    attached.push(a);
                }
                Err(e) => log::warn!("decoder P25 control at {:.4} MHz: the bus refused it: {e:?}", center_hz / 1e6),
            }
        }
        drop(attached);
        // In slices, so a stop is seen promptly (the engine's drop joins this thread).
        let t0 = std::time::Instant::now();
        while t0.elapsed() < TICK && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    for a in shared.attached.lock().drain(..) {
        if let Some(ch) = a.channel.lock().take() {
            bus.unsubscribe(ch);
        }
    }
}

/// A decoder fed from the bus; its claim goes on its emitter's open record whenever it changes.
struct Feed {
    attached: Arc<Attached>,
    book: Arc<RecordBook>,
    decoder: Box<dyn Decoder>,
    hop: u64,
    input_rate: f64,
    configured: bool,
    /// The claim last set, and on which record.
    last: Option<(RecordId, Claim)>,
}

impl Consumer for Feed {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        if self.attached.stop.load(Ordering::Relaxed) {
            return Flow::Stop;
        }
        match d {
            Delivery::Samples { samples, .. } => {
                if !self.configured {
                    self.decoder.configure(self.input_rate * samples.len() as f64 / self.hop as f64);
                    self.configured = true;
                }
                self.decoder.feed(samples);
                *self.attached.view.lock() = (self.decoder.telemetry(), self.decoder.summary());
                let record = self.attached.record.load(Ordering::Relaxed);
                if let (Some(c), true) = (self.decoder.claim(), record != u64::MAX) {
                    let id = RecordId(record);
                    if self.last.as_ref() != Some(&(id, c.clone())) {
                        self.book.set_claims(id, DECODER, vec![c.clone()]);
                        self.last = Some((id, c));
                    }
                }
                Flow::Continue
            }
            // A gap loses a few frames; the decoder's state carries on.
            Delivery::Gap { .. } => {
                self.decoder.reset();
                Flow::Continue
            }
            Delivery::Shed | Delivery::Ended { .. } => {
                self.attached.ended.store(true, Ordering::Relaxed);
                Flow::Stop
            }
        }
    }
}

/// The systems the emitters' headlines name: one row per control channel emitter whose headline is a decoder claim
/// with a P25 identity, with the records whose claims name the same system. Built from claims only, so a row cannot
/// disagree with the emitter or the records it lists.
pub fn systems(view: &RecordsView) -> SystemsView {
    let mut systems: Vec<SystemRow> = view
        .emitters
        .iter()
        .filter_map(|e| {
            let h = e.headline.as_ref()?;
            let Evidence::Decoder { mode, summary, identity: Some(identity @ Identity::P25 { .. }), .. } = &h.by else { return None };
            if mode != "P25 control" {
                return None;
            }
            let mut records: Vec<&Arc<SignalRecord>> = view
                .records
                .iter()
                .filter(|r| r.emitter == Some(e.id) && r.claims.iter().any(|c| claim_identity(c).is_some_and(|i| i.same(identity))))
                .collect();
            records.sort_by_key(|r| (r.t_start, r.id));
            let first = view.records.iter().find(|r| r.emitter == Some(e.id))?;
            Some(SystemRow {
                identity: identity.clone(),
                mode: mode.clone(),
                source: first.source,
                emitter: e.id,
                control_hz: e.center_hz,
                records: records.iter().map(|r| r.id).collect(),
                summary: summary.clone(),
                last_seen: e.last_seen,
            })
        })
        .collect();
    systems.sort_by(|a, b| a.control_hz.total_cmp(&b.control_hz));
    SystemsView { systems }
}

fn claim_identity(c: &Claim) -> Option<&Identity> {
    match &c.by {
        Evidence::Decoder { identity, .. } => identity.as_ref(),
        _ => None,
    }
}
