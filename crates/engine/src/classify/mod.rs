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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
/// An open record nothing has named (no sync, no decode) is looked at again sooner: 2 s after its last window was cut,
/// then twice as long each time, up to [`RELOOK`]. A P25 call's first 1.5 s can hold no sync the bank finds while the
/// rest of it decodes: on air at 851.90 and 853.39 MHz (2026-11-10, R2), 5–7 s calls whose first looks found nothing
/// had 8–11 syncs and decoded frames in 1.5 s windows 2–3 s later, and closed long before a relook 30 s on.
pub const UNNAMED_RELOOK: Duration = Duration::from_secs(2);
/// A record that closes with nothing having named it, longer than two windows, gets one more look: at up to this much
/// of its end, in one window. A weak call's syncs can fall where none of its 1.5 s looks were: on air at 852.575 MHz
/// (2026-11-10, mean SNR 15 dB) an 8.5 s P25 call had syncs in 6 of 17 half-second-spaced 1.5 s windows, its looks
/// found none, and the whole call decoded 14 of 14 frames.
pub const CLOSING_LOOK: Duration = Duration::from_secs(8);
/// `cut_at`'s look count once a record has had its closing look.
const CLOSED: u32 = u32::MAX;

/// Which look a window is.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Look {
    /// From [`LEAD`] before the record's first sample.
    First,
    /// From the live edge, while the record is open.
    Again,
    /// The last [`CLOSING_LOOK`] of a closed record.
    Closing,
}
/// A window is never narrower than a 12.5 kHz channel, nor wider than 1 MHz (the bank's widest detectors want 600 kHz).
const MIN_WINDOW_HZ: f64 = 12.5e3;
/// A window is collected this much wider each side than its record, and re-centred on the record's centre when it is
/// looked at, then narrowed to the record's width. The centre a record's first rows give can be kHz off: a fading
/// FLEX pager's sat 3 kHz from its final one on air at 931.3375 MHz, and the 12.5 kHz window cut there held half its
/// spectrum, which no header search survives.
const CENTRE_SLACK_HZ: f64 = 6e3;
/// A look in the FM broadcast band takes at least a station's width, centred where its record is, however little of the
/// station the record holds yet: a weak station's records are fragments of 2–30 kHz (its instantaneous spectrum,
/// swinging with the programme) up to 75 kHz off its carrier, and a station with HD Radio starts as one sideband. Cut
/// to the fragment, a look held no pilot to find (on air at 99.1 and 105.9 MHz no fragment was ever named).
const STATION_LOOK_HZ: f64 = 280e3;
/// A weak station's pilot is lost in a station-wide look (the noise across it puts the discriminator under FM's
/// threshold), but the look still finds the carrier, within 4 kHz on air at 99.1 MHz. A second look this wide, centred
/// there, found its pilot at 26 dB.
const STATION_CORE_HZ: f64 = 120e3;
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
    /// What the look takes as the record's width (see [`look_bandwidth`]).
    bandwidth_hz: f64,
    /// The record's own width.
    record_hz: f64,
    /// The channel's passband.
    width_hz: f64,
    /// The record's centre now, less the channel's: the pool re-centres the window by this and narrows it to the
    /// record's width (see [`CENTRE_SLACK_HZ`]).
    offset_hz: f64,
    snr_db: f32,
    /// The band plan says AM voice ([`detect::Hints::am_band`]).
    am_band: bool,
    /// The channel's centre.
    center_hz: f64,
    /// The band plan says FM broadcast: a look that names nothing looks again at the carrier it found.
    broadcast_fm: bool,
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
    /// Jobs a thread has taken off the queue and not yet counted: raised under the queue's lock, lowered under the
    /// stats', so a view never misses a look in progress (CI's debug build takes ~0.8 s a look, and a test that read
    /// "nothing waiting" there found a record neither looked at nor counted).
    busy: AtomicUsize,
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
            busy: AtomicUsize::new(0),
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
            waiting: self.shared.windows.lock().len() + {
                let q = self.shared.queue.lock();
                q.len() + self.shared.busy.load(Ordering::SeqCst)
            },
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
    // When each open record's last window was cut, and how many it has had.
    let mut cut_at: HashMap<RecordId, (Instant, u32)> = HashMap::new();
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
                let named = r.claims.iter().any(api::Claim::is_decoded);
                let looks = cut_at.get(&r.id).map_or(0, |c| c.1);
                let due = r.open && !in_flight && cut_at.get(&r.id).is_some_and(|c| c.0.elapsed() >= relook_after(c.1, named));
                let long = r.t_end.duration_since(r.t_start).is_ok_and(|d| d.as_secs_f64() > 2.0 * shared.cfg.window_secs);
                let closing = !r.open && !in_flight && !named && long && looks > 0 && looks != CLOSED;
                let look = if due { Look::Again } else { Look::Closing };
                if (due || closing)
                    && let Some(src) = sources.lock().get(&r.source).cloned()
                {
                    cut_at.insert(r.id, (Instant::now(), if closing { CLOSED } else { looks + 1 }));
                    if let Err(why) = cut(&shared, &bus, &src, r, look) {
                        log::debug!("classify: record {} not looked at again ({look:?}): {why}", r.id.0);
                    }
                }
                continue;
            }
            if r.role == Role::Survey {
                shared.stats.lock().survey += 1;
                continue;
            }
            let source = sources.lock().get(&r.source).cloned();
            cut_at.insert(r.id, (Instant::now(), 1));
            if let Err(why) = source.ok_or("its source is gone").and_then(|s| cut(&shared, &bus, &s, r, Look::First)) {
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

/// How long after an open record's last window (its `looks`-th) the next is cut: [`UNNAMED_RELOOK`], doubling, while
/// nothing has named it; [`RELOOK`] once something has.
fn relook_after(looks: u32, named: bool) -> Duration {
    if named {
        return RELOOK;
    }
    UNNAMED_RELOOK.saturating_mul(1 << looks.saturating_sub(1).min(4)).min(RELOOK)
}

/// Subscribes the record's window, where `look` says.
fn cut(shared: &Arc<Shared>, bus: &ChannelBus, source: &Arc<Source>, r: &SignalRecord, look: Look) -> Result<(), &'static str> {
    let relook = look != Look::First;
    let stage = bus.stage(source.id()).ok_or("its source is not on the bus")?;
    let bw = look_bandwidth(r.bandwidth_hz(), broadcast_fm(r.center_hz()));
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
        max_secs: if look == Look::Closing { CLOSING_LOOK.as_secs_f64() } else { shared.cfg.window_secs },
        center_hz: r.center_hz(),
        width_hz,
        group_delay: stage.fft_size() as u64 / 4,
        hop: stage.hop(),
        input_rate: segment.sample_rate,
        iq: Vec::new(),
        rate: 0.0,
        sent: false,
        am_band: am_band(r.center_hz()),
        broadcast_fm: broadcast_fm(r.center_hz()),
    };
    shared.windows.lock().insert(r.id, win.clone());
    let lead = (LEAD.as_secs_f64() * segment.sample_rate) as u64;
    let from = r.first_sample.saturating_sub(lead).max(segment.from_sample);
    let start = match look {
        Look::First => Start::At(from),
        Look::Again => Start::Live,
        Look::Closing => Start::At(from.max(r.end_sample.saturating_sub((CLOSING_LOOK.as_secs_f64() * segment.sample_rate) as u64))),
    };
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
    /// Most IQ it takes, seconds.
    max_secs: f64,
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
    broadcast_fm: bool,
}

