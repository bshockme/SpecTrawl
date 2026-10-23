//! Decodes the P25 Phase 1 voice in a recording to an 8 kHz WAV file:
//! every LDU's nine IMBE frames, in order, with silence for gaps.
//!
//!     cargo run --release -p detect --example imbe_wav -- <path without extension> <out.wav> [frames.bin]
//!
//! The optional third file gets the same stream as records for other
//! decoders to compare against: 0 + u32 samples of silence, 1 = decoder
//! reset, 2 + u0..u7 (u16) + ε0..ε6 (u8) = a frame.

use detect::{decode, imbe};
use num_complex::Complex32;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (stem, out) = (&args[1], &args[2]);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let frames = detect::p25_voice_frames(&x, fs);
    let mut dec = imbe::Decoder::default();
    let mut pcm: Vec<i16> = Vec::new();
    let mut last_end: Option<f64> = None;
    let (mut counts, mut ldus, mut skipped) = ([0usize; 3], 0, 0);
    // Whether the current transmission is encrypted: LDU1's service
    // options, then LDU2's algorithm.
    let mut encrypted = None;
    let mut dump: Vec<u8> = Vec::new();
    let silence = |dump: &mut Vec<u8>, n: usize| {
        dump.push(0);
        dump.extend((n as u32).to_le_bytes());
    };
    for f in &frames {
        if !matches!(decode::p25_nid(f).map(|n| n.duid), Some(0x5 | 0xA)) {
            continue;
        }
        let Some(v) = imbe::ldu_frames(f) else { continue };
        // An LDU is 180 ms; fill longer gaps with silence (capped).
        let t = f.at_symbol as f64 / 4800.0;
        if let Some(e) = last_end {
            let gap = (t - e).clamp(0.0, 2.0);
            if gap > 0.05 {
                pcm.extend(std::iter::repeat_n(0, (gap * 8000.0) as usize));
                silence(&mut dump, (gap * 8000.0) as usize);
                dump.push(1);
                dec = imbe::Decoder::default();
                encrypted = None;
            }
        }
        last_end = Some(t + 0.18);
        ldus += 1;
        match decode::p25_nid(f).map(|n| n.duid) {
            Some(0x5) => encrypted = detect::p25_lc::ldu1(f).and_then(|lc| lc.call()).map(|c| c.encrypted).or(encrypted),
            _ => encrypted = detect::p25_lc::ldu2(f).map(|e| e.encrypted()).or(encrypted),
        }
        if encrypted == Some(true) {
            skipped += 1;
            pcm.extend(std::iter::repeat_n(0, 9 * 160));
            silence(&mut dump, 9 * 160);
            continue;
        }
        for c in &v {
            dump.push(2);
            c.u.iter().for_each(|u| dump.extend(u.to_le_bytes()));
            dump.extend(c.errors);
            let (s, o) = dec.decode(c);
            counts[o as usize] += 1;
            pcm.extend(s.iter().map(|&v| v.clamp(-32768.0, 32767.0) as i16));
        }
    }
    println!(
        "{ldus} LDUs ({skipped} encrypted, silent); frames decoded/repeated/muted {counts:?}; {:.1} s of audio",
        pcm.len() as f64 / 8000.0
    );
    let peak = pcm.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    let rms = (pcm.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / pcm.len().max(1) as f64).sqrt();
    println!("peak {peak}, rms {rms:.0}");
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    let data = (pcm.len() * 2) as u32;
    w.extend(b"RIFF");
    w.extend((36 + data).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(8000u32.to_le_bytes());
    w.extend(16000u32.to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend(data.to_le_bytes());
    for s in pcm {
        w.extend(s.to_le_bytes());
    }
    std::fs::write(out, w).unwrap();
    if let Some(p) = args.get(3) {
        std::fs::write(p, dump).unwrap();
    }
}
