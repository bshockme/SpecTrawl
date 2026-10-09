//! Listens to one channel and writes the audio to a WAV file.
//!
//!     cargo run --release -p engine --example listen_check -- <serial> <tune MHz> <channel MHz> <nfm|am|wfm> <seconds> <out.wav>

use std::io::Write;
use std::time::Duration;

use engine::{DemodMode, ListenSpec, Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, tune, chan, secs, out) =
        (&a[1], a[2].parse::<f64>().unwrap() * 1e6, a[3].parse::<f64>().unwrap() * 1e6, a[5].parse::<f64>().unwrap(), &a[6]);
    let mode = match a[4].as_str() {
        "am" => DemodMode::Am,
        "wfm" => DemodMode::Wfm,
        _ => DemodMode::Nfm,
    };
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(tune).unwrap();
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    src.listen(Some(ListenSpec { center_hz: chan, mode }));
    let q = src.audio();
    let mut pcm: Vec<f32> = Vec::new();
    let mut open_blocks = (0, 0);
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_millis(20));
        let mut b = vec![0.0f32; 960];
        q.pop_into(&mut b);
        pcm.extend(b);
        open_blocks.1 += 1;
        if q.open.load(std::sync::atomic::Ordering::Relaxed) {
            open_blocks.0 += 1;
        }
    }
    let rms = (pcm.iter().map(|v| v * v).sum::<f32>() / pcm.len() as f32).sqrt();
    let peak = pcm.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!(
        "{:.1} s of audio, rms {rms:.3}, peak {peak:.3}, squelch open {}/{} blocks",
        pcm.len() as f64 / 48e3,
        open_blocks.0,
        open_blocks.1
    );
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    let n = pcm.len() as u32;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + n * 2).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&48_000u32.to_le_bytes()).unwrap();
    f.write_all(&96_000u32.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(n * 2).to_le_bytes()).unwrap();
    for v in pcm {
        f.write_all(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).unwrap();
    }
}
