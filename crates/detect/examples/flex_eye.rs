//! FLEX block-by-block eye diagnostic: for every frame, per block, the
//! words that passed BCH, the carrier offset, the envelope (amplitude)
//! spread and a histogram of the discriminator across the block, so a
//! closed eye (smeared histogram), a fade or beat (envelope dips) and a
//! slicer fault (clean histogram, few words) can be told apart.
//!
//! Reads a cf32 channel capture, or a ci16 wideband capture with
//! `--offset-khz` (the channel relative to the capture centre), run through
//! the same down-converter and rates the live pager monitor uses.
//!
//!     cargo run --release -p detect --example flex_eye -- <stem> [--offset-khz K] [--cutoff-khz C] [--sweep] [--sync2] [--show]
//!
//! `--sweep` brute-forces slicer settings on lossy blocks; `--sync2` prints
//! the sync 2 symbols on the decoder's block-0 grid.

use detect::{Ddc, features, flex, pocsag};
use num_complex::Complex32;

fn load(stem: &str, offset_hz: f64, cutoff_hz: f64) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let dt = meta["global"]["core:datatype"].as_str().unwrap_or("cf32_le").to_string();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    match dt.as_str() {
        "cf32_le" => (
            raw.chunks_exact(8)
                .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
                .collect(),
            fs,
        ),
        "ci16_le" => {
            // The monitor's channel: 10 kHz cutoff, ≥48 kHz out.
            let mut ddc = Ddc::new(fs, offset_hz, cutoff_hz, 48e3);
            let mut out = Vec::new();
            let mut chunk = Vec::with_capacity(1 << 20);
            for block in raw.chunks(4 << 20) {
                chunk.clear();
                chunk.extend(block.chunks_exact(4).map(|c| {
                    Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0, i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0)
                }));
                ddc.process(&chunk, &mut out);
            }
            (out, ddc.fs_out)
        }
        other => panic!("datatype {other}"),
    }
}

/// Moving average over a quarter of a 3200-baud symbol, as the decoder's
/// data slicer sees it.
fn smooth(d: &[f32], fs: f64) -> Vec<f32> {
    let n = ((fs / 3200.0 / 4.0).round() as usize).max(1);
    let mut out = vec![0.0; d.len()];
    let mut acc = 0.0f32;
    for i in 0..d.len() + n / 2 {
        if i < d.len() {
            acc += d[i];
        }
        if i >= n {
            acc -= d[i - n];
        }
        if i >= n / 2 && i - n / 2 < d.len() {
            out[i - n / 2] = acc / n as f32;
        }
    }
    out
}

fn pct(v: &mut [f32], p: f64) -> f32 {
    v.sort_by(f32::total_cmp);
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn at(d: &[f32], t: f64) -> f32 {
    if t <= 0.0 {
        return d[0];
    }
    let i = t as usize;
    if i + 1 >= d.len() {
        return d[d.len() - 1];
    }
    let f = (t - i as f64) as f32;
    d[i] * (1.0 - f) + d[i + 1] * f
}

fn smooth_n(d: &[f32], n: usize) -> Vec<f32> {
    let n = n.max(1);
    let mut out = vec![0.0; d.len()];
    let mut acc = 0.0f32;
    for i in 0..d.len() + n / 2 {
        if i < d.len() {
            acc += d[i];
        }
        if i >= n {
            acc -= d[i - n];
        }
        if i >= n / 2 && i - n / 2 < d.len() {
            out[i - n / 2] = acc / n as f32;
        }
    }
    out
}

/// Words passing BCH when a 3200/4 block is sliced with fixed parameters.
fn slice_ok(sm: &[f32], t0: f64, s: f64, dc: f32, thr: f32, inv: bool) -> usize {
    let n = 512;
    let v: Vec<f32> = (0..n)
        .map(|k| {
            let x = at(sm, t0 + (k as f64 + 0.5) * s);
            if inv { dc - x } else { x - dc }
        })
        .collect();
    let bits: Vec<(u8, u8)> = v.iter().map(|&x| ((x > 0.0) as u8, (x.abs() < thr) as u8)).collect();
    let phases: [Vec<u8>; 4] = [
        bits.iter().step_by(2).map(|b| b.0).collect(),
        bits.iter().step_by(2).map(|b| b.1).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.0).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.1).collect(),
    ];
    let mut ok = 0;
    for ph in &phases {
        for w in 0..8 {
            let cw = (0..32).fold(0u32, |a, b| a << 1 | ph[b * 8 + w] as u32);
            if pocsag::correct(cw).is_some() {
                ok += 1;
            }
        }
    }
    ok
}

