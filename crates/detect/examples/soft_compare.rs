//! Hard vs soft decision IMBE decoding on a recording: where the two
//! disagree, which one's pitch is continuous with the neighbouring frames
//! (speech pitch moves slowly; a wrong decode jumps).
//!
//!     cargo run --release -p detect --example soft_compare -- <path without extension>

use detect::{decode, imbe};
use num_complex::Complex32;

fn main() {
    let stem = std::env::args().nth(1).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let mut enc = None;
    let (mut hard, mut soft) = (Vec::new(), Vec::new());
    for f in detect::p25_voice_frames(&x, fs) {
        match decode::p25_nid(&f).map(|n| n.duid) {
            Some(0x5) => enc = detect::p25_lc::ldu1(&f).and_then(|lc| lc.call()).map(|c| c.encrypted).or(enc),
            _ => enc = detect::p25_lc::ldu2(&f).filter(|e| e.algorithm() != "other").map(|e| e.encrypted()).or(enc),
        }
        if enc != Some(false) {
            continue;
        }
        hard.extend(imbe::ldu_frames_hard(&f).unwrap());
        soft.extend(imbe::ldu_frames(&f).unwrap());
    }
    let b0 = |c: &imbe::Coded| imbe::pitch_code(&c.u) as i32;
    let (mut differ, mut soft_better, mut hard_better, mut tie) = (0, 0, 0, 0);
    for i in 1..hard.len() - 1 {
        if hard[i].u == soft[i].u {
            continue;
        }
        differ += 1;
        // Neighbours from the soft decode where both agree, else skip.
        let around = (b0(&soft[i - 1]) + b0(&soft[i + 1])) / 2;
        let (h, s) = ((b0(&hard[i]) - around).abs(), (b0(&soft[i]) - around).abs());
        match h.cmp(&s) {
            std::cmp::Ordering::Greater => soft_better += 1,
            std::cmp::Ordering::Less => hard_better += 1,
            _ => tie += 1,
        }
    }
    let invalid = |v: &[imbe::Coded]| v.iter().filter(|c| b0(c) > 207).count();
    println!(
        "{} clear frames; decodes differ on {differ}: pitch smoother with soft {soft_better}, hard {hard_better}, same {tie}; reserved pitch hard {} soft {}",
        hard.len(),
        invalid(&hard),
        invalid(&soft)
    );
}
