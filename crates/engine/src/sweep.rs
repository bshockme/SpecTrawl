//! Retune-and-dwell sweep (the generic Survey implementation).
//!
//! Each hop: retune just as a block of samples arrives, skip the device's
//! retune latency (longer if the tuner reports a slow PLL lock), average a few
//! FFTs, keep only the alias-free middle of the band, and paste it into a
//! stitched row. One row is published per completed pass. On an Airspy that
//! is one hop per 6.55 ms USB transfer.
//!
//! A plan may name segments to measure; hops outside them are skipped and
//! their bins stay `EMPTY_DB`, so the row's frequency axis is unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use dsp::SpectrumAnalyzer;
use sdr_core::Complex32;

use crate::{BurstDetector, EMPTY_DB, Shared, SharedDevice, SourceConfig, SpectrumRow, Tuning, is_empty_db};

#[derive(Debug, Clone)]
pub struct SweepPlan {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub fft_size: usize,
    pub ffts_per_hop: usize,
    /// Samples discarded after each retune, in seconds, counted from the
    /// start of the block after the retune. The device's `retune_latency_s`
    /// is used if larger.
    pub guard_s: f64,
    /// Published rows are max-decimated to at most this many bins.
    pub max_bins: usize,
    /// Blank narrow spurs at every multiple of this frequency. The radio's own
    /// clocks put harmonics there (Airspy R2: 20 and 25 MHz, so a 5 MHz grid
    /// covers both, as Airspy's Spectrum Spy does). None leaves them in.
    pub spur_grid_hz: Option<f64>,
    /// Half-width of each blanked spur.
    pub spur_halfwidth_hz: f64,
    /// Frequency ranges (Hz, lo < hi) to measure; empty means the whole
    /// `start_hz..stop_hz`. The hop grid still runs from `start_hz` over the
    /// whole span, so rows keep one frequency axis; hops that touch no
    /// segment are skipped and their bins stay `EMPTY_DB`. Normalised
    /// (sorted, clamped, merged) by `normalised`, which `run` applies.
    pub segments: Vec<(f64, f64)>,
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
        }
    }
}

impl SweepPlan {
    /// A plan over `start_hz..stop_hz` measuring only `segments` (any
    /// order, may overlap or stick out; empty = everything).
    pub fn with_segments(start_hz: f64, stop_hz: f64, segments: Vec<(f64, f64)>) -> Self {
        Self { start_hz, stop_hz, segments, ..Default::default() }.normalised()
    }

    /// The same plan with its segments sorted, clamped to the span, merged
    /// where they overlap or touch, and empty ones dropped. A list that
    /// covers the whole span collapses to "no segments".
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

/// Which hops of a grid of `hops` hops of `hop_hz` from `plan.start_hz` the
/// plan measures: those overlapping a segment (all of them without segments).
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

#[derive(Debug, Clone)]
pub struct SweepStatus {
    pub start_hz: f64,
    /// Hop width actually used (alias-free part of the sample rate).
    pub hop_hz: f64,
    pub hops: usize,
    pub current_hop: usize,
    /// When each hop was last measured.
    pub hop_times: Vec<Option<Instant>>,
    pub last_pass_s: f64,
    pub hops_per_sec: f64,
    pub redone_hops: u64,
    /// Dwells thrown away because their FFTs disagreed (tuner still settling).
    pub settle_retries: u64,
    /// Hops that read differently after a long wait, so always get one
    /// (only for devices that can't report tuner lock).
    pub slow_hops: usize,
    /// The device reports tuner lock, so slow steps are waited out exactly.
    pub tuner_lock: bool,
    /// Retunes where the tuner was still searching for lock.
    pub lock_waits: u64,
    /// The plan's normalised segments (empty: the whole span is measured).
    pub segments: Vec<(f64, f64)>,
    /// Per grid hop, whether it is measured (`hops` counts the grid).
    pub measured: Vec<bool>,
    /// How many of `hops` are measured.
    pub measured_hops: usize,
}

impl SweepStatus {
    /// End of the hop grid (the row's frequency axis), measured or not.
    pub fn stop_hz(&self) -> f64 {
        self.start_hz + self.hop_hz * self.hops as f64
    }

