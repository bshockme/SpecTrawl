//! Phase 2 encryption sync on real recordings: per logical channel, the
//! last four ESS-B pieces (4V bursts) and the next ESS-A (2V burst) form a
//! Reed-Solomon (44,16) word; prints what decodes, for both field orders,
//! next to the MAC_PTT algorithm seen on the channel.
//!
//!     cargo run --release -p detect --example p2_ess_probe -- <wacn hex> <system hex> <nac hex> <path without extension>

use std::collections::{BTreeMap, BTreeSet};

use detect::p25p2::{self, ScrambleKey};
use num_complex::Complex32;

fn hexbits(bits: &[u8]) -> Vec<u8> {
    bits.chunks(6).map(|c| c.iter().fold(0, |a, &b| a << 1 | b)).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let seq = p25p2::scrambling(ScrambleKey { wacn: hex(&a[1]), system: hex(&a[2]) as u16, nac: hex(&a[3]) as u16 });
    let stem = &a[4];
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let mut bursts: BTreeMap<(usize, usize), p25p2::VoiceBurst> = BTreeMap::new();
    let mut ptt: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    for (n, w) in x.chunks((10.0 * fs) as usize).enumerate() {
        for f in detect::frames(w, fs, 12_500.0, 560, 380).iter().filter(|f| f.protocol == "P25" && f.detail == "Phase 2") {
            let Some(info) = p25p2::fragment(f, &[&seq]) else { continue };
            let start = n * 60_000 + f.at_symbol + info.start - f.sync_start;
            if seen.range(start.saturating_sub(4)..start + 5).next().is_some() {
                continue;
            }
            seen.insert(start);
            for s in &info.signals {
                if let p25p2::Mac::Ptt { end: false, algid, .. } = s.mac {
                    *ptt.entry(format!("ch{} 0x{algid:02X}", s.channel)).or_default() += 1;
                }
            }
            for v in info.voice {
                bursts.insert((start + v.slot * 180, v.slot), v);
            }
        }
    }
    println!("MAC_PTT algorithms: {ptt:?}");
    for ch in 0..2u8 {
        let mut b_parts: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut results: BTreeMap<String, usize> = BTreeMap::new();
        for ((at, _), v) in bursts.iter().filter(|(_, v)| v.channel == ch) {
            if v.ess.len() == 24 {
                b_parts.push((*at, hexbits(&v.ess)));
                continue;
            }
            let recent: Vec<&Vec<u8>> = b_parts.iter().filter(|(t, _)| at - t < 2200).map(|(_, h)| h).collect();
            if recent.len() < 4 {
                *results.entry("too few ESS-B".into()).or_default() += 1;
                continue;
            }
            let mut cw: Vec<u8> = recent[recent.len() - 4..].iter().flat_map(|h| h.iter().copied()).collect();
            cw.extend(hexbits(&v.ess));
            let mut word = [0u8; 63];
            word[63 - 44..].copy_from_slice(&cw);
            match detect::p25_lc::rs_decode_erasures(&mut word, 28, &[]) {
                Some(fixed) if word[..19].iter().all(|&h| h == 0) => {
                    let d = &word[19..35];
                    let v = d.iter().fold(0u128, |a, &h| a << 6 | h as u128);
                    // ALGID, KID, MI vs MI, ALGID, KID.
                    let first = format!("algid-first 0x{:02X} kid {}", (v >> 88) as u8, (v >> 72) as u16);
                    let last = format!("mi-first algid 0x{:02X} kid {}", (v >> 16) as u8, v as u16);
                    *results.entry(format!("ok ({fixed} fixed): {first} | {last}")).or_default() += 1;
                }
                _ => *results.entry("RS failed".into()).or_default() += 1,
            }
        }
        println!("channel {ch}:");
        for (k, n) in &results {
            println!("  {n:4} x {k}");
        }
    }
}
