//! Does the R820T's PLL-lock bit say when a retune has really landed?
//!
//! Retunes `from` -> `to` repeatedly. After each retune it polls the tuner's
//! status register as fast as USB allows, and records the samples. Ground truth
//! for "where is the tuner" is the Airspy's own clock spurs (multiples of 5 MHz,
//! see spur_check): they sit at known absolute frequencies, so the spur seen
//! at the new tuning shows the tuner has arrived whatever is on the air, and
//! the spur seen at the old tuning shows it hasn't left.
//!
//!     cargo run --release -p engine --example lock_probe -- <serial> <from MHz> <to MHz> [repeats] [reg]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::{Complex32, Device, Samples, StreamEvent, StreamSink};

const N: usize = 4096;
const AVG: usize = 4;
const PRE_MS: f64 = 10.0;
const POST_MS: f64 = 60.0;
/// R820T status register holding the PLL lock flag (bit 6), as rtl-sdr's
/// r82xx driver reads it.
const LOCK_REG: u8 = 2;
const LOCK_BIT: u8 = 0x40;

#[derive(Default)]
struct Rec {
    start: Option<u64>,
    want: usize,
    buf: Vec<Complex32>,
}

struct Sink {
    pos: Arc<AtomicU64>,
    rec: Arc<Mutex<Rec>>,
}

impl StreamSink for Sink {
    fn on_samples(&mut self, first: u64, s: Samples<'_>) {
        let Samples::Cf32(s) = s else { return };
        let mut r = self.rec.lock();
        if let Some(st) = r.start
            && r.buf.len() < r.want
            && first + s.len() as u64 > st
        {
            let skip = st.saturating_sub(first) as usize;
            let take = (r.want - r.buf.len()).min(s.len() - skip);
            r.buf.extend_from_slice(&s[skip..skip + take]);
        }
        self.pos.store(first + s.len() as u64, Ordering::Release);
    }

    fn on_event(&mut self, e: StreamEvent) {
        if let StreamEvent::Gap { lost_samples, .. } = e {
            eprintln!("gap: {lost_samples} samples");
        }
    }
}

/// Peak (±1 bin) minus the median of ±300 kHz around `bin`.
fn prominence(db: &[f32], bin: usize, bin_hz: f64) -> f32 {
    let w = (300e3 / bin_hz) as usize;
    let mut around: Vec<f32> = db[bin - w..=bin + w].to_vec();
    around.sort_by(f32::total_cmp);
    let pk = db[bin - 1..=bin + 1].iter().copied().fold(f32::MIN, f32::max);
    pk - around[around.len() / 2]
}

/// Bins of the 5 MHz grid points inside the alias-free part of a tuning.
fn grid_bins(center: f64, rate: f64) -> Vec<(f64, usize)> {
    let bin_hz = rate / N as f64;
    let lo = center - 0.42 * rate;
    let mut out = Vec::new();
    let mut f = (lo / 5e6).ceil() * 5e6;
    while f < center + 0.42 * rate {
        out.push((f, ((f - center) / bin_hz + N as f64 / 2.0).round() as usize));
        f += 5e6;
    }
    out
}

