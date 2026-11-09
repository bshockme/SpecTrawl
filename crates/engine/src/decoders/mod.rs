//! The decoder bank (spec §4.6): decoders attached to confirmed emitters, as bus consumers. Native first: P25 Phase 1
//! control channels decode with `detect`; air band voice is named from the bank's looks ([`airband`]). A decoder's identity reaches the user only as a claim on its emitter's open
//! record, so the record, the emitter's headline and the system row read the same value (spec §5).
//!
//! A manager thread watches the record book. An emitter whose records carry a `P25 control` decoder claim from the
//! bank (TSBKs decoded in a window) gets a [`P25Control`] on the bus at `Priority::TrunkControl`, back-filled from its
//! open record's start so its site appears without waiting for the next broadcast. A decoder detaches when its emitter
//! has had no open record for [`IDLE`] ([`PAGER_IDLE`] for a pager monitor), or its source goes.
//!
//! An emitter gets one decoder per protocol its records' claims want (931.7125 MHz sends both POCSAG and FLEX), but a
//! P25 control channel's alone. A monitor's claim goes only on the record its decode fell in, so a record is never
//! named by a protocol that was not decoded in it (spec §7's evidence rule).

pub mod airband;
pub mod monitor;
pub mod p25;

use std::collections::{HashMap, VecDeque};
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

/// What a decoder's claims are filed under in the book, followed by its mode ("decoder:FLEX").
pub const DECODER: &str = "decoder";
/// A decoder whose emitter has had no open record this long is detached.
pub const IDLE: Duration = Duration::from_secs(30);
/// A pager monitor's, instead: pagers send in bursts minutes apart (931.3375 MHz sent one-frame transmissions over
/// 30 s apart on air, and its monitor, attached afresh for each, kept only the last one's pages).
pub const PAGER_IDLE: Duration = Duration::from_secs(300);
/// Decoders per source, at most (a land-mobile span holds several control channels and pagers).
pub const MAX_PER_SOURCE: usize = 8;
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
    /// Where the decodes since the last call fell, in samples fed since `configure`. `None` (the default) when the
    /// decoder doesn't say: its claim then goes on its emitter's newest open record (a control channel sends without
    /// a break).
    fn take_heard(&mut self) -> Option<Vec<u64>> {
        None
    }
    fn telemetry(&self) -> Telemetry;
    /// One sentence.
    fn summary(&self) -> String;
    /// The newest calls or pages it decoded, one line each, newest last (none by default).
    fn recent(&self) -> Vec<String> {
        Vec::new()
    }
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
    /// The emitter's records in the book: where a decode is placed (see [`Decoder::take_heard`]).
    spans: Mutex<Vec<Span>>,
    stop: AtomicBool,
    ended: AtomicBool,
    view: Mutex<(Telemetry, String)>,
    /// The decoder's newest calls or pages.
    recent: Mutex<Vec<String>>,
    /// The fed channel's power, dBFS, smoothed (NaN before the first samples).
    level_dbfs: Mutex<f32>,
    channel: Mutex<Option<ChannelId>>,
}

/// A record's extent in its source's samples; an open record's end is still moving.
#[derive(Debug, Clone, Copy)]
struct Span {
    record: RecordId,
    first: u64,
    end: u64,
    open: bool,
}

impl Span {
    /// How far `at` is outside the record (0 inside it).
    fn distance(&self, at: u64) -> u64 {
        if at < self.first {
            self.first - at
        } else if self.open {
            0
        } else {
            at.saturating_sub(self.end)
        }
    }
}

