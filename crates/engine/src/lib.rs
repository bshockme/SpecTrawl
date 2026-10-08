//! Per-source pipeline: driver → IQ ring (look-back) → spectrum rows.
//! A source runs in one of two modes: `Live` (fixed tuning, FFT rows at a
//! steady rate) or `Sweep` (retune-and-dwell across a span, one stitched row
//! per pass). See DESIGN.md §5.

mod bursts;
mod ring;
mod sweep;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::{Capabilities, Complex32, Device, DeviceInfo, Samples, StreamEvent, StreamSink};

pub use bursts::{BurstDetector, DetectorConfig, SignalRecord};
pub use ring::IqRing;
pub use sweep::{SweepPlan, SweepStatus};

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub fft_size: usize,
    pub rows_per_sec: f64,
    pub ring_seconds: f64,
    /// Spectrum rows kept for scrolling back the waterfall.
    pub row_history: usize,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self { fft_size: 16384, rows_per_sec: 50.0, ring_seconds: 10.0, row_history: 3000 }
    }
}

#[derive(Debug, Clone)]
pub enum Mode {
    Live,
    Sweep(SweepPlan),
}

/// Frequency extent of a row: `center_hz ± span_hz/2`. For a live row the
/// span is the sample rate; for a sweep row it is the swept span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub center_hz: f64,
    pub span_hz: f64,
}

/// One waterfall row: power spectrum in dBFS, lowest frequency first.
#[derive(Debug)]
pub struct SpectrumRow {
    pub seq: u64,
    pub tuning: Tuning,
    /// Sample index of the newest sample that went into this row.
    pub end_sample: u64,
    pub time: SystemTime,
    pub db: Vec<f32>,
}

#[derive(Default)]
struct StreamState {
    center_hz: f64,
    sample_rate: f64,
    gaps: u64,
    lost_samples: u64,
    overloads: u64,
    lost_device: bool,
}

/// Finished records (bounded) plus the snapshot of still-open ones.
#[derive(Default)]
struct RecordStore {
    closed: VecDeque<SignalRecord>,
    open: Vec<SignalRecord>,
}

const MAX_CLOSED_RECORDS: usize = 20_000;

/// State shared between the driver callback, the worker and the UI.
///
/// Locking rule: hold at most one of these mutexes at a time. Copy what you
/// need out and drop the guard before taking another. (A UI read of
/// state→ring against a sweep read of ring→state deadlocked the app.)
struct Shared {
    ring: Mutex<IqRing>,
    records: Mutex<RecordStore>,
    state: Mutex<StreamState>,
    rows: Mutex<VecDeque<Arc<SpectrumRow>>>,
    sweep: Mutex<Option<SweepStatus>>,
    ffts_per_sec: Mutex<f64>,
}

impl Shared {
    /// Runs the detector on a row and files what it reports.
    fn detect(&self, det: &mut BurstDetector, row: &SpectrumRow) {
        let closed = det.process(row);
        let mut st = self.records.lock();
        for r in closed {
            if st.closed.len() >= MAX_CLOSED_RECORDS {
                st.closed.pop_front();
            }
            st.closed.push_back(r);
        }
        st.open = det.open_records();
    }

    fn push_row(&self, row: SpectrumRow, cap: usize) {
        let mut rows = self.rows.lock();
        while rows.len() >= cap {
            rows.pop_front();
        }
        rows.push_back(Arc::new(row));
    }
}

struct RingSink {
    shared: Arc<Shared>,
}