fn spectrum(sa: &mut SpectrumAnalyzer, x: &[Complex32]) -> Vec<f32> {
    for k in 0..x.len() / N {
        sa.push(&x[k * N..(k + 1) * N]);
    }
    let mut db = Vec::new();
    sa.take_db(&mut db);
    db
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let serial = &a[1];
    let from = a[2].parse::<f64>().unwrap() * 1e6;
    let to = a[3].parse::<f64>().unwrap() * 1e6;
    let repeats = a.get(4).and_then(|v| v.parse().ok()).unwrap_or(10usize);
    let reg = a.get(5).and_then(|v| v.parse().ok()).unwrap_or(LOCK_REG);

    let backend = backend_airspy::AirspyBackend::new();
    let mut dev = backend.open_device(serial).expect("open");
    let rate = dev.sample_rate();
    let bin_hz = rate / N as f64;
    let pos = Arc::new(AtomicU64::new(0));
    let rec = Arc::new(Mutex::new(Rec::default()));
    dev.set_center(from).unwrap();
    dev.start(Box::new(Sink { pos: pos.clone(), rec: rec.clone() })).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    print!("status registers at {:.1} MHz:", from / 1e6);
    for r in 0..5u8 {
        print!(" r{r}=0x{:02X}", dev.r820t_read(r).unwrap_or(0xEE));
    }
    println!();

    let pre = (PRE_MS * 1e-3 * rate) as usize / N * N;
    let post = (POST_MS * 1e-3 * rate) as usize / N * N;
    let mut sa = SpectrumAnalyzer::new(N);
    let ms = |samples: usize| samples as f64 / rate * 1e3;
    let mut summary = Vec::new();

    for rep in 0..repeats {
        dev.set_center(from).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        {
            let mut r = rec.lock();
            *r = Rec { start: Some(pos.load(Ordering::Acquire)), want: pre + post, buf: Vec::with_capacity(pre + post) };
        }
        // Let the pre-retune stretch arrive, then retune and poll.
        let t_pre = Instant::now();
        while rec.lock().buf.len() < pre && t_pre.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_micros(200));
        }
        let rec_start = rec.lock().start.unwrap();
        let n_call = pos.load(Ordering::Acquire);
        let t0 = Instant::now();
        dev.set_center(to).unwrap();
        let call_ms = t0.elapsed().as_secs_f64() * 1e3;
        let mut polls: Vec<(f64, u64, u8)> = Vec::new(); // (ms since call, samples received, value)
        while t0.elapsed() < Duration::from_millis(40) {
            let v = dev.r820t_read(reg).unwrap_or(0xEE);
            polls.push((t0.elapsed().as_secs_f64() * 1e3, pos.load(Ordering::Acquire), v));
        }
        while rec.lock().buf.len() < pre + post && t0.elapsed() < Duration::from_millis(1000) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let buf = std::mem::take(&mut rec.lock().buf);
        if buf.len() < pre + post {
            println!("rep {rep}: short recording");
            continue;
        }
        // Retune call happened at this offset into the recording.
        let call_at = (n_call - rec_start) as usize;

        // References: the settled end of the recording (new tuning) and the pre-retune stretch (old).
        let new_ref = spectrum(&mut sa, &buf[buf.len() - (10e-3 * rate) as usize / N * N..]);
        let old_ref = spectrum(&mut sa, &buf[..pre]);
        let pick = |db: &[f32], c: f64| {
            grid_bins(c, rate).into_iter().map(|(f, b)| (f, b, prominence(db, b, bin_hz))).max_by(|x, y| x.2.total_cmp(&y.2)).unwrap()
        };
        let (fnew, bnew, pnew) = pick(&new_ref, to);
        let (fold, bold, pold) = pick(&old_ref, from);

        // Sliding windows of AVG FFTs, one FFT apart, from the retune call on.
        let mut rows = Vec::new();
        let mut w = call_at / N * N;
        while w + AVG * N <= buf.len() {
            let db = spectrum(&mut sa, &buf[w..w + AVG * N]);
            let band = 10.0 * (db[N / 10..N * 9 / 10].iter().map(|d| 10f64.powf(*d as f64 / 10.0)).sum::<f64>() / (N as f64 * 0.8)).log10();
            // Time of the window's middle, relative to the retune call, in the sample domain.
            let t = ms(w + AVG * N / 2) - ms(call_at);
            rows.push((t, prominence(&db, bnew, bin_hz), prominence(&db, bold, bin_hz), band));
            w += N;
        }
        let arrived = (0..rows.len()).find(|&i| rows[i..].iter().all(|r| r.1 > pnew / 2.0)).map(|i| rows[i].0);
        let left = rows.iter().rposition(|r| r.2 > pold / 2.0).map(|i| rows[i].0);

        let unlocked: Vec<&(f64, u64, u8)> = polls.iter().filter(|p| p.2 & LOCK_BIT == 0).collect();
        let lock_ms = polls.iter().rposition(|p| p.2 & LOCK_BIT == 0).and_then(|i| polls.get(i + 1)).map(|p| p.0);
        let values: std::collections::BTreeSet<u8> = polls.iter().map(|p| p.2).collect();
        let poll_ms = polls.last().map(|p| p.0).unwrap_or(0.0) / polls.len().max(1) as f64;

        if rep == 0 {
            println!("tracking spurs: new {:.0} MHz ({pnew:.1} dB settled), old {:.0} MHz ({pold:.1} dB before)", fnew / 1e6, fold / 1e6);
            println!("timeline (ms after set_freq, sample domain): new-spur dB, old-spur dB, band dB");
            for r in rows.iter().step_by(3).take_while(|r| r.0 < 30.0) {
                println!("  {:>6.2}  {:>6.1}  {:>6.1}  {:>6.1}", r.0, r.1, r.2, r.3);
            }
            println!("register {reg} polls (ms after set_freq returned → value):");
            let mut last = None;
            for p in &polls {
                if last != Some(p.2) {
                    println!("  {:>6.2} ms  0x{:02X}  (samples since call: {:.2} ms)", p.0, p.2, ms(p.1.saturating_sub(n_call) as usize));
                    last = Some(p.2);
                }
            }
        }
        let f = |o: Option<f64>| o.map(|v| format!("{v:.2}")).unwrap_or("-".into());
        summary.push(format!(
            "rep {rep:>2}: call {call_ms:.2} ms · old spur gone {:>6} · new spur stable {:>6} · {} polls ({poll_ms:.2} ms each), values {:02X?}, lock bit clear in {} → locked at {}",
            f(left),
            f(arrived),
            polls.len(),
            values,
            unlocked.len(),
            f(lock_ms)
        ));
    }
    dev.stop().unwrap();
    for s in summary {
        println!("{s}");
    }
}
