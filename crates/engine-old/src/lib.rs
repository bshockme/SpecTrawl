//! Per-source pipeline: driver → IQ ring (look-back) → spectrum rows.
//! A source runs in one of two modes: `Live` (fixed tuning, FFT rows at a
//! steady rate) or `Sweep` (retune-and-dwell across a span, one stitched row
//! per pass). See DESIGN.md §5.

mod bursts;
mod calls;
mod cell_monitor;
mod classify;
mod emitters;
pub mod explore;
pub mod hop_profile;
mod listen;
mod monitor;
mod occupancy;
mod rds_monitor;
mod record_all;
mod ring;
pub mod sigmf;
pub mod store;
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
pub use calls::CallEvent;
pub use classify::ClassifierLoad;
pub use detect::demod::Mode as DemodMode;
pub use detect::lte::LteCell;
pub use detect::p25::SystemInfo as P25System;
pub use detect::spectral::{EMPTY_DB, is_empty_db};
pub use detect::trunk::{SystemKey, SystemKind};
pub use emitters::Emitter;
pub use listen::{AudioQueue, FollowPolicy, ListenSpec, Playback, VoiceStatus, recordings_dir};
pub use monitor::{GrantEvent, MonitorInfo, PageEvent, TrunkSystem};
pub use occupancy::KnownOccupant;
pub use rds_monitor::StationInfo;
pub use record_all::{RecordAllConfig, RecordAllStatus};
pub use ring::IqRing;
pub use store::Store;
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

/// One waterfall row: power spectrum in dBFS, lowest frequency first. Bins
/// a segmented sweep never measured hold `EMPTY_DB` (test with
/// `is_empty_db`); detectors and the floor estimates skip them, and the view
/// should draw them as gaps.
#[derive(Debug)]
pub struct SpectrumRow {
    pub seq: u64,
    pub tuning: Tuning,
    /// Sample index of the newest sample that went into this row.
    pub end_sample: u64,
    pub time: SystemTime,
    pub db: Vec<f32>,
}

/// A stretch of the stream with one tuning: from `start` up to the next
/// epoch's start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Epoch {
    pub start: u64,
    pub center_hz: f64,
    pub sample_rate: f64,
}

