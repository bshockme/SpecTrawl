//! Decodes DMR data bursts (CSBK, voice headers, terminators, data headers)
//! from a cf32 SigMF channel capture, reporting how many pass their checks.
//!
//!     cargo run --release -p detect --example dmr_bursts -- <path without extension>...

use std::collections::BTreeMap;

use detect::{decode, dmr};
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
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        let frames = detect::frames(&x, fs, 12_500.0, 54, 78);
        let mut types: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut voice = 0;
        let mut msgs: BTreeMap<String, usize> = BTreeMap::new();
        for fr in frames.iter().filter(|f| f.protocol == "DMR") {
            if fr.detail.contains("voice") {
                voice += 1;
                continue;
            }
            let Some(st) = decode::dmr_slot_type(fr) else { continue };
            if let Some(raw) = dmr::info_bits(fr) {
                let (r, c) = dmr::bptc_parity_stats(&raw);
                println!("  {} rows ok {r}/9 cols ok {c}/15 slot-type errs {}", decode::dmr_data_type_name(st.data_type), st.errors);
            }
            let e = types.entry(decode::dmr_data_type_name(st.data_type).to_string()).or_default();
            e.0 += 1;
            if let Some(b) = dmr::decode_burst(fr, st.data_type) {
                e.1 += 1;
                let k = match b {
                    dmr::Burst::Csbk(c) => {
                        format!("CSBK fid {:02X} op {:02X} {} {:02X?}", c.fid, c.opcode, dmr::csbk_name(c.fid, c.opcode), c.args)
                    }
                    dmr::Burst::VoiceHeader(l) | dmr::Burst::Terminator(l) => {
                        format!("LC flco {} fid {:02X} {} -> {}", l.flco, l.fid, l.source, l.target)
                    }
                    dmr::Burst::DataHeader { dpf, target, source, group } => format!("data hdr dpf {dpf} {source} -> {target} grp {group}"),
                };
                *msgs.entry(k).or_default() += 1;
            }
        }
        println!("{name}: {} DMR frames, {voice} voice syncs", frames.iter().filter(|f| f.protocol == "DMR").count());
        for (t, (n, ok)) in &types {
            println!("  {t}: {ok}/{n} pass their check");
        }
        let mut m: Vec<_> = msgs.into_iter().collect();
        m.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in m.iter().take(25) {
            println!("    {n:>4} × {k}");
        }
    }
}
