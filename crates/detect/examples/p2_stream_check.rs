//! Runs a recording through the live Phase 2 voice decoder
//! (`Phase2Stream`) in small chunks, as the engine would: voice bursts
//! decoded per logical channel, calls and ciphers seen, time taken.
//!
//!     cargo run --release -p detect --example p2_stream_check -- <wacn hex> <system hex> <nac hex> <path without extension>

use detect::p25_voice::{LiveVoice, Phase2Stream};
use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    detect::p25p2::register_system(hex(&a[1]), hex(&a[2]) as u16);
    detect::p25p2::register_nac(hex(&a[3]) as u16);
    let stem = &a[4];
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let dur = x.len() as f64 / fs;
    for ch in 0..2u8 {
        let t0 = std::time::Instant::now();
        let mut vs = Phase2Stream::new(fs, Some(ch));
        let (mut bursts, mut starts, mut audio) = (0, 0, 0usize);
        let mut calls = std::collections::BTreeMap::<String, usize>::new();
        for chunk in x.chunks((fs / 50.0) as usize) {
            let o = vs.push(chunk);
            bursts += o.ldus;
            starts += o.started as usize;
            audio += o.audio.len();
            if o.ldus > 0 {
                let k = format!("{} {}", vs.call().map(|c| c.summary()).unwrap_or("?".into()), vs.cipher().unwrap_or("clear"));
                *calls.entry(k).or_default() += o.ldus;
            }
        }
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "ch{ch}: {bursts} voice bursts in {starts} transmissions, {:.1} s audio, {:.1}% of real time",
            audio as f64 / 8000.0,
            100.0 * dt / dur
        );
        for (k, n) in &calls {
            println!("    {n:4} bursts  {k}");
        }
    }
}