    /// Spectrum measured per second (skipped hops cost nothing).
    pub fn mhz_per_sec(&self) -> f64 {
        self.hops_per_sec * self.hop_hz / 1e6
    }

    /// Spectrum actually measured per pass, Hz.
    pub fn measured_hz(&self) -> f64 {
        self.measured_hops as f64 * self.hop_hz
    }
}

pub(crate) fn run(
    shared: Arc<Shared>,
    dev: SharedDevice,
    plan: SweepPlan,
    cfg: SourceConfig,
    stop: Arc<AtomicBool>,
    mut det: BurstDetector,
) {
    let plan = plan.normalised();
    let (rate, usable, latency, fmax) = {
        let d = dev.lock();
        let c = d.capabilities();
        let fmax = c.freq_ranges_hz.iter().map(|r| r.1).fold(f64::MIN, f64::max);
        (d.sample_rate(), c.usable_bandwidth_fraction, c.retune_latency_s, fmax)
    };
    let n = plan.fft_size;
    let hop_hz = rate * usable;
    // Enough hops to cover the span, but no hop centre beyond the tuner's range.
    let max_hops = ((fmax - plan.start_hz) / hop_hz + 0.5).floor().max(1.0) as usize;
    let hops = (((plan.stop_hz - plan.start_hz) / hop_hz).ceil().max(1.0) as usize).min(max_hops);
    let bins_per_hop = ((n as f64 * usable).round() as usize).min(n);
    let first_bin = (n - bins_per_hop) / 2;
    let guard = (plan.guard_s.max(latency) * rate).ceil() as u64;
    let dwell = (n * plan.ffts_per_hop) as u64;
    let long_guard = (LONG_GUARD_S * rate).ceil() as u64;

    // Hops touching no segment are never tuned to; their bins stay EMPTY_DB.
    let measured = hop_mask(&plan, hop_hz, hops);
    let measured_hops = measured.iter().filter(|m| **m).count();
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
    let mut full = vec![EMPTY_DB; hops * bins_per_hop];
    let out_bins = full.len().min(plan.max_bins);
    // Common response of a hop (passband ripple + fixed spurs), relative to its median.
    let mut profile: Vec<f32> = Vec::new();
    let mut corrected = vec![0.0f32; full.len()];
    let mut seq = 0u64;
    let span = hops as f64 * hop_hz;
    let tuning = Tuning { center_hz: plan.start_hz + span / 2.0, span_hz: span };

    *shared.sweep.lock() = Some(SweepStatus {
        start_hz: plan.start_hz,
        hop_hz,
        hops,
        current_hop: 0,
        hop_times: vec![None; hops],
        last_pass_s: 0.0,
        hops_per_sec: 0.0,
        redone_hops: 0,
        settle_retries: 0,
        slow_hops: 0,
        tuner_lock: false,
        lock_waits: 0,
        segments: plan.segments.clone(),
        measured: measured.clone(),
        measured_hops,
    });

    let mut rate_window = (Instant::now(), 0u32);
    let mut last_center = f64::NAN;
    let mut lin = vec![0.0f32; n];
    let (mut pw, mut band) = (Vec::with_capacity(n), Vec::with_capacity(n));
    // Some hop-to-hop steps leave the tuner sitting on a wrong, quiet frequency
    // for 10-15 ms (an R2 stepping 872.5 -> 881.5 MHz). The FFTs agree with each
    // other, so the settle check can't see it. Instead each hop is compared,
    // now and then, against a reading taken after a long wait; hops that differ
    // always get the long wait. Steps are the same every pass, so this sticks.
    let mut slow = vec![false; hops];
    let mut agreed = vec![0u8; hops];
    let mut pass_no = 0usize;
    let (mut probe_hop, mut last_probe) = (0usize, Instant::now());
    // Known after the first retune: whether the device reports tuner lock. If
    // it does, slow steps are waited out exactly and the probing isn't needed.
    let mut has_lock: Option<bool> = None;
    log::info!("sweep: {:.1}–{:.1} MHz, {hops} hops of {:.2} MHz ({measured_hops} measured), {} FFTs of {n} per hop", plan.start_hz / 1e6, (plan.start_hz + span) / 1e6, hop_hz / 1e6, plan.ffts_per_hop);
    // Typical pass time (a slow average) and the counters at the end of the
    // last pass, to notice and explain a sweep that has slowed down.
    let mut typical_pass: Option<f64> = None;
    let mut last_counters = (0u64, 0u64, 0u64);
    let mut last_partial = Instant::now();
    if measured_hops > 0 && !measured[probe_hop] {
        probe_hop = next_measured(probe_hop);
    }
    'passes: loop {
        let pass_start = Instant::now();
        let mut h = 0;
        while h < hops {
            if stop.load(Ordering::Relaxed) {
                break 'passes;
            }
            if !measured[h] {
                h += 1;
                continue;
            }
            let center = plan.start_hz + hop_hz * (h as f64 + 0.5);
            // Separate statements: never hold two engine locks at once (a guard
            // in a tuple lives until the end of the statement).
            let gaps0 = shared.state.lock().gaps;
            // Retune just as a block arrives: then the retune shows up a fixed
            // time into the next block, with no doubt about how many samples
            // were already in flight. `late` covers missing the moment a little.
            let Some((n0, late)) = sync_to_block(&shared, &stop) else { break 'passes };
            let issued = Instant::now();
            if let Err(e) = dev.lock().set_center(center) {
                log::warn!("sweep retune to {:.3} MHz: {e}", center / 1e6);
                h += 1;
                continue;
            }
            let late = (late.as_secs_f64() * rate) as u64;
            // A slow PLL lock (an R820T stepping through its VCO range: ~13 ms
            // on some 9 MHz steps, ~1 ms on most) is waited out, counting from
            // when it locked. Polling happens during the wait we'd do anyway.
            let mut after_lock = 0u64;
            let locked = dev.lock().tuner_locked();
            if has_lock.is_none() {
                has_lock = Some(locked.is_some());
                if let Some(s) = shared.sweep.lock().as_mut() {
                    s.tuner_lock = locked.is_some();
                }
            }
            if locked == Some(false) {
                while dev.lock().tuner_locked() == Some(false) && issued.elapsed() < LOCK_TIMEOUT {
                    if stop.load(Ordering::Relaxed) {
                        break 'passes;
                    }
                }
                after_lock = late + ((issued.elapsed() + LOCK_MARGIN).as_secs_f64() * rate) as u64;
                if let Some(s) = shared.sweep.lock().as_mut() {
                    s.lock_waits += 1;
                }
            }
            // Big jumps (the wrap-around from the top of the span to the bottom)
            // took longer to settle before lock was checked; kept as a margin.
            let jump = (center - last_center).abs();
            last_center = center;
            let short = (late + if jump > 3.0 * hop_hz { guard * 3 } else { guard }).max(after_lock);
            // Without a lock flag: the first pass checks every hop; after that,
            // one hop at a time in turn, rate-limited so short sweeps don't
            // spend their time waiting.
            let probe = has_lock == Some(false)
                && measured_hops > 1
                && short < long_guard
                && (pass_no == 0 || (h == probe_hop && last_probe.elapsed() >= PROBE_INTERVAL));
            if pass_no > 0 && h == probe_hop && (probe || short >= long_guard) {
                probe_hop = next_measured(probe_hop);
            }
            if probe {
                last_probe = Instant::now();
                let (a, b) = (n0 + short, n0 + long_guard);
                match (
                    band_power(&shared, &stop, &mut sa, &mut block, &mut db, a, plan.ffts_per_hop, first_bin, bins_per_hop),
                    band_power(&shared, &stop, &mut sa, &mut block, &mut db, b, plan.ffts_per_hop, first_bin, bins_per_hop),
                ) {
                    _ if stop.load(Ordering::Relaxed) => break 'passes,
                    (Some(pa), Some(pb)) => {
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
                        if let Some(s) = shared.sweep.lock().as_mut() {
                            s.slow_hops = count;
                        }
                    }
                    _ => {}
                }
            }
            let mut from = n0 + if slow[h] { long_guard.max(short) } else { short };
            log::trace!("sweep: hop {h} {:.3} MHz late {late} guard {short} lock-wait {after_lock} slow {}", center / 1e6, slow[h]);
            let mut redo_hop = false;
            let mut attempts = 0;
            loop {
                let to = from + dwell;
                match wait_for_samples(&shared, &stop, to, Duration::from_millis(500)) {
                    Wait::Ready => {}
                    Wait::Stopped => break 'passes,
                    Wait::TimedOut => log::warn!("sweep: no samples for hop {h}"),
                }
                if shared.state.lock().gaps != gaps0 {
                    redo_hop = true;
                    break;
                }
                // Measure each FFT separately: if the first one's noise floor
                // is off from the rest, the tuner was still settling and the
                // whole hop would light up as a broadband stripe. Then measure
                // the next stretch instead - no retune. (Only the first: in a
                // busy band, e.g. LTE, the 'floor' is a carrier whose level
                // moves from one FFT to the next, which isn't settling.)
                let mut ok = true;
                let mut floors = Vec::with_capacity(plan.ffts_per_hop);
                lin.iter_mut().for_each(|v| *v = 0.0);
                for k in 0..plan.ffts_per_hop {
                    ok &= shared.ring.lock().read(from + (k * n) as u64, &mut block);
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
                if settling(&floors) && attempts < MAX_SETTLE_RETRIES {
                    attempts += 1;
                    from = to;
                    if let Some(s) = shared.sweep.lock().as_mut() {
                        s.settle_retries += 1;
                    }
                    continue;
                }
                if ok {
                    let k = plan.ffts_per_hop as f32;
                    db.clear();
                    db.extend(lin.iter().map(|l| 10.0 * (l / k + 1e-20).log10()));
                    if let Some(grid) = plan.spur_grid_hz {
                        blank_grid_spurs(&mut db, center, rate, grid, plan.spur_halfwidth_hz);
                    }
                    full[h * bins_per_hop..(h + 1) * bins_per_hop].copy_from_slice(&db[first_bin..first_bin + bins_per_hop]);
                }
                break;
            }
            if redo_hop {
                // Samples were dropped somewhere in this hop: measure it again.
                if let Some(s) = shared.sweep.lock().as_mut() {
                    s.redone_hops += 1;
                }
                continue;
            }
            let now = Instant::now();
            // The row in progress, for clock-driven views.
            if now.duration_since(last_partial) >= PARTIAL_EVERY {
                last_partial = now;
                let mut row = vec![0.0f32; out_bins];
                if profile.len() == bins_per_hop {
                    for seg in corrected.chunks_mut(bins_per_hop).zip(full.chunks(bins_per_hop)) {
                        for ((c, &v), &p) in seg.0.iter_mut().zip(seg.1).zip(&profile) {
                            *c = if is_empty_db(v) { EMPTY_DB } else { v - p };
                        }
                    }
                    dsp::decimate_max(&corrected, &mut row);
                } else {
                    dsp::decimate_max(&full, &mut row);
                }
                let head = shared.ring.lock().head();
                *shared.partial.lock() = Some(Arc::new(SpectrumRow { seq: seq + 1, tuning, end_sample: head, time: SystemTime::now(), db: row }));
            }
            rate_window.1 += 1;
            let el = rate_window.0.elapsed().as_secs_f64();
            if let Some(s) = shared.sweep.lock().as_mut() {
                s.current_hop = h;
                s.hop_times[h] = Some(now);
                if el >= 1.0 {
                    s.hops_per_sec = rate_window.1 as f64 / el;
                }
            }
            if el >= 1.0 {
                *shared.ffts_per_sec.lock() = rate_window.1 as f64 * plan.ffts_per_hop as f64 / el;
                rate_window = (Instant::now(), 0);
            }
            h += 1;
        }
        pass_no += 1;
        {
            let took = pass_start.elapsed().as_secs_f64();
            let (sr, rd, lw) = shared.sweep.lock().as_ref().map(|s| (s.settle_retries, s.redone_hops, s.lock_waits)).unwrap_or_default();
            let d = (sr - last_counters.0, rd - last_counters.1, lw - last_counters.2);
            last_counters = (sr, rd, lw);
            log::debug!("sweep: pass {pass_no} in {took:.2} s ({:.0} hops/s), settle retries {}, gap redos {}, lock waits {}", measured_hops as f64 / took, d.0, d.1, d.2);
            match typical_pass {
                Some(t) if took > 3.0 * t && took > 1.0 => {
                    log::warn!("sweep: pass {pass_no} took {took:.2} s, typical {t:.2} s (settle retries {}, gap redos {}, lock waits {}, slow hops {})", d.0, d.1, d.2, slow.iter().filter(|s| **s).count());
                }
                _ => {}
            }
            typical_pass = Some(match typical_pass {
                Some(t) => 0.9 * t + 0.1 * took.min(3.0 * t),
                None => took,
            });
        }
        if measured_hops == 0 {
            // Nothing to measure: idle rather than spin, but keep the row
            // flowing so the view shows the (empty) span.
            std::thread::sleep(Duration::from_millis(250));
        }
        let mut row = vec![0.0f32; out_bins];
        if measured_hops >= MIN_HOPS_FOR_PROFILE {
            update_hop_profile(&full, bins_per_hop, &mut profile);
            for seg in corrected.chunks_mut(bins_per_hop).zip(full.chunks(bins_per_hop)) {
                for ((c, &v), &p) in seg.0.iter_mut().zip(seg.1).zip(&profile) {
                    // Unmeasured bins stay the sentinel, not sentinel-minus-profile.
                    *c = if is_empty_db(v) { EMPTY_DB } else { v - p };
                }
            }
            dsp::decimate_max(&corrected, &mut row);
        } else {
            dsp::decimate_max(&full, &mut row);
        }
        seq += 1;
        let row = SpectrumRow { seq, tuning, end_sample: shared.ring.lock().head(), time: SystemTime::now(), db: row };
        shared.detect(&mut det, &row);
        shared.push_row(
            row,
            // Stitched rows are big; ~1000 passes is already tens of minutes.
            cfg.row_history.min(1024),
        );
        if let Some(s) = shared.sweep.lock().as_mut() {
            s.last_pass_s = pass_start.elapsed().as_secs_f64();
        }
    }
}

/// How often the row in progress is published while a pass runs.
const PARTIAL_EVERY: Duration = Duration::from_millis(100);

/// Replaces the bins within `halfwidth_hz` of every multiple of `grid_hz`
/// with the level just outside them (the quieter side, so a neighbouring
/// signal isn't smeared over the gap). `db` is one DC-centred FFT at `center_hz`.
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

enum Wait {
    Ready,
    Stopped,
    TimedOut,
}

/// Waits (woken by each arriving block) until the ring holds samples up to `to`.
fn wait_for_samples(shared: &Shared, stop: &AtomicBool, to: u64, timeout: Duration) -> Wait {
    let deadline = Instant::now() + timeout;
    let mut r = shared.ring.lock();
    while r.head() < to {
        if stop.load(Ordering::Relaxed) {
            return Wait::Stopped;
        }
        let now = Instant::now();
        if now >= deadline {
            return Wait::TimedOut;
        }
        shared.arrived.wait_for(&mut r, (deadline - now).min(Duration::from_millis(20)));
    }
    Wait::Ready
}

/// Returns (ring head, time since that block arrived) at a moment just after a
/// block arrived: immediately if one arrived within `SYNC_SLACK`, otherwise
/// after waiting for the next. None if stopped.
fn sync_to_block(shared: &Shared, stop: &AtomicBool) -> Option<(u64, Duration)> {
    let mut r = shared.ring.lock();
    if r.since_write() > SYNC_SLACK {
        let h = r.head();
        let deadline = Instant::now() + Duration::from_millis(500);
        while r.head() == h && Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            shared.arrived.wait_for(&mut r, Duration::from_millis(20));
        }
    }
    // A stalled stream carries on unsynchronised, with a bounded allowance.
    Some((r.head(), r.since_write().min(Duration::from_millis(50))))
}

/// Total power in a hop's usable band (dB), over `ffts` FFTs starting at
/// sample `from`; waits for the samples. None if they never arrive or have
/// already left the ring.
#[allow(clippy::too_many_arguments)]
fn band_power(
    shared: &Shared,
    stop: &AtomicBool,
    sa: &mut SpectrumAnalyzer,
    block: &mut [Complex32],
    db: &mut Vec<f32>,
    from: u64,
    ffts: usize,
    first_bin: usize,
    bins: usize,
) -> Option<f32> {
    let n = block.len();
    let to = from + (n * ffts) as u64;
    if !matches!(wait_for_samples(shared, stop, to, Duration::from_millis(500)), Wait::Ready) {
        return None;
    }
    let mut sum = 0.0f64;
    for k in 0..ffts {
        if !shared.ring.lock().read(from + (k * n) as u64, block) {
            return None;
        }
        sa.push(block);
        sa.take_db(db);
        sum += db[first_bin..first_bin + bins].iter().map(|d| 10f64.powf(*d as f64 / 10.0)).sum::<f64>();
    }
    Some((10.0 * (sum / (ffts * bins) as f64 + 1e-30).log10()) as f32)
}

/// Retune immediately if a block arrived at most this long ago; otherwise wait
/// for the next one. The lateness is added to the guard, so this only trades
/// a little dwell room against waiting a whole block.
const SYNC_SLACK: Duration = Duration::from_micros(1500);
/// Give up waiting for tuner lock after this long (measure anyway).
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);
/// Added to the time lock was seen. On an R2 the time since the retune was
/// issued maps to the position in the next block within ~0.5 ms, and the
/// lock happened before the read that saw it returned.
const LOCK_MARGIN: Duration = Duration::from_micros(500);

