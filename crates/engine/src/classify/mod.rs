//! The classify pool (spec §4.5): every record with IQ gets at least one look by the detector bank before its IQ
//! leaves the ring, and the snapshot says how many didn't.
//!
//! A scheduler thread watches the record book. For each new live record it subscribes a window on the bus at
//! `Priority::Classify`, back-filled from just before the record's first sample, at the record's centre and a little
//! wider than it.
//! The window collects until it holds [`ClassifyConfig::window_secs`] or the record has ended (its end plus the channel
//! filter's delay), then goes to a bounded queue. The pool's own threads take windows off the queue, run the bank and
//! set the record's `bank` claims in the book; a slow look never holds a bus worker. All threads are the engine's and
//! are joined when the classifier drops.

pub mod bank;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use api::{ClassifyView, RecordId, Role, SignalRecord};
use parking_lot::{Condvar, Mutex};
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::channels::{BusError, ChannelBus, ChannelSpec, Consumer, Delivery, Flow, Priority, Start};
use crate::records::RecordBook;
use crate::source::Source;

/// What the claims the pool sets are filed under in the book.
pub const BANK: &str = "bank";

#[derive(Debug, Clone)]
pub struct ClassifyConfig {
    /// Off: no windows, no threads (tests that measure something else).
    pub enabled: bool,
    /// The pool's threads; 0 = `max(1, (available_parallelism − 2) / 4)`.
    pub workers: usize,
    /// Most IQ a look takes, seconds (`engine-old`'s 1.5 s).
    pub window_secs: f64,
    /// Windows waiting for a thread, at most; a full queue drops the newest, counted.
    pub queue: usize,
}

impl Default for ClassifyConfig {
    fn default() -> Self {
        ClassifyConfig { enabled: true, workers: 0, window_secs: 1.5, queue: 64 }
    }
}

impl ClassifyConfig {
    pub fn off() -> Self {
        ClassifyConfig { enabled: false, ..Default::default() }
    }

    fn threads(&self) -> usize {
        if self.workers > 0 {
            return self.workers;
        }
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        (cpus.saturating_sub(2) / 4).max(1)
    }
}

/// The shortest window the bank looks at.
const MIN_WINDOW_SECS: f64 = detect::bank::MIN_SECONDS;
/// The scheduler's tick.
const TICK: Duration = Duration::from_millis(50);
/// A record still open this long after its last window was cut gets another, from the live edge: a control channel or
/// a TDMA channel is on for hours, and what can be decoded changes (a Phase 2 channel descrambles once its site's
/// control channel has been heard).
pub const RELOOK: Duration = Duration::from_secs(30);
/// A window is never narrower than a 12.5 kHz channel, nor wider than 1 MHz (the bank's widest detectors want 600 kHz).
const MIN_WINDOW_HZ: f64 = 12.5e3;
/// A window is collected this much wider each side than its record, and re-centred on the record's centre when it is
/// looked at, then narrowed to the record's width. The centre a record's first rows give can be kHz off: a fading
/// FLEX pager's sat 3 kHz from its final one on air at 931.3375 MHz, and the 12.5 kHz window cut there held half its
/// spectrum, which no header search survives.
const CENTRE_SLACK_HZ: f64 = 6e3;
/// Rows a new open record has before its window is cut (8 rows: 180 ms).
const SETTLE_ROWS: u32 = 8;
/// A window starts this long before its record (never before the record's segment): the detector opens a record once
/// its rows clear the threshold, and a transmission's start can be below it. A FLEX pager's header, the only one in a
/// one-frame transmission, is its first 80 ms: on air at 931.3375 MHz its first 55 ms were in a simulcast fade, and
/// a window from the record's start never named the pager.
pub(crate) const LEAD: Duration = Duration::from_millis(150);
const MAX_WINDOW_HZ: f64 = 1e6;
/// The bank's sync search wants at least this rate (`engine-old` cut windows at `max(48 kHz, 2.5 × width)`).
const MIN_RATE: f64 = 48e3;

pub(crate) type Sources = Arc<Mutex<HashMap<SourceId, Arc<Source>>>>;

