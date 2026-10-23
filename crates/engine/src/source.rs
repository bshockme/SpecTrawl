//! A streaming device wired to its block ring, segment table and timeline.
//!
//! The device's callback ([`Sink`]) writes samples into the ring without a
//! lock; it takes the segment table's mutex only for `Retune`/`Gap` events
//! and on ring starvation (which is a gap). The keeper thread applies the
//! budget's allowance, housekeeps the ring, prunes and runs the device
//! watchdog through [`Housekept::tick`].

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use sdr_core::{Capabilities, Complex32, Device, DeviceInfo, GainSetting, Samples, StreamEvent, StreamSink};

use crate::budget::{Allowance, Mode, RingBudget, SourceId};
use crate::ring::keeper::{Housekept, Keeper};
use crate::ring::{self, IqRing, RingConfig, RingStats};
use crate::segments::{Boundary, Segment, SegmentTable};
use crate::timeline::Timeline;

/// Seconds of samples the keeper keeps provisioned ahead of the writer.
const PROVISION_AHEAD_SECS: f64 = 0.25;

/// Blocks covering [`PROVISION_AHEAD_SECS`] at `rate`, between 2 and 64.
fn provision_ahead(rate: f64, block_samples: u64) -> u32 {
    let blocks = (PROVISION_AHEAD_SECS * rate / block_samples as f64).ceil();
    if blocks.is_finite() { (blocks as u32).clamp(2, 64) } else { 2 }
}

/// How long a device may go without samples before the watchdog asks
/// whether it is still streaming.
const WATCHDOG_SILENCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub ring: RingConfig,
    /// Share of the opportunistic budget (0 = guaranteed window only).
    pub weight: f32,
}

impl Default for SourceConfig {
    fn default() -> Self {
        SourceConfig { ring: RingConfig::default(), weight: 1.0 }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub info: DeviceInfo,
    pub caps: Capabilities,
    pub center_hz: f64,
    pub sample_rate: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SourceStats {
    pub ring: RingStats,
    /// Seconds of history the ring holds, at the current rate.
    pub held_secs: f64,
    pub guaranteed_secs: f64,
    pub ceiling_secs: f64,
    pub gaps: u64,
    pub lost_samples: u64,
    pub overloads: u64,
    pub device_lost: bool,
    pub segments: usize,
}

/// [`ring::ReadError`] plus the segment check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    Overwritten,
    Lost,
    Future,
    /// The range crosses a retune or a gap.
    SpansSegments,
}

impl From<ring::ReadError> for ReadError {
    fn from(e: ring::ReadError) -> Self {
        match e {
            ring::ReadError::Overwritten => ReadError::Overwritten,
            ring::ReadError::Lost => ReadError::Lost,
            ring::ReadError::Future => ReadError::Future,
        }
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Nanoseconds from a process epoch to `at`.
fn nanos_at(at: Instant) -> u64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    at.saturating_duration_since(epoch).as_nanos() as u64
}

fn mode_to_u8(m: Mode) -> u8 {
    match m {
        Mode::Live => 0,
        Mode::Sweep => 1,
    }
}

fn mode_from_u8(v: u8) -> Mode {
    if v == 0 { Mode::Live } else { Mode::Sweep }
}

type SharedDevice = Arc<Mutex<Box<dyn Device>>>;

struct SourceInner {
    id: SourceId,
    ring: Arc<IqRing>,
    segments: SegmentTable,
    timeline: Timeline,
    mode: AtomicU8,
    gaps: AtomicU64,
    lost_samples: AtomicU64,
    overloads: AtomicU64,
    device_lost: AtomicBool,
    /// Nanoseconds since `EPOCH` of the last `on_samples`.
    last_samples_at: AtomicU64,
    device: Weak<Mutex<Box<dyn Device>>>,
    budget: Arc<RingBudget>,
}

impl SourceInner {
    fn mode(&self) -> Mode {
        mode_from_u8(self.mode.load(Ordering::SeqCst))
    }

    fn apply(&self, a: Allowance) {
        self.ring.set_ceiling_blocks(a.ceiling_blocks);
        self.ring.set_guaranteed_samples(a.guaranteed_samples);
    }

    /// Driver thread. No lock unless the ring starved (a gap).
    fn on_samples(&self, first_sample: u64, samples: Samples<'_>) {
        let out = self.ring.write(first_sample, samples);
        if out.starved > 0 {
            // The ring moved `oldest` up to the new head: what follows is a new run.
            self.lost_samples.fetch_add(out.starved, Ordering::Relaxed);
            self.gaps.fetch_add(1, Ordering::Relaxed);
            let cur = self.segments.current();
            self.segments.push(self.ring.head(), cur.center_hz, cur.sample_rate, Boundary::Gap { lost_samples: out.starved });
        }
        self.last_samples_at.store(nanos_at(Instant::now()), Ordering::Relaxed);
    }

