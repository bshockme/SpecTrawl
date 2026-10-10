//! Per-source pipeline: driver → IQ ring (look-back) → spectrum rows.
//! A source runs in one of two modes: `Live` (fixed tuning, FFT rows at a
//! steady rate) or `Sweep` (retune-and-dwell across a span, one stitched row
//! per pass). See DESIGN.md §5.

mod calls;
mod bursts;
mod classify;
mod emitters;
mod listen;
mod monitor;
mod occupancy;
mod ring;
pub mod sigmf;
mod sweep;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::{Capabilities, Complex32, Device, DeviceInfo, Samples, StreamEvent, StreamSink};

pub use bursts::{BurstDetector, DetectorConfig, SignalRecord};
pub use classify::ClassifierLoad;
pub use detect::demod::Mode as DemodMode;
pub use detect::p25::SystemInfo as P25System;
pub use emitters::Emitter;
pub use listen::{AudioQueue, FollowPolicy, ListenSpec, Playback, VoiceStatus, recordings_dir};
pub use calls::CallEvent;
pub use monitor::{GrantEvent, MonitorInfo, PageEvent};
pub use occupancy::KnownOccupant;
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
    /// Latest (sample index, wall time) pair from a TimeAnchor event.
    anchor: Option<(u64, SystemTime)>,
}

impl StreamState {
    /// Wall-clock time of a sample, from the latest time anchor.
    fn time_of(&self, sample: u64, rate: f64) -> SystemTime {
        match self.anchor {
            Some((at, t)) if sample >= at => t + Duration::from_secs_f64((sample - at) as f64 / rate),
            Some((at, t)) => t - Duration::from_secs_f64((at - sample) as f64 / rate),
            None => SystemTime::now(),
        }
    }
}

/// Finished records (bounded) plus the snapshot of still-open ones, and the
/// classifier's results keyed by record id.
#[derive(Default)]
struct RecordStore {
    closed: VecDeque<SignalRecord>,
    open: Vec<SignalRecord>,
    /// Result, when it was made, and whether the record was still open then.
    classes: HashMap<u64, (Arc<detect::Classification>, Instant, bool)>,
    emitters: emitters::EmitterBook,
}

impl RecordStore {
    fn with_class(&self, r: &SignalRecord) -> SignalRecord {
        let mut r = r.clone();
        r.class = self.classes.get(&r.id).map(|c| c.0.clone());
        r.emitter = self.emitters.by_record.get(&r.id).copied();
        r.emitter_class = r.emitter.and_then(|id| self.emitters.get(id)).and_then(|e| e.best.clone());
        r
    }
}

const MAX_CLOSED_RECORDS: usize = 20_000;

/// State shared between the driver callback, the worker and the UI.
///
/// Locking rule: hold at most one of these mutexes at a time. Copy what you
/// need out and drop the guard before taking another. (A UI read of
/// state→ring against a sweep read of ring→state deadlocked the app.)
struct Shared {
    ring: Mutex<IqRing>,
    /// Notified (with `ring`) each time a block of samples arrives.
    arrived: parking_lot::Condvar,
    records: Mutex<RecordStore>,
    state: Mutex<StreamState>,
    rows: Mutex<VecDeque<Arc<SpectrumRow>>>,
    sweep: Mutex<Option<SweepStatus>>,
    ffts_per_sec: Mutex<f64>,
    classifier_load: Mutex<classify::ClassifierLoad>,
    p25: Mutex<monitor::P25Book>,
    pager: Mutex<monitor::PagerBook>,
    calls: Mutex<calls::CallLog>,
    occupancy: Mutex<occupancy::OccupancyTracker>,
    /// The channel being listened to, if any, and its audio.
    listen: Mutex<Option<ListenSpec>>,
    audio: Arc<AudioQueue>,
    voice: Mutex<VoiceStatus>,
    follow_policy: Mutex<FollowPolicy>,
}

