//! Runs a recording through the live P25 voice decoder (`VoiceStream`) in
//! small chunks, as the engine would, and compares it with decoding the
//! whole recording at once: LDUs found, audio produced, time taken.
//!
//!     cargo run --release -p detect --example voice_stream_check -- <path without extension> [out.wav]

use detect::p25_voice::VoiceStream;
use num_complex::Complex32;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stem = &args[1];
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x: Vec<Complex32> = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    let batch = detect::p25_voice_frames(&x, fs).len();
    let t0 = std::time::Instant::now();
    let mut vs = VoiceStream::new(fs);
    let (mut ldus, mut starts, mut audio) = (0, 0, Vec::new());
    for chunk in x.chunks((fs / 50.0) as usize) {
        let o = vs.push(chunk);
        ldus += o.ldus;
        starts += o.started as usize;
        audio.extend(o.audio);
    }
    let dt = t0.elapsed().as_secs_f64();
    let dur = x.len() as f64 / fs;
    println!(
        "{stem}: {dur:.0} s at {fs} Hz; batch {batch} LDUs, stream {ldus} LDUs in {starts} transmissions, {:.1} s audio",
        audio.len() as f64 / 8000.0
    );
    println!("  {dt:.1} s to process ({:.1}% of real time)", 100.0 * dt / dur);
    if let Some(p) = args.get(2) {
        let pcm: Vec<i16> = audio.iter().map(|&v| (v * 4.0 * 32767.0).clamp(-32767.0, 32767.0) as i16).collect();
        let mut w = Vec::new();
        let data = (pcm.len() * 2) as u32;
        for part in [&b"RIFF"[..], &(36 + data).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes(), &1u16.to_le_bytes()]
        {
            w.extend(part);
        }
        for part in
            [&8000u32.to_le_bytes()[..], &16000u32.to_le_bytes(), &2u16.to_le_bytes(), &16u16.to_le_bytes(), b"data", &data.to_le_bytes()]
        {
            w.extend(part);
        }
        pcm.iter().for_each(|s| w.extend(s.to_le_bytes()));
        std::fs::write(p, w).unwrap();
    }
}
