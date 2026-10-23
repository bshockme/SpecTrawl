//! Replays a channel capture through the live pager monitor's windowing
//! (decode every `BLOCK_S` of new signal over a buffer that keeps the last
//! `OVERLAP_S`, feeding the fragment joiner) and compares the pages that
//! come out with one decode of the whole capture, to see what the
//! windowing itself costs.
//!
//!     cargo run --release -p detect --example flex_windowed -- [--show] <stem>...

use detect::{flex, flex_join::FragmentJoiner};
use num_complex::Complex32;

const BLOCK_S: f64 = 1.8;
const OVERLAP_S: f64 = 2.2;

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
    let show = std::env::args().any(|a| a == "--show");
    for stem in std::env::args().skip(1).filter(|a| a != "--show") {
        let (x, fs) = load(&stem);
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        // Whole capture at once.
        let frames = flex::decode(&x, fs);
        let whole = detect::flex_join::join_frames(&frames);
        // The monitor's way: chunks of ~26 ms arrive, decode when the buffer
        // holds BLOCK_S + OVERLAP_S, keep OVERLAP_S.
        let mut joiner = FragmentJoiner::new();
        let mut buf: Vec<Complex32> = Vec::new();
        let mut out: Vec<flex::Page> = Vec::new();
        let mut frames_seen = 0;
        let mut decodes = 0;
        for chunk in x.chunks((fs * 0.026) as usize) {
            buf.extend_from_slice(chunk);
            if buf.len() as f64 >= (BLOCK_S + OVERLAP_S) * fs {
                decodes += 1;
                for f in flex::decode(&buf, fs) {
                    frames_seen += 1;
                    if !joiner.new_frame(f.cycle, f.frame) {
                        continue;
                    }
                    for p in f.pages {
                        out.extend(joiner.push(p, f.cycle, f.frame));
                    }
                    out.extend(joiner.expire(f.cycle, f.frame));
                }
                let keep = (OVERLAP_S * fs) as usize;
                let cut = buf.len() - keep;
                buf.drain(..cut);
            }
        }
        out.extend(joiner.flush());
        let dmg = |v: &[flex::Page]| v.iter().filter(|p| p.damaged).count();
        println!(
            "{name}: whole: {} frames, {} pages ({} damaged); windowed: {decodes} decodes, {frames_seen} frame decodes, {} pages ({} damaged)",
            frames.len(),
            whole.len(),
            dmg(&whole),
            out.len(),
            dmg(&out)
        );
        if show {
            for p in out.iter().filter(|p| p.damaged) {
                println!("    windowed {}", p.summary());
            }
        }
    }
}