struct Job {
    record: RecordId,
    /// A later look at a record already looked at.
    relook: bool,
    iq: Vec<Complex32>,
    rate: f64,
    bandwidth_hz: f64,
    /// The channel's passband.
    width_hz: f64,
    /// The record's centre now, less the channel's: the pool re-centres the window by this and narrows it to the
    /// record's width (see [`CENTRE_SLACK_HZ`]).
    offset_hz: f64,
    snr_db: f32,
    /// The band plan says AM voice ([`detect::Hints::am_band`]).
    am_band: bool,
}

/// A window being cut: where its record ends (`u64::MAX` while it is open), and its width and SNR as they now stand
/// (a record's first rows see only its core: the weak ~6 kHz signals on air were 3–4 kHz wide when their windows were
/// cut, which the shape rules called "Carrier").
struct Win {
    end: AtomicU64,
    center_hz: AtomicU64,
    bandwidth_hz: AtomicU64,
    snr_db: AtomicU64,
}

impl Win {
    fn update(&self, r: &SignalRecord) {
        self.end.store(if r.open { u64::MAX } else { r.end_sample }, Ordering::Relaxed);
        self.center_hz.store(r.center_hz().to_bits(), Ordering::Relaxed);
        self.bandwidth_hz.store(r.bandwidth_hz().to_bits(), Ordering::Relaxed);
        self.snr_db.store(f64::from(r.mean_snr_db).to_bits(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Stats {
    looked: u64,
    missed: u64,
    dropped: u64,
    survey: u64,
    looked_secs: f64,
    work_secs: f64,
}

struct Shared {
    cfg: ClassifyConfig,
    workers: usize,
    stop: AtomicBool,
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    stats: Mutex<Stats>,
    windows: Mutex<HashMap<RecordId, Arc<Win>>>,
}

impl Shared {
    fn submit(&self, job: Job) {
        let secs = job.iq.len() as f64 / job.rate;
        if secs < MIN_WINDOW_SECS {
            self.stats.lock().missed += 1;
            return;
        }
        let mut q = self.queue.lock();
        if q.len() >= self.cfg.queue {
            drop(q);
            self.stats.lock().dropped += 1;
            log::debug!("classify: queue full, record {} not looked at", job.record.0);
            return;
        }
        q.push_back(job);
        drop(q);
        self.ready.notify_one();
    }
}

pub struct Classifier {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Classifier {
    /// Starts the scheduler and the pool (nothing, when `cfg.enabled` is false).
    pub(crate) fn start(cfg: ClassifyConfig, book: Arc<RecordBook>, bus: Arc<ChannelBus>, sources: Sources) -> Classifier {
        let workers = if cfg.enabled { cfg.threads() } else { 0 };
        let shared = Arc::new(Shared {
            cfg: cfg.clone(),
            workers,
            stop: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            stats: Mutex::new(Stats::default()),
            windows: Mutex::new(HashMap::new()),
        });
        let mut threads = Vec::new();
        if cfg.enabled {
            let (s, b) = (shared.clone(), book.clone());
            threads.push(std::thread::Builder::new().name("classify".into()).spawn(move || schedule(s, b, bus, sources)).expect("spawn"));
            for k in 0..workers {
                let (s, b) = (shared.clone(), book.clone());
                threads.push(std::thread::Builder::new().name(format!("classify-{k}")).spawn(move || work(s, b)).expect("spawn"));
            }
        }
        Classifier { shared, threads }
    }

    pub fn view(&self) -> ClassifyView {
        let s = self.shared.stats.lock();
        ClassifyView {
            workers: self.shared.workers,
            looked: s.looked,
            waiting: self.shared.windows.lock().len() + self.shared.queue.lock().len(),
            missed: s.missed,
            dropped: s.dropped,
            survey: s.survey,
            looked_secs: s.looked_secs,
            work_secs: s.work_secs,
        }
    }

    /// Stops and joins the threads (windows still on the bus end with it).
    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Classifier {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The scheduler: a window for every new live record; open records' ends kept current.
fn schedule(shared: Arc<Shared>, book: Arc<RecordBook>, bus: Arc<ChannelBus>, sources: Sources) {
    // Records already in the book when the engine started (loaded from the store) are not this run's to look at.
    let mut seen: HashSet<RecordId> = book.view().records.iter().map(|r| r.id).collect();
    // When each open record's last window was cut.
    let mut cut_at: HashMap<RecordId, Instant> = HashMap::new();
    while !shared.stop.load(Ordering::Relaxed) {
        let view = book.view();
        let mut present = HashSet::with_capacity(view.records.len());
        for r in view.records.iter() {
            present.insert(r.id);
            // A new open record is cut once its first rows have settled its centre and width (it is back-filled from its
            // start, so waiting costs nothing but a moment): a FLEX pager's first rows sat 4.5 kHz below it on air,
            // which no FLEX slicer survives.
            if r.open && r.rows < SETTLE_ROWS && !seen.contains(&r.id) {
                continue;
            }
            if !seen.insert(r.id) {
                let in_flight = if let Some(w) = shared.windows.lock().get(&r.id) {
                    w.update(r);
                    true
                } else {
                    false
                };
                let due = r.open && !in_flight && cut_at.get(&r.id).is_some_and(|t| t.elapsed() >= RELOOK);
                if due && let Some(src) = sources.lock().get(&r.source).cloned() {
                    cut_at.insert(r.id, Instant::now());
                    if cut(&shared, &bus, &src, r, true).is_err() {
                        log::debug!("classify: record {} not looked at again", r.id.0);
                    }
                }
                continue;
            }
            if r.role == Role::Survey {
                shared.stats.lock().survey += 1;
                continue;
            }
            let source = sources.lock().get(&r.source).cloned();
            cut_at.insert(r.id, Instant::now());
            if let Err(why) = source.ok_or("its source is gone").and_then(|s| cut(&shared, &bus, &s, r, false)) {
                log::debug!("classify: record {} not looked at: {why}", r.id.0);
                shared.stats.lock().missed += 1;
            }
        }
        // Ids never come back; forget the ones the view no longer holds.
        seen.retain(|id| present.contains(id));
        cut_at.retain(|id, _| present.contains(id));
        std::thread::sleep(TICK);
    }
}

/// Subscribes the record's window: from [`LEAD`] before its first sample, or (`relook`) from the live edge.
fn cut(shared: &Arc<Shared>, bus: &ChannelBus, source: &Arc<Source>, r: &SignalRecord, relook: bool) -> Result<(), &'static str> {
    let stage = bus.stage(source.id()).ok_or("its source is not on the bus")?;
    let bw = r.bandwidth_hz();
    let min_rate = (2.5 * bw).max(MIN_RATE);
    let win =
        Arc::new(Win { end: AtomicU64::new(0), center_hz: AtomicU64::new(0), bandwidth_hz: AtomicU64::new(0), snr_db: AtomicU64::new(0) });
    win.update(r);
    let segment = source.segment_at(r.first_sample);
    let consumer = |width_hz: f64| Window {
        shared: shared.clone(),
        win: win.clone(),
        record: r.id,
        relook,
        center_hz: r.center_hz(),
        width_hz,
        group_delay: stage.fft_size() as u64 / 4,
        hop: stage.hop(),
        input_rate: segment.sample_rate,
        iq: Vec::new(),
        rate: 0.0,
        sent: false,
        am_band: am_band(r.center_hz()),
    };
    shared.windows.lock().insert(r.id, win.clone());
    let lead = (LEAD.as_secs_f64() * segment.sample_rate) as u64;
    let start = if relook { Start::Live } else { Start::At(r.first_sample.saturating_sub(lead).max(segment.from_sample)) };
    // Collected with slack either side for the centre to move; at a span's edge, where the bus may refuse the wider
    // channel, as the record stands.
    let width = window_width(bw);
    let mut result = Err(BusError::NoSuchSource);
    for w in [(width + 2.0 * CENTRE_SLACK_HZ).min(MAX_WINDOW_HZ), width] {
        let spec = ChannelSpec { source: source.id(), center_hz: r.center_hz(), bandwidth_hz: w, min_rate, parent: None };
        result = bus.subscribe(spec, start, Priority::Classify, Box::new(consumer(w)));
        if !matches!(result, Err(BusError::Design(_))) {
            break;
        }
    }
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            shared.windows.lock().remove(&r.id);
            Err(match e {
                BusError::NotHeld { .. } => "its start has left the ring",
                _ => "the bus refused its channel",
            })
        }
    }
}

/// A window's passband for a record `bw` wide: a little wider than it, at least a 12.5 kHz channel.
fn window_width(bw: f64) -> f64 {
    (1.25 * bw).clamp(MIN_WINDOW_HZ, MAX_WINDOW_HZ)
}

/// One record's window, collected on the bus.
struct Window {
    shared: Arc<Shared>,
    win: Arc<Win>,
    record: RecordId,
    relook: bool,
    /// The channel's centre: the record's when the window was cut.
    center_hz: f64,
    width_hz: f64,
    group_delay: u64,
    hop: u64,
    input_rate: f64,
    iq: Vec<Complex32>,
    rate: f64,
    sent: bool,
    am_band: bool,
}

/// Whether the band plan puts AM voice at `hz`: the civil and military air bands.
fn am_band(hz: f64) -> bool {
    use detect::bandplan::{Service, lookup};
    lookup(hz).is_some_and(|b| matches!(b.service, Service::Aviation | Service::Military))
}

impl Window {
    fn finish(&mut self) -> Flow {
        if !self.sent {
            self.sent = true;
            self.shared.windows.lock().remove(&self.record);
            let job = Job {
                record: self.record,
                relook: self.relook,
                iq: std::mem::take(&mut self.iq),
                rate: self.rate.max(1.0),
                bandwidth_hz: f64::from_bits(self.win.bandwidth_hz.load(Ordering::Relaxed)),
                width_hz: self.width_hz,
                offset_hz: f64::from_bits(self.win.center_hz.load(Ordering::Relaxed)) - self.center_hz,
                snr_db: f64::from_bits(self.win.snr_db.load(Ordering::Relaxed)) as f32,
                am_band: self.am_band,
            };
            self.shared.submit(job);
        }
        Flow::Stop
    }
}

impl Consumer for Window {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        if self.shared.stop.load(Ordering::Relaxed) {
            self.sent = true;
            self.shared.windows.lock().remove(&self.record);
            return Flow::Stop;
        }
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                if first_sample.saturating_sub(self.group_delay) >= self.win.end.load(Ordering::Relaxed) {
                    return self.finish();
                }
                self.rate = self.input_rate * samples.len() as f64 / self.hop as f64;
                let room = (self.shared.cfg.window_secs * self.rate) as usize;
                let take = samples.len().min(room.saturating_sub(self.iq.len()));
                self.iq.extend_from_slice(&samples[..take]);
                if self.iq.len() >= room { self.finish() } else { Flow::Continue }
            }
            // A record that starts at a retune is back-filled from the segment's first sample, and the block that
            // straddles the boundary is a gap: nothing collected yet, so carry on. A gap later ends the window.
            Delivery::Gap { .. } if self.iq.is_empty() => Flow::Continue,
            // A gap, a shed or the source going: look at what there is.
            _ => self.finish(),
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.sent {
            self.shared.windows.lock().remove(&self.record);
        }
    }
}

/// The window without the quiet at either end, for the shape measurements (the decoders get the whole window). A
/// record's extent is whole rows (22.9 ms), so its window starts and ends
/// up to a row or two before and after the transmission; the shape rules measure the envelope's variation over the
/// whole window, and 60 ms of floor each side of a clean FM tone made it "Unknown". Cut where the envelope, averaged
/// over 1 ms, is below a quarter of its 90th percentile; nothing inside is touched.
fn trim_edges(iq: &[Complex32], rate: f64) -> &[Complex32] {
    let k = ((rate * 1e-3) as usize).max(1);
    if iq.len() < 4 * k {
        return iq;
    }
    let env: Vec<f32> = iq.chunks(k).map(|c| c.iter().map(|v| v.norm()).sum::<f32>() / c.len() as f32).collect();
    let mut sorted = env.clone();
    let i90 = sorted.len() * 9 / 10;
    let p90 = *sorted.select_nth_unstable_by(i90, f32::total_cmp).1;
    let on = |e: &f32| *e >= 0.25 * p90;
    let (Some(first), Some(last)) = (env.iter().position(on), env.iter().rposition(on)) else { return iq };
    &iq[first * k..((last + 1) * k).min(iq.len())]
}

/// The window's SNR as `engine-old` measured it, which `detect`'s weak gate (7 dB) was tuned on: the mean power density
/// over the record's bandwidth against the 20th percentile of the channel's other passband bins, in dB. The record's
/// own SNR is its peak bin's (610 Hz) over the row's floor, which reads several dB higher on a weak signal: on air,
/// 11–12 dB records 1–4 kHz wide passed the gate and the shape rules called their noise "Carrier". `None` when the
/// record leaves fewer than 8 bins of floor in its channel (it fills it: strong signals, which the gate passes anyway).
fn window_snr(iq: &[Complex32], rate: f64, bandwidth_hz: f64, width_hz: f64) -> Option<f32> {
    const N: usize = 1024;
    const MAX_FFTS: usize = 64;
    if iq.len() < N {
        return None;
    }
    let mut sa = dsp::SpectrumAnalyzer::new(N);
    let ffts = (iq.len() / N).min(MAX_FFTS);
    let stride = (iq.len() - N) / ffts.max(1);
    for k in 0..ffts {
        sa.push(&iq[k * stride..k * stride + N]);
    }
    let mut p = Vec::with_capacity(N);
    if !sa.take_power(&mut p) {
        return None;
    }
    let bin_hz = rate / N as f64;
    let (c, half) = (N / 2, (bandwidth_hz / 2.0 / bin_hz).ceil().max(1.0) as usize);
    // Beside the record, inside the flat part of the channel filter.
    let (near, pass) = (half * 3 / 2 + 2, ((0.8 * width_hz / 2.0) / bin_hz) as usize);
    if half >= c || pass >= c || near >= pass {
        return None;
    }
    let band = &p[c - half..=c + half];
    let density = band.iter().map(|v| f64::from(*v)).sum::<f64>() / band.len() as f64;
    let mut beside: Vec<f32> = p[c - pass..c - near].iter().chain(&p[c + near + 1..=c + pass]).copied().collect();
    if beside.len() < 8 {
        return None;
    }
    let i20 = beside.len() / 5;
    let floor = f64::from(*beside.select_nth_unstable_by(i20, f32::total_cmp).1).max(1e-30);
    Some((10.0 * (density / floor).log10()) as f32)
}

/// The window centred on its record as the record now stands, and narrowed to the record's width: (samples, passband).
fn recentre(job: &Job) -> (std::borrow::Cow<'_, [Complex32]>, f64) {
    let width = window_width(job.bandwidth_hz);
    if job.offset_hz.abs() < 1.0 && job.width_hz <= width + 1.0 {
        return (std::borrow::Cow::Borrowed(&job.iq), job.width_hz);
    }
    let mut ddc = detect::Ddc::new(job.rate, job.offset_hz, width / 2.0, job.rate);
    let mut out = Vec::with_capacity(job.iq.len());
    ddc.process(&job.iq, &mut out);
    (std::borrow::Cow::Owned(out), width)
}

/// A pool thread: windows off the queue, through the bank, into the book.
fn work(shared: Arc<Shared>, book: Arc<RecordBook>) {
    let bank = bank::bank();
    loop {
        let job = {
            let mut q = shared.queue.lock();
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(j) = q.pop_front() {
                    break j;
                }
                shared.ready.wait_for(&mut q, Duration::from_millis(200));
            }
        };
        let t0 = Instant::now();
        let looked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (full, width_hz) = recentre(&job);
            let iq = trim_edges(&full, job.rate);
            let snr = window_snr(iq, job.rate, job.bandwidth_hz, width_hz).or(Some(job.snr_db).filter(|s| s.is_finite()));
            let hints = detect::Hints { snr_db: snr, am_band: job.am_band };
            if (iq.len() as f64) < MIN_WINDOW_SECS * job.rate {
                return Vec::new();
            }
            let w = detect::bank::Window::new(iq, job.rate, job.bandwidth_hz, hints).with_full(&full);
            bank::look(&bank, &w)
        }));
        let claims = match looked {
            Ok(c) => c,
            Err(_) => {
                log::warn!("classify: the bank panicked on record {}", job.record.0);
                Vec::new()
            }
        };
        // A later look replaces the first only if it decoded something, or the first didn't: a fade must not undo it.
        let decoded = |c: &[api::Claim]| c.iter().any(api::Claim::is_decoded);
        if !job.relook || decoded(&claims) || !decoded(&book.claims_by(job.record, BANK)) {
            book.set_claims(job.record, BANK, claims);
        }
        let mut s = shared.stats.lock();
        s.looked += 1;
        s.looked_secs += job.iq.len() as f64 / job.rate;
        s.work_secs += t0.elapsed().as_secs_f64();
    }
}
