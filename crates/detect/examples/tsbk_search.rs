//! Finds the TSBK coding parameters that make CRCs check on a real P25
//! control-channel capture, then prints the decoded messages.
//!
//!     cargo run --release -p detect --example tsbk_search -- <capture stem>

use std::collections::BTreeMap;

use detect::p25::{self, Coding};
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).expect("stem");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    // Long frames: sync + NID + three TSBKs ≈ 360 dibits.
    let frames: Vec<_> = detect::frames(&x, fs, 12_500.0, 8, 380)
        .into_iter()
        .filter(|f| f.protocol == "P25" && f.detail == "Phase 1")
        .filter(|f| detect::decode::p25_nid(f).is_some_and(|n| n.duid == 0x7))
        .collect();
    println!("{} TSDU frames", frames.len());
    let blocks: Vec<[u8; 98]> = frames.iter().flat_map(p25::tsbk_blocks).collect();
    println!("{} TSBK blocks", blocks.len());

    let mut results = Vec::new();
    for scatter in [true, false] {
        for transpose in [false, true] {
            for crc_init in [0x0000u16, 0xFFFF] {
                for crc_xorout in [0x0000u16, 0xFFFF] {
                    let c = Coding { scatter, transpose, crc_init, crc_xorout };
                    let ok = blocks.iter().filter(|b| p25::decode_block(b, c).crc_ok).count();
                    results.push((ok, c));
                }
            }
        }
    }
    results.sort_by(|a, b| b.0.cmp(&a.0));
    for (ok, c) in &results[..4] {
        println!("{ok:>4} CRC ok · {c:?}");
    }
    let best = results[0].1;
    let mut ops: BTreeMap<String, usize> = BTreeMap::new();
    let mut msgs = Vec::new();
    let mut plans: BTreeMap<u8, p25::BandPlan> = BTreeMap::new();
    for f in &frames {
        for t in p25::tsbks(f, best) {
            if !t.crc_ok {
                continue;
            }
            *ops.entry(format!("{:#04x}/{:#04x} {}", t.mfid(), t.opcode(), p25::opcode_name(t.opcode()))).or_default() += 1;
            let m = p25::parse(&t);
            if let p25::Message::IdenUpdate(bp) = &m {
                plans.insert(bp.iden, *bp);
            }
            msgs.push(m);
        }
    }
    println!("band plans:");
    for bp in plans.values() {
        println!("  iden {:>2}: base {:.5} MHz, spacing {:.2} kHz, {} slot(s), bw {:.2} kHz, tx offset {:+.3} MHz", bp.iden, bp.base_hz / 1e6, bp.spacing_hz / 1e3, bp.slots, bp.bandwidth_hz / 1e3, bp.tx_offset_hz / 1e6);
    }
    let f = |ch: p25::Channel| plans.get(&ch.iden).map(|bp| format!("{:.5} MHz{}", bp.freq(ch) / 1e6, if bp.slots > 1 { format!(" slot {}", bp.slot(ch)) } else { String::new() })).unwrap_or(format!("ch {ch}"));
    let mut seen = std::collections::BTreeSet::new();
    for m in &msgs {
        let line = match m {
            p25::Message::GroupGrant { channel, group, source, .. } => format!("GRANT  TG {group} on {} (src {source})", f(*channel)),
            p25::Message::GroupGrantUpdate { a, b } => {
                let mut s = format!("UPDATE TG {} on {}", a.1, f(a.0));
                if let Some(b) = b {
                    s += &format!(", TG {} on {}", b.1, f(b.0));
                }
                s
            }
            p25::Message::RfssStatus { system, rfss, site, channel } => format!("THIS SITE   sys {system:#05x} rfss {rfss} site {site} control {}", f(*channel)),
            p25::Message::NetworkStatus { wacn, system, channel } => format!("NETWORK     WACN {wacn:#07x} sys {system:#05x} control {}", f(*channel)),
            p25::Message::AdjacentSite { system, rfss, site, channel } => format!("ADJACENT    sys {system:#05x} rfss {rfss} site {site} control {}", f(*channel)),
            _ => continue,
        };
        if seen.insert(line.clone()) {
            println!("  {line}");
        }
    }
    println!("opcodes (mfid/op): {ops:?}");
}
