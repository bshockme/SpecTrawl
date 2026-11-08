//! Retune-and-dwell sweep: the row producer for a source in Sweep mode (re-homed from `engine-old::sweep`, spec §4.6a).
//!
//! Each hop: retune, wait for the segment table to show where the retune landed (the backend stamps it at that sample),
//! skip a settle guard (longer if the tuner reports a slow PLL lock), average a few FFTs, keep only the alias-free
//! middle of the band, and paste it into a stitched row. Every hop is a segment in the source's table. One row is
//! published per completed pass, through the stage's history and row sink like a live row: linear power over the
//! stitched span, NaN where a hop was not measured, under a synthetic segment that stays the same from pass to pass
//! (so the waterfall keeps its history and the detector its tracks).
//!
//! A plan may name segments to measure; hops outside them are skipped and their bins stay unmeasured, so the row's
//! frequency axis is unchanged. Narrow spurs on the radio's clock grid and the other receivers' LO leaks are blanked.

pub mod profile;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use api::{Boundary, Mode, Row, Segment, SegmentId};
use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::source::Source;

/// "No measurement" inside the sweep (as `engine-old` kept it); published as NaN.
const EMPTY_DB: f32 = -200.0;

fn is_empty_db(v: f32) -> bool {
    v <= EMPTY_DB + 1.0
}

/// Stitched rows carry segment ids with this bit set, apart from the source's real (per-hop) segments.
pub const STITCHED: u64 = 1 << 63;

#[derive(Debug, Clone, PartialEq)]
pub struct SweepPlan {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub fft_size: usize,
    pub ffts_per_hop: usize,
    /// Samples discarded after each retune lands, in seconds (at least [`SETTLE_S`]).
    pub guard_s: f64,
    /// Published rows are max-decimated to at most this many bins.
    pub max_bins: usize,
    /// Blank narrow spurs at every multiple of this frequency (the radio's own clocks: an Airspy R2's 20 and 25 MHz,
    /// so a 5 MHz grid covers both). None leaves them in.
    pub spur_grid_hz: Option<f64>,
    /// Half-width of each blanked spur.
    pub spur_halfwidth_hz: f64,
    /// Frequency ranges to measure; empty means the whole span. The hop grid still runs from `start_hz` over the whole
    /// span, so rows keep one axis. Normalised by [`SweepPlan::normalised`].
    pub segments: Vec<(f64, f64)>,
    /// On a device that sweeps by itself (`Capabilities::native_sweep`), the blocks it sends per hop (one FFT each):
    /// 1 is fastest, more average the noise down; 0 sweeps by retuning anyway. `ffts_per_hop`, `guard_s` and the settle checks are
    /// the retune sweep's.
    pub native_dwell: u8,
}

impl Default for SweepPlan {
    fn default() -> Self {
        Self {
            start_hz: 24e6,
            stop_hz: 1800e6,
            fft_size: 4096,
            ffts_per_hop: 4,
            guard_s: 0.0,
            max_bins: 32768,
            spur_grid_hz: Some(5e6),
            spur_halfwidth_hz: 5e3,
            segments: Vec::new(),
            // As many FFTs as the retune sweep averages, so the survey detector sees the same statistics (on an RFOne:
            // 375 hops/s, a 24–1800 MHz pass in 0.54 s; 1 block: 648 hops/s, 0.33 s, but noisier rows).
            native_dwell: 4,
        }
    }
}

impl SweepPlan {
    /// A plan over `start_hz..stop_hz` measuring only `segments` (any order, may overlap or stick out; empty =
    /// everything).
    pub fn with_segments(start_hz: f64, stop_hz: f64, segments: Vec<(f64, f64)>) -> Self {
        Self { start_hz, stop_hz, segments, ..Default::default() }.normalised()
    }

    /// The same plan with its segments sorted, clamped to the span, merged where they overlap or touch, and empty ones
    /// dropped. A list that covers the whole span collapses to "no segments".
    pub fn normalised(mut self) -> Self {
        let (lo, hi) = (self.start_hz.min(self.stop_hz), self.start_hz.max(self.stop_hz));
        let mut v: Vec<(f64, f64)> =
            self.segments.iter().map(|&(a, b)| (a.min(b).max(lo), a.max(b).min(hi))).filter(|(a, b)| b > a).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(v.len());
        for s in v {
            match merged.last_mut() {
                Some(m) if s.0 <= m.1 => m.1 = m.1.max(s.1),
                _ => merged.push(s),
            }
        }
        if merged.len() == 1 && merged[0] == (lo, hi) {
            merged.clear();
        }
        self.segments = merged;
        self
    }

    /// The ranges actually measured: the segments, or the whole span.
    pub fn measured_ranges(&self) -> Vec<(f64, f64)> {
        if self.segments.is_empty() { vec![(self.start_hz, self.stop_hz)] } else { self.segments.clone() }
    }
}

