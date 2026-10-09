//! Retune-and-dwell sweep (the generic Survey implementation).
//!
//! Each hop: note the ring's head, retune, skip a guard interval (retune
//! latency + USB transfer granularity), average a few FFTs, keep only the
//! alias-free middle of the band, and paste it into a stitched row. One row is
//! published per completed pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use dsp::SpectrumAnalyzer;
use sdr_core::Complex32;

use crate::{BurstDetector, Shared, SharedDevice, SourceConfig, SpectrumRow, Tuning};

#[derive(Debug, Clone)]
pub struct SweepPlan {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub fft_size: usize,
    pub ffts_per_hop: usize,
    /// Samples discarded after each retune, in seconds. The device's
    /// `retune_latency_s` is used if larger.
    pub guard_s: f64,
    /// Published rows are max-decimated to at most this many bins.
    pub max_bins: usize,
    /// Blank narrow spurs at every multiple of this frequency. The radio's own
    /// clocks put harmonics there (Airspy R2: 20 and 25 MHz, so a 5 MHz grid
    /// covers both, as Airspy's Spectrum Spy does). None leaves them in.
    pub spur_grid_hz: Option<f64>,
    /// Half-width of each blanked spur.
    pub spur_halfwidth_hz: f64,
}

impl Default for SweepPlan {
    fn default() -> Self {
        Self {
            start_hz: 24e6,
            stop_hz: 1800e6,
            fft_size: 4096,
            ffts_per_hop: 4,
            guard_s: 0.0095,
            max_bins: 32768,
            spur_grid_hz: Some(5e6),
            spur_halfwidth_hz: 5e3,
        }
    }
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
    /// Hops that read differently after a long wait, so always get one.
    pub slow_hops: usize,
}

impl SweepStatus {
    pub fn stop_hz(&self) -> f64 {
        self.start_hz + self.hop_hz * self.hops as f64
    }

    pub fn mhz_per_sec(&self) -> f64 {
        self.hops_per_sec * self.hop_hz / 1e6
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

    let mut sa = SpectrumAnalyzer::new(n);
    let mut block = vec![Complex32::default(); n];
    let mut db = Vec::with_capacity(n);
    let mut full = vec![-200.0f32; hops * bins_per_hop];
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
    });

    let mut rate_window = (Instant::now(), 0u32);
    let mut last_center = f64::NAN;
    let mut lin = vec![0.0f32; n];
    // Some hop-to-hop steps leave the tuner sitting on a wrong, quiet frequency
    // for 10-15 ms (an R2 stepping 872.5 -> 881.5 MHz). The FFTs agree with each
    // other, so the settle check can't see it. Instead each hop is compared,
    // now and then, against a reading taken after a long wait; hops that differ
    // always get the long wait. Steps are the same every pass, so this sticks.
    let mut slow = vec![false; hops];
    let mut agreed = vec![0u8; hops];
    let mut pass_no = 0usize;
    let (mut probe_hop, mut last_probe) = (0usize, Instant::now());
    'passes: loop {
        let pass_start = Instant::now();
        let mut h = 0;
        while h < hops {
            if stop.load(Ordering::Relaxed) {
                break 'passes;
            }
            let center = plan.start_hz + hop_hz * (h as f64 + 0.5);
            // Separate statements: never hold two engine locks at once (a guard
            // in a tuple lives until the end of the statement).
            let n0 = shared.ring.lock().head();
            let gaps0 = shared.state.lock().gaps;
            if let Err(e) = dev.lock().set_center(center) {
                log::warn!("sweep retune to {:.3} MHz: {e}", center / 1e6);
                h += 1;
                continue;
            }
            // Big jumps (the wrap-around from the top of the span to the bottom)
            // take longer to settle: up to ~30 ms measured on an R2.
            let jump = (center - last_center).abs();
            last_center = center;
            let short = if jump > 3.0 * hop_hz { guard * 3 } else { guard };
            // First pass checks every hop; after that, one hop at a time in turn,
            // rate-limited so short sweeps don't spend their time waiting.
            let probe = hops > 1
                && short < long_guard
                && (pass_no == 0 || (h == probe_hop && last_probe.elapsed() >= PROBE_INTERVAL));
            if pass_no > 0 && h == probe_hop && (probe || short >= long_guard) {
                probe_hop = (probe_hop + 1) % hops;
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
            let mut from = n0 + if slow[h] { long_guard } else { short };
            let mut redo_hop = false;
            let mut attempts = 0;
            loop {
                let to = from + dwell;
                let deadline = Instant::now() + Duration::from_millis(500);
                while shared.ring.lock().head() < to {
                    if stop.load(Ordering::Relaxed) {
                        break 'passes;
                    }
                    if Instant::now() > deadline {
                        log::warn!("sweep: no samples for hop {h}");
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
                if shared.state.lock().gaps != gaps0 {
                    redo_hop = true;
                    break;
                }
                // Measure each FFT separately: if their noise floors disagree,
                // the tuner was still settling (or the retune command was
                // delayed) and the whole hop would light up as a broadband
                // stripe. Then measure the next stretch instead - no retune.
                let mut ok = true;
                let mut floors = Vec::with_capacity(plan.ffts_per_hop);
                lin.iter_mut().for_each(|v| *v = 0.0);
                for k in 0..plan.ffts_per_hop {
                    ok &= shared.ring.lock().read(from + (k * n) as u64, &mut block);
                    sa.push(&block);
                    sa.take_db(&mut db);
                    for (l, d) in lin.iter_mut().zip(&db) {
                        *l += 10f32.powf(*d / 10.0);
                    }
                    let mut band: Vec<f32> = db[first_bin..first_bin + bins_per_hop].to_vec();
                    let mid = band.len() / 2;
                    floors.push(*band.select_nth_unstable_by(mid, f32::total_cmp).1);
                }
                let spread = floors.iter().copied().fold(f32::MIN, f32::max) - floors.iter().copied().fold(f32::MAX, f32::min);
                if spread > MAX_FLOOR_SPREAD_DB && attempts < MAX_SETTLE_RETRIES {
                    attempts += 1;
                    from = to;
                    if let Some(s) = shared.sweep.lock().as_mut() {
                        s.settle_retries += 1;
                    }
                    continue;
                }
                if ok {
                    let k = plan.ffts_per_hop as f32;
                    for (i, d) in db.iter_mut().enumerate() {
                        *d = 10.0 * (lin[i] / k + 1e-20).log10();
                    }
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
        let mut row = vec![0.0f32; out_bins];
        if hops >= MIN_HOPS_FOR_PROFILE {
            update_hop_profile(&full, bins_per_hop, &mut profile);
            for seg in corrected.chunks_mut(bins_per_hop).zip(full.chunks(bins_per_hop)) {
                for ((c, &v), &p) in seg.0.iter_mut().zip(seg.1).zip(&profile) {
                    *c = v - p;
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
    let deadline = Instant::now() + Duration::from_millis(500);
    while shared.ring.lock().head() < to {
        if stop.load(Ordering::Relaxed) || Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_micros(500));
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

/// A dwell whose FFT noise floors differ by more than this was measured
/// while the tuner was still settling.
const MAX_FLOOR_SPREAD_DB: f32 = 3.0;
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
        col.extend((0..hops).map(|h| full[h * bins_per_hop + k]).filter(|v| *v > -199.0));
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
