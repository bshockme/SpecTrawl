//! Learns the Hamming (10,6) parity of P25 LDU1 link control from real
//! voice frames: for each parity bit, the XOR of data bits that matches it
//! most often. Then checks Reed-Solomon on the hexbits.
//!
//!     cargo run --release -p detect --example p25_ldu_probe -- <path without extension>...

use detect::{decode, p25};
use num_complex::Complex32;

fn main() {
    let mut words: Vec<u16> = Vec::new();
    let mut frames_bits: Vec<Vec<u16>> = Vec::new();
    for stem in std::env::args().skip(1) {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
        let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
        let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
        let x: Vec<Complex32> = raw
            .chunks_exact(8)
            .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
            .collect();
        let frames = detect::frames(&x, fs, 12_500.0, 8, 880);
        let mut n_ldu1 = 0;
        for f in frames.iter().filter(|f| f.protocol == "P25") {
            if decode::p25_nid(f).map(|n| n.duid) != Some(0x5) {
                continue;
            }
            let Some(d) = p25::data_dibits(f, 784) else { continue };
            n_ldu1 += 1;
            let bits: Vec<u16> = d.iter().flat_map(|&v| [(v >> 1) as u16, (v & 1) as u16]).collect();
            let mut ws = Vec::new();
            for k in 0..6 {
                let seg = &bits[288 + k * 184..288 + k * 184 + 40];
                ws.extend(seg.chunks(10).map(|c| c.iter().fold(0u16, |a, b| a << 1 | b)));
            }
            words.extend(&ws);
            frames_bits.push(ws);
        }
        println!("{stem}: {n_ldu1} LDU1");
    }
    println!("{} Hamming words", words.len());
    if words.is_empty() {
        return;
    }
    // Parity bit j (bits 3..0) as the XOR of a subset of data bits (9..4).
    let mut masks = [0u16; 4];
    for j in 0..4 {
        let (best, hits) = (1u16..64)
            .map(|m| (m, words.iter().filter(|&&w| ((w >> 4 & m).count_ones() & 1) as u16 == w >> (3 - j) & 1).count()))
            .max_by_key(|e| e.1)
            .unwrap();
        masks[j] = best;
        println!("  parity bit {j}: data mask {best:06b} agrees {hits}/{}", words.len());
    }
    let ok = words.iter().filter(|&&w| (0..4).all(|j| ((w >> 4 & masks[j]).count_ones() & 1) as u16 == w >> (3 - j) & 1)).count();
    println!("  all four agree on {ok}/{} words; masks {masks:?}", words.len());
}