/// Which hops of a grid of `hops` hops of `hop_hz` from `plan.start_hz` the plan measures: those overlapping a segment
/// (all of them without segments).
pub fn hop_mask(plan: &SweepPlan, hop_hz: f64, hops: usize) -> Vec<bool> {
    if plan.segments.is_empty() {
        return vec![true; hops];
    }
    (0..hops)
        .map(|h| {
            let lo = plan.start_hz + h as f64 * hop_hz;
            let hi = lo + hop_hz;
            plan.segments.iter().any(|&(a, b)| a < hi && b > lo)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SweepStatus {
    pub start_hz: f64,
    /// Hop width actually used (alias-free part of the sample rate).
    pub hop_hz: f64,
    pub hops: usize,
    pub current_hop: usize,
    pub passes: u64,
    pub last_pass_s: f64,
    pub hops_per_sec: f64,
    /// Hops measured again because their samples were refused (a gap, an overwrite).
    pub redone_hops: u64,
    /// Dwells thrown away because their FFTs disagreed (tuner still settling).
    pub settle_retries: u64,
    /// Hops that read differently after a long wait, so always get one (only devices that can't report tuner lock).
    pub slow_hops: usize,
    /// The device reports tuner lock, so slow steps are waited out exactly.
    pub tuner_lock: bool,
    /// Retunes where the tuner was still searching for lock.
    pub lock_waits: u64,
    /// Retunes whose landing never showed in the segment table in time (the hop was measured from the head instead).
    pub unseen_retunes: u64,
    pub segments: Vec<(f64, f64)>,
    /// Per grid hop, whether it is measured.
    pub measured: Vec<bool>,
    pub measured_hops: usize,
    /// The device steps through the hops itself (no retunes from here).
    pub on_board: bool,
}

impl SweepStatus {
    /// End of the hop grid (the row's frequency axis), measured or not.
    pub fn stop_hz(&self) -> f64 {
        self.start_hz + self.hop_hz * self.hops as f64
    }

    /// Spectrum measured per second.
    pub fn mhz_per_sec(&self) -> f64 {
        self.hops_per_sec * self.hop_hz / 1e6
    }
}

/// Settle time after a retune lands, at least. An R2's PLL locks within ~1 ms of the landing on most steps (the old
/// sweep's guard, counted from the block after the request, came to 0.6–1.6 ms after it); slow steps are caught by the
/// lock flag or the long-wait probe.
pub const SETTLE_S: f64 = 0.001;
/// How long a retune may take to show in the segment table before the hop is measured from the head anyway.
const LANDING_TIMEOUT: Duration = Duration::from_millis(500);
/// Give up waiting for tuner lock after this long (measure anyway).
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);
/// Added to the time lock was seen.
const LOCK_MARGIN: Duration = Duration::from_micros(500);
/// How long after the request a tune lands (the backend's estimate; the lock wait counts from the request).
const LANDS_AFTER: Duration = Duration::from_millis(2);
/// Wait used to check a hop, and then used for hops that need it (15 ms was enough for every step seen on an R2).
const LONG_GUARD_S: f64 = 0.02;
/// At most one slow-hop check this often, after the first pass.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Band power that differs by more than this between a normal and a long wait means the normal wait is too short.
const SLOW_HOP_DB: f32 = 0.75;
/// A slow hop goes back to the normal wait after this many agreeing checks in a row.
const AGREE_TO_CLEAR: u8 = 20;
/// A dwell whose first FFT's noise floor is this far from the others' median was measured while the tuner settled.
const MAX_FLOOR_SPREAD_DB: f32 = 3.0;
const MAX_SETTLE_RETRIES: u32 = 3;
/// Below this many hops the across-hop median isn't a reliable profile.
const MIN_HOPS_FOR_PROFILE: usize = 8;
/// How long a wait for samples may take before the hop is given up.
const SAMPLES_TIMEOUT: Duration = Duration::from_millis(500);
/// The poll while waiting for a retune to land or samples to arrive.
const POLL: Duration = Duration::from_micros(200);

/// The first FFT's floor stands apart from the rest of the dwell.
fn settling(floors: &[f32]) -> bool {
    if floors.len() < 2 {
        return false;
    }
    let mut rest = floors[1..].to_vec();
    let mid = rest.len() / 2;
    let m = *rest.select_nth_unstable_by(mid, f32::total_cmp).1;
    (floors[0] - m).abs() > MAX_FLOOR_SPREAD_DB
}

/// Replaces the bins within `halfwidth_hz` of every multiple of `grid_hz` with the level just outside them (the
/// quieter side, so a neighbouring signal isn't smeared over the gap). `db` is one DC-centred FFT at `center_hz`.
fn blank_grid_spurs(db: &mut [f32], center_hz: f64, rate: f64, grid_hz: f64, halfwidth_hz: f64) {
    let n = db.len();
    let bin_hz = rate / n as f64;
    let lo = center_hz - rate / 2.0;
    let hw = ((halfwidth_hz / bin_hz).round() as isize).max(1);
    let side = 3isize;
    let mut f = (lo / grid_hz).ceil() * grid_hz;
    while f < lo + rate {
        let c = ((f - lo) / bin_hz).round() as isize;
        let (a, b) = (c - hw, c + hw);
        if a - side >= 0 && b + side < n as isize {
            let mean = |r: std::ops::Range<isize>| r.clone().map(|j| db[j as usize]).sum::<f32>() / r.len() as f32;
            let fill = mean(a - side..a).min(mean(b + 1..b + 1 + side));
            db[a as usize..=b as usize].iter_mut().for_each(|v| *v = fill);
        }
        f += grid_hz;
    }
}

/// Replaces the bins within `lo_hz..=hi_hz` with the quieter of the levels just outside (as `blank_grid_spurs` does
/// for one spur). No-op when the range misses this hop or runs off its edge.
fn blank_range(db: &mut [f32], center_hz: f64, rate: f64, lo_hz: f64, hi_hz: f64) {
    let n = db.len() as isize;
    let bin_hz = rate / n as f64;
    let start = center_hz - rate / 2.0;
    let a = ((lo_hz - start) / bin_hz).floor() as isize;
    let b = ((hi_hz - start) / bin_hz).ceil() as isize;
    let side = 3isize;
    if b < 0 || a >= n || a > b {
        return;
    }
    let (a, b) = (a.max(side), b.min(n - 1 - side));
    if a > b {
        return;
    }
    let mean = |r: std::ops::Range<isize>| r.clone().map(|j| db[j as usize]).sum::<f32>() / r.len() as f32;
    let fill = mean(a - side..a).min(mean(b + 1..b + 1 + side));
    db[a as usize..=b as usize].iter_mut().for_each(|v| *v = fill);
}

/// Estimates what every hop has in common: for each position within a hop, the median level across all hops, minus
/// that profile's own median. Real signals sit at different positions in different hops, so they don't survive the
/// median; the tuner's passband shape and fixed-offset spurs do. Smoothed across passes.
fn update_hop_profile(full: &[f32], bins_per_hop: usize, profile: &mut Vec<f32>) {
    let hops = full.len() / bins_per_hop;
    let mut col = Vec::with_capacity(hops);
    let mut now = vec![0.0f32; bins_per_hop];
    for (k, out) in now.iter_mut().enumerate() {
        col.clear();
        col.extend((0..hops).map(|h| full[h * bins_per_hop + k]).filter(|v| !is_empty_db(*v)));
        if col.is_empty() {
            continue;
        }
        let mid = col.len() / 2;
        *out = *col.select_nth_unstable_by(mid, f32::total_cmp).1;
    }
    let mut sorted = now.clone();
    let mid = sorted.len() / 2;
    let centre = *sorted.select_nth_unstable_by(mid, f32::total_cmp).1;
    now.iter_mut().for_each(|v| *v -= centre);
    if profile.len() != bins_per_hop {
        *profile = now;
    } else {
        for (p, n) in profile.iter_mut().zip(&now) {
            *p += 0.3 * (n - *p);
        }
    }
}

/// The hop profile's file for the current key, and when to write it.
struct SavedProfile {
    path: std::path::PathBuf,
    gain_key: String,
    initial: Option<Vec<f32>>,
    passes: u32,
    written_at: u32,
}

/// A profile's gain key; a board sweep's samples come through a different converter, so its passband gets its own.
fn profile_key(gain: Option<sdr_core::GainSetting>, on_board: bool) -> String {
    let k = profile::gain_key(gain);
    if on_board { format!("{k}-board") } else { k }
}

const SAVE_AFTER_PASSES: u32 = 20;
const SAVE_EVERY_PASSES: u32 = 100;
const SAVE_MIN_PASSES: u32 = 5;

impl SavedProfile {
    fn open(serial: &str, fft: usize, bins: usize, gain: Option<sdr_core::GainSetting>, on_board: bool) -> Self {
        let gain_key = profile_key(gain, on_board);
        let path = profile::path(serial, fft, bins, &gain_key);
        let initial = profile::load(&path, bins);
        match &initial {
            Some(_) => log::info!("sweep: hop profile loaded from {}", path.display()),
            None => log::debug!("sweep: no saved hop profile at {}", path.display()),
        }
        Self { path, gain_key, initial, passes: 0, written_at: 0 }
    }

    fn refined(&mut self, p: &[f32]) {
        self.passes += 1;
        let due = self.passes == SAVE_AFTER_PASSES
            || (self.passes > SAVE_AFTER_PASSES && (self.passes - SAVE_AFTER_PASSES).is_multiple_of(SAVE_EVERY_PASSES));
        if due {
            self.write(p);
        }
    }

    fn finish(&mut self, p: &[f32]) {
        if self.passes >= SAVE_MIN_PASSES && self.passes != self.written_at {
            self.write(p);
        }
    }

    fn write(&mut self, p: &[f32]) {
        if p.is_empty() {
            return;
        }
        match profile::save(&self.path, p, self.passes) {
            Ok(()) => self.written_at = self.passes,
            Err(e) => log::warn!("sweep: hop profile {}: {e}", self.path.display()),
        }
    }
}

/// The hop grid a plan sweeps on a device.
struct Grid {
    fft: usize,
    hop_hz: f64,
    hops: usize,
    bins_per_hop: usize,
    /// First FFT bin of a hop that is kept (the alias-free middle).
    first_bin: usize,
    measured: Vec<bool>,
    measured_hops: usize,
}

impl Grid {
    fn new(plan: &SweepPlan, snap: &crate::source::DeviceSnapshot) -> Self {
        let (rate, usable) = (snap.sample_rate, snap.caps.usable_bandwidth_fraction);
        let fmax = snap.caps.freq_ranges_hz.iter().map(|r| r.1).fold(f64::MIN, f64::max);
        let n = plan.fft_size;
        let hop_hz = rate * usable;
        // Enough hops to cover the span, but no hop centre beyond the tuner's range.
        let max_hops = ((fmax - plan.start_hz) / hop_hz + 0.5).floor().max(1.0) as usize;
        let hops = (((plan.stop_hz - plan.start_hz) / hop_hz).ceil().max(1.0) as usize).min(max_hops);
        let bins_per_hop = ((n as f64 * usable).round() as usize).min(n);
        let measured = hop_mask(plan, hop_hz, hops);
        let measured_hops = measured.iter().filter(|m| **m).count();
        Self { fft: n, hop_hz, hops, bins_per_hop, first_bin: (n - bins_per_hop) / 2, measured, measured_hops }
    }

    fn center(&self, plan: &SweepPlan, h: usize) -> f64 {
        plan.start_hz + self.hop_hz * (h as f64 + 0.5)
    }

    fn status(&self, plan: &SweepPlan, on_board: bool) -> SweepStatus {
        SweepStatus {
            start_hz: plan.start_hz,
            hop_hz: self.hop_hz,
            hops: self.hops,
            current_hop: 0,
            passes: 0,
            last_pass_s: 0.0,
            hops_per_sec: 0.0,
            redone_hops: 0,
            settle_retries: 0,
            slow_hops: 0,
            tuner_lock: on_board,
            lock_waits: 0,
            unseen_retunes: 0,
            segments: plan.segments.clone(),
            measured: self.measured.clone(),
            measured_hops: self.measured_hops,
            on_board,
        }
    }
}

/// One FFT's power in dB of a hop at `center`, with the radio's spur grid and the other receivers' LO leaks blanked.
fn blank_hop(ctx: &Ctx<'_>, plan: &SweepPlan, db: &mut [f32], center: f64, rate: f64) {
    if let Some(grid) = plan.spur_grid_hz {
        blank_grid_spurs(db, center, rate, grid, plan.spur_halfwidth_hz);
    }
    for (mlo, mhi) in ctx.source.lo_masks() {
        blank_range(db, center, rate, mlo, mhi);
    }
}

/// The hops of a pass pasted into one row, corrected by the hop profile and published.
struct Stitcher {
    full: Vec<f32>,
    corrected: Vec<f32>,
    out_bins: usize,
    bins_per_hop: usize,
    fft: usize,
    serial: String,
    on_board: bool,
    saved: Option<SavedProfile>,
    prof: Vec<f32>,
    /// The row's segment, the same from pass to pass.
    segment: Segment,
}

impl Stitcher {
    fn new(ctx: &Ctx<'_>, plan: &SweepPlan, grid: &Grid, serial: &str, generation: u64, on_board: bool) -> Self {
        let (n, bph) = (grid.fft, grid.bins_per_hop);
        let full = vec![EMPTY_DB; grid.hops * bph];
        let mut saved = ctx.profiles.then(|| SavedProfile::open(serial, n, bph, ctx.source.gain(), on_board));
        let prof = saved.as_mut().and_then(|s| s.initial.take()).unwrap_or_default();
        let span = grid.hops as f64 * grid.hop_hz;
        Self {
            corrected: vec![0.0; full.len()],
            out_bins: full.len().min(plan.max_bins),
            full,
            bins_per_hop: bph,
            fft: n,
            serial: serial.to_string(),
            on_board,
            saved,
            prof,
            segment: Segment {
                id: SegmentId(STITCHED | generation),
                from_sample: 0,
                center_hz: plan.start_hz + span / 2.0,
                sample_rate: span,
                boundary: Boundary::Start,
            },
        }
    }

    fn hop(&mut self, h: usize) -> &mut [f32] {
        &mut self.full[h * self.bins_per_hop..(h + 1) * self.bins_per_hop]
    }

    /// Publishes the pass from `first_sample` to the head.
    fn publish(&mut self, ctx: &Ctx<'_>, first_sample: u64, measured_hops: usize) {
        let bph = self.bins_per_hop;
        let mut out_db = vec![0.0f32; self.out_bins];
        if measured_hops >= MIN_HOPS_FOR_PROFILE {
            // A gain change means a different passband: save what we have under the old key and start from the new.
            let gain = ctx.source.gain();
            if let Some(s) = self.saved.as_mut().filter(|s| profile_key(gain, self.on_board) != s.gain_key) {
                s.finish(&self.prof);
                *s = SavedProfile::open(&self.serial, self.fft, bph, gain, self.on_board);
                self.prof = s.initial.take().unwrap_or_default();
            }
            update_hop_profile(&self.full, bph, &mut self.prof);
            if let Some(s) = self.saved.as_mut() {
                s.refined(&self.prof);
            }
            for seg in self.corrected.chunks_mut(bph).zip(self.full.chunks(bph)) {
                for ((c, &v), &p) in seg.0.iter_mut().zip(seg.1).zip(&self.prof) {
                    *c = if is_empty_db(v) { EMPTY_DB } else { v - p };
                }
            }
            dsp::decimate_max(&self.corrected, &mut out_db);
        } else {
            dsp::decimate_max(&self.full, &mut out_db);
        }
        let power = out_db.iter().map(|&v| if is_empty_db(v) { f32::NAN } else { 10f32.powf(v / 10.0) }).collect();
        let end_sample = ctx.source.head();
        let segment = Segment { from_sample: first_sample, ..self.segment.clone() };
        (ctx.publish)(Row { first_sample, end_sample, blocks: measured_hops as u32, segment, power });
    }

    fn finish(&mut self) {
        if let Some(s) = self.saved.as_mut() {
            s.finish(&self.prof);
        }
    }
}

/// What the sweep needs from the stage that runs it.
pub(crate) struct Ctx<'a> {
    pub source: &'a Arc<Source>,
    pub stop: &'a AtomicBool,
    pub status: &'a Mutex<Option<SweepStatus>>,
    /// Publishes a finished pass (history and row sink).
    pub publish: &'a dyn Fn(Row),
    /// Saved hop profiles on (off in tests that must not touch the user's files).
    pub profiles: bool,
}

/// Why `run` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Stopped,
    /// The source left Sweep mode.
    ModeChanged,
}

