//! Runs one Airspy live and lists emitters sorted by frequency, flagging
//! emitters that sit closer together than a channel (likely one transmitter
//! split in two), with the records that went into each.
//!
//!     cargo run --release -p engine-old --example emitter_check -- <serial> <MHz | loMHz-hiMHz> <seconds> [channel kHz]
//!
//! A range runs the device in Sweep mode (the survey) instead of live.

use std::time::Duration;

use engine_old::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, secs) = (&a[1], a[3].parse::<u64>().unwrap());
    let chan = a.get(4).map(|v| v.parse::<f64>().unwrap() * 1e3).unwrap_or(12.5e3);
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    let mode = match a[2].split_once('-') {
        Some((lo, hi)) => Mode::Sweep(SweepPlan {
            start_hz: lo.parse::<f64>().unwrap() * 1e6,
            stop_hz: hi.parse::<f64>().unwrap() * 1e6,
            ..Default::default()
        }),
        None => {
            d.set_center(a[2].parse::<f64>().unwrap() * 1e6).unwrap();
            Mode::Live
        }
    };
    let src = Source::start(d, SourceConfig::default(), mode).unwrap();
    let mut counts = Vec::new();
    for s in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        if s % 10 == 9 {
            let em = src.emitters();
            counts.push(format!("{}/{}", em.iter().filter(|e| e.confirmed).count(), em.len()));
        }
    }
    let mut em = src.emitters();
    em.sort_by(|x, y| x.center_hz.total_cmp(&y.center_hz));
    let recs = src.records_since(0);
    println!("{} emitters, {} records; confirmed/all every 10 s: {}", em.len(), recs.len(), counts.join(" "));
    em.retain(|e| e.confirmed);

    let mut dup = 0;
    for (i, e) in em.iter().enumerate() {
        let near = i > 0 && e.center_hz - em[i - 1].center_hz < chan / 2.0;
        if near {
            dup += 1;
        }
        println!(
            "{} {:>10.4} bw {:>6.1} kHz  recs {:>4}  duty {:>3.0}%  snr {:>3.0}  {}",
            if near { "~" } else { " " },
            e.center_hz / 1e6,
            e.bandwidth_hz / 1e3,
            e.records,
            e.duty() * 100.0,
            e.peak_snr_db,
            e.label().unwrap_or_default()
        );
    }
    println!("{dup} emitters within {:.1} kHz of the one below", chan / 2e3);

    // Record shapes per emitter for the busiest clusters.
    println!("\nrecord centre/bw (kHz rel. emitter) for emitters with a near neighbour:");
    for (i, e) in em.iter().enumerate() {
        let near = (i > 0 && e.center_hz - em[i - 1].center_hz < chan / 2.0)
            || (i + 1 < em.len() && em[i + 1].center_hz - e.center_hz < chan / 2.0);
        if !near {
            continue;
        }
        let rs: Vec<String> = recs
            .iter()
            .filter(|r| r.emitter == Some(e.id))
            .take(12)
            .map(|r| format!("{:+.1}/{:.1}x{}", (r.center_hz() - e.center_hz) / 1e3, r.bandwidth_hz() / 1e3, r.rows))
            .collect();
        println!("  {:>10.4}: {}", e.center_hz / 1e6, rs.join(" "));
    }
}