impl StreamSink for RingSink {
    fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
        self.shared.ring.lock().write(first_sample, samples);
    }

    fn on_event(&mut self, event: StreamEvent) {
        let mut st = self.shared.state.lock();
        match event {
            StreamEvent::Retune { center_hz, sample_rate, .. } => {
                st.center_hz = center_hz;
                st.sample_rate = sample_rate;
            }
            StreamEvent::Gap { lost_samples, .. } => {
                st.gaps += 1;
                st.lost_samples += lost_samples;
            }
            StreamEvent::Overload { .. } => st.overloads += 1,
            StreamEvent::DeviceLost => st.lost_device = true,
            StreamEvent::TimeAnchor { .. } | StreamEvent::GainChange { .. } => {}
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourceStats {
    pub gaps: u64,
    pub lost_samples: u64,
    pub overloads: u64,
    pub device_lost: bool,
    pub ring_samples: u64,
    pub ring_capacity: usize,
    pub total_samples: u64,
    pub ffts_per_sec: f64,
}

/// Copy of the device's identity and settings, safe to hold in the UI.
#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub info: DeviceInfo,
    pub caps: Capabilities,
    pub center_hz: f64,
    pub sample_rate: f64,
}

type SharedDevice = Arc<Mutex<Box<dyn Device>>>;

/// A running source: owns the device, its IQ ring and one worker thread.
pub struct Source {
    device: SharedDevice,
    info: DeviceInfo,
    shared: Arc<Shared>,
    cfg: SourceConfig,
    mode: Mode,
    worker: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Source {
    pub fn start(mut device: Box<dyn Device>, cfg: SourceConfig, mode: Mode) -> sdr_core::Result<Self> {
        let rate = device.sample_rate();
        let cap = (rate * cfg.ring_seconds).ceil() as usize;
        let shared = Arc::new(Shared {
            ring: Mutex::new(IqRing::new(cap)),
            records: Mutex::new(RecordStore::default()),
            state: Mutex::new(StreamState { center_hz: device.center_hz(), sample_rate: rate, ..Default::default() }),
            rows: Mutex::new(VecDeque::with_capacity(cfg.row_history)),
            sweep: Mutex::new(None),
            ffts_per_sec: Mutex::new(0.0),
        });
        device.start(Box::new(RingSink { shared: shared.clone() }))?;
        let info = device.info().clone();
        let mut s = Self { device: Arc::new(Mutex::new(device)), info, shared, cfg, mode: Mode::Live, worker: None };
        s.set_mode(mode);
        Ok(s)
    }

    /// Stops the current worker and starts one for `mode`. Row history is
    /// cleared because rows from different modes don't share a frequency axis.
    pub fn set_mode(&mut self, mode: Mode) {
        self.stop_worker();
        self.shared.rows.lock().clear();
        *self.shared.records.lock() = RecordStore::default();
        *self.shared.sweep.lock() = None;
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, cfg, st) = (self.shared.clone(), self.cfg.clone(), stop.clone());
        let serial = self.info.serial.clone();
        let handle = match &mode {
            Mode::Live => {
                let usable = self.device.lock().capabilities().usable_bandwidth_fraction as f32;
                let det = BurstDetector::new(DetectorConfig { usable_fraction: usable, ..DetectorConfig::live(cfg.rows_per_sec) }, serial);
                std::thread::Builder::new().name("spectrum".into()).spawn(move || live_worker(shared, cfg, st, det))
            }
            Mode::Sweep(plan) => {
                let (dev, plan) = (self.device.clone(), plan.clone());
                let det = BurstDetector::new(DetectorConfig::sweep(), serial);
                std::thread::Builder::new().name("sweep".into()).spawn(move || sweep::run(shared, dev, plan, cfg, st, det))
            }
        }
        .expect("spawn worker");
        self.worker = Some((stop, handle));
        self.mode = mode;
    }

    fn stop_worker(&mut self) {
        if let Some((stop, h)) = self.worker.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        let d = self.device.lock();
        DeviceSnapshot { info: d.info().clone(), caps: d.capabilities().clone(), center_hz: d.center_hz(), sample_rate: d.sample_rate() }
    }

    pub fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    /// Retunes a live source. Ignored while sweeping (the sweep owns tuning).
    pub fn set_center(&mut self, hz: f64) -> sdr_core::Result<()> {
        if matches!(self.mode, Mode::Sweep(_)) {
            return Ok(());
        }
        self.device.lock().set_center(hz)
    }

    pub fn set_gain(&self, gain: sdr_core::GainSetting) -> sdr_core::Result<()> {
        self.device.lock().set_gain(gain)
    }

    /// Rows with `seq > after`, oldest first.
    pub fn rows_after(&self, after: Option<u64>) -> Vec<Arc<SpectrumRow>> {
        let rows = self.shared.rows.lock();
        match after {
            None => rows.iter().cloned().collect(),
            Some(a) => rows.iter().filter(|r| r.seq > a).cloned().collect(),
        }
    }

    /// Records that were still active at or after row `min_seq` (open ones
    /// always included), oldest first.
    pub fn records_since(&self, min_seq: u64) -> Vec<SignalRecord> {
        let st = self.shared.records.lock();
        st.closed.iter().filter(|r| r.last_seq >= min_seq).chain(st.open.iter()).cloned().collect()
    }

    /// The newest `n` records (open first, then most recently closed).
    pub fn recent_records(&self, n: usize) -> Vec<SignalRecord> {
        let st = self.shared.records.lock();
        st.open.iter().cloned().chain(st.closed.iter().rev().cloned()).take(n).collect()
    }

    pub fn sweep_status(&self) -> Option<SweepStatus> {
        self.shared.sweep.lock().clone()
    }

    pub fn stats(&self) -> SourceStats {
        // One lock at a time (see the lock-order note on `Shared`).
        let (ring_samples, ring_capacity, total_samples) = {
            let ring = self.shared.ring.lock();
            (ring.available(), ring.capacity(), ring.head())
        };
        let ffts_per_sec = *self.shared.ffts_per_sec.lock();
        let st = self.shared.state.lock();
        SourceStats {
            gaps: st.gaps,
            lost_samples: st.lost_samples,
            overloads: st.overloads,
            device_lost: st.lost_device,
            ring_samples,
            ring_capacity,
            total_samples,
            ffts_per_sec,
        }
    }

    /// Copies `out.len()` samples starting at `start` out of the look-back ring.
    pub fn read_iq(&self, start: u64, out: &mut [Complex32]) -> bool {
        self.shared.ring.lock().read(start, out)
    }

    pub fn ring_head(&self) -> u64 {
        self.shared.ring.lock().head()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop_worker();
        let _ = self.device.lock().stop();
    }
}

fn live_worker(shared: Arc<Shared>, cfg: SourceConfig, stop: Arc<AtomicBool>, mut det: BurstDetector) {
    let n = cfg.fft_size;
    let mut sa = SpectrumAnalyzer::new(n);
    let mut block = vec![Complex32::default(); n];
    let mut cursor: Option<u64> = None;
    let mut seq = 0u64;
    let row_interval = Duration::from_secs_f64(1.0 / cfg.rows_per_sec);
    let mut next_row = Instant::now() + row_interval;
    let mut fft_count = 0u64;
    let mut fft_window = Instant::now();
    let mut db = Vec::with_capacity(n);

    while !stop.load(Ordering::Relaxed) {
        let (head, oldest) = {
            let r = shared.ring.lock();
            (r.head(), r.oldest())
        };
        let cur = cursor.get_or_insert(head.saturating_sub(n as u64));
        // Fell behind the ring's tail: skip forward rather than read overwritten data.
        if *cur < oldest {
            *cur = head.saturating_sub(n as u64);
        }
        let mut did_work = false;
        // Cap the work per pass so rows keep flowing even if the CPU can't keep up.
        let mut budget = 64;
        while *cur + n as u64 <= head && budget > 0 {
            if shared.ring.lock().read(*cur, &mut block) {
                sa.push(&block);
                fft_count += 1;
            }
            *cur += n as u64;
            budget -= 1;
            did_work = true;
        }
        if budget == 0 {
            // Can't keep up: drop to the live edge.
            *cur = head.saturating_sub(n as u64);
        }

        let now = Instant::now();
        if now >= next_row {
            next_row += row_interval;
            if next_row < now {
                next_row = now + row_interval;
            }
            if sa.take_db(&mut db) {
                let tuning = {
                    let st = shared.state.lock();
                    Tuning { center_hz: st.center_hz, span_hz: st.sample_rate }
                };
                seq += 1;
                let row = SpectrumRow {
                    seq,
                    tuning,
                    end_sample: *cur,
                    time: SystemTime::now(),
                    db: std::mem::replace(&mut db, Vec::with_capacity(n)),
                };
                shared.detect(&mut det, &row);
                shared.push_row(row, cfg.row_history);
            }
        }
        let el = fft_window.elapsed();
        if el >= Duration::from_secs(1) {
            *shared.ffts_per_sec.lock() = fft_count as f64 / el.as_secs_f64();
            fft_count = 0;
            fft_window = Instant::now();
        }
        if !did_work {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
