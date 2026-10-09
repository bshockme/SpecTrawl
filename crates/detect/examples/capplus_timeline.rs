//! Capacity Plus status over time on a recorded channel: when each rest
//! channel announcement was heard, and voice link control.
//!
//!     cargo run --release -p detect --example capplus_timeline -- <path without extension>

use detect::{decode, dmr};
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).expect("capture");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    // Decode in 10 s pieces so frame times are easy to place.
    let piece = (10.0 * fs) as usize;
    let mut last = String::new();
    let mut since = 0.0;
    let mut n = 0;
    for (k, w) in x.chunks(piece).enumerate() {
        let frames = detect::frames(w, fs, 12_500.0, 54, 78);
        for fr in frames.iter().filter(|f| f.protocol == "DMR" && f.detail.contains("data")) {
            let t = k as f64 * 10.0 + fr.at_symbol as f64 / 4800.0;
            let Some(st) = decode::dmr_slot_type(fr) else { continue };
            let s = match dmr::decode_burst(fr, st.data_type) {
                Some(dmr::Burst::Csbk(c)) => match c.capplus_status() {
                    Some(s) => format!("status rest {}/{} act {:02X?}", s.rest_repeater, s.rest_slot, s.activity),
                    None => format!("CSBK {}", dmr::csbk_name(c.fid, c.opcode)),
                },
                Some(dmr::Burst::VoiceHeader(l) | dmr::Burst::Terminator(l)) => format!("LC {l:?}"),
                _ => continue,
            };
            if s != last {
                if n > 0 {
                    println!("{since:7.1}-{t:7.1} s  x{n:<4} {last}");
                }
                last = s;
                since = t;
                n = 0;
            }
            n += 1;
        }
    }
    println!("{since:7.1}-  end     x{n:<4} {last}");
}