/// One retune's outcome.
enum Landed {
    At(u64),
    Unseen(u64),
    Leave(Ended),
}

impl Ctx<'_> {
    fn leave(&self) -> Option<Ended> {
        if self.stop.load(Ordering::Relaxed) {
            Some(Ended::Stopped)
        } else if self.source.mode() != Mode::Sweep {
            Some(Ended::ModeChanged)
        } else {
            None
        }
    }

    /// Retunes to `center` and waits for the segment table to show where it landed.
    fn retune(&self, center: f64) -> Result<Landed, sdr_core::Error> {
        let before = self.source.current_segment();
        if (before.center_hz - center).abs() < 0.5 {
            return Ok(Landed::At(self.source.head()));
        }
        self.source.set_center(center)?;
        let t0 = Instant::now();
        loop {
            if let Some(e) = self.leave() {
                return Ok(Landed::Leave(e));
            }
            let seg = self.source.current_segment();
            if seg.id != before.id && (seg.center_hz - center).abs() < 0.5 {
                return Ok(Landed::At(seg.data_from()));
            }
            if t0.elapsed() > LANDING_TIMEOUT {
                return Ok(Landed::Unseen(self.source.head()));
            }
            std::thread::park_timeout(POLL);
        }
    }

    /// Waits until the ring holds samples up to `to`.
    fn wait_for(&self, to: u64) -> Result<bool, Ended> {
        let deadline = Instant::now() + SAMPLES_TIMEOUT;
        while self.source.head() < to {
            if let Some(e) = self.leave() {
                return Err(e);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::park_timeout(POLL);
        }
        Ok(true)
    }

    /// Total power in a hop's usable band (dB), over `ffts` FFTs from sample `from`.
    #[allow(clippy::too_many_arguments)]
    fn band_power(
        &self,
        sa: &mut SpectrumAnalyzer,
        block: &mut [Complex32],
        db: &mut Vec<f32>,
        from: u64,
        ffts: usize,
        first_bin: usize,
        bins: usize,
    ) -> Result<Option<f32>, Ended> {
        let n = block.len();
        if !self.wait_for(from + (n * ffts) as u64)? {
            return Ok(None);
        }
        let mut sum = 0.0f64;
        for k in 0..ffts {
            if self.source.read_ring(from + (k * n) as u64, block).is_err() {
                return Ok(None);
            }
            sa.push(block);
            sa.take_db(db);
            sum += db[first_bin..first_bin + bins].iter().map(|d| 10f64.powf(*d as f64 / 10.0)).sum::<f64>();
        }
        Ok(Some((10.0 * (sum / (ffts * bins) as f64 + 1e-30).log10()) as f32))
    }
}