/// Best (ok, timing offset in symbols, threshold ratio, dc shift, smoothing) over a sweep.
fn sweep(disc: &[f32], fs: f64, t0: f64, s: f64, centre: f32, outer: f32) -> (usize, f64, f32, f32, &'static str) {
    let sym = (fs / 3200.0).round() as usize;
    let mut best = (0usize, 0.0f64, 0.0f32, 0.0f32, "");
    for (name, n) in [("raw", 1usize), ("half", sym / 2), ("full", sym)] {
        let sm = smooth_n(disc, n);
        for inv in [false, true] {
            for oi in -10..=10 {
                let off = oi as f64 * 0.05;
                for ri in 0..=6 {
                    let r = 0.5 + ri as f32 * 0.05;
                    for di in -2..=2 {
                        let dc = centre + di as f32 * 150.0;
                        let ok = slice_ok(&sm, t0 + off * s, s, dc, r * outer, inv);
                        if ok > best.0 {
                            best = (ok, off, r, dc - centre, name);
                        }
                    }
                }
            }
        }
    }
    best
}

/// Per-phase words passing BCH, and symbol-centre clusters: counts and RMS
/// spread per ideal level, and how many centres sit within 600 Hz of a
/// decision boundary (0, ±2/3 outer).
fn eye_stats(sm: &[f32], t0: f64, s: f64, dc: f32, outer: f32) -> String {
    let n = 512;
    let v: Vec<f32> = (0..n).map(|k| at(sm, t0 + (k as f64 + 0.5) * s) - dc).collect();
    let thr = outer * 2.0 / 3.0;
    let bits: Vec<(u8, u8)> = v.iter().map(|&x| ((x > 0.0) as u8, (x.abs() < thr) as u8)).collect();
    let phases: [Vec<u8>; 4] = [
        bits.iter().step_by(2).map(|b| b.0).collect(),
        bits.iter().step_by(2).map(|b| b.1).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.0).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.1).collect(),
    ];
    let ok: Vec<usize> = phases
        .iter()
        .map(|ph| (0..8).filter(|&w| pocsag::correct((0..32).fold(0u32, |a, b| a << 1 | ph[b * 8 + w] as u32)).is_some()).count())
        .collect();
    let levels = [-outer, -outer / 3.0, outer / 3.0, outer];
    let mut cnt = [0usize; 4];
    let mut sum = [0.0f32; 4];
    let mut sq = [0.0f32; 4];
    let mut amb = 0;
    for &x in &v {
        let k = (0..4).min_by(|&a, &b| (x - levels[a]).abs().total_cmp(&(x - levels[b]).abs())).unwrap();
        cnt[k] += 1;
        sum[k] += x;
        sq[k] += x * x;
        if x.abs() < 600.0 || (x.abs() - thr).abs() < 600.0 {
            amb += 1;
        }
    }
    let mean: Vec<String> = (0..4)
        .map(|k| {
            if cnt[k] > 0 {
                format!(
                    "{:+5.0}±{:3.0}",
                    sum[k] / cnt[k] as f32,
                    (sq[k] / cnt[k] as f32 - (sum[k] / cnt[k] as f32).powi(2)).max(0.0).sqrt()
                )
            } else {
                "   -   ".into()
            }
        })
        .collect();
    format!("phases ok {ok:?}  levels n {cnt:?} mean±sd [{}]  ambiguous {amb}", mean.join(", "))
}

