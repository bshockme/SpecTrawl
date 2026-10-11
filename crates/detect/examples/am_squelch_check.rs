//! Runs the AM demodulator with its carrier squelch over a cf32 SigMF
//! channel capture and prints, per 0.5 s, whether the squelch was open and
//! the audio level, so the thresholds can be judged on a real channel.
//!
//!     cargo run --release -p detect --example am_squelch_check -- <path without extension>

use detect::demod::{Demod, Mode};
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).expect("path stem");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    println!("{stem}: {:.1} s at {:.0} kS/s", x.len() as f64 / fs, fs / 1e3);
    let mut d = Demod::new(Mode::Am, fs);
    let mut out = Vec::new();
    let step = (fs * 0.5) as usize;
    let mut line = String::new();
    for (k, chunk) in x.chunks(step).enumerate() {
        out.clear();
        d.process(chunk, &mut out);
        let rms = (out.iter().map(|v| v * v).sum::<f32>() / out.len().max(1) as f32).sqrt();
        let carrier = (chunk.iter().map(|s| s.norm_sqr()).sum::<f32>() / chunk.len() as f32).sqrt();
        line += &format!("{}{:.2} ", if d.is_open() { "O" } else { "." }, rms);
        if k % 8 == 7 {
            println!("  {:5.1} s: {line} carrier {:.3}", (k + 1) as f64 * 0.5, carrier);
            line.clear();
        }
    }
    if !line.is_empty() {
        println!("  end: {line}");
    }
}
