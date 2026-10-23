//! Fragment bookkeeping per frame, without message text: for every page
//! its capcode, phase, kind, fragment number, continued flag, message
//! number, length and damage, so joiner gaps can be traced to the frames
//! that should have carried the missing fragments.
//!
//!     cargo run --release -p detect --example flex_frags -- <stem>

use detect::flex;
use num_complex::Complex32;

fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn main() {
    for stem in std::env::args().skip(1) {
        let (x, fs) = load(&stem);
        let frames = flex::decode(&x, fs);
        for f in &frames {
            println!("t {:7.2} {} cycle {:2} frame {:3} blocks {:?}", f.at as f64 / fs, f.mode(), f.cycle, f.frame, f.block_ok);
            for p in &f.pages {
                println!(
                    "      {:09} {} {:7} frag {} cont {} msg {:2} {:3} chars{}{}",
                    p.capcode,
                    p.phase,
                    p.kind,
                    p.fragment,
                    p.continued as u8,
                    p.message_no,
                    p.text.chars().count(),
                    if p.damaged { " DAMAGED" } else { "" },
                    if p.text.contains("???") { " ???" } else { "" }
                );
            }
        }
        let joined = detect::flex_join::join_frames(&frames);
        println!(
            "joined: {} pages, {} damaged, {} with gaps",
            joined.len(),
            joined.iter().filter(|p| p.damaged).count(),
            joined.iter().filter(|p| p.text.contains('…')).count()
        );
        for p in joined.iter().filter(|p| p.damaged) {
            println!(
                "   damaged {:09} {} {} frag {} cont {} msg {} {} chars, gaps at char {:?}",
                p.capcode,
                p.phase,
                p.kind,
                p.fragment,
                p.continued as u8,
                p.message_no,
                p.text.chars().count(),
                p.text.char_indices().filter(|(_, c)| *c == '…').map(|(i, _)| i).collect::<Vec<_>>()
            );
        }
    }
}
