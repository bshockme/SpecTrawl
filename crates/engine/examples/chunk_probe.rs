//! Where does a retune land in the sample stream, if it's issued the moment a
//! USB chunk arrives?
//!
//! Alternates between two tunings, retuning right after a chunk arrives, and
//! finds the switch in the samples from the Airspy's own clock spurs (one in
//! each tuning, see spur_check). Reports the switch position relative to the
//! start of the next chunk: if it's reliably early, the tail of that chunk can
//! be measured and the sweep can do one hop per chunk.
//!
//!     cargo run --release -p engine --example chunk_probe -- <serial> <a MHz> <b MHz> [cycles]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::{Complex32, Device, Samples, StreamEvent, StreamSink};

const N: usize = 4096;
const AVG: usize = 2;
const STEP: usize = 1024;

struct Store {
    buf: Vec<Complex32>,
    /// (first sample, length) of each chunk as delivered.
    chunks: Vec<(u64, usize)>,
}

struct Sink {
    store: Arc<Mutex<Store>>,
    chunks: Arc<AtomicU64>,
    cap: usize,
}

impl StreamSink for Sink {
    fn on_samples(&mut self, first: u64, s: Samples<'_>) {
        let Samples::Cf32(s) = s else { return };
        {
            let mut st = self.store.lock();
            st.chunks.push((first, s.len()));
            if st.buf.len() == first as usize && st.buf.len() + s.len() <= self.cap {
                st.buf.extend_from_slice(s);
            }
        }
        self.chunks.fetch_add(1, Ordering::Release);
    }

    fn on_event(&mut self, e: StreamEvent) {
        if let StreamEvent::Gap { lost_samples, .. } = e {
            eprintln!("gap: {lost_samples} samples");
        }
    }
}

fn prominence(db: &[f32], bin: usize, bin_hz: f64) -> f32 {
    let w = (300e3 / bin_hz) as usize;
    let mut around: Vec<f32> = db[bin - w..=bin + w].to_vec();
    around.sort_by(f32::total_cmp);
    let pk = db[bin - 1..=bin + 1].iter().copied().fold(f32::MIN, f32::max);
    pk - around[around.len() / 2]
}