/// The record a decode at source sample `at` fell in: the one holding it, else the nearest within `margin` (a record
/// starts where the detector saw the signal rise, a little after a transmission's first symbols).
fn record_at(spans: &[Span], at: u64, margin: u64) -> Option<RecordId> {
    spans.iter().map(|s| (s.distance(at), s.record)).filter(|(d, _)| *d <= margin).min().map(|(_, r)| r)
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
                    level_dbfs: Some(*a.level_dbfs.lock()).filter(|v| v.is_finite()),
                    channel: *a.channel.lock(),
                    recent: a.recent.lock().clone(),
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

/// What decoder a claim says its emitter wants: a P25 control channel's, or a land-mobile monitor for the protocol the
/// bank decoded (a `Decoder` claim; POCSAG's pages name no identity, so its `SyncBank` claim with frames decoded).
#[derive(Debug, Clone, PartialEq)]
enum Want {
    P25Control,
    Monitor { protocol: &'static str, label: String },
}

impl Want {
    fn mode(&self) -> &'static str {
        match self {
            Want::P25Control => "P25 control",
            Want::Monitor { protocol, .. } => protocol,
        }
    }
}

fn wants(c: &Claim) -> Option<Want> {
    let protocol = |name: &str| monitor::PROTOCOLS.iter().copied().find(|p| name == *p || name.starts_with(&format!("{p} ")));
    match &c.by {
        Evidence::Decoder { mode, .. } if mode == "P25 control" => Some(Want::P25Control),
        Evidence::Decoder { mode, .. } => protocol(mode).map(|p| Want::Monitor { protocol: p, label: c.label.clone() }),
        Evidence::SyncBank { protocol: name, frames_decoded, .. } if *frames_decoded > 0 => {
            protocol(name).map(|p| Want::Monitor { protocol: p, label: c.label.clone() })
        }
        _ => None,
    }
}

/// A decoder's channel: its width and least rate, and its bus priority.
fn channel_for(want: &Want, bandwidth_hz: f64) -> (f64, f64, Priority) {
    match want {
        Want::P25Control => (P25_WIDTH_HZ, P25_RATE, Priority::TrunkControl),
        Want::Monitor { protocol, .. } => {
            let pager = matches!(*protocol, "POCSAG" | "FLEX");
            // A pager gets its whole 25 kHz channel: a 4-level FLEX signal clipped to its measured width (a record often
            // catches only part of it) loses its outer levels.
            let width = if pager { PAGER_WIDTH_HZ } else { (1.25 * bandwidth_hz).clamp(MONITOR_MIN_WIDTH_HZ, MONITOR_MAX_WIDTH_HZ) };
            let priority = if pager { Priority::PagerMonitor } else { Priority::EmitterDecoder };
            (width, MONITOR_RATE, priority)
        }
    }
}

/// A monitor's channel: as the bank's looks cut it (1.25 × the width, at least 12.5 kHz, at 48 kHz or more), at most
/// a 25 kHz pager channel's.
const MONITOR_MIN_WIDTH_HZ: f64 = 12.5e3;
const MONITOR_MAX_WIDTH_HZ: f64 = 30e3;
const MONITOR_RATE: f64 = 48e3;
/// A pager monitor's channel: a paging channel's whole 25 kHz.
const PAGER_WIDTH_HZ: f64 = 25e3;

/// A decoder follows its emitter: one tuned more than this from where the emitter now is (1.5 kHz, or 15 % of its
/// width) is detached and attached again there. An emitter's first records can put it kHz off (a fading FLEX pager's
/// first record sat 4 kHz from its others on air, and the monitor attached there never decoded); its estimate moves
/// a few hundred hertz once it has settled.
fn moved(tuned_hz: f64, emitter_hz: f64, emitter_bw_hz: f64) -> bool {
    (tuned_hz - emitter_hz).abs() > (0.15 * emitter_bw_hz).max(1.5e3)
}

fn manage(shared: Arc<Shared>, book: Arc<RecordBook>, bus: Arc<ChannelBus>, sources: Sources) {
    while !shared.stop.load(Ordering::Relaxed) {
        let view = book.view();
        // Air band voice: a claim per AM record on a raster channel (no bus consumer: the bank's look measured it).
        airband::file_claims(&view, &book);
        // Each emitter's newest open record, its records' extents, and the decoders its records want: a P25 control
        // channel's alone, or a monitor per protocol (the first claim that names it).
        let mut open: HashMap<EmitterId, &Arc<SignalRecord>> = HashMap::new();
        let mut spans: HashMap<EmitterId, Vec<Span>> = HashMap::new();
        let mut wanted: HashMap<EmitterId, Vec<(&Arc<SignalRecord>, Want)>> = HashMap::new();
        for r in view.records.iter() {
            let Some(em) = r.emitter else { continue };
            if r.open {
                open.entry(em).and_modify(|o| *o = if r.first_sample > o.first_sample { r } else { o }).or_insert(r);
            }
            spans.entry(em).or_default().push(Span { record: r.id, first: r.first_sample, end: r.end_sample, open: r.open });
            for w in r.claims.iter().filter_map(wants) {
                let list = wanted.entry(em).or_default();
                if list.iter().any(|(_, x)| *x == Want::P25Control || x.mode() == w.mode()) {
                    continue;
                }
                if w == Want::P25Control {
                    list.clear();
                }
                list.push((r, w));
            }
        }
        // Where each emitter's decoders belong: the newest record whose claim asks for that mode (the look that synced
        // or decoded it saw the signal there). An emitter's own centre is an average of its records and can sit between
        // two channels, or drift off one, so a decoder never tunes to it.
        let at: HashMap<(EmitterId, &'static str), (f64, f64)> = wanted
            .iter()
            .flat_map(|(em, list)| list.iter().map(move |(r, w)| ((*em, w.mode()), (r.center_hz(), r.bandwidth_hz()))))
            .collect();
        let mut attached = shared.attached.lock();
        // The newest time the book has seen: idleness is measured on the records' own clock.
        let now = view.records.iter().map(|r| r.t_end).max();
        // Keep each decoder pointed at its emitter's newest open record; detach the idle and the ended.
        attached.retain(|a| {
            *a.spans.lock() = spans.get(&a.emitter).cloned().unwrap_or_default();
            let rec = open.get(&a.emitter);
            a.record.store(rec.map_or(u64::MAX, |r| r.id.0), Ordering::Relaxed);
            let mut last = a.last_open.lock();
            if let Some(r) = rec {
                *last = r.t_end;
            }
            let idle = if matches!(a.mode, "POCSAG" | "FLEX") { PAGER_IDLE } else { IDLE };
            let quiet = now.is_some_and(|now| now.duration_since(*last).unwrap_or_default() > idle);
            let moved = at.get(&(a.emitter, a.mode)).copied().filter(|&(c, bw)| moved(a.center_hz, c, bw));
            // A retune leaves a decoder outside its receiver's span: it ends with the span it was attached in.
            let in_span = sources.lock().get(&a.source).is_some_and(|src| {
                let seg = src.current_segment();
                (a.center_hz - seg.center_hz).abs() < seg.sample_rate / 2.0
            });
            let keep = !a.ended.load(Ordering::Relaxed) && !quiet && moved.is_none() && in_span;
            if !keep {
                a.stop.store(true, Ordering::Relaxed);
                if let Some(ch) = a.channel.lock().take() {
                    bus.unsubscribe(ch);
                }
                match moved {
                    Some((c, _)) => {
                        log::info!("decoder {} at {:.4} MHz detached: its signal is now at {:.4} MHz", a.mode, a.center_hz / 1e6, c / 1e6)
                    }
                    None => log::info!("decoder {} at {:.4} MHz detached", a.mode, a.center_hz / 1e6),
                }
            }
            keep
        });
        for (em, r, want) in wanted.into_iter().flat_map(|(em, list)| list.into_iter().map(move |(r, w)| (em, r, w))) {
            let mode = want.mode();
            if attached.iter().any(|a| a.emitter == em && a.mode == mode)
                || attached.iter().filter(|a| a.source == r.source).count() >= MAX_PER_SOURCE
            {
                continue;
            }
            let Some(src) = sources.lock().get(&r.source).cloned() else { continue };
            let Some(stage) = bus.stage(src.id()) else { continue };
            // On the signal as the claiming record saw it, not the emitter's average (see `at` above).
            let (center_hz, bandwidth_hz) = (r.center_hz(), r.bandwidth_hz());
            let title = match &want {
                Want::P25Control => mode.to_string(),
                Want::Monitor { label, .. } => label.clone(),
            };
            let newest = open.get(&em);
            let a = Arc::new(Attached {
                source: src.id(),
                emitter: em,
                center_hz,
                mode,
                record: AtomicU64::new(newest.map_or(u64::MAX, |o| o.id.0)),
                last_open: Mutex::new(newest.map_or(r.t_end, |o| o.t_end)),
                spans: Mutex::new(spans.get(&em).cloned().unwrap_or_default()),
                stop: AtomicBool::new(false),
                ended: AtomicBool::new(false),
                view: Mutex::new((
                    Telemetry { lamp: Lamp::Searching, blocks_ok: 0, blocks_bad: 0, identity: None },
                    format!("{title} {:.4} MHz: searching", center_hz / 1e6),
                )),
                channel: Mutex::new(None),
                level_dbfs: Mutex::new(f32::NAN),
                recent: Mutex::new(Vec::new()),
            });
            let feed = || -> Box<dyn Consumer> {
                let decoder: Box<dyn Decoder> = match &want {
                    Want::P25Control => Box::new(P25Control::new(center_hz)),
                    Want::Monitor { protocol, label } => Box::new(monitor::Monitor::new(protocol, label, center_hz, bandwidth_hz)),
                };
                Box::new(Feed {
                    attached: a.clone(),
                    book: book.clone(),
                    key: format!("{DECODER}:{mode}"),
                    decoder,
                    hop: stage.hop(),
                    input_rate: src.current_segment().sample_rate,
                    configured: false,
                    fed: 0,
                    deliveries: VecDeque::new(),
                    last: None,
                })
            };
            let (width, min_rate, priority) = channel_for(&want, bandwidth_hz);
            let spec = ChannelSpec { source: src.id(), center_hz, bandwidth_hz: width, min_rate, parent: None };
            // Back-filled from the open record's start, so what the ring already holds is decoded at once; live if that
            // has gone (the channel is on the air: it will say it again).
            // From a little before it, as a look's window is: a pager's first header is its transmission's start.
            let from = |o: &SignalRecord| {
                let seg = src.segment_at(o.first_sample);
                o.first_sample.saturating_sub((crate::classify::LEAD.as_secs_f64() * seg.sample_rate) as u64).max(seg.from_sample)
            };
            let sub = match newest.map(|o| bus.subscribe(spec.clone(), Start::At(from(o)), priority, feed())) {
                Some(Ok(info)) => Ok(info),
                _ => bus.subscribe(spec, Start::Live, priority, feed()),
            };
            match sub {
                Ok(info) => {
                    *a.channel.lock() = Some(info.id);
                    log::info!("decoder {mode} attached at {:.4} MHz (emitter {})", center_hz / 1e6, em.0);
                    attached.push(a);
                }
                Err(e) => log::warn!("decoder {mode} at {:.4} MHz: the bus refused it: {e:?}", center_hz / 1e6),
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

/// A decoder fed from the bus. Its claim goes on the record each decode fell in, or (a decoder that doesn't place its
/// decodes) on its emitter's open record whenever the claim changes.
struct Feed {
    attached: Arc<Attached>,
    book: Arc<RecordBook>,
    /// What its claims are filed under: one key per decoder mode, so two monitors on one emitter keep both claims.
    key: String,
    decoder: Box<dyn Decoder>,
    hop: u64,
    input_rate: f64,
    configured: bool,
    /// Samples fed to the decoder.
    fed: u64,
    /// Recent deliveries: (samples fed before it, its first input sample, its length), oldest first.
    deliveries: VecDeque<(u64, u64, usize)>,
    /// The claim last set, and on which record.
    last: Option<(RecordId, Claim)>,
}

/// The time constant of a decoder's level, seconds.
const LEVEL_SECS: f32 = 1.0;

/// Deliveries remembered to place a decode: more than a monitor's longest block (FLEX's 4 s) lags.
const PLACE_SECS: f64 = 12.0;

impl Feed {
    /// The input sample of fed sample `k`.
    fn input_sample(&self, k: u64) -> Option<u64> {
        let i = self.deliveries.partition_point(|d| d.0 <= k).checked_sub(1)?;
        let (fed, first, len) = self.deliveries[i];
        (k < fed + len as u64).then(|| first + ((k - fed) as f64 * self.hop as f64 / len as f64) as u64)
    }

    /// Folds a block's power into the attached decoder's level: an average with a time constant of about a second.
    fn measure_level(&self, samples: &[Complex32]) {
        if samples.is_empty() {
            return;
        }
        let power = samples.iter().map(|x| x.norm_sqr()).sum::<f32>() / samples.len() as f32;
        let db = 10.0 * power.max(1e-20).log10();
        let block_secs = (self.hop as f64 / self.input_rate) as f32;
        let k = (block_secs / LEVEL_SECS).clamp(0.0, 1.0);
        let mut level = self.attached.level_dbfs.lock();
        *level = if level.is_finite() { *level + k * (db - *level) } else { db };
    }

    fn set_claim(&mut self, id: RecordId) {
        let Some(c) = self.decoder.claim() else { return };
        if self.last.as_ref() != Some(&(id, c.clone())) {
            self.book.set_claims(id, &self.key, vec![c.clone()]);
            self.last = Some((id, c));
        }
    }
}

impl Consumer for Feed {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        if self.attached.stop.load(Ordering::Relaxed) {
            return Flow::Stop;
        }
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                if !self.configured {
                    self.decoder.configure(self.input_rate * samples.len() as f64 / self.hop as f64);
                    self.configured = true;
                }
                self.deliveries.push_back((self.fed, first_sample, samples.len()));
                self.fed += samples.len() as u64;
                let keep = (PLACE_SECS * self.input_rate / self.hop as f64) as usize + 1;
                while self.deliveries.len() > keep {
                    self.deliveries.pop_front();
                }
                self.decoder.feed(samples);
                *self.attached.view.lock() = (self.decoder.telemetry(), self.decoder.summary());
                let recent = self.decoder.recent();
                if *self.attached.recent.lock() != recent {
                    *self.attached.recent.lock() = recent;
                }
                self.measure_level(samples);
                match self.decoder.take_heard() {
                    Some(heard) => {
                        let margin = (crate::classify::LEAD.as_secs_f64() * self.input_rate) as u64;
                        let spans = self.attached.spans.lock().clone();
                        let mut records: Vec<RecordId> =
                            heard.iter().filter_map(|&k| record_at(&spans, self.input_sample(k)?, margin)).collect();
                        records.dedup();
                        for id in records {
                            self.set_claim(id);
                        }
                    }
                    None => {
                        let record = self.attached.record.load(Ordering::Relaxed);
                        if record != u64::MAX {
                            self.set_claim(RecordId(record));
                        }
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
