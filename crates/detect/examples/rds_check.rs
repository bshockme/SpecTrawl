//! Runs the RDS decoder on a cf32 SigMF channel capture of an FM broadcast
//! station (±100 kHz or wider, any rate ≥ 200 kS/s) and prints what it
//! got, second by second.
//!
//!     cargo run --release -p detect --example rds_check -- <path without extension>

use detect::rds::{Discriminator, RdsDecoder};
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).expect("path stem");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], "cf32_le", "channel captures only");
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    println!("{stem}: {:.1} s at {:.0} kS/s", x.len() as f64 / fs, fs / 1e3);
    let mut disc = Discriminator::new(fs);
    let mut dec = RdsDecoder::new(fs);
    let mut mpx = Vec::new();
    let step = fs as usize;
    for (k, chunk) in x.chunks(step).enumerate() {
        mpx.clear();
        disc.process(chunk, &mut mpx);
        dec.push(&mpx);
        let i = dec.info();
        println!(
            "  {:3} s: synced {} · ok {} bad {} corr {} · {} · PS {:?} · PTY {} · RT {:?}{}",
            k + 1,
            i.synced,
            i.groups_ok,
            i.groups_bad,
            i.blocks_corrected,
            i.station().unwrap_or_else(|| "-".into()),
            i.ps,
            i.pty_name(),
            i.rt,
            if i.af_mhz.is_empty() { String::new() } else { format!(" · AF {:?}", i.af_mhz) }
        );
    }
    let i = dec.info();
    println!("final: {} PS {:?} RT {:?} clock {:?}", i.station().unwrap_or_else(|| "-".into()), i.ps, i.rt, i.clock);
}
