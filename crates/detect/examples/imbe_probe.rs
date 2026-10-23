//! Pulls the IMBE voice frames out of every LDU in P25 Phase 1 recordings
//! and reports how cleanly they error-correct: with the right positions,
//! interleave and bit modulation most frames correct a handful of bits;
//! with any of them wrong every frame looks like a 50% BER channel.
//!
//!     cargo run --release -p detect --example imbe_probe -- <path without extension>...

use detect::{decode, imbe};
use num_complex::Complex32;

fn main() {
    for stem in std::env::args().skip(1) {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
        let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
        let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
        let x: Vec<Complex32> = raw
            .chunks_exact(8)
            .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
            .collect();
        let frames = detect::p25_voice_frames(&x, fs);
        let (mut ldus, mut voice) = (0, 0);
        let mut total_hist = [0usize; 8];
        let (mut e0_ok, mut b0_ok) = (0, 0);
        let mut algs: std::collections::BTreeMap<String, usize> = Default::default();
        let mut enc = None;
        // Clear frames: (all, ε0 = 0, ε0 = 0 but b0 reserved, repeat rule hit).
        let mut clear = [0usize; 4];
        for f in &frames {
            if !matches!(decode::p25_nid(f).map(|n| n.duid), Some(0x5 | 0xA)) {
                continue;
            }
            let Some(v) = imbe::ldu_frames(f) else { continue };
            ldus += 1;
            match decode::p25_nid(f).map(|n| n.duid) {
                Some(0x5) => enc = detect::p25_lc::ldu1(f).and_then(|lc| lc.call()).map(|c| c.encrypted).or(enc),
                _ => enc = detect::p25_lc::ldu2(f).map(|e| e.encrypted()).or(enc),
            }
            if enc == Some(false) {
                for c in &v {
                    let b0 = imbe::pitch_code(&c.u);
                    clear[0] += 1;
                    clear[1] += (c.errors[0] == 0) as usize;
                    clear[2] += (c.errors[0] == 0 && b0 > 207) as usize;
                    clear[3] += (c.errors[0] >= 2 && c.total_errors() >= 10) as usize;
                }
            }
            if decode::p25_nid(f).map(|n| n.duid) == Some(0xA) {
                let a = detect::p25_lc::ldu2(f)
                    .map(|e| format!("{} 0x{:02X} key {}", e.algorithm(), e.algid, e.kid))
                    .unwrap_or("RS failed".into());
                *algs.entry(a).or_default() += 1;
            }
            for c in v {
                voice += 1;
                let t = c.total_errors() as usize;
                total_hist[(t / 4).min(7)] += 1;
                e0_ok += (c.errors[0] <= 1) as usize;
                // b0: u0 bits 11..6 are b0 bits 7..2; u7 bits 2..1 are b0 bits 1..0.
                let b0 = (c.u[0] >> 6) << 2 | (c.u[7] >> 1 & 3);
                b0_ok += (b0 <= 207) as usize;
            }
        }
        println!("{stem}: {ldus} LDUs, {voice} voice frames");
        println!("  LDU2 encryption sync: {algs:?}");
        println!("  clear frames {}, ε0=0 {}, of those b0 reserved {}; garbled-c0 rule {}", clear[0], clear[1], clear[2], clear[3]);
        if voice == 0 {
            continue;
        }
        let pct = |n: usize| 100.0 * n as f64 / voice as f64;
        println!("  ε0 ≤ 1: {:.0}%   b0 valid: {:.0}%", pct(e0_ok), pct(b0_ok));
        let labels = ["0-3", "4-7", "8-11", "12-15", "16-19", "20-23", "24-27", "28+"];
        for (l, n) in labels.iter().zip(total_hist) {
            println!("  εT {l:>5}: {n:6} ({:.0}%)", pct(n));
        }
    }
}