/// Whether the band plan puts FM broadcast at `hz`.
fn broadcast_fm(hz: f64) -> bool {
    detect::bandplan::lookup(hz).is_some_and(|b| b.service == detect::bandplan::Service::BroadcastFm)
}

/// What a look takes as a record's width: its own, or in the FM broadcast band at least [`STATION_LOOK_HZ`].
fn look_bandwidth(record_hz: f64, broadcast_fm: bool) -> f64 {
    if broadcast_fm { record_hz.max(STATION_LOOK_HZ) } else { record_hz }
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
            let job = Job {
                record: self.record,
                relook: self.relook,
                iq: std::mem::take(&mut self.iq),
                rate: self.rate.max(1.0),
                bandwidth_hz: look_bandwidth(f64::from_bits(self.win.bandwidth_hz.load(Ordering::Relaxed)), self.broadcast_fm),
                record_hz: f64::from_bits(self.win.bandwidth_hz.load(Ordering::Relaxed)),
                width_hz: self.width_hz,
                offset_hz: f64::from_bits(self.win.center_hz.load(Ordering::Relaxed)) - self.center_hz,
                snr_db: f64::from_bits(self.win.snr_db.load(Ordering::Relaxed)) as f32,
                am_band: self.am_band,
                center_hz: self.center_hz,
                broadcast_fm: self.broadcast_fm,
            };
            // Submitted (queued, or counted missed or dropped) before it leaves the in-flight windows, so it is never
            // in neither.
            self.shared.submit(job);
            self.shared.windows.lock().remove(&self.record);
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
                let room = (self.max_secs * self.rate) as usize;
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

/// The bank's look at `job`'s window (`full`, and `iq` within it with its quiet ends trimmed). An FM broadcast claim
/// carries the station's carrier; in the FM broadcast band a look that names no station looks again at the carrier it
/// found (see [`STATION_CORE_HZ`]).
fn look(bank: &[Box<dyn bank::Detector>], job: &Job, full: &[Complex32], iq: &[Complex32], hints: detect::Hints) -> Vec<api::Claim> {
    let w = detect::bank::Window::new(iq, job.rate, job.bandwidth_hz, hints).with_full(full);
    let mut claims = bank::look(bank, &w);
    // A station's carrier: the window's centre (the record's, as it now stands) and the shape's offset from it.
    let mut carrier = job.center_hz + job.offset_hz + w.features.offset_hz as f64;
    let station = |c: &[api::Claim]| c.iter().any(|c| c.label.starts_with("FM broadcast"));
    if job.broadcast_fm && !station(&claims) {
        let mut ddc = detect::Ddc::new(job.rate, w.features.offset_hz as f64, window_width(STATION_CORE_HZ) / 2.0, job.rate);
        let mut core = Vec::with_capacity(iq.len());
        ddc.process(iq, &mut core);
        let w = detect::bank::Window::new(&core, job.rate, STATION_CORE_HZ, hints);
        let again = bank::look(bank, &w);
        if station(&again) {
            claims = again;
            carrier += w.features.offset_hz as f64;
        } else if job.record_hz < job.bandwidth_hz {
            // No station: what the record itself shows, as a look at its own width says (a distant station's bare
            // carrier, 2–6 kHz on air, is not "Wideband").
            let width = window_width(job.record_hz);
            let mut ddc = detect::Ddc::new(job.rate, 0.0, width / 2.0, job.rate);
            let mut own = Vec::with_capacity(iq.len());
            ddc.process(iq, &mut own);
            let snr = window_snr(&own, job.rate, job.record_hz, width).or(Some(job.snr_db).filter(|s| s.is_finite()));
            let hints = detect::Hints { snr_db: snr, ..hints };
            claims = bank::look(bank, &detect::bank::Window::new(&own, job.rate, job.record_hz, hints));
        }
    }
    for c in claims.iter_mut().filter(|c| c.label.starts_with("FM broadcast")) {
        if let api::Evidence::Features { carrier_hz, .. } = &mut c.by {
            *carrier_hz = Some(carrier);
        }
    }
    claims
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
                    shared.busy.fetch_add(1, Ordering::SeqCst);
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
            look(&bank, &job, &full, iq, hints)
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
        shared.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::Evidence;

    /// A stereo FM broadcast station `offset_hz` from the window's centre at `fs`: programme tones in both channels,
    /// the 19 kHz pilot at 9 %, the stereo difference on 38 kHz, ±75 kHz at full modulation, and noise (`noise` is its
    /// amplitude per component, uniform, against a carrier of 1).
    fn station(fs: f64, offset_hz: f64, secs: f64, noise_amp: f32) -> Vec<Complex32> {
        let n = (fs * secs) as usize;
        let tau = std::f64::consts::TAU;
        let mut phase = 0.0f64;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let left = 0.5 * (tau * 440.0 * t).sin() + 0.3 * (tau * 3100.0 * t).sin();
                let right = 0.5 * (tau * 660.0 * t).sin() + 0.3 * (tau * 7300.0 * t).sin();
                let m = 0.45 * (left + right) / 1.6 + 0.09 * (tau * 19e3 * t).sin() + 0.45 * (left - right) / 1.6 * (tau * 38e3 * t).sin();
                phase += tau * (offset_hz + 75e3 * m) / fs;
                Complex32::from_polar(1.0, phase as f32) + Complex32::new(noise_amp * noise() as f32, noise_amp * noise() as f32)
            })
            .collect()
    }

    /// `x` through a channel `width_hz` wide, as the bus and the pool's re-centring cut a window.
    fn channel(x: &[Complex32], fs: f64, width_hz: f64) -> Vec<Complex32> {
        let mut ddc = detect::Ddc::new(fs, 0.0, width_hz / 2.0, fs);
        let mut out = Vec::new();
        ddc.process(x, &mut out);
        out
    }

    #[test]
    fn a_look_at_a_fragment_of_a_broadcast_station_names_it_and_finds_its_carrier() {
        let bank = bank::bank();
        let fs = 781_250.0;
        let hints = detect::Hints { snr_db: Some(20.0), am_band: false };
        let wide = look_bandwidth(20e3, true);
        assert_eq!(wide, STATION_LOOK_HZ);
        // A fragment's record 60 kHz below the carrier, and an HD Radio sideband's 157 kHz below: the window is cut there.
        for offset in [60e3, 157e3] {
            let x = channel(&station(fs, offset, 1.5, 0.05), fs, window_width(wide));
            let w = detect::bank::Window::new(&x, fs, wide, hints);
            let claims = bank::look(&bank, &w);
            assert!(claims.iter().any(|c| c.label.starts_with("FM broadcast")), "{offset} Hz off: {claims:?}");
            let err = w.features.offset_hz as f64 - offset;
            assert!(err.abs() < 3e3, "{offset} Hz off: carrier measured {err:+.0} Hz from it");
        }
        // Cut to the fragment, as before: no pilot to find.
        let narrow = look_bandwidth(20e3, false);
        let x = channel(&station(fs, 60e3, 1.5, 0.05), fs, window_width(narrow));
        let w = detect::bank::Window::new(&x, fs, narrow, hints);
        assert!(!bank::look(&bank, &w).iter().any(|c| c.label.starts_with("FM broadcast")));
    }

    #[test]
    fn an_unnamed_open_record_is_looked_at_again_soon_then_less_often() {
        let secs: Vec<u64> = (1..=6).map(|k| relook_after(k, false).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 30, 30]);
        assert_eq!(relook_after(1, true), RELOOK);
    }

    #[test]
    fn only_the_fm_broadcast_band_takes_station_wide_looks() {
        assert!(broadcast_fm(99.1e6) && !broadcast_fm(162.55e6) && !broadcast_fm(118.0e6));
        assert_eq!(look_bandwidth(12.5e3, false), 12.5e3);
        assert_eq!(look_bandwidth(400e3, true), 400e3);
    }

    #[test]
    fn a_weak_station_lost_in_a_station_wide_look_is_found_at_its_carrier() {
        let bank = bank::bank();
        let fs = 781_250.0;
        let hints = detect::Hints { snr_db: Some(20.0), am_band: false };
        // A weak station, its record 20 kHz below the carrier: across 350 kHz the noise hides its pilot.
        let width = window_width(STATION_LOOK_HZ);
        let x = channel(&station(fs, 20e3, 1.5, 4.5), fs, width);
        let w = detect::bank::Window::new(&x, fs, STATION_LOOK_HZ, hints);
        assert!(!bank::look(&bank, &w).iter().any(|c| c.label.starts_with("FM broadcast")));
        let job = Job {
            record: RecordId(1),
            relook: false,
            iq: Vec::new(),
            rate: fs,
            bandwidth_hz: STATION_LOOK_HZ,
            record_hz: 30e3,
            width_hz: width,
            offset_hz: 0.0,
            snr_db: 20.0,
            am_band: false,
            center_hz: 99.08e6,
            broadcast_fm: true,
        };
        let claims = look(&bank, &job, &x, &x, hints);
        let c = claims.iter().find(|c| c.label.starts_with("FM broadcast")).expect("named at its carrier");
        let Evidence::Features { carrier_hz: Some(carrier), .. } = c.by else { panic!("{c:?}") };
        assert!((carrier - 99.1e6).abs() < 8e3, "carrier {:+.0} Hz off", carrier - 99.1e6);
        // Outside the FM broadcast band there is no second look.
        let elsewhere = Job { broadcast_fm: false, ..job };
        assert!(!look(&bank, &elsewhere, &x, &x, hints).iter().any(|c| c.label.starts_with("FM broadcast")));
    }

    #[test]
    fn a_record_in_the_fm_band_that_is_no_station_is_described_at_its_own_width() {
        let bank = bank::bank();
        let fs = 781_250.0;
        // A bare carrier (a distant station's, as on air at 96.5 and 104.7 MHz: records of 2–6 kHz), and noise.
        let mut seed = 7u64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 as f32 / (1u64 << 53) as f32 - 0.5
        };
        let x: Vec<Complex32> = (0..(1.5 * fs) as usize).map(|_| Complex32::new(1.0 + 0.3 * noise(), 0.3 * noise())).collect();
        let x = channel(&x, fs, window_width(STATION_LOOK_HZ));
        let job = Job {
            record: RecordId(1),
            relook: false,
            iq: Vec::new(),
            rate: fs,
            bandwidth_hz: STATION_LOOK_HZ,
            record_hz: 3e3,
            width_hz: window_width(STATION_LOOK_HZ),
            offset_hz: 0.0,
            snr_db: 30.0,
            am_band: false,
            center_hz: 104.7e6,
            broadcast_fm: true,
        };
        let claims = look(&bank, &job, &x, &x, detect::Hints { snr_db: Some(30.0), am_band: false });
        let shape = claims.iter().find(|c| matches!(c.by, Evidence::Features { .. })).expect("a shape");
        assert_eq!(shape.label, "Carrier", "{claims:?}");
    }
}
