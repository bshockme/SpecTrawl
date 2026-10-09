//! Measures how long an Airspy takes from `set_center` returning to the new
//! frequency's samples arriving and settling.
//!
//! Flips between a busy band (FM broadcast) and a quiet one, records the power
//! of every 1024-sample chunk, and finds where each flip shows up in the data.
//!
//!     cargo run --release -p backend-airspy --example retune_probe -- <serial> [busy_mhz] [quiet_mhz]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sdr_core::{Backend, Samples, StreamEvent, StreamSink};

const CHUNK: usize = 1024;

struct Probe {
    pos: Arc<AtomicU64>,
    power: Arc<Mutex<Vec<f32>>>, // one entry per CHUNK samples, indexed by sample/CHUNK
}

impl StreamSink for Probe {
    fn on_samples(&mut self, first: u64, s: Samples<'_>) {
        let Samples::Cf32(s) = s else { return };
        let mut p = self.power.lock();
        let base = (first as usize).div_ceil(CHUNK) * CHUNK;
        let mut i = base - first as usize;
        while i + CHUNK <= s.len() {
            let e: f32 = s[i..i + CHUNK].iter().map(|c| c.norm_sqr()).sum::<f32>() / CHUNK as f32;
            let idx = (first as usize + i) / CHUNK;
            if p.len() <= idx {
                p.resize(idx + 1, f32::NAN);
            }
            p[idx] = 10.0 * (e + 1e-20).log10();
            i += CHUNK;
        }
        self.pos.store(first + s.len() as u64, Ordering::Release);
    }

    fn on_event(&mut self, e: StreamEvent) {
        if let StreamEvent::Gap { lost_samples, .. } = e {
            eprintln!("gap: {lost_samples} samples");
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let serial = args.next().expect("serial");
    let busy: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(98.0) * 1e6;
    let quiet: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(430.0) * 1e6;

    let backend = backend_airspy::AirspyBackend::new();
    let mut dev = backend.open(&serial).expect("open");
    let rate = dev.sample_rate();
    dev.set_center(busy).unwrap();
    let pos = Arc::new(AtomicU64::new(0));
    let power = Arc::new(Mutex::new(Vec::new()));
    dev.start(Box::new(Probe { pos: pos.clone(), power: power.clone() })).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    // (sample position when set_center was called, call duration, target is busy?)
    let mut flips = Vec::new();
    for i in 0..40 {
        let to_busy = i % 2 == 1;
        let n = pos.load(Ordering::Acquire);
        let t = Instant::now();
        dev.set_center(if to_busy { busy } else { quiet }).unwrap();
        flips.push((n, t.elapsed(), to_busy));
        std::thread::sleep(Duration::from_millis(60));
    }
    std::thread::sleep(Duration::from_millis(200));
    dev.stop().unwrap();

    let p = power.lock().clone();
    let level = |a: usize, b: usize| {
        let v: Vec<f32> = p[a.min(p.len())..b.min(p.len())].iter().copied().filter(|x| x.is_finite()).collect();
        v.iter().sum::<f32>() / v.len().max(1) as f32
    };
    let ms = |chunks: f64| chunks * CHUNK as f64 / rate * 1e3;
    println!("rate {:.1} MS/s, chunk {:.3} ms", rate / 1e6, ms(1.0));
    println!("{:>4} {:>6} {:>9} {:>10} {:>10} {:>8} {:>8}", "flip", "to", "call ms", "switch ms", "settle ms", "from dB", "to dB");
    let (mut sw, mut st) = (Vec::new(), Vec::new());
    for (k, &(n, call, to_busy)) in flips.iter().enumerate() {
        let c0 = (n as usize) / CHUNK;
        // Steady levels: before the call, and late in the 60 ms dwell.
        let before = level(c0.saturating_sub(20), c0);
        let after = level(c0 + ms_to_chunks(40.0, rate), c0 + ms_to_chunks(55.0, rate));
        let mid = (before + after) / 2.0;
        let crossed = |x: f32| if after > before { x > mid } else { x < mid };
        let end = (c0 + ms_to_chunks(60.0, rate)).min(p.len());
        let first_cross = (c0..end).find(|&c| p[c].is_finite() && crossed(p[c]));
        // Settled = start of a 10 ms run where every chunk is within 1.5 dB of the final level.
        let win = ms_to_chunks(10.0, rate);
        let settled = first_cross
            .and_then(|c1| (c1..end.saturating_sub(win)).find(|&c| (c..c + win).all(|j| !p[j].is_finite() || (p[j] - after).abs() < 1.5)));
        let f = |o: Option<usize>| o.map(|c| format!("{:.2}", ms((c - c0) as f64))).unwrap_or("-".into());
        if let Some(c) = first_cross {
            sw.push(ms((c - c0) as f64));
        }
        if let Some(c) = settled {
            st.push(ms((c - c0) as f64));
        }
        println!(
            "{:>4} {:>6} {:>9.2} {:>10} {:>10} {:>8.1} {:>8.1}",
            k,
            if to_busy { "busy" } else { "quiet" },
            call.as_secs_f64() * 1e3,
            f(first_cross),
            f(settled),
            before,
            after
        );
    }
    let stats = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return "n/a".to_string();
        }
        format!("min {:.2}  median {:.2}  max {:.2} ms", v[0], v[v.len() / 2], v[v.len() - 1])
    };
    println!("switch visible: {}", stats(&mut sw));
    println!("settled (±1 dB): {}", stats(&mut st));
}

fn ms_to_chunks(ms: f64, rate: f64) -> usize {
    (ms * 1e-3 * rate / CHUNK as f64).round() as usize
}