    /// Driver thread. Takes the segment table's lock for `Retune` and `Gap` only.
    fn on_event(&self, event: StreamEvent) {
        match event {
            StreamEvent::Retune { at_sample, center_hz, sample_rate } => {
                let cur = self.segments.current();
                if cur.center_hz == center_hz && cur.sample_rate == sample_rate {
                    // The stream-start announcement of the tuning we began with.
                    return;
                }
                self.segments.push(at_sample, center_hz, sample_rate, Boundary::Retune);
                if sample_rate != cur.sample_rate {
                    self.timeline.set_rate(at_sample, sample_rate);
                    self.budget.update(self.id, sample_rate, self.mode());
                }
            }
            StreamEvent::Gap { at_sample, lost_samples } => {
                let cur = self.segments.current();
                self.segments.push(at_sample + lost_samples, cur.center_hz, cur.sample_rate, Boundary::Gap { lost_samples });
                self.gaps.fetch_add(1, Ordering::Relaxed);
                self.lost_samples.fetch_add(lost_samples, Ordering::Relaxed);
            }
            StreamEvent::TimeAnchor { at_sample, host_time } => self.timeline.anchor(at_sample, host_time),
            StreamEvent::Overload { .. } => {
                self.overloads.fetch_add(1, Ordering::Relaxed);
            }
            StreamEvent::DeviceLost => {
                if !self.device_lost.swap(true, Ordering::SeqCst) {
                    log::warn!("source {}: device lost", self.id.0);
                }
            }
            StreamEvent::GainChange { .. } => {}
        }
    }

    /// No samples for `WATCHDOG_SILENCE` and the device says it is not
    /// streaming: flag it lost. `try_lock`, so a UI call holding the device
    /// never blocks the keeper (the check simply waits for a later tick).
    fn watchdog(&self, now: Instant) {
        if self.device_lost.load(Ordering::SeqCst) {
            return;
        }
        let silent = nanos_at(now).saturating_sub(self.last_samples_at.load(Ordering::Relaxed));
        if silent < WATCHDOG_SILENCE.as_nanos() as u64 {
            return;
        }
        let Some(device) = self.device.upgrade() else { return };
        let Some(d) = device.try_lock() else { return };
        if !d.is_streaming() && !self.device_lost.swap(true, Ordering::SeqCst) {
            log::warn!("source {}: no samples for {:.1} s and the device is not streaming; device lost", self.id.0, silent as f64 / 1e9);
        }
    }
}

impl Housekept for SourceInner {
    fn id(&self) -> SourceId {
        self.id
    }

    fn tick(&self, now: Instant, allowance: Allowance) {
        self.apply(allowance);
        self.ring.set_provision_ahead(provision_ahead(self.segments.current().sample_rate, self.ring.block_samples()));
        self.ring.housekeep();
        let oldest = self.ring.oldest();
        self.segments.prune_before(oldest);
        self.timeline.prune_before(oldest);
        self.watchdog(now);
    }
}

/// The device's view of a source, called on the driver's thread.
struct Sink {
    inner: Arc<SourceInner>,
}

impl StreamSink for Sink {
    fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
        self.inner.on_samples(first_sample, samples);
    }

    fn on_event(&mut self, event: StreamEvent) {
        self.inner.on_event(event);
    }
}

pub struct Source {
    inner: Arc<SourceInner>,
    device: SharedDevice,
    id: SourceId,
}

impl Source {
    /// Build the ring, segment table and timeline, join the budget, register
    /// with the keeper and start the device streaming into the ring.
    pub fn start(device: Box<dyn Device>, cfg: SourceConfig, mode: Mode, keeper: &Arc<Keeper>) -> sdr_core::Result<Source> {
        let id = SourceId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let (center, rate) = (device.center_hz(), device.sample_rate());
        let device: SharedDevice = Arc::new(Mutex::new(device));
        let budget = keeper.budget().clone();
        let inner = Arc::new(SourceInner {
            id,
            ring: IqRing::new(cfg.ring.clone(), ring::volatile::system()),
            segments: SegmentTable::new(0, center, rate),
            timeline: Timeline::new(rate),
            mode: AtomicU8::new(mode_to_u8(mode)),
            gaps: AtomicU64::new(0),
            lost_samples: AtomicU64::new(0),
            overloads: AtomicU64::new(0),
            device_lost: AtomicBool::new(false),
            last_samples_at: AtomicU64::new(nanos_at(Instant::now())),
            device: Arc::downgrade(&device),
            budget: budget.clone(),
        });
        budget.join(id, rate, mode, cfg.weight, cfg.ring.block_bytes);
        // One pass before the keeper knows about this ring (so there is still a
        // single housekeeper): the writer's first blocks exist before its first sample.
        if let Some(&(_, a)) = budget.allowances().iter().find(|(i, _)| *i == id) {
            inner.apply(a);
        }
        inner.ring.set_provision_ahead(provision_ahead(rate, inner.ring.block_samples()));
        inner.ring.housekeep();
        let member: Arc<dyn Housekept> = inner.clone();
        keeper.register(Arc::downgrade(&member));
        let started = device.lock().start(Box::new(Sink { inner: inner.clone() }));
        if let Err(e) = started {
            budget.leave(id);
            return Err(e);
        }
        Ok(Source { inner, device, id })
    }

