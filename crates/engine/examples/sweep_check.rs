//! Runs one Airspy in Sweep mode and prints the stitched spectrum, coarsely,
//! for checking against known signals.
//!
//!     cargo run --release -p engine --example sweep_check -- <serial> <lo MHz> <hi MHz> <seconds> [guard ms]

use std::time::Duration;

use engine::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, lo, hi, secs) = (&a[1], a[2].parse::<f64>().unwrap(), a[3].parse::<f64>().unwrap(), a[4].parse::<u64>().unwrap());
    let guard_s = a.get(5).map(|g| g.parse::<f64>().unwrap() / 1e3).unwrap_or(SweepPlan::default().guard_s);
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let d = b.open(serial).expect("open");
    let src = Source::start(
        d,
        SourceConfig::default(),
        Mode::Sweep(SweepPlan { start_hz: lo * 1e6, stop_hz: hi * 1e6, guard_s, ..Default::default() }),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(secs));
    let rows = src.rows_after(None);
    let st = src.sweep_status().unwrap();
    println!(
        "{} passes ({:.3} s each) · {:.0} hops/s · settle re-measured {} · slow hops {} · lock waits {} (lock flag {}) · gaps redone {} · hop {:.1} MHz",
        rows.len(),
        st.last_pass_s,
        st.hops_per_sec,
        st.settle_retries,
        st.slow_hops,
        st.lock_waits,
        st.tuner_lock,
        st.redone_hops,
        st.hop_hz / 1e6
    );
    // Average the last 10 passes, then print 0.5 MHz buckets (mean dB).
    let last: Vec<_> = rows.iter().rev().take(10).collect();
    let n = last[0].db.len();
    let t = last[0].tuning;
    let mut avg = vec![0.0f64; n];
    for r in &last {
        for (a, v) in avg.iter_mut().zip(&r.db) {
            *a += 10f64.powf(*v as f64 / 10.0);
        }
    }
    let f0 = t.center_hz - t.span_hz / 2.0;
    let bw = 0.5e6;
    let mut f = f0;
    while f < f0 + t.span_hz {
        let i0 = ((f - f0) / t.span_hz * n as f64) as usize;
        let i1 = (((f + bw - f0) / t.span_hz * n as f64) as usize).min(n);
        let m = avg[i0..i1].iter().sum::<f64>() / (i1 - i0).max(1) as f64 / last.len() as f64;
        let db = 10.0 * m.log10();
        let bar = "#".repeat(((db + 100.0).max(0.0) / 1.5) as usize);
        println!("{:>8.1} {:>7.1} {bar}", f / 1e6, db);
        f += bw;
    }
}