/// Sweeps `ctx.source` until it leaves Sweep mode or the stage stops. The plan, LO masks and gain are re-read at the
/// start of every pass; a changed plan starts a new grid (and a new stitched segment).
pub(crate) fn run(ctx: Ctx<'_>) -> Ended {
    let mut generation = 0u64;
    let mut last_plan: Option<SweepPlan> = None;
    loop {
        let plan = ctx.source.sweep_plan().normalised();
        if last_plan.as_ref() != Some(&plan) {
            generation += 1;
            last_plan = Some(plan.clone());
        }
        match run_plan(&ctx, &plan, generation) {
            Some(e) => return e,
            None => continue,
        }
    }
}

/// Sweeps `plan` pass after pass; returns `None` when the plan changed, else why it ended.
fn run_plan(ctx: &Ctx<'_>, plan: &SweepPlan, generation: u64) -> Option<Ended> {
    let snap = ctx.source.snapshot();
    if snap.caps.native_sweep && plan.native_dwell > 0 {
        match run_native(ctx, plan, generation, &snap) {
            Ok(ended) => return ended,
            Err(e) => log::warn!("sweep: the board's own sweep would not start ({e}); retuning from here instead"),
        }
    }
    let (rate, latency) = (snap.sample_rate, snap.caps.retune_latency_s);
    let serial = snap.info.serial.clone();
    let grid = Grid::new(plan, &snap);
    let (n, hop_hz, hops, bins_per_hop, first_bin) = (grid.fft, grid.hop_hz, grid.hops, grid.bins_per_hop, grid.first_bin);
    // The device's latency is from the request; the landing is known, so only the PLL's settling remains.
    let settle = (plan.guard_s.max(SETTLE_S).max(latency - LANDS_AFTER.as_secs_f64()) * rate).ceil() as u64;
    let dwell = (n * plan.ffts_per_hop) as u64;
    let long_guard = (LONG_GUARD_S * rate).ceil() as u64;

    let (measured, measured_hops) = (grid.measured.clone(), grid.measured_hops);
    if measured_hops == 0 {
        log::warn!("sweep: no hop of {hops} falls in the segments {:?}; nothing to measure", plan.segments);
    }
    let next_measured = |h: usize| -> usize {
        if measured_hops == 0 {
            return 0;
        }
        let mut k = (h + 1) % hops;
        while !measured[k] {
            k = (k + 1) % hops;
        }
        k
    };

    let mut sa = SpectrumAnalyzer::new(n);
    let mut block = vec![Complex32::default(); n];
    let mut db = Vec::with_capacity(n);
    let mut st = Stitcher::new(ctx, plan, &grid, &serial, generation, false);
    let span = hops as f64 * hop_hz;
    *ctx.status.lock() = Some(grid.status(plan, false));
    let bump = |f: &dyn Fn(&mut SweepStatus)| {
        if let Some(s) = ctx.status.lock().as_mut() {
            f(s);
        }
    };
    log::info!(
        "sweep: {:.1}–{:.1} MHz, {hops} hops of {:.2} MHz ({measured_hops} measured), {} FFTs of {n} per hop",
        plan.start_hz / 1e6,
        (plan.start_hz + span) / 1e6,
        hop_hz / 1e6,
        plan.ffts_per_hop
    );

    let mut rate_window = (Instant::now(), 0u32);
    let mut last_center = f64::NAN;
    let mut lin = vec![0.0f32; n];
    let (mut pw, mut band) = (Vec::with_capacity(n), Vec::with_capacity(n));
    // Some hop-to-hop steps leave the tuner on a wrong, quiet frequency for 10–15 ms (an R2 stepping 872.5 → 881.5 MHz)
    // where the FFTs agree with each other; without a lock flag each hop is compared, now and then, against a reading
    // after a long wait, and hops that differ always get the long wait.
    let mut slow = vec![false; hops];
    let mut agreed = vec![0u8; hops];
    let mut pass_no = 0usize;
    let (mut probe_hop, mut last_probe) = (0usize, Instant::now());
    let mut has_lock: Option<bool> = None;
    if measured_hops > 0 && !measured[probe_hop] {
        probe_hop = next_measured(probe_hop);
    }
    loop {
        let pass_start = Instant::now();
        let pass_first = ctx.source.head();
        let mut h = 0;
        while h < hops {
            if let Some(e) = ctx.leave() {
                st.finish();
                return Some(e);
            }
            if !measured[h] {
                h += 1;
                continue;
            }
            let center = grid.center(plan, h);
            let issued = Instant::now();
            let landing = match ctx.retune(center) {
                Ok(Landed::At(s)) => s,
                Ok(Landed::Unseen(s)) => {
                    bump(&|s| s.unseen_retunes += 1);
                    s
                }
                Ok(Landed::Leave(e)) => {
                    st.finish();
                    return Some(e);
                }
                Err(e) => {
                    log::warn!("sweep retune to {:.3} MHz: {e}", center / 1e6);
                    h += 1;
                    continue;
                }
            };
            // A slow PLL lock (an R820T stepping through its VCO range: ~13 ms on some 9 MHz steps) is waited out,
            // counting from when it locked.
            let mut after_lock = 0u64;
            let locked = ctx.source.tuner_locked();
            if has_lock.is_none() {
                has_lock = Some(locked.is_some());
                bump(&|s| s.tuner_lock = locked.is_some());
            }
            if locked == Some(false) {
                while ctx.source.tuner_locked() == Some(false) && issued.elapsed() < LOCK_TIMEOUT {
                    if let Some(e) = ctx.leave() {
                        st.finish();
                        return Some(e);
                    }
                    std::thread::park_timeout(POLL);
                }
                let since_landing = (issued.elapsed() + LOCK_MARGIN).saturating_sub(LANDS_AFTER);
                after_lock = landing + (since_landing.as_secs_f64() * rate) as u64;
                bump(&|s| s.lock_waits += 1);
            }
            // Big jumps (the wrap from the top of the span to the bottom) settle longer.
            let jump = (center - last_center).abs();
            last_center = center;
            let short = (landing + if jump > 3.0 * hop_hz { settle * 3 } else { settle }).max(after_lock);
            let probe = has_lock == Some(false)
                && measured_hops > 1
                && short < landing + long_guard
                && (pass_no == 0 || (h == probe_hop && last_probe.elapsed() >= PROBE_INTERVAL));
            if pass_no > 0 && h == probe_hop && (probe || short >= landing + long_guard) {
                probe_hop = next_measured(probe_hop);
            }
            if probe {
                last_probe = Instant::now();
                let pa = ctx.band_power(&mut sa, &mut block, &mut db, short, plan.ffts_per_hop, first_bin, bins_per_hop);
                let pb = ctx.band_power(&mut sa, &mut block, &mut db, landing + long_guard, plan.ffts_per_hop, first_bin, bins_per_hop);
                match (pa, pb) {
                    (Err(e), _) | (_, Err(e)) => {
                        st.finish();
                        return Some(e);
                    }
                    (Ok(Some(pa)), Ok(Some(pb))) => {
                        if (pa - pb).abs() > SLOW_HOP_DB {
                            if !slow[h] {
                                log::info!("sweep: hop {:.1} MHz needs a long settle ({pa:.1} vs {pb:.1} dB)", center / 1e6);
                            }
                            slow[h] = true;
                            agreed[h] = 0;
                        } else if slow[h] {
                            agreed[h] += 1;
                            if agreed[h] >= AGREE_TO_CLEAR {
                                slow[h] = false;
                                agreed[h] = 0;
                            }
                        }
                        let count = slow.iter().filter(|s| **s).count();
                        bump(&|s| s.slow_hops = count);
                    }
                    _ => {}
                }
            }
            let mut from = if slow[h] { short.max(landing + long_guard) } else { short };
            let mut redo = false;
            let mut attempts = 0;
            loop {
                let to = from + dwell;
                match ctx.wait_for(to) {
                    Ok(true) => {}
                    Ok(false) => log::warn!("sweep: no samples for hop {h}"),
                    Err(e) => {
                        st.finish();
                        return Some(e);
                    }
                }
                // Each FFT separately: if the first one's floor is off from the rest, the tuner was still settling and
                // the hop would light up as a broadband stripe; measure the next stretch instead, no retune.
                let mut ok = true;
                let mut floors = Vec::with_capacity(plan.ffts_per_hop);
                lin.iter_mut().for_each(|v| *v = 0.0);
                for k in 0..plan.ffts_per_hop {
                    if ctx.source.read_ring(from + (k * n) as u64, &mut block).is_err() {
                        ok = false;
                        break;
                    }
                    sa.push(&block);
                    sa.take_power(&mut pw);
                    for (l, p) in lin.iter_mut().zip(&pw) {
                        *l += p;
                    }
                    band.clear();
                    band.extend_from_slice(&pw[first_bin..first_bin + bins_per_hop]);
                    let mid = band.len() / 2;
                    floors.push(10.0 * (*band.select_nth_unstable_by(mid, f32::total_cmp).1 + 1e-20).log10());
                }
                if !ok {
                    // Samples refused (a gap, a boundary, overwritten): measure the hop again.
                    redo = true;
                    break;
                }
                if settling(&floors) && attempts < MAX_SETTLE_RETRIES {
                    attempts += 1;
                    from = to;
                    bump(&|s| s.settle_retries += 1);
                    continue;
                }
                let k = plan.ffts_per_hop as f32;
                db.clear();
                db.extend(lin.iter().map(|l| 10.0 * (l / k + 1e-20).log10()));
                blank_hop(ctx, plan, &mut db, center, rate);
                st.hop(h).copy_from_slice(&db[first_bin..first_bin + bins_per_hop]);
                break;
            }
            if redo {
                bump(&|s| s.redone_hops += 1);
                // Forget the landing: the next try retunes from wherever the tuner is.
                last_center = f64::NAN;
                continue;
            }
            rate_window.1 += 1;
            let el = rate_window.0.elapsed().as_secs_f64();
            bump(&|s| {
                s.current_hop = h;
                if el >= 1.0 {
                    s.hops_per_sec = rate_window.1 as f64 / el;
                }
            });
            if el >= 1.0 {
                rate_window = (Instant::now(), 0);
            }
            h += 1;
        }
        pass_no += 1;
        if measured_hops == 0 {
            // Nothing to measure: idle rather than spin, but keep the row flowing so the view shows the (empty) span.
            std::thread::sleep(Duration::from_millis(250));
        }
        st.publish(ctx, pass_first, measured_hops);
        let took = pass_start.elapsed().as_secs_f64();
        bump(&|s| {
            s.passes += 1;
            s.last_pass_s = took;
        });
        // A new plan is picked up between passes.
        if ctx.source.sweep_plan().normalised() != *plan {
            st.finish();
            return None;
        }
    }
}