/// The strongest 5 MHz grid spur in a tuning: (bin, prominence) from a settled spectrum.
fn best_spur(db: &[f32], center: f64, rate: f64) -> (usize, f32) {
    let bin_hz = rate / N as f64;
    let mut f = ((center - 0.42 * rate) / 5e6).ceil() * 5e6;
    let mut best = (0, f32::MIN);
    while f < center + 0.42 * rate {
        let b = ((f - center) / bin_hz + N as f64 / 2.0).round() as usize;
        let p = prominence(db, b, bin_hz);
        if p > best.1 {
            best = (b, p);
        }
        f += 5e6;
    }
    best
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
    let fa = a[2].parse::<f64>().unwrap() * 1e6;
    let fb = a[3].parse::<f64>().unwrap() * 1e6;
    let cycles = a.get(4).and_then(|v| v.parse().ok()).unwrap_or(40usize);

    let backend = backend_airspy::AirspyBackend::new();
    let mut dev = backend.open_device(serial).expect("open");
    let rate = dev.sample_rate();
    let bin_hz = rate / N as f64;
    let ms = |s: f64| s / rate * 1e3;

    // References: each tuning's best spur, measured settled.
    let cap = (rate * 4.0) as usize;
    let store = Arc::new(Mutex::new(Store { buf: Vec::with_capacity(cap), chunks: Vec::new() }));
    let chunks = Arc::new(AtomicU64::new(0));
    dev.set_center(fa).unwrap();
    dev.start(Box::new(Sink { store: store.clone(), chunks: chunks.clone(), cap })).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Alternate a <-> b: retune right after a chunk arrives, then leave it for
    // 3 more chunks so each switch has a settled stretch on both sides.
    let mut events = Vec::new(); // (chunk count when retuned, ms from arrival to retune issued, call ms, to)
    let mut to = fb;
    for _ in 0..cycles {
        let c0 = chunks.load(Ordering::Acquire);
        while chunks.load(Ordering::Acquire) == c0 {
            std::hint::spin_loop();
        }
        let t = Instant::now();
        let c = chunks.load(Ordering::Acquire);
        dev.set_center(to).unwrap();
        events.push((c, t.elapsed().as_secs_f64() * 1e3, to));
        to = if to == fb { fa } else { fb };
        let c1 = chunks.load(Ordering::Acquire);
        while chunks.load(Ordering::Acquire) < c1 + 3 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    dev.stop().unwrap();
    let st = store.lock();
    let lens: std::collections::BTreeSet<usize> = st.chunks.iter().map(|c| c.1).collect();
    println!(
        "{:.1} MS/s, chunk sizes {:?} ({:.2} ms), {} samples recorded",
        rate / 1e6,
        lens,
        ms(*lens.iter().next().unwrap() as f64),
        st.buf.len()
    );

    let mut sa = SpectrumAnalyzer::new(N);
    // Settled references from the middle of the 3rd chunk after each of the first two retunes.
    let chunk_start = |c: u64| st.chunks[c as usize].0 as usize;
    let mut refs = std::collections::HashMap::new();
    for &(c, _, f) in events.iter().take(2) {
        let s0 = chunk_start(c + 2);
        let db = spectrum(&mut sa, &st.buf[s0..s0 + 8 * N]);
        let (bin, p) = best_spur(&db, f, rate);
        println!(
            "tuning {:.1} MHz: tracking spur at bin {bin} ({:+.3} MHz), {p:.1} dB settled",
            f / 1e6,
            (bin as f64 - N as f64 / 2.0) * bin_hz / 1e6
        );
        refs.insert(f.to_bits(), (bin, p));
    }

    // For each retune: switch position relative to the start of the next chunk.
    let mut leave = Vec::new();
    let mut arrive = Vec::new();
    println!("retune#  issue ms  call ms   old spur gone  new spur stable   (ms after next chunk starts)");
    for (i, &(c, issue, f)) in events.iter().enumerate() {
        let from = if f == fb { fa } else { fb };
        let (Some(&(bn, pn)), Some(&(bo, po))) = (refs.get(&f.to_bits()), refs.get(&from.to_bits())) else { continue };
        // `c` chunks had arrived when we retuned, so chunk index c is the next one.
        if c as usize + 3 >= st.chunks.len() {
            break;
        }
        let next = chunk_start(c);
        let end = chunk_start(c + 3);
        if end > st.buf.len() {
            break;
        }
        let mut rows = Vec::new();
        let mut w = next.saturating_sub(chunk_start(c) - chunk_start(c - 1));
        while w + AVG * N <= end {
            let db = spectrum(&mut sa, &st.buf[w..w + AVG * N]);
            let t = ms(w as f64 + (AVG * N) as f64 / 2.0 - next as f64);
            rows.push((t, prominence(&db, bo, bin_hz), prominence(&db, bn, bin_hz)));
            w += STEP;
        }
        let gone = rows.iter().rposition(|r| r.1 > po / 2.0).map(|k| rows[k].0);
        let stable = (0..rows.len()).find(|&k| rows[k..].iter().all(|r| r.2 > pn / 2.0)).map(|k| rows[k].0);
        let f = |o: Option<f64>| o.map(|v| format!("{v:>7.2}")).unwrap_or("      -".into());
        println!("{i:>7}  {:>8.3}  {:>7.2}  {}        {}", issue, 0.0, f(gone), f(stable));
        if let Some(g) = gone {
            leave.push(g);
        }
        if let Some(s) = stable {
            arrive.push(s);
        }
    }
    let stats = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return "n/a".into();
        }
        format!(
            "min {:.2}  median {:.2}  p90 {:.2}  max {:.2} ms  (n={})",
            v[0],
            v[v.len() / 2],
            v[v.len() * 9 / 10],
            v[v.len() - 1],
            v.len()
        )
    };
    println!("old spur gone:   {}", stats(&mut leave));
    println!("new spur stable: {}", stats(&mut arrive));
}