/// Wait used to check a hop, and then used for hops that need it. 15 ms was
/// enough for every step seen on an R2; this leaves margin.
const LONG_GUARD_S: f64 = 0.02;
/// At most one slow-hop check this often, after the first pass.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Band power that differs by more than this between a normal and a long wait
/// means the normal wait is too short for that hop.
const SLOW_HOP_DB: f32 = 0.75;
/// A slow hop goes back to the normal wait after this many agreeing checks in
/// a row. Slowness is a property of the tuner step, so it should stick; but a
/// burst during one check can mark a hop slow by mistake. Quiet bands agree
/// either way, so this needs to be long enough to outlast a lull.
const AGREE_TO_CLEAR: u8 = 20;

/// A dwell whose first FFT's noise floor is this far from the others' median
/// was measured while the tuner was still settling.
const MAX_FLOOR_SPREAD_DB: f32 = 3.0;

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
const MAX_SETTLE_RETRIES: u32 = 3;

/// Below this many hops the across-hop median isn't a reliable profile.
const MIN_HOPS_FOR_PROFILE: usize = 8;

/// Estimates what every hop has in common: for each position within a hop,
/// the median level across all hops, minus that profile's own median. Real
/// signals sit at different positions in different hops, so they don't
/// survive the median; the tuner's passband shape and fixed-offset spurs do.
/// Smoothed across passes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settling_is_a_first_fft_outlier_only() {
        assert!(settling(&[-85.0, -74.4, -73.9, -73.9]));
        assert!(settling(&[-80.0, -80.0, -74.0, -74.0]));
        // A busy band's carrier level moving around isn't settling.
        assert!(!settling(&[-73.5, -73.1, -78.8, -74.4]));
        assert!(!settling(&[-71.5, -72.5, -73.6, -81.7]));
    }

    fn plan(segs: &[(f64, f64)]) -> SweepPlan {
        SweepPlan::with_segments(100e6, 200e6, segs.to_vec())
    }

    #[test]
    fn segments_are_sorted_clamped_merged_and_emptied() {
        assert!(plan(&[]).segments.is_empty());
        // Covering the whole span is the same as no segments.
        assert!(plan(&[(0.0, 300e6)]).segments.is_empty());
        assert!(plan(&[(100e6, 150e6), (150e6, 200e6)]).segments.is_empty());
        let p = plan(&[(180e6, 250e6), (120e6, 110e6), (115e6, 130e6), (140e6, 140e6)]);
        assert_eq!(p.segments, vec![(110e6, 130e6), (180e6, 200e6)]);
        assert_eq!(p.measured_ranges(), p.segments);
        assert_eq!(plan(&[]).measured_ranges(), vec![(100e6, 200e6)]);
    }

    #[test]
    fn hop_mask_covers_touched_hops_only() {
        // 10 hops of 10 MHz over 100-200 MHz.
        let all = hop_mask(&plan(&[]), 10e6, 10);
        assert_eq!(all, vec![true; 10]);
        // A segment narrower than a hop still gets its hop; boundaries on
        // the grid don't leak into the neighbour.
        let m = hop_mask(&plan(&[(123e6, 124e6), (150e6, 170e6)]), 10e6, 10);
        assert_eq!(m, vec![false, false, true, false, false, true, true, false, false, false]);
        // A segment crossing a hop boundary takes both hops.
        let m = hop_mask(&plan(&[(139e6, 141e6)]), 10e6, 10);
        assert_eq!(m, vec![false, false, false, true, true, false, false, false, false, false]);
        // Segments outside the span measure nothing.
        assert_eq!(
            hop_mask(&SweepPlan { start_hz: 100e6, stop_hz: 200e6, segments: vec![(300e6, 310e6)], ..Default::default() }, 10e6, 10),
            vec![false; 10]
        );
    }

    #[test]
    fn blanks_spurs_on_grid_only() {
        // 1024 bins over 10 MHz around 873 MHz: 870 and 875 MHz are on the grid.
        let (n, rate, centre) = (1024, 10e6, 873e6);
        let bin = |f: f64| ((f - (centre - rate / 2.0)) / (rate / n as f64)).round() as usize;
        let mut db = vec![-90.0f32; n];
        db[bin(870e6)] = -60.0;
        db[bin(875e6)] = -65.0;
        db[bin(872.3e6)] = -50.0; // a real signal off the grid
        blank_grid_spurs(&mut db, centre, rate, 5e6, 5e3);
        assert_eq!(db[bin(870e6)], -90.0);
        assert_eq!(db[bin(875e6)], -90.0);
        assert_eq!(db[bin(872.3e6)], -50.0);
    }

    #[test]
    fn profile_removes_repeating_spur_but_keeps_unique_signal() {
        let (hops, bph) = (20, 100);
        let mut full = vec![-100.0f32; hops * bph];
        for h in 0..hops {
            full[h * bph + 37] = -80.0; // spur at the same offset in every hop
            for k in 0..bph {
                full[h * bph + k] += -3.0 * ((k as f32 - 50.0) / 50.0).powi(2); // passband droop
            }
        }
        full[5 * bph + 70] = -60.0; // a real signal in one hop
        let mut profile = Vec::new();
        update_hop_profile(&full, bph, &mut profile);
        let corr: Vec<f32> = full.iter().enumerate().map(|(i, v)| v - profile[i % bph]).collect();
        let spur = corr[3 * bph + 37] - corr[3 * bph + 36];
        let sig = corr[5 * bph + 70] - corr[5 * bph + 69];
        assert!(spur.abs() < 1.0, "spur left: {spur}");
        assert!(sig > 35.0, "signal lost: {sig}");
        let edge = corr[3 * bph] - corr[3 * bph + 50];
        assert!(edge.abs() < 0.5, "droop left: {edge}");
    }
}