/// IQ this close to a reported retune (either side) is not trusted: the
/// driver reports the new tuning at a transfer boundary, the tuner moved
/// somewhere inside the previous transfer, and its PLL settles after that.
const RETUNE_GUARD_S: f64 = 0.01;

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
    /// Tuning history, oldest first, pruned to what the ring still holds.
    /// The ring keeps IQ from before and after a retune; this says which
    /// tuning each sample was taken at.
    epochs: VecDeque<Epoch>,
    /// Sample ranges `[start, end)` holding no usable IQ: zero-filled
    /// losses and the settling interval around each retune.
    holes: VecDeque<(u64, u64)>,
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

    fn retune(&mut self, at_sample: u64, center_hz: f64, sample_rate: f64) {
        self.center_hz = center_hz;
        self.sample_rate = sample_rate;
        self.epochs.push_back(Epoch { start: at_sample, center_hz, sample_rate });
        let guard = (RETUNE_GUARD_S * sample_rate) as u64;
        self.holes.push_back((at_sample.saturating_sub(guard), at_sample + guard));
    }

    fn lost(&mut self, at_sample: u64, lost_samples: u64) {
        self.gaps += 1;
        self.lost_samples += lost_samples;
        self.holes.push_back((at_sample, at_sample + lost_samples));
    }

    /// Drops history the ring no longer holds (`oldest`: the ring's oldest
    /// sample). The epoch in force at `oldest` is kept.
    fn prune(&mut self, oldest: u64) {
        while self.epochs.len() > 1 && self.epochs[1].start <= oldest {
            self.epochs.pop_front();
        }
        self.holes.retain(|&(_, end)| end > oldest);
    }

    /// The epoch holding `sample`: the newest that starts at or before it.
    fn epoch_at(&self, sample: u64) -> Option<Epoch> {
        self.epochs.iter().rev().find(|e| e.start <= sample).copied()
    }

    /// Tuning at `sample` (the current one before any epoch is known).
    fn tuning_at(&self, sample: u64) -> Tuning {
        match self.epoch_at(sample) {
            Some(e) => Tuning { center_hz: e.center_hz, span_hz: e.sample_rate },
            None => Tuning { center_hz: self.center_hz, span_hz: self.sample_rate },
        }
    }

    fn in_hole(&self, pos: u64) -> Option<u64> {
        self.holes.iter().find(|&&(s, e)| s <= pos && pos < e).map(|&(_, e)| e)
    }

    /// What one DSP chain may consume from `pos` as a continuous stream of
    /// one tuning: `Ok(n)`, at most `max` samples, stopping short of the
    /// next retune or hole; or `Err(resume)` when `pos` lies in a hole,
    /// with the first sample after it (which may lie in another hole: call
    /// again). A chain that skips should reset its filters.
    fn run_from(&self, pos: u64, max: u64) -> Result<u64, u64> {
        if let Some(end) = self.in_hole(pos) {
            return Err(end);
        }
        let mut end = pos.saturating_add(max);
        for e in &self.epochs {
            if e.start > pos && e.start < end {
                end = e.start;
            }
        }
        for &(s, _) in &self.holes {
            if s > pos && s < end {
                end = s;
            }
        }
        Ok(end - pos)
    }

    /// The earliest `p` such that `p..end` is one tuning with no hole: a
    /// chain that wants to look back from `end` must not start before it.
    fn continuous_from(&self, end: u64) -> u64 {
        let mut p = self.epoch_at(end.saturating_sub(1)).map_or(0, |e| e.start);
        for &(s, e) in &self.holes {
            if s < end {
                p = p.max(e);
            }
        }
        p
    }

    /// The epochs covering `start..end`, the first clipped to `start`.
    fn segments(&self, start: u64, end: u64) -> Vec<Epoch> {
        let mut v = Vec::new();
        match self.epoch_at(start) {
            Some(e) => v.push(Epoch { start, ..e }),
            None => v.push(Epoch { start, center_hz: self.center_hz, sample_rate: self.sample_rate }),
        }
        v.extend(self.epochs.iter().filter(|e| e.start > start && e.start < end).copied());
        v
    }

    /// Holes overlapping `start..end`, clipped to it.
    fn holes_in(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        self.holes.iter().filter(|&&(s, e)| s < end && e > start).map(|&(s, e)| (s.max(start), e.min(end))).collect()
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
        let em = r.emitter.and_then(|id| self.emitters.get(id));
        r.emitter_class = em.and_then(|e| e.best.clone());
        r.emitter_first_seen = em.map(|e| e.first_seen);
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
    /// The sweep's row in progress: the stitched row as it stands part way
    /// through a pass, refreshed every `PARTIAL_EVERY` while hops arrive.
    partial: Mutex<Option<Arc<SpectrumRow>>>,
    /// Frequency ranges (Hz) the sweep blanks before detection: the other
    /// receivers' local-oscillator leakage (an Airspy radiates at its centre
    /// + 5 MHz), which moves whenever they retune.
    masks: Mutex<Vec<(f64, f64)>>,
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
    /// "Record every signal": the mode's settings while it is on, and its counters.
    record_all: Mutex<Option<RecordAllConfig>>,
    record_all_status: Mutex<RecordAllStatus>,
    /// The gain last set through `Source::set_gain` (None: the backend's
    /// opening default); part of the saved hop profile's key.
    gain: Mutex<Option<sdr_core::GainSetting>>,
    /// Where records, classifications and emitters are filed, if anywhere.
    store: Mutex<Option<store::SourceStore>>,
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
            let em = st.emitters.observe(&r, true);
            if let Some(db) = self.store.lock().as_ref() {
                db.record(&r, Some(em));
                if let Some(e) = st.emitters.get(em) {
                    db.emitter(e);
                }
            }
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
        // One lock at a time (see the lock-order note on `Shared`).
        let oldest = self.shared.ring.lock().oldest();
        let mut st = self.shared.state.lock();
        match event {
            StreamEvent::Retune { at_sample, center_hz, sample_rate } => {
                st.retune(at_sample, center_hz, sample_rate);
                st.prune(oldest);
            }
            StreamEvent::Gap { at_sample, lost_samples } => {
                st.lost(at_sample, lost_samples);
                st.prune(oldest);
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
            partial: Mutex::new(None),
            masks: Mutex::new(Vec::new()),
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
            record_all: Mutex::new(None),
            record_all_status: Mutex::new(Default::default()),
            gain: Mutex::new(None),
            store: Mutex::new(None),
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
        match &mode {
            Mode::Live => log::info!(
                "{}: live at {:.4} MHz, ring {:.0} s",
                self.info.serial,
                self.device.lock().center_hz() / 1e6,
                self.cfg.ring_seconds
            ),
            Mode::Sweep(p) => log::info!(
                "{}: sweep {:.1}–{:.1} MHz, ring {:.0} s",
                self.info.serial,
                p.start_hz / 1e6,
                p.stop_hz / 1e6,
                self.cfg.ring_seconds
            ),
        }
        self.stop_worker();
        self.shared.rows.lock().clear();
        *self.shared.records.lock() = RecordStore::default();
        *self.shared.sweep.lock() = None;
        *self.shared.partial.lock() = None;
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
                let (sh, s) = (shared.clone(), st.clone());
                handles.push(std::thread::Builder::new().name("rds".into()).spawn(move || rds_monitor::run(sh, s, rate)));
                let (sh, s, c) = (shared.clone(), st.clone(), cfg.clone());
                let hw = format!("{} {}", self.info.label, self.info.serial);
                handles.push(std::thread::Builder::new().name("record".into()).spawn(move || record_all::run(sh, s, rate, c, hw)));
                let (sh, s) = (shared.clone(), st.clone());
                handles.push(std::thread::Builder::new().name("cell".into()).spawn(move || cell_monitor::run(sh, s, rate)));
                let caps = self.device.lock().capabilities().clone();
                handles.push(std::thread::Builder::new().name("classify".into()).spawn(move || classify::run(shared, cfg, st, rate, caps)));
            }
            Mode::Sweep(plan) => {
                let (dev, plan) = (self.device.clone(), plan.clone());
                let det = BurstDetector::new(DetectorConfig::sweep(), serial.clone());
                handles.push(
                    std::thread::Builder::new().name("sweep".into()).spawn(move || sweep::run(shared, dev, plan, cfg, st, det, serial)),
                );
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
        log::debug!("{}: tune {:.4} MHz", self.info.serial, hz / 1e6);
        if let Some(db) = self.shared.store.lock().as_ref() {
            db.note(&format!("tune {:.4} MHz", hz / 1e6));
        }
        self.device.lock().set_center(hz)
    }

    pub fn set_gain(&self, gain: sdr_core::GainSetting) -> sdr_core::Result<()> {
        // One lock at a time (see the lock-order note on `Shared`).
        let r = self.device.lock().set_gain(gain);
        if r.is_ok() {
            *self.shared.gain.lock() = Some(gain);
        }
        r
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

    /// Puts a label on an emitter from outside the classifier (an explore
    /// verdict made on another receiver).
    pub fn label_emitter(&self, id: u64, class: Arc<detect::Classification>) {
        let mut st = self.shared.records.lock();
        st.emitters.adopt(id, class);
        if let (Some(db), Some(e)) = (self.shared.store.lock().as_ref(), st.emitters.get(id)) {
            db.emitter(e);
        }
    }

    /// Files this source's records, classifications and emitters in
    /// `store` from now on (None stops).
    pub fn set_store(&self, store: Option<&Store>) {
        let s = store.map(|s| s.for_source(&self.info.serial));
        if let Some(s) = &s {
            s.note(&format!("source opened: {} {:?}", self.info.serial, self.mode_name()));
        }
        *self.shared.store.lock() = s;
    }

    fn mode_name(&self) -> String {
        match &self.mode {
            Mode::Live => format!("live at {:.4} MHz", self.device.lock().center_hz() / 1e6),
            Mode::Sweep(p) => format!("sweep {:.1}-{:.1} MHz", p.start_hz / 1e6, p.stop_hz / 1e6),
        }
    }

    /// The store this source files into, if any.
    pub fn store(&self) -> Option<store::SourceStore> {
        self.shared.store.lock().clone()
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

    /// Turns "record every signal" on (with its settings) or off. Only a
    /// live source records; a sweep ignores it. Switching off finishes the
    /// captures in progress.
    pub fn set_record_all(&self, cfg: Option<RecordAllConfig>) {
        *self.shared.record_all.lock() = cfg;
    }

    pub fn record_all(&self) -> Option<RecordAllConfig> {
        self.shared.record_all.lock().clone()
    }

    pub fn record_all_status(&self) -> RecordAllStatus {
        self.shared.record_all_status.lock().clone()
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

    /// Known wideband occupants (FM broadcast, TV, LTE/NR) identified so far
    /// (including hinted ones, see `set_hints`).
    pub fn occupants(&self) -> Vec<KnownOccupant> {
        self.shared.occupancy.lock().known.clone()
    }

    /// Occupants another source (the survey) knows inside this source's
    /// span, so the station and cell monitors can work on carriers this
    /// source can't judge itself (an LTE carrier filling the whole span).
    /// Replaces the previous hints.
    pub fn set_hints(&self, found: Vec<detect::spectral::Occupant>) {
        self.shared.occupancy.lock().hint(found, SystemTime::now());
    }

    /// Writes part of the look-back ring to SigMF on a background thread.
    /// Returns a handle to join for the result.
    pub fn export(&self, req: sigmf::ExportRequest) -> JoinHandle<Result<sigmf::ExportResult, String>> {
        let ctx = sigmf::ExportContext { shared: self.shared.clone(), hw: format!("{} {}", self.info.label, self.info.serial) };
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

    /// Every trunked system the monitors have identified, any protocol.
    pub fn systems(&self) -> Vec<TrunkSystem> {
        self.shared.p25.lock().systems()
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

    /// The sweep's row in progress (hops measured so far this pass pasted
    /// over the previous pass), for views that advance on a clock rather
    /// than per pass. Its `seq` is the pass row it will become.
    pub fn sweep_partial(&self) -> Option<Arc<SpectrumRow>> {
        self.shared.partial.lock().clone()
    }

    /// Ranges (Hz) a sweeping source blanks in every hop before detection,
    /// e.g. where another receiver's local oscillator leaks in. Replaces the
    /// previous list; takes effect from the next hop.
    pub fn set_masks(&self, masks: Vec<(f64, f64)>) {
        *self.shared.masks.lock() = masks;
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
    // Start of the epoch the accumulated FFTs were taken in: a row averages
    // IQ of one tuning only, and is labelled with that tuning.
    let mut row_epoch: Option<u64> = None;

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
            budget -= 1;
            did_work = true;
            // Only whole blocks clear of retunes and losses go into the average.
            let (run, epoch) = {
                let st = shared.state.lock();
                (st.run_from(*cur, n as u64), st.epoch_at(*cur).map(|e| e.start))
            };
            match run {
                Err(resume) => {
                    *cur = resume;
                    continue;
                }
                Ok(m) if m < n as u64 => {
                    *cur += m;
                    continue;
                }
                Ok(_) => {}
            }
            if epoch != row_epoch {
                // Drop what was averaged at the previous tuning.
                if sa.pending() > 0 {
                    sa.take_db(&mut db);
                }
                row_epoch = epoch;
            }
            if shared.ring.lock().read(*cur, &mut block) {
                sa.push(&block);
                fft_count += 1;
            }
            *cur += n as u64;
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
                let tuning = shared.state.lock().tuning_at(row_epoch.unwrap_or(*cur));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_log_splits_at_retunes_and_losses() {
        let mut st = StreamState::default();
        st.retune(0, 100e6, 1e6); // hole 0..10_000
        st.lost(50_000, 1_000); // hole 50_000..51_000
        st.retune(200_000, 101e6, 1e6); // hole 190_000..210_000
        assert_eq!(st.tuning_at(150_000), Tuning { center_hz: 100e6, span_hz: 1e6 });
        assert_eq!(st.tuning_at(200_000), Tuning { center_hz: 101e6, span_hz: 1e6 });
        assert_eq!(st.tuning_at(199_999), Tuning { center_hz: 100e6, span_hz: 1e6 });
        // A chain reading 16k blocks: stopped short of the loss, skipped
        // over it, stopped at the retune's guard, skipped past settling.
        assert_eq!(st.run_from(40_000, 16_384), Ok(10_000));
        assert_eq!(st.run_from(50_000, 16_384), Err(51_000));
        assert_eq!(st.run_from(50_500, 16_384), Err(51_000));
        assert_eq!(st.run_from(51_000, 16_384), Ok(16_384));
        assert_eq!(st.run_from(180_000, 16_384), Ok(10_000));
        assert_eq!(st.run_from(190_000, 16_384), Err(210_000));
        assert_eq!(st.run_from(205_000, 16_384), Err(210_000));
        assert_eq!(st.run_from(210_000, 16_384), Ok(16_384));
        // Looking back from the live edge stays inside the new epoch, past settling.
        assert_eq!(st.continuous_from(300_000), 210_000);
        assert_eq!(st.continuous_from(150_000), 51_000);
        assert_eq!(st.continuous_from(30_000), 10_000);
        // Export segments and holes for a stretch spanning the retune.
        let segs = st.segments(100_000, 300_000);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start, segs[0].center_hz), (100_000, 100e6));
        assert_eq!((segs[1].start, segs[1].center_hz), (200_000, 101e6));
        assert_eq!(st.holes_in(100_000, 300_000), vec![(190_000, 210_000)]);
        assert_eq!(st.holes_in(0, 50_500), vec![(0, 10_000), (50_000, 50_500)]);
        // Pruning keeps the epoch in force at the ring's oldest sample.
        st.prune(199_000);
        assert_eq!(st.epochs.len(), 2);
        assert_eq!(st.holes.len(), 1);
        st.prune(250_000);
        assert_eq!(st.epochs.len(), 1);
        assert_eq!(st.tuning_at(250_000).center_hz, 101e6);
        assert!(st.holes.is_empty());
    }
}
