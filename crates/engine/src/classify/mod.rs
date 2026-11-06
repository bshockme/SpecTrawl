//! The classify pool (spec §4.5): every record with IQ gets at least one look by the detector bank before its IQ
//! leaves the ring, and the snapshot says how many didn't.
//!
//! A scheduler thread watches the record book. For each new live record it subscribes a window on the bus at
//! `Priority::Classify`, back-filled from the record's first sample, at the record's centre and a little wider than it.
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
/// A window is never narrower than a 12.5 kHz channel, nor wider than 1 MHz (the bank's widest detectors want 600 kHz).
const MIN_WINDOW_HZ: f64 = 12.5e3;
const MAX_WINDOW_HZ: f64 = 1e6;
/// The bank's sync search wants at least this rate (`engine-old` cut windows at `max(48 kHz, 2.5 × width)`).
const MIN_RATE: f64 = 48e3;

pub(crate) type Sources = Arc<Mutex<HashMap<SourceId, Arc<Source>>>>;

struct Job {
    record: RecordId,
    iq: Vec<Complex32>,
    rate: f64,
    bandwidth_hz: f64,
    snr_db: f32,
}

/// A window being cut: where its record ends (`u64::MAX` while it is open).
struct Win {
    end: AtomicU64,
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
    while !shared.stop.load(Ordering::Relaxed) {
        let view = book.view();
        let mut present = HashSet::with_capacity(view.records.len());
        for r in view.records.iter() {
            present.insert(r.id);
            if !seen.insert(r.id) {
                if let Some(w) = shared.windows.lock().get(&r.id) {
                    w.end.store(if r.open { u64::MAX } else { r.end_sample }, Ordering::Relaxed);
                }
                continue;
            }
            if r.role == Role::Survey {
                shared.stats.lock().survey += 1;
                continue;
            }
            let source = sources.lock().get(&r.source).cloned();
            if let Err(why) = source.ok_or("its source is gone").and_then(|s| cut(&shared, &bus, &s, r)) {
                log::debug!("classify: record {} not looked at: {why}", r.id.0);
                shared.stats.lock().missed += 1;
            }
        }
        // Ids never come back; forget the ones the view no longer holds.
        seen.retain(|id| present.contains(id));
        std::thread::sleep(TICK);
    }
}

/// Subscribes the record's window.
fn cut(shared: &Arc<Shared>, bus: &ChannelBus, source: &Arc<Source>, r: &SignalRecord) -> Result<(), &'static str> {
    let stage = bus.stage(source.id()).ok_or("its source is not on the bus")?;
    let bw = r.bandwidth_hz();
    let width = (1.25 * bw).clamp(MIN_WINDOW_HZ, MAX_WINDOW_HZ);
    let min_rate = (2.5 * bw).max(MIN_RATE);
    let win = Arc::new(Win { end: AtomicU64::new(if r.open { u64::MAX } else { r.end_sample }) });
    let consumer = Window {
        shared: shared.clone(),
        win: win.clone(),
        record: r.id,
        bandwidth_hz: bw,
        snr_db: r.mean_snr_db,
        group_delay: stage.fft_size() as u64 / 4,
        hop: stage.hop(),
        input_rate: source.segment_at(r.first_sample).sample_rate,
        iq: Vec::new(),
        rate: 0.0,
        sent: false,
    };
    shared.windows.lock().insert(r.id, win);
    let spec = ChannelSpec { source: source.id(), center_hz: r.center_hz(), bandwidth_hz: width, min_rate, parent: None };
    match bus.subscribe(spec, Start::At(r.first_sample), Priority::Classify, Box::new(consumer)) {
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

/// One record's window, collected on the bus.
struct Window {
    shared: Arc<Shared>,
    win: Arc<Win>,
    record: RecordId,
    bandwidth_hz: f64,
    snr_db: f32,
    group_delay: u64,
    hop: u64,
    input_rate: f64,
    iq: Vec<Complex32>,
    rate: f64,
    sent: bool,
}

impl Window {
    fn finish(&mut self) -> Flow {
        if !self.sent {
            self.sent = true;
            self.shared.windows.lock().remove(&self.record);
            let job = Job {
                record: self.record,
                iq: std::mem::take(&mut self.iq),
                rate: self.rate.max(1.0),
                bandwidth_hz: self.bandwidth_hz,
                snr_db: self.snr_db,
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

/// The window without the quiet at either end. A record's extent is whole rows (22.9 ms), so its window starts and ends
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
            let hints = detect::Hints { snr_db: Some(job.snr_db).filter(|s| s.is_finite()) };
            let iq = trim_edges(&job.iq, job.rate);
            if (iq.len() as f64) < MIN_WINDOW_SECS * job.rate {
                return Vec::new();
            }
            let w = detect::bank::Window::new(iq, job.rate, job.bandwidth_hz, hints);
            bank::look(&bank, &w)
        }));
        let claims = match looked {
            Ok(c) => c,
            Err(_) => {
                log::warn!("classify: the bank panicked on record {}", job.record.0);
                Vec::new()
            }
        };
        book.set_claims(job.record, BANK, claims);
        let mut s = shared.stats.lock();
        s.looked += 1;
        s.looked_secs += job.iq.len() as f64 / job.rate;
        s.work_secs += t0.elapsed().as_secs_f64();
    }
}
