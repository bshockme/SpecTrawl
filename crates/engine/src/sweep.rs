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

use crate::{Shared, SharedDevice, SourceConfig, SpectrumRow, Tuning};

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
}

impl Default for SweepPlan {
    fn default() -> Self {
        Self { start_hz: 24e6, stop_hz: 1800e6, fft_size: 4096, ffts_per_hop: 4, guard_s: 0.0095, max_bins: 32768 }
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
}

impl SweepStatus {
    pub fn stop_hz(&self) -> f64 {
        self.start_hz + self.hop_hz * self.hops as f64
    }

    pub fn mhz_per_sec(&self) -> f64 {
        self.hops_per_sec * self.hop_hz / 1e6
    }
}

pub(crate) fn run(shared: Arc<Shared>, dev: SharedDevice, plan: SweepPlan, cfg: SourceConfig, stop: Arc<AtomicBool>) {
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

    let mut sa = SpectrumAnalyzer::new(n);
    let mut block = vec![Complex32::default(); n];
    let mut db = Vec::with_capacity(n);
    let mut full = vec![-200.0f32; hops * bins_per_hop];
    let out_bins = full.len().min(plan.max_bins);
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
    });

    let mut rate_window = (Instant::now(), 0u32);
    'passes: loop {
        let pass_start = Instant::now();
        let mut h = 0;
        while h < hops {
            if stop.load(Ordering::Relaxed) {
                break 'passes;
            }
            let center = plan.start_hz + hop_hz * (h as f64 + 0.5);
            let (n0, gaps0) = (shared.ring.lock().head(), shared.state.lock().gaps);
            if let Err(e) = dev.lock().set_center(center) {
                log::warn!("sweep retune to {:.3} MHz: {e}", center / 1e6);
                h += 1;
                continue;
            }
            let from = n0 + guard;
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
                // Samples were dropped somewhere in this hop: measure it again.
                if let Some(s) = shared.sweep.lock().as_mut() {
                    s.redone_hops += 1;
                }
                continue;
            }
            let mut ok = true;
            for k in 0..plan.ffts_per_hop {
                ok &= shared.ring.lock().read(from + (k * n) as u64, &mut block);
                sa.push(&block);
            }
            sa.take_db(&mut db);
            if ok {
                full[h * bins_per_hop..(h + 1) * bins_per_hop].copy_from_slice(&db[first_bin..first_bin + bins_per_hop]);
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
        let mut row = vec![0.0f32; out_bins];
        dsp::decimate_max(&full, &mut row);
        seq += 1;
        shared.push_row(
            SpectrumRow { seq, tuning, end_sample: shared.ring.lock().head(), time: SystemTime::now(), db: row },
            // Stitched rows are big; ~1000 passes is already tens of minutes.
            cfg.row_history.min(1024),
        );
        if let Some(s) = shared.sweep.lock().as_mut() {
            s.last_pass_s = pass_start.elapsed().as_secs_f64();
        }
    }
}
