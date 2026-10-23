//! Streams channels to cf32 SigMF files for longer than the look-back ring
//! holds: follows the ring's live edge and down-converts as it goes.
//!
//!     cargo run --release -p engine --example capture_channel -- [--gain L,M,V] <serial> <tune MHz> <channel MHz>[,<channel MHz>...] <bandwidth kHz> <seconds> <out stem>
//!
//! With several channels, each file's stem gets "_<channel MHz>" appended.
//! A channel may carry its own bandwidth as `<MHz>:<kHz>`, otherwise the
//! common one applies. `--gain` sets the LNA, mixer and VGA stages by hand
//! (AGC off).

use std::io::Write;
use std::time::{Duration, Instant};

use detect::Ddc;
use engine::{Mode, Source, SourceConfig};
use sdr_core::{Backend, Complex32, GainSetting};

fn main() {
    let mut a: Vec<String> = std::env::args().collect();
    let gain = a.iter().position(|x| x == "--gain").map(|i| {
        let v: Vec<u8> = a[i + 1].split(',').map(|g| g.parse().expect("gain stage")).collect();
        a.drain(i..i + 2);
        GainSetting::Manual { lna: v[0], mixer: v[1], vga: v[2] }
    });
    let (serial, tune, bw, secs, out) =
        (&a[1], a[2].parse::<f64>().unwrap() * 1e6, a[4].parse::<f64>().unwrap() * 1e3, a[5].parse::<f64>().unwrap(), &a[6]);
    // (centre Hz, bandwidth Hz) per channel.
    let chans: Vec<(f64, f64)> = a[3]
        .split(',')
        .map(|c| match c.split_once(':') {
            Some((m, k)) => (m.parse::<f64>().unwrap() * 1e6, k.parse::<f64>().unwrap() * 1e3),
            None => (c.parse::<f64>().unwrap() * 1e6, bw),
        })
        .collect();
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(tune).unwrap();
    if let Some(g) = gain {
        d.set_gain(g).unwrap();
    }
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    let rate = src.snapshot().sample_rate;
    std::thread::sleep(Duration::from_millis(300));
    let stems: Vec<String> =
        if chans.len() == 1 { vec![out.clone()] } else { chans.iter().map(|(c, _)| format!("{out}_{:.4}", c / 1e6)).collect() };
    let mut outs: Vec<(Ddc, std::io::BufWriter<std::fs::File>, u64)> = chans
        .iter()
        .zip(&stems)
        .map(|((c, bw), s)| {
            let ddc = Ddc::new(rate, c - tune, bw * 0.65 + 1e3, (bw * 2.5).max(48e3));
            (ddc, std::io::BufWriter::new(std::fs::File::create(format!("{s}.sigmf-data")).unwrap()), 0)
        })
        .collect();
    let mut pos = src.ring_head();
    let mut buf = vec![Complex32::default(); 1 << 16];
    let mut y = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        let head = src.ring_head();
        if head <= pos {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let n = ((head - pos) as usize).min(buf.len());
        if !src.read_iq(pos, &mut buf[..n]) {
            eprintln!("fell behind the ring");
            pos = head;
            continue;
        }
        for (ddc, f, n_out) in outs.iter_mut() {
            y.clear();
            ddc.process(&buf[..n], &mut y);
            for c in &y {
                f.write_all(&c.re.to_le_bytes()).unwrap();
                f.write_all(&c.im.to_le_bytes()).unwrap();
            }
            *n_out += y.len() as u64;
        }
        pos += n as u64;
    }
    for ((ddc, f, n_out), (s, (c, _))) in outs.iter_mut().zip(stems.iter().zip(&chans)) {
        f.flush().unwrap();
        let meta = serde_json::json!({
            "global": { "core:version": "1.2.0", "core:datatype": "cf32_le", "core:sample_rate": ddc.fs_out, "core:recorder": "SpecTrawl capture_channel" },
            "captures": [{ "core:sample_start": 0, "core:frequency": c }],
            "annotations": [],
        });
        std::fs::write(format!("{s}.sigmf-meta"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        println!("{s}: {:.1} s at {:.0} Hz", *n_out as f64 / ddc.fs_out, ddc.fs_out);
    }
    let st = src.stats();
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "device samples {:.0}/s over {wall:.1} s wall; gaps {} ({} samples lost)",
        (src.ring_head() as f64) / wall,
        st.gaps,
        st.lost_samples
    );
}
