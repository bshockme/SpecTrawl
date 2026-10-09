//! Sweeps a range and measures how far narrow peaks at exact multiples of a
//! grid step stand above their surroundings, against control offsets. Internal
//! clock spurs line up on the grid; real signals don't.
//!
//!     cargo run --release -p engine --example spur_check -- <serial> <lo MHz> <hi MHz> <seconds> [blank]

use std::time::Duration;

use engine::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, lo, hi, secs) = (
        &a[1],
        a[2].parse::<f64>().unwrap(),
        a[3].parse::<f64>().unwrap(),
        a[4].parse::<u64>().unwrap(),
    );
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let d = b.open(serial).expect("open");
    let plan = SweepPlan {
        start_hz: lo * 1e6,
        stop_hz: hi * 1e6,
        max_bins: 1 << 20,
        spur_grid_hz: (a.get(5).map(String::as_str) == Some("blank")).then_some(5e6),
        ..Default::default()
    };
    let src = Source::start(d, SourceConfig::default(), Mode::Sweep(plan)).unwrap();
    std::thread::sleep(Duration::from_secs(secs));
    let rows = src.rows_after(None);
    let last: Vec<_> = rows.iter().rev().take(10).collect();
    let n = last[0].db.len();
    let t = last[0].tuning;
    let mut avg = vec![0.0f64; n];
    for r in &last {
        for (a, v) in avg.iter_mut().zip(&r.db) {
            *a += 10f64.powf(*v as f64 / 10.0) / last.len() as f64;
        }
    }
    let db: Vec<f64> = avg.iter().map(|v| 10.0 * v.log10()).collect();
    let f0 = t.center_hz - t.span_hz / 2.0;
    let bin_hz = t.span_hz / n as f64;
    println!("{} passes, {:.1} kHz/bin", rows.len(), bin_hz / 1e3);
    // Peak within ±2 kHz-ish (at least ±1 bin) minus median of ±300 kHz.
    let prom = |f: f64| -> Option<f64> {
        let i = ((f - f0) / bin_hz) as isize;
        let w = ((300e3 / bin_hz) as isize).max(5);
        if i - w < 0 || i + w >= n as isize {
            return None;
        }
        let pk = (i - 1..=i + 1)
            .map(|j| db[j as usize])
            .fold(f64::MIN, f64::max);
        let mut around: Vec<f64> = (i - w..=i + w).map(|j| db[j as usize]).collect();
        around.sort_by(f64::total_cmp);
        Some(pk - around[around.len() / 2])
    };
    for (name, step, off) in [
        ("20 MHz multiples", 20e6, 0.0),
        ("10 MHz odd multiples", 20e6, 10e6),
        ("5 MHz odd multiples", 10e6, 5e6),
        ("2.5 MHz odd multiples", 5e6, 2.5e6),
        ("control +1.234 MHz", 5e6, 1.234e6),
        ("control +3.71 MHz", 5e6, 3.71e6),
    ] {
        let mut v = Vec::new();
        let mut f = ((lo * 1e6) / step).ceil() * step + off;
        while f < hi * 1e6 {
            if let Some(p) = prom(f) {
                v.push((f, p));
            }
            f += step;
        }
        let mut s: Vec<f64> = v.iter().map(|x| x.1).collect();
        s.sort_by(f64::total_cmp);
        let above = s.iter().filter(|x| **x > 3.0).count();
        println!(
            "{name:<24} n={:<4} median {:>5.1} dB  p90 {:>5.1} dB  >3dB {:>3} ({:.0}%)",
            s.len(),
            s[s.len() / 2],
            s[s.len() * 9 / 10],
            above,
            100.0 * above as f64 / s.len() as f64
        );
        if !name.starts_with("control") {
            let list: Vec<String> = v
                .iter()
                .filter(|x| x.1 > 3.0)
                .map(|x| format!("{:.0}:{:.0}", x.0 / 1e6, x.1))
                .collect();
            println!("    {}", list.join(" "));
        }
    }
}
