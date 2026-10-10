//! Decodes the P25 Phase 2 voice in a recording: every voice burst of each
//! logical channel through the half-rate vocoder, one WAV per channel
//! (silence where the channel carried no voice), with frame statistics.
//!
//!     cargo run --release -p detect --example p2_voice_wav -- <wacn hex> <system hex> <nac hex> <path without extension> <out prefix>
//!
//! Also writes `<out prefix>_ch<n>.bin`, the frames for other decoders to
//! compare: 0 + u32 samples of silence, 1 = reset, 3 + u0..u3 (u16) +
//! ε0, ε1 (u8).

use std::collections::{BTreeMap, BTreeSet};

use detect::ambe;
use detect::p25p2::{self, ScrambleKey};
use num_complex::Complex32;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let key = ScrambleKey { wacn: hex(&a[1]), system: hex(&a[2]) as u16, nac: hex(&a[3]) as u16 };
    let seq = p25p2::scrambling(key);
    let (stem, prefix) = (&a[4], &a[5]);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    // Each voice burst once, by (absolute fragment start in symbols, slot).
    let mut bursts: BTreeMap<(usize, usize), p25p2::VoiceBurst> = BTreeMap::new();
    let mut algids: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let piece = (10.0 * fs) as usize;
    for (n, w) in x.chunks(piece).enumerate() {
        let offset = n * 60_000; // 10 s of 6000-baud symbols
        for f in detect::frames(w, fs, 12_500.0, 560, 380).iter().filter(|f| f.protocol == "P25" && f.detail == "Phase 2") {
            let Some(info) = p25p2::fragment(f, &[&seq]) else { continue };
            let start = offset + f.at_symbol + info.start - f.sync_start;
            // The same fragment found from its other sync lands within a
            // couple of symbols.
            if seen.range(start.saturating_sub(4)..start + 5).next().is_some() {
                continue;
            }
            seen.insert(start);
            for s in &info.signals {
                if let p25p2::Mac::Ptt { end: false, algid, .. } = s.mac {
                    *algids.entry(format!("ch{} 0x{algid:02X}", s.channel)).or_default() += 1;
                }
            }
            for v in info.voice {
                bursts.insert((start, v.slot), v);
            }
        }
    }
    println!("{} fragments, {} voice bursts; PTT algorithms {algids:?}", seen.len(), bursts.len());
    for ch in 0..2u8 {
        let mut dec = ambe::Decoder::default();
        let mut pcm: Vec<i16> = Vec::new();
        let (mut frames, mut errs, mut outcomes) = (0, [0usize; 8], BTreeMap::<String, usize>::new());
        let mut last_end: Option<usize> = None;
        let mut dump: Vec<u8> = Vec::new();
        for ((start, slot), v) in bursts.iter().filter(|(_, v)| v.channel == ch) {
            // Timeslots are 180 symbols (30 ms); keep the channel's timeline
            // with silence for gaps.
            let t = start + slot * 180;
            if let Some(e) = last_end {
                if t > e + 1500 {
                    let gap = ((t - e) as f64 / 6000.0).min(2.0);
                    pcm.extend(std::iter::repeat_n(0, (gap * 8000.0) as usize));
                    dump.push(0);
                    dump.extend(((gap * 8000.0) as u32).to_le_bytes());
                    dump.push(1);
                    dec = ambe::Decoder::default();
                }
            }
            last_end = Some(t + 180);
            for f in &v.frames {
                let c = ambe::decode(f);
                dump.push(3);
                c.u.iter().for_each(|u| dump.extend(u.to_le_bytes()));
                dump.extend(c.errors);
                frames += 1;
                errs[(c.errors[0] + c.errors[1]).min(7) as usize] += 1;
                let (s, o) = dec.decode(&c);
                *outcomes.entry(format!("{o:?}/{:?}", ambe::kind(&c.u))).or_default() += 1;
                pcm.extend(s.iter().map(|&v| (v * 4.0).clamp(-32767.0, 32767.0) as i16));
            }
        }
        println!("channel {ch}: {frames} frames, errors 0..7+ {errs:?}");
        println!("  {outcomes:?}");
        if pcm.is_empty() {
            continue;
        }
        let mut w = Vec::new();
        let data = (pcm.len() * 2) as u32;
        for part in [&b"RIFF"[..], &(36 + data).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes(), &1u16.to_le_bytes()] {
            w.extend(part);
        }
        for part in [&8000u32.to_le_bytes()[..], &16000u32.to_le_bytes(), &2u16.to_le_bytes(), &16u16.to_le_bytes(), b"data", &data.to_le_bytes()] {
            w.extend(part);
        }
        pcm.iter().for_each(|s| w.extend(s.to_le_bytes()));
        std::fs::write(format!("{prefix}_ch{ch}.wav"), w).unwrap();
        std::fs::write(format!("{prefix}_ch{ch}.bin"), dump).unwrap();
    }
}