/// Block 0 sliced at `t0`: per phase, whether word 0 is a valid block
/// information word ('B'), idle ('i'), a valid codeword but neither ('w'),
/// or failed BCH ('x').
fn biw_check(sm: &[f32], t0: f64, s: f64, dc: f32, outer: f32) -> String {
    let n = 512;
    let v: Vec<f32> = (0..n).map(|k| at(sm, t0 + (k as f64 + 0.5) * s) - dc).collect();
    let thr = outer * 2.0 / 3.0;
    let bits: Vec<(u8, u8)> = v.iter().map(|&x| ((x > 0.0) as u8, (x.abs() < thr) as u8)).collect();
    let phases: [Vec<u8>; 4] = [
        bits.iter().step_by(2).map(|b| b.0).collect(),
        bits.iter().step_by(2).map(|b| b.1).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.0).collect(),
        bits.iter().skip(1).step_by(2).map(|b| b.1).collect(),
    ];
    phases
        .iter()
        .map(|ph| {
            let cw = (0..32).fold(0u32, |a, b| a << 1 | ph[b * 8] as u32);
            match pocsag::correct(cw) {
                None => 'x',
                Some((c, _)) => {
                    let x = (c >> 11).reverse_bits() >> 11;
                    if x == 0x1F_FFFF || x == 0 {
                        'i'
                    } else if (0..6).map(|k| x >> (4 * k) & 0xF).sum::<u32>() & 0xF == 0xF && (x >> 10 & 0x3F) > (x >> 8 & 3) {
                        'B'
                    } else {
                        'w'
                    }
                }
            }
        })
        .collect()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let show = a.iter().any(|x| x == "--show");
    let do_sweep = a.iter().any(|x| x == "--sweep");
    let (mut clean_full, mut clean_lossy, mut clean_lost_words, mut dirty) = (0usize, 0usize, 0usize, 0usize);
    let arg = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    let offset_hz = arg("--offset-khz").unwrap_or(0.0) * 1e3;
    let cutoff_hz = arg("--cutoff-khz").unwrap_or(10.0) * 1e3;
    let stem = a.iter().filter(|x| !x.starts_with("--")).filter(|x| x.parse::<f64>().is_err()).next_back().expect("stem");
    let (x, fs) = load(stem, offset_hz, cutoff_hz);
    println!("{stem}: {:.1} s at {fs:.1} Hz (offset {:+.1} kHz)", x.len() as f64 / fs, offset_hz / 1e3);
    let disc = features::discriminator(&x, fs);
    let sm = smooth(&disc, fs);
    let env: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let sps = fs / 1600.0;
    let frames = flex::decode(&x, fs);
    println!("{} frames; histogram bins are 800 Hz wide from -6.4 to +6.4 kHz (ideal 4FSK: peaks at -4.8, -1.6, +1.6, +4.8)", frames.len());
    for f in &frames {
        let s = fs / f.baud as f64;
        let n = (f.baud as f64 * 0.16).round() as usize;
        let t0 = f.at as f64 + 48.5 * sps + 0.025 * fs;
        println!(
            "t {:7.2} {} cycle {:2} frame {:3}  ok {:?} = {}",
            f.at as f64 / fs,
            f.mode(),
            f.cycle,
            f.frame,
            f.block_ok,
            f.block_ok.iter().map(|&b| b as u32).sum::<u32>()
        );
        // Header: the 1600-baud section before the end of sync 1.
        let h0 = (f.at as f64 - 112.0 * sps).max(0.0) as usize;
        let mut hv: Vec<f32> = sm[h0..f.at].to_vec();
        let mut he: Vec<f32> = env[h0..f.at].to_vec();
        let hmed = pct(&mut he, 0.5);
        let hdev = (pct(&mut hv, 0.98) - pct(&mut hv, 0.02)) / 2.0;
        println!(
            "   header  offset {:+6.0} Hz  dev {:5.0} Hz  env p1 {:+5.1} dB p99 {:+5.1} dB",
            (pct(&mut hv, 0.02) + pct(&mut hv, 0.98)) / 2.0,
            (pct(&mut hv, 0.98) - pct(&mut hv, 0.02)) / 2.0,
            20.0 * (pct(&mut he, 0.01) / hmed).log10(),
            20.0 * (pct(&mut he, 0.99) / hmed).log10()
        );
        // The frame's symbol timing: best offset for block 0 with the decoder's slicer settings.
        let frame_off = if do_sweep && f.levels == 4 && f.baud == 3200 {
            let hv0 = {
                let mut w: Vec<f32> = sm[t0 as usize..(t0 + n as f64 * s) as usize].to_vec();
                (pct(&mut w, 0.02) + pct(&mut w, 0.98)) / 2.0
            };
            (-60..=60).map(|k| k as f64 * 0.05).max_by_key(|&o| slice_ok(&sm, t0 + o * s, s, hv0, hdev * 2.0 / 3.0, false)).unwrap_or(0.0)
        } else {
            0.0
        };
        println!(
            "   decoder timing per block (symbols from nominal): {:?}",
            f.block_off.iter().map(|o| format!("{o:+.1}")).collect::<Vec<_>>()
        );
        if a.iter().any(|x| x == "--sync2") {
            // Sync 2 and the first data symbols on the nominal 3200-baud
            // grid from the header, quarter-symbol smoothing, 4-level
            // quantised: '+' / 'p' / 'n' / '-' for +outer / +inner / -inner / -outer.
            let q = smooth_n(&disc, ((fs / 3200.0 / 4.0).round() as usize).max(1));
            let s32 = fs / 3200.0;
            let start = f.at as f64 + 48.5 * sps;
            let hoff = {
                let mut w: Vec<f32> = sm[h0..f.at].to_vec();
                (pct(&mut w, 0.02) + pct(&mut w, 0.98)) / 2.0
            };
            for (name, step) in [("sym ", 1.0), ("half", 0.5)] {
                let row: String = (0..((80.0 + 24.0) / step) as usize)
                    .map(|k| {
                        // On the decoder's grid for block 0 (symbol centres).
                        let y = at(&q, start + (k as f64 * step + 0.5 + f.block_off[0] as f64) * s32) - hoff;
                        match (y > 0.0, y.abs() > hdev * 2.0 / 3.0) {
                            (true, true) => '+',
                            (true, false) => 'p',
                            (false, false) => 'n',
                            (false, true) => '-',
                        }
                    })
                    .collect();
                println!("   sync2 {name} from FIW end: {row}");
            }
        }
        if f.levels == 4 && f.baud == 3200 {
            let hv0 = {
                let mut w: Vec<f32> = sm[t0 as usize..(t0 + n as f64 * s) as usize].to_vec();
                (pct(&mut w, 0.02) + pct(&mut w, 0.98)) / 2.0
            };
            let row: Vec<String> = (-3..=3)
                .map(|k| {
                    let o = f.block_off[0] as f64 + k as f64;
                    format!("{o:+.1}:{}", biw_check(&sm, t0 + o * s, s, hv0, hdev))
                })
                .collect();
            println!(
                "   block-0 word 0 per phase ABCD at whole-symbol shifts around the decoder's choice (B=BIW i=idle w=other x=fail): {}",
                row.join("  ")
            );
        }
        if do_sweep {
            println!("   timing from block 0: {frame_off:+.2} symbols from nominal");
            // Profile: ok words vs timing for block 0 at the decoder's settings.
            let hv0 = {
                let mut w: Vec<f32> = sm[t0 as usize..(t0 + n as f64 * s) as usize].to_vec();
                (pct(&mut w, 0.02) + pct(&mut w, 0.98)) / 2.0
            };
            let prof: Vec<String> = (-80..=80)
                .map(|k| k as f64 * 0.1)
                .map(|o| (o, slice_ok(&sm, t0 + o * s, s, hv0, hdev * 2.0 / 3.0, false)))
                .filter(|(_, ok)| *ok >= 24)
                .map(|(o, ok)| format!("{o:+.1}:{ok}"))
                .collect();
            println!("   block-0 ok ≥ 24 at offsets: {}", prof.join(" "));
        }
        let raw_sm = smooth_n(&disc, 1);
        for blk in 0..11 {
            let b0 = (t0 + (blk * n) as f64 * s) as usize;
            let b1 = ((b0 as f64 + n as f64 * s) as usize).min(sm.len());
            if b0 >= b1 {
                break;
            }
            let mut v: Vec<f32> = sm[b0..b1].to_vec();
            let mut e: Vec<f32> = env[b0..b1].to_vec();
            let med = pct(&mut e, 0.5);
            let (e01, e10) = (pct(&mut e, 0.01), pct(&mut e, 0.10));
            let centre = (pct(&mut v, 0.02) + pct(&mut v, 0.98)) / 2.0;
            let mut hist = [0u32; 16];
            for &y in &v {
                let k = ((y - centre + 6400.0) / 800.0).floor();
                if (0.0..16.0).contains(&k) {
                    hist[k as usize] += 1;
                }
            }
            let peak = *hist.iter().max().unwrap() as f32;
            let row: String = hist
                .iter()
                .map(|&h| {
                    let r = h as f32 / peak;
                    if h == 0 {
                        '.'
                    } else if r < 0.1 {
                        '1'
                    } else if r < 0.3 {
                        '3'
                    } else if r < 0.6 {
                        '6'
                    } else {
                        '#'
                    }
                })
                .collect();
            println!(
                "   blk {blk:2} ok {:2}  offset {:+6.0} Hz  env p1 {:+5.1} dB p10 {:+5.1} dB  |{}|",
                f.block_ok[blk],
                centre,
                20.0 * (e01 / med).log10(),
                20.0 * (e10 / med).log10(),
                row
            );
            let e10db = 20.0 * (e10 / med).log10();
            if e10db < -4.0 {
                dirty += 1;
            } else if f.block_ok[blk] == 32 {
                clean_full += 1;
                if do_sweep && blk == 0 && f.levels == 4 && f.baud == 3200 {
                    let bt = t0 + frame_off * s;
                    println!("          (reference, full block) full-symbol: {}", eye_stats(&sm, bt, s, centre, hdev));
                    println!("          (reference, full block) raw:         {}", eye_stats(&raw_sm, bt, s, centre, hdev));
                }
            } else {
                clean_lossy += 1;
                clean_lost_words += 32 - f.block_ok[blk] as usize;
                if do_sweep && f.levels == 4 && f.baud == 3200 {
                    let bt = t0 + ((blk * n) as f64 + frame_off) * s;
                    let (ok, off, r, dc, sm_name) = sweep(&disc, fs, bt, s, centre, hdev);
                    println!(
                        "          sweep best ok {ok:2} at timing {off:+.2} sym, thr {r:.2}·outer, dc {dc:+.0} Hz, smoothing {sm_name}"
                    );
                    println!("          full-symbol filter: {}", eye_stats(&sm, bt, s, centre, hdev));
                    println!("          raw discriminator:  {}", eye_stats(&raw_sm, bt, s, centre, hdev));
                }
            }
        }
        if show {
            for p in &f.pages {
                println!("      {}", p.summary());
            }
        } else {
            for p in &f.pages {
                println!(
                    "      {:09} {} {:7} {}{} chars",
                    p.capcode,
                    p.phase,
                    p.kind,
                    if p.damaged { "[damaged] " } else { "" },
                    p.text.chars().count()
                );
            }
        }
    }
    println!("summary: blocks clean&full {clean_full}, clean&lossy {clean_lossy} ({clean_lost_words} words lost), interfered {dirty}");
}