/// How long the board may go without sending a hop before its sweep is started again.
const BOARD_SILENCE: Duration = Duration::from_millis(500);
/// A segment whose centre is this far off the grid is not one of the board's hops (the tuning before it started).
const OFF_GRID_HZ: f64 = 1e3;

/// The device sweeps by itself (an RFOne with sweep firmware): it retunes, waits for PLL lock and sends each hop's
/// blocks, every hop a segment of its own. The hops are read off the segment table as they arrive: no retunes, guards
/// or settle checks here. The board steps the measured hops' whole range (hops in between that the plan skips are
/// swept and dropped). `Err` when the device won't start it; the caller then sweeps by retuning.
fn run_native(
    ctx: &Ctx<'_>,
    plan: &SweepPlan,
    generation: u64,
    snap: &crate::source::DeviceSnapshot,
) -> Result<Option<Ended>, sdr_core::Error> {
    let grid = Grid::new(plan, snap);
    let rate = snap.sample_rate;
    let (Some(h0), Some(h1)) = (grid.measured.iter().position(|m| *m), grid.measured.iter().rposition(|m| *m)) else {
        return Err(sdr_core::Error::Unsupported("no hop to measure".into()));
    };
    if h1 == h0 {
        // One step would repeat one centre, which the segment table can't tell apart from a single long hop.
        return Err(sdr_core::Error::Unsupported("a single hop".into()));
    }
    let want = sdr_core::NativeSweep {
        first_center_hz: grid.center(plan, h0),
        step_hz: grid.hop_hz,
        steps: (h1 - h0 + 1) as u32,
        dwell_blocks: plan.native_dwell,
        block_samples: 0,
    };
    let mut cursor = ctx.source.head();
    let got = ctx.source.start_native_sweep(want)?;
    let (n, bph, first_bin) = (grid.fft, grid.bins_per_hop, grid.first_bin);
    let (block, dwell) = (got.block_samples, usize::from(got.dwell_blocks));
    let need = (block * dwell) as u64;
    let take = block.min(n);
    let mut st = Stitcher::new(ctx, plan, &grid, &snap.info.serial, generation, true);
    *ctx.status.lock() = Some(grid.status(plan, true));
    let bump = |f: &dyn Fn(&mut SweepStatus)| {
        if let Some(s) = ctx.status.lock().as_mut() {
            f(s);
        }
    };
    log::info!(
        "sweep: on the board, {:.1}–{:.1} MHz, {} hops of {:.2} MHz ({} measured), {dwell} block(s) of {block} per hop",
        plan.start_hz / 1e6,
        (plan.start_hz + grid.hops as f64 * grid.hop_hz) / 1e6,
        grid.hops,
        grid.hop_hz / 1e6,
        grid.measured_hops
    );
    let end = |st: &mut Stitcher, e: Option<Ended>| {
        st.finish();
        if e.is_some()
            && let Err(err) = ctx.source.stop_native_sweep()
        {
            log::warn!("sweep: stopping the board's sweep: {err}");
        }
        Ok(e)
    };

    let mut sa = SpectrumAnalyzer::new(n);
    let mut buf = vec![Complex32::default(); n];
    let (mut lin, mut pw, mut db) = (vec![0.0f32; n], Vec::with_capacity(n), Vec::with_capacity(n));
    let mut last_h: Option<usize> = None;
    let (mut pass_first, mut pass_start) = (cursor, Instant::now());
    let mut last_hop_at = Instant::now();
    let mut rate_window = (Instant::now(), 0u32);
    loop {
        if let Some(e) = ctx.leave() {
            return end(&mut st, Some(e));
        }
        let segs = ctx.source.segments_from(cursor);
        let head = ctx.source.head();
        let mut progressed = false;
        for (i, seg) in segs.iter().enumerate() {
            let next = segs.get(i + 1).map(|s| s.from_sample);
            let from = seg.data_from();
            if from + need > next.unwrap_or(head) {
                match next {
                    // Still arriving.
                    None => break,
                    // Cut short (a torn or dropped block): the board's next hop has begun.
                    Some(nx) => {
                        cursor = nx;
                        bump(&|s| s.redone_hops += 1);
                        continue;
                    }
                }
            }
            cursor = next.unwrap_or(from + need);
            let k = ((seg.center_hz - got.first_center_hz) / got.step_hz).round();
            let on_grid = k >= 0.0
                && k < f64::from(got.steps)
                && (seg.center_hz - (got.first_center_hz + k * got.step_hz)).abs() <= OFF_GRID_HZ
                && (seg.sample_rate - rate).abs() < 1.0;
            if !on_grid {
                continue;
            }
            progressed = true;
            let h = h0 + k as usize;
            if !grid.measured[h] {
                continue;
            }
            if last_h.is_some_and(|l| h <= l) {
                // The board wrapped: the pass is done.
                st.publish(ctx, pass_first, grid.measured_hops);
                let took = pass_start.elapsed().as_secs_f64();
                bump(&|s| {
                    s.passes += 1;
                    s.last_pass_s = took;
                });
                (pass_first, pass_start) = (seg.from_sample, Instant::now());
                if ctx.source.sweep_plan().normalised() != *plan {
                    // The board keeps sweeping; the next plan's start replaces its grid.
                    return end(&mut st, None);
                }
            }
            last_h = Some(h);
            lin.iter_mut().for_each(|v| *v = 0.0);
            let mut ok = true;
            for b in 0..dwell {
                if ctx.source.read_ring(from + (b * block) as u64, &mut buf[..take]).is_err() {
                    ok = false;
                    break;
                }
                // A block is a little shorter than the FFT (4088 of 4096 on an RFOne): zero-padded.
                buf[take..].iter_mut().for_each(|v| *v = Complex32::default());
                sa.push(&buf);
                sa.take_power(&mut pw);
                for (l, p) in lin.iter_mut().zip(&pw) {
                    *l += p;
                }
            }
            if !ok {
                // Overwritten before it was read (the stage fell behind).
                bump(&|s| s.redone_hops += 1);
                continue;
            }
            db.clear();
            db.extend(lin.iter().map(|l| 10.0 * (l / dwell as f32 + 1e-20).log10()));
            blank_hop(ctx, plan, &mut db, seg.center_hz, rate);
            st.hop(h).copy_from_slice(&db[first_bin..first_bin + bph]);
            rate_window.1 += 1;
            let el = rate_window.0.elapsed().as_secs_f64();
            bump(&|s| {
                s.current_hop = h;
                if el >= 1.0 {
                    s.hops_per_sec = rate_window.1 as f64 / el;
                }
            });
            if el >= 1.0 {
                rate_window = (Instant::now(), 0);
            }
        }
        if progressed {
            last_hop_at = Instant::now();
        } else {
            if last_hop_at.elapsed() > BOARD_SILENCE {
                log::warn!("sweep: no hop from the board for {} ms; starting its sweep again", BOARD_SILENCE.as_millis());
                bump(&|s| s.unseen_retunes += 1);
                if let Err(e) = ctx.source.start_native_sweep(want) {
                    log::warn!("sweep: restarting the board's sweep: {e}");
                }
                last_hop_at = Instant::now();
            }
            std::thread::park_timeout(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settling_is_a_first_fft_outlier_only() {
        assert!(settling(&[-85.0, -74.4, -73.9, -73.9]));
        assert!(settling(&[-80.0, -80.0, -74.0, -74.0]));
        assert!(!settling(&[-73.5, -73.1, -78.8, -74.4]));
        assert!(!settling(&[-71.5, -72.5, -73.6, -81.7]));
    }

    fn plan(segs: &[(f64, f64)]) -> SweepPlan {
        SweepPlan::with_segments(100e6, 200e6, segs.to_vec())
    }

    #[test]
    fn segments_are_sorted_clamped_merged_and_emptied() {
        assert!(plan(&[]).segments.is_empty());
        assert!(plan(&[(0.0, 300e6)]).segments.is_empty());
        assert!(plan(&[(100e6, 150e6), (150e6, 200e6)]).segments.is_empty());
        let p = plan(&[(180e6, 250e6), (120e6, 110e6), (115e6, 130e6), (140e6, 140e6)]);
        assert_eq!(p.segments, vec![(110e6, 130e6), (180e6, 200e6)]);
        assert_eq!(p.measured_ranges(), p.segments);
        assert_eq!(plan(&[]).measured_ranges(), vec![(100e6, 200e6)]);
    }

    #[test]
    fn hop_mask_covers_touched_hops_only() {
        let all = hop_mask(&plan(&[]), 10e6, 10);
        assert_eq!(all, vec![true; 10]);
        let m = hop_mask(&plan(&[(123e6, 124e6), (150e6, 170e6)]), 10e6, 10);
        assert_eq!(m, vec![false, false, true, false, false, true, true, false, false, false]);
        let m = hop_mask(&plan(&[(139e6, 141e6)]), 10e6, 10);
        assert_eq!(m, vec![false, false, false, true, true, false, false, false, false, false]);
        assert_eq!(
            hop_mask(&SweepPlan { start_hz: 100e6, stop_hz: 200e6, segments: vec![(300e6, 310e6)], ..Default::default() }, 10e6, 10),
            vec![false; 10]
        );
    }

    #[test]
    fn blank_range_fills_from_the_quieter_side_and_ignores_misses() {
        let (centre, rate, n) = (872.5e6, 10e6, 1000);
        let bin = |f: f64| ((f - (centre - rate / 2.0)) / (rate / n as f64)).round() as usize;
        let mut db = vec![-90.0f32; n];
        db[bin(876.5e6) - 1..=bin(876.5e6) + 1].iter_mut().for_each(|v| *v = -40.0);
        db[bin(870e6)] = -50.0;
        blank_range(&mut db, centre, rate, 876.5e6 - 15e3, 876.5e6 + 15e3);
        assert_eq!(db[bin(876.5e6)], -90.0, "the leak is gone");
        assert_eq!(db[bin(870e6)], -50.0, "a signal elsewhere is untouched");
        let before = db.clone();
        blank_range(&mut db, centre, rate, 900e6, 900.1e6);
        assert_eq!(db, before);
    }

    #[test]
    fn blanks_spurs_on_grid_only() {
        let (n, rate, centre) = (1024, 10e6, 873e6);
        let bin = |f: f64| ((f - (centre - rate / 2.0)) / (rate / n as f64)).round() as usize;
        let mut db = vec![-90.0f32; n];
        db[bin(870e6)] = -60.0;
        db[bin(875e6)] = -65.0;
        db[bin(872.3e6)] = -50.0;
        blank_grid_spurs(&mut db, centre, rate, 5e6, 5e3);
        assert_eq!(db[bin(870e6)], -90.0);
        assert_eq!(db[bin(875e6)], -90.0);
        assert_eq!(db[bin(872.3e6)], -50.0);
    }

    #[test]
    fn loaded_profile_is_refined_not_replaced() {
        let bph = 16;
        let hops = 10;
        let loaded: Vec<f32> = (0..bph).map(|k| if k == 3 { 6.0 } else { 0.0 }).collect();
        let mut p = loaded.clone();
        let full = vec![-100.0f32; hops * bph];
        update_hop_profile(&full, bph, &mut p);
        assert!((p[3] - 6.0 * 0.7).abs() < 1e-3, "blended toward the new estimate: {}", p[3]);
        let mut fresh = Vec::new();
        update_hop_profile(&full, bph, &mut fresh);
        assert_eq!(fresh.len(), bph);
        assert!(fresh.iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn profile_removes_repeating_spur_but_keeps_unique_signal() {
        let (hops, bph) = (20, 100);
        let mut full = vec![-100.0f32; hops * bph];
        for h in 0..hops {
            full[h * bph + 37] = -80.0;
            for k in 0..bph {
                full[h * bph + k] += -3.0 * ((k as f32 - 50.0) / 50.0).powi(2);
            }
        }
        full[5 * bph + 70] = -60.0;
        let mut p = Vec::new();
        update_hop_profile(&full, bph, &mut p);
        let corr: Vec<f32> = full.iter().enumerate().map(|(i, v)| v - p[i % bph]).collect();
        let spur = corr[3 * bph + 37] - corr[3 * bph + 36];
        let sig = corr[5 * bph + 70] - corr[5 * bph + 69];
        assert!(spur.abs() < 1.0, "spur left: {spur}");
        assert!(sig > 35.0, "signal lost: {sig}");
        let edge = corr[3 * bph] - corr[3 * bph + 50];
        assert!(edge.abs() < 0.5, "droop left: {edge}");
    }
}