impl Shared {
    /// Runs the detector on a row and files what it reports.
    fn detect(&self, det: &mut BurstDetector, row: &SpectrumRow) {
        let closed = det.process(row);
        let mut st = self.records.lock();
        for r in closed {
            if st.closed.len() >= MAX_CLOSED_RECORDS {
                if let Some(old) = st.closed.pop_front() {
                    st.classes.remove(&old.id);
                    st.emitters.by_record.remove(&old.id);
                }
            }
            st.emitters.observe(&r, true);
            st.closed.push_back(r);
        }
        let open = det.open_records();
        for r in &open {
            st.emitters.observe(r, false);
        }
        st.emitters.mark_active(&open);
        st.open = open;
        drop(st);
        self.occupancy.lock().push(row);
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
        self.shared.arrived.notify_all();
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
            StreamEvent::TimeAnchor { at_sample, host_time } => st.anchor = Some((at_sample, host_time)),
            StreamEvent::GainChange { .. } => {}
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

/// A running source: owns the device, its IQ ring and its worker threads.
pub struct Source {
    device: SharedDevice,
    info: DeviceInfo,
    shared: Arc<Shared>,
    cfg: SourceConfig,
    mode: Mode,
    worker: Option<(Arc<AtomicBool>, Vec<JoinHandle<()>>)>,
}

impl Source {
    pub fn start(mut device: Box<dyn Device>, cfg: SourceConfig, mode: Mode) -> sdr_core::Result<Self> {
        let rate = device.sample_rate();
        let cap = (rate * cfg.ring_seconds).ceil() as usize;
        let shared = Arc::new(Shared {
            ring: Mutex::new(IqRing::new(cap)),
            arrived: parking_lot::Condvar::new(),
            records: Mutex::new(RecordStore::default()),
            state: Mutex::new(StreamState { center_hz: device.center_hz(), sample_rate: rate, ..Default::default() }),
            rows: Mutex::new(VecDeque::with_capacity(cfg.row_history)),
            sweep: Mutex::new(None),
            ffts_per_sec: Mutex::new(0.0),
            classifier_load: Mutex::new(Default::default()),
            p25: Mutex::new(Default::default()),
            pager: Mutex::new(Default::default()),
            calls: Mutex::new(Default::default()),
            occupancy: Mutex::new(occupancy::OccupancyTracker::new(1)),
            listen: Mutex::new(None),
            audio: Arc::new(AudioQueue::default()),
            voice: Mutex::new(Default::default()),
            follow_policy: Mutex::new(Default::default()),
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
        // Identify on ~2 s of live rows, or two sweep passes.
        let every = match &mode {
            Mode::Live => (2.0 * self.cfg.rows_per_sec) as u32,
            Mode::Sweep(_) => 2,
        };
        *self.shared.occupancy.lock() = occupancy::OccupancyTracker::new(every);
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, cfg, st) = (self.shared.clone(), self.cfg.clone(), stop.clone());
        let serial = self.info.serial.clone();
        let mut handles = Vec::new();
        match &mode {
            Mode::Live => {
                let usable = self.device.lock().capabilities().usable_bandwidth_fraction as f32;
                let det = BurstDetector::new(DetectorConfig { usable_fraction: usable, ..DetectorConfig::live(cfg.rows_per_sec) }, serial);
                let (sh, c, s) = (shared.clone(), cfg.clone(), st.clone());
                handles.push(std::thread::Builder::new().name("spectrum".into()).spawn(move || live_worker(sh, c, s, det)));
                let rate = self.device.lock().sample_rate();
                let (sh, s) = (shared.clone(), st.clone());
                handles.push(std::thread::Builder::new().name("monitor".into()).spawn(move || monitor::run(sh, s, rate)));
                let (sh, s) = (shared.clone(), st.clone());
                handles.push(std::thread::Builder::new().name("listen".into()).spawn(move || listen::run(sh, s, rate)));
                let caps = self.device.lock().capabilities().clone();
                handles.push(std::thread::Builder::new().name("classify".into()).spawn(move || classify::run(shared, cfg, st, rate, caps)));
            }
            Mode::Sweep(plan) => {
                let (dev, plan) = (self.device.clone(), plan.clone());
                let det = BurstDetector::new(DetectorConfig::sweep(), serial);
                handles.push(std::thread::Builder::new().name("sweep".into()).spawn(move || sweep::run(shared, dev, plan, cfg, st, det)));
            }
        }
        let handles = handles.into_iter().map(|h| h.expect("spawn worker")).collect();
        self.worker = Some((stop, handles));
        self.mode = mode;
    }

    fn stop_worker(&mut self) {
        if let Some((stop, hs)) = self.worker.take() {
            stop.store(true, Ordering::Relaxed);
            for h in hs {
                let _ = h.join();
            }
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
        st.closed.iter().filter(|r| r.last_seq >= min_seq).chain(st.open.iter()).map(|r| st.with_class(r)).collect()
    }

    /// The newest `n` records (open first, then most recently closed).
    pub fn recent_records(&self, n: usize) -> Vec<SignalRecord> {
        let st = self.shared.records.lock();
        st.open.iter().chain(st.closed.iter().rev()).take(n).map(|r| st.with_class(r)).collect()
    }

    /// All emitters, active first, then most recently seen.
    pub fn emitters(&self) -> Vec<Emitter> {
        let mut v: Vec<Emitter> = self.shared.records.lock().emitters.map.values().cloned().collect();
        v.sort_by(|a, b| b.active.cmp(&a.active).then(b.last_seen.cmp(&a.last_seen)));
        v
    }

    /// Starts (or changes, or with None stops) listening to a channel. Only
    /// a live source listens; a sweep ignores it.
    pub fn listen(&self, spec: Option<ListenSpec>) {
        *self.shared.listen.lock() = spec;
    }

    pub fn listening(&self) -> Option<ListenSpec> {
        *self.shared.listen.lock()
    }

    /// Talkgroups following prefers or ignores.
    pub fn set_follow_policy(&self, p: FollowPolicy) {
        *self.shared.follow_policy.lock() = p;
    }

    /// The digital voice channel being played, if any.
    pub fn voice(&self) -> VoiceStatus {
        self.shared.voice.lock().clone()
    }

    /// The audio the listener produces, for an audio output to drain.
    pub fn audio(&self) -> Arc<AudioQueue> {
        self.shared.audio.clone()
    }

    /// Known wideband occupants (FM broadcast, TV, LTE/NR) identified so far.
    pub fn occupants(&self) -> Vec<KnownOccupant> {
        self.shared.occupancy.lock().known.clone()
    }

    /// Writes part of the look-back ring to SigMF on a background thread.
    /// Returns a handle to join for the result.
    pub fn export(&self, req: sigmf::ExportRequest) -> JoinHandle<Result<sigmf::ExportResult, String>> {
        let snap = self.snapshot();
        let ctx = sigmf::ExportContext {
            shared: self.shared.clone(),
            sample_rate: snap.sample_rate,
            tuner_center_hz: snap.center_hz,
            hw: format!("{} {}", snap.info.label, snap.info.serial),
        };
        std::thread::Builder::new().name("export".into()).spawn(move || sigmf::run(ctx, req)).expect("spawn export")
    }

    /// Ring sample range a record's IQ occupies (with a little margin),
    /// for exports.
    pub fn record_sample_range(&self, r: &SignalRecord) -> (u64, u64) {
        let rate = self.snapshot().sample_rate;
        let per_row = (rate / self.cfg.rows_per_sec) as u64;
        let margin = (0.05 * rate) as u64;
        (r.first_row_end_sample.saturating_sub(per_row + margin), r.last_row_end_sample + margin)
    }

    /// The ring's held range (oldest, head) in samples.
    pub fn ring_range(&self) -> (u64, u64) {
        let r = self.shared.ring.lock();
        (r.oldest(), r.head())
    }

    /// P25 systems learned by the control-channel monitors, keyed by NAC.
    pub fn p25_systems(&self) -> Vec<(u16, P25System)> {
        let mut v: Vec<(u16, P25System)> = self.shared.p25.lock().systems.iter().map(|(k, v)| (*k, v.clone())).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    /// The newest `n` talkgroup grants, newest first.
    pub fn p25_grants(&self, n: usize) -> Vec<GrantEvent> {
        self.shared.p25.lock().grants.iter().rev().take(n).cloned().collect()
    }

    /// The newest `n` calls (grants and voice-channel sightings merged),
    /// newest first.
    pub fn calls(&self, n: usize) -> Vec<CallEvent> {
        self.shared.calls.lock().calls.iter().rev().take(n).cloned().collect()
    }

    /// The newest `n` pager messages (FLEX, POCSAG), newest first.
    pub fn pages(&self, n: usize) -> Vec<PageEvent> {
        self.shared.pager.lock().pages.iter().rev().take(n).cloned().collect()
    }

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.shared.p25.lock().monitors.clone()
    }

    pub fn classifier_load(&self) -> ClassifierLoad {
        *self.shared.classifier_load.lock()
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
