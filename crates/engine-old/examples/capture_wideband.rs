//! Records the whole span of an Airspy to an i16 SigMF file (ci16_le) by
//! following the ring's live edge. For the rebuild's acceptance captures
//! (docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md §10.4).
//!
//!     cargo run --release -p engine-old --example capture_wideband -- [--gain L,M,V] <serial> <tune MHz> <seconds> <out stem>
//!
//! Writes `<out stem>.sigmf-data` and `<out stem>.sigmf-meta`. Exits 1 if the
//! device can't be opened (it may be in use); a scheduler script can retry.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use engine_old::{Mode, Source, SourceConfig};
use sdr_core::{Backend, Complex32, GainSetting};

fn main() {
    let mut a: Vec<String> = std::env::args().collect();
    let gain = a.iter().position(|x| x == "--gain").map(|i| {
        let v: Vec<u8> = a[i + 1].split(',').map(|g| g.parse().expect("gain stage")).collect();
        a.drain(i..i + 2);
        GainSetting::Manual { lna: v[0], mixer: v[1], vga: v[2] }
    });
    if a.len() < 5 {
        eprintln!("usage: capture_wideband [--gain L,M,V] <serial> <tune MHz> <seconds> <out stem>");
        std::process::exit(2);
    }
    let (serial, tune, secs, out) = (&a[1], a[2].parse::<f64>().unwrap() * 1e6, a[3].parse::<f64>().unwrap(), &a[4]);

    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = match b.open(serial) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open {serial} failed: {e}");
            std::process::exit(1);
        }
    };
    d.set_center(tune).unwrap();
    if let Some(g) = gain {
        d.set_gain(g).unwrap();
    }
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    let rate = src.snapshot().sample_rate;
    // Let the tuner and AGC settle before the first sample is kept.
    std::thread::sleep(Duration::from_millis(500));

    let started = SystemTime::now();
    let mut f = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(format!("{out}.sigmf-data")).unwrap());
    let mut pos = src.ring_head();
    let mut buf = vec![Complex32::default(); 1 << 18];
    let mut bytes = vec![0u8; buf.len() * 4];
    let (mut written, mut overruns) = (0u64, 0u32);
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        let head = src.ring_head();
        if head <= pos {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let n = ((head - pos) as usize).min(buf.len());
        if !src.read_iq(pos, &mut buf[..n]) {
            overruns += 1;
            eprintln!("fell behind the ring at {:.2} s (overrun {overruns})", t0.elapsed().as_secs_f64());
            pos = head;
            continue;
        }
        for (c, o) in buf[..n].iter().zip(bytes.chunks_exact_mut(4)) {
            o[..2].copy_from_slice(&((c.re * 32767.0).round() as i16).to_le_bytes());
            o[2..].copy_from_slice(&((c.im * 32767.0).round() as i16).to_le_bytes());
        }
        f.write_all(&bytes[..n * 4]).unwrap();
        written += n as u64;
        pos += n as u64;
    }
    f.flush().unwrap();
    let stats = src.stats();
    let meta = format!(
        concat!(
            "{{\n  \"global\": {{\n    \"core:datatype\": \"ci16_le\",\n    \"core:sample_rate\": {rate},\n",
            "    \"core:version\": \"1.0.0\",\n    \"core:hw\": \"{hw}\",\n    \"core:recorder\": \"spectrawl capture_wideband\",\n",
            "    \"core:description\": \"wideband capture, {secs} s requested, {written} samples written, {overruns} ring overruns, {gaps} device gaps, {lost} lost samples\"\n  }},\n",
            "  \"captures\": [ {{ \"core:sample_start\": 0, \"core:frequency\": {tune}, \"core:datetime\": \"{dt}\" }} ],\n  \"annotations\": []\n}}\n"
        ),
        rate = rate,
        hw = format!("{} {}", src.snapshot().info.label, serial),
        secs = secs,
        written = written,
        overruns = overruns,
        gaps = stats.gaps,
        lost = stats.lost_samples,
        tune = tune,
        dt = engine_old::sigmf::rfc3339(started),
    );
    std::fs::write(format!("{out}.sigmf-meta"), meta).unwrap();
    println!(
        "{out}: {written} samples ({:.1} s at {:.3} MS/s), {overruns} ring overruns, {} device gaps, {} lost samples",
        written as f64 / rate,
        rate / 1e6,
        stats.gaps,
        stats.lost_samples
    );
}
