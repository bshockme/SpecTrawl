//! Finds the Golay / Reed-Solomon orientation of P25 TDULC link control on
//! a real traffic channel capture: counts codewords that check under each
//! variant.
//!
//!     cargo run --release -p detect --example p25_lc_probe -- <path without extension>

use detect::{decode, p25};
use num_complex::Complex32;

fn golay_parity(data: u32, poly: u32) -> u32 {
    let mut r = data << 11;
    for b in (11..23).rev() {
        if r >> b & 1 == 1 {
            r ^= poly << (b - 11);
        }
    }
    r & 0x7FF
}

fn main() {
    let stem = std::env::args().nth(1).expect("capture");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let frames = detect::frames(&x, fs, 12_500.0, 8, 300);
    let mut words: Vec<u32> = Vec::new();
    let mut clean: Vec<Vec<u8>> = Vec::new();
    let mut lcs: std::collections::BTreeMap<String, usize> = Default::default();
    for f in frames.iter().filter(|f| f.protocol == "P25") {
        let Some(n) = decode::p25_nid(f) else { continue };
        if n.duid != 0xF {
            continue;
        }
        if let Some(lc) = detect::p25_lc::tdulc(f) {
            *lcs.entry(format!("lco {:02X} p {} mfid {:02X} args {:02X?}", lc.lco, lc.protected, lc.mfid, lc.args)).or_insert(0usize) += 1;
        } else {
            *lcs.entry("undecodable".into()).or_insert(0usize) += 1;
        }
        let Some(d) = p25::data_dibits(f, 144) else { continue };
        let bits: Vec<u32> = d.iter().flat_map(|&v| [(v >> 1) as u32, (v & 1) as u32]).collect();
        let ws: Vec<u32> = bits.chunks(24).map(|c| c.iter().fold(0, |a, b| a << 1 | b)).collect();
        if ws.iter().all(|&w| golay_parity(w >> 12, 0xC75) == w >> 1 & 0x7FF) {
            clean.push(ws.iter().flat_map(|&w| [(w >> 18) as u8, (w >> 12 & 0x3F) as u8]).collect());
        }
        words.extend(ws);
    }
    for (k, n) in &lcs {
        println!("{n:>4} x {k}");
    }
    println!("{} TDULC Golay words", words.len());
    for poly in [0xC75u32, 0xAE3] {
        for layout in ["data|par11|p", "data|p|par11", "par|data"] {
            let ok = words
                .iter()
                .filter(|&&w| {
                    let (data, par) = match layout {
                        "data|par11|p" => (w >> 12, w >> 1 & 0x7FF),
                        "data|p|par11" => (w >> 12, w & 0x7FF),
                        _ => (w & 0xFFF, w >> 13),
                    };
                    golay_parity(data, poly) == par
                })
                .count();
            println!("  poly {poly:#05x} {layout:14} {ok}/{}", words.len());
        }
    }
    // GF(64), x^6 + x + 1.
    let mut exp = [0u8; 126];
    let mut v = 1u8;
    for e in exp.iter_mut().take(63) {
        *e = v;
        v <<= 1;
        if v & 0x40 != 0 {
            v ^= 0x43;
        }
    }
    for i in 63..126 {
        exp[i] = exp[i - 63];
    }
    let mut log = [0usize; 64];
    for (i, &e) in exp.iter().take(63).enumerate() {
        log[e as usize] = i;
    }
    let mul = |a: u8, b: u8| if a == 0 || b == 0 { 0 } else { exp[log[a as usize] + log[b as usize]] };
    println!("{} TDULCs with clean Golay", clean.len());
    for rev in [false, true] {
        for first in [0usize, 1] {
            let ok = clean
                .iter()
                .filter(|h| {
                    (first..first + 12).all(|j| {
                        // c(x) evaluated at alpha^j; h[0] the highest degree unless rev.
                        let mut acc = 0u8;
                        let it: Vec<u8> = if rev { h.iter().rev().copied().collect() } else { h.to_vec() };
                        for &c in &it {
                            acc = mul(acc, exp[j % 63]) ^ c;
                        }
                        acc == 0
                    })
                })
                .count();
            println!("  rev {rev} roots {first}..{} : {ok}/{}", first + 11, clean.len());
        }
    }
    if let Some(h) = clean.first() {
        println!("  first: {h:02X?}");
    }
}