    pub fn id(&self) -> SourceId {
        self.id
    }

    pub fn mode(&self) -> Mode {
        self.inner.mode()
    }

    /// Change how the budget treats this source. The device keeps streaming.
    pub fn set_mode(&self, mode: Mode) {
        self.inner.mode.store(mode_to_u8(mode), Ordering::SeqCst);
        let rate = self.inner.segments.current().sample_rate;
        self.inner.budget.update(self.id, rate, mode);
    }

    pub fn set_center(&self, hz: f64) -> sdr_core::Result<()> {
        self.device.lock().set_center(hz)
    }

    pub fn set_gain(&self, g: GainSetting) -> sdr_core::Result<()> {
        self.device.lock().set_gain(g)
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        let d = self.device.lock();
        DeviceSnapshot { info: d.info().clone(), caps: d.capabilities().clone(), center_hz: d.center_hz(), sample_rate: d.sample_rate() }
    }

    pub fn head(&self) -> u64 {
        self.inner.ring.head()
    }

    pub fn oldest(&self) -> u64 {
        self.inner.ring.oldest()
    }

    pub fn segment_at(&self, sample: u64) -> Segment {
        self.inner.segments.at(sample)
    }

    /// Copy `start .. start + out.len()` as stored; refuses a range that
    /// crosses a segment boundary.
    pub fn read_raw(&self, start: u64, out: &mut [[i16; 2]]) -> Result<Segment, ReadError> {
        let seg = self.covering(start, out.len())?;
        self.inner.ring.read_raw(start, out)?;
        Ok(seg)
    }

    /// Copy `start .. start + out.len()` scaled to ±1.0; refuses a range that
    /// crosses a segment boundary.
    pub fn read(&self, start: u64, out: &mut [Complex32]) -> Result<Segment, ReadError> {
        let seg = self.covering(start, out.len())?;
        self.inner.ring.read(start, out)?;
        Ok(seg)
    }

    fn covering(&self, start: u64, len: usize) -> Result<Segment, ReadError> {
        self.inner.segments.covering(start, start + len as u64).map_err(|_| ReadError::SpansSegments)
    }

    pub fn time_of(&self, sample: u64) -> Option<SystemTime> {
        self.inner.timeline.time_of(sample)
    }

    /// Ring figures in seconds at the current segment's rate.
    pub fn stats(&self) -> SourceStats {
        let inner = &self.inner;
        let r = inner.ring.stats();
        let rate = inner.segments.current().sample_rate;
        let secs = |samples: f64| if rate > 0.0 { samples / rate } else { 0.0 };
        SourceStats {
            held_secs: secs(r.held_samples as f64),
            guaranteed_secs: secs(r.guaranteed_samples as f64),
            ceiling_secs: secs(f64::from(r.ceiling_blocks) * r.block_samples as f64),
            gaps: inner.gaps.load(Ordering::Relaxed),
            lost_samples: inner.lost_samples.load(Ordering::Relaxed),
            overloads: inner.overloads.load(Ordering::Relaxed),
            device_lost: inner.device_lost.load(Ordering::SeqCst),
            segments: inner.segments.len(),
            ring: r,
        }
    }
}

impl Drop for Source {
    /// Leave the budget, then stop the device (which drops its sink); the
    /// inner state goes with the last reference, and the keeper drops its
    /// dead weak reference on its next tick.
    fn drop(&mut self) {
        self.inner.budget.leave(self.id);
        if let Err(e) = self.device.lock().stop() {
            log::warn!("source {}: stopping the device failed: {e}", self.id.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::provision_ahead;

    #[test]
    fn provisions_a_quarter_second_ahead_within_2_to_64_blocks() {
        assert_eq!(provision_ahead(10e6, (1 << 20) / 4), 10); // 1 MiB blocks
        assert_eq!(provision_ahead(10e6, (64 << 20) / 4), 2); // 64 MiB blocks
        assert_eq!(provision_ahead(10e6, 4096 / 4), 64);
        assert_eq!(provision_ahead(0.0, 1024), 2);
    }
}
