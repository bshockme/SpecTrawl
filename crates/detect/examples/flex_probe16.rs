//! FLEX 1600/4 block probe: slices each block of each frame the way the
//! decoder does (decision-directed levels, quarter-symbol filter) and says
//! which phase's words fail, then sweeps filter length, timing and
//! threshold per lossy block to see what would have decoded it.
//!
//!     cargo run --release -p detect --example flex_probe16 -- <stem> [--frame T] [--sweep]

use detect::{features, flex, pocsag};
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

/// Discriminator centred on its median, boxcar of `n` samples.
fn smooth(d: &[f32], n: usize) -> Vec<f32> {
    let mut s = d.to_vec();
    s.sort_by(f32::total_cmp);
    let mid = s[s.len() / 2];
    let n = n.max(1);
    let mut out = vec![0.0; d.len()];
    let mut acc = 0.0f32;
    for i in 0..d.len() + n / 2 {
        if i < d.len() {
            acc += d[i] - mid;
        }
        if i >= n {
            acc -= d[i - n] - mid;
        }
        if i >= n / 2 && i - n / 2 < d.len() {
            out[i - n / 2] = acc / n as f32;
        }
    }
    out
}

struct Sliced {
    /// Per phase (A, B): per word (ok, bits fixed, data21 or raw cw).
    phases: Vec<Vec<(bool, u32, u32)>>,
    inner: usize,
    outer_est: f32,
    dc: f32,
}

fn slice(d: &[f32], t0: f64, s: f64, header_level: f32, thr_ratio: f32) -> Sliced {
    let n = 256;
    let v: Vec<f32> = (0..n).map(|k| at(d, t0 + (k as f64 + 0.5) * s)).collect();
    let mut outer = header_level;
    let mut dc = 0.0;
    for _ in 0..4 {
        let (mut resid, mut out_sum, mut out_n) = (0.0, 0.0, 0);
        for &x in &v {
            let u = x - dc;
            let level = if u.abs() < outer * 2.0 / 3.0 { outer / 3.0 } else { outer };
            resid += u - level.copysign(u);
            if level == outer {
                out_sum += u.abs();
                out_n += 1;
            }
        }
        dc += resid / n as f32;
        if out_n * 5 >= n {
            outer = out_sum / out_n as f32;
        }
    }
    let thr = outer * thr_ratio;
    let bits: Vec<(u8, u8)> = v.iter().map(|&x| (((x - dc) > 0.0) as u8, ((x - dc).abs() < thr) as u8)).collect();
    let inner = bits.iter().filter(|b| b.1 == 1).count();
    let ph: Vec<Vec<u8>> = vec![bits.iter().map(|b| b.0).collect(), bits.iter().map(|b| b.1).collect()];
    let phases = ph
        .iter()
        .map(|bits| {
            (0..8)
                .map(|w| {
                    let cw = (0..32).fold(0u32, |a, b| a << 1 | bits[b * 8 + w] as u32);
                    match pocsag::correct(cw) {
                        Some((c, k)) => (true, k, (c >> 11).reverse_bits() >> 11),
                        None => (false, 0, cw),
                    }
                })
                .collect()
        })
        .collect();
    Sliced { phases, inner, outer_est: outer, dc }
}

fn ok_count(s: &Sliced) -> usize {
    s.phases.iter().flatten().filter(|w| w.0).count()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    let want_t = arg("--frame");
    let do_sweep = a.iter().any(|x| x == "--sweep");
    let stem = a.iter().filter(|x| !x.starts_with("--")).filter(|x| x.parse::<f64>().is_err()).next_back().expect("stem");
    let (x, fs) = load(stem);
    let raw = features::discriminator(&x, fs);
    let frames = flex::decode(&x, fs);
    let sps = fs / 1600.0;
    for f in frames.iter().filter(|f| f.baud == 1600 && f.levels == 4) {
        let t = f.at as f64 / fs;
        if want_t.is_some_and(|w| (w - t).abs() > 0.5) {
            continue;
        }
        let s = fs / 1600.0;
        let end = f.at as f64;
        let t0 = end + 48.5 * sps + 0.025 * fs;
        // The decoder's data filter: a quarter of a 3200-baud symbol.
        let dq = smooth(&raw, (fs / 3200.0 * 0.25).round() as usize);
        let dh = smooth(&raw, (fs / 1600.0).round() as usize);
        let level = (0..112).map(|k| at(&dh, end - (63 - k) as f64 * sps).abs()).sum::<f32>() / 112.0;
        println!("t {t:7.2} cycle {} frame {} ok {:?} header level {level:.0} Hz", f.cycle, f.frame, f.block_ok);
        for blk in 0..11 {
            let off = f.block_off[blk] as f64;
            let bt0 = t0 + ((blk * 256) as f64 + off) * s;
            let sl = slice(&dq, bt0, s, level, 2.0 / 3.0);
            let fmt = |p: &Vec<(bool, u32, u32)>| {
                p.iter().map(|w| if w.0 { format!("{}", w.1) } else { "X".into() }).collect::<Vec<_>>().join("")
            };
            print!(
                "  blk {blk:2} ok {:2} off {off:+.1} inner {:3}/256 outer {:.0} dc {:+.0}  A {}  B {}",
                ok_count(&sl),
                sl.inner,
                sl.outer_est,
                sl.dc,
                fmt(&sl.phases[0]),
                fmt(&sl.phases[1])
            );
            // Data of phase B's words (idle fill looks the same everywhere).
            let b: Vec<String> = sl.phases[1].iter().map(|w| if w.0 { format!("{:05X}", w.2) } else { format!("x{:08X}", w.2) }).collect();
            println!("  B={}", b.join(","));
            if a.iter().any(|x| x == "--anatomy") && ok_count(&sl) < 16 && blk == 5 {
                print!("{}", anatomy(&dq, bt0, s, level));
            }
            if a.iter().any(|x| x == "--rate") {
                let (n, hist) = transitions(&dq, fs, bt0, bt0 + 256.0 * s, sl.dc);
                // Hypotheses: 1600/2 with a whole-symbol slip; 3200/2 (A, C) with timing search.
                let s32 = fs / 3200.0;
                let mut best16 = (0usize, 0i32);
                for slip in -8..=8 {
                    let ok: usize = ok_2level(&dq, bt0 + slip as f64 * s, s, 256, 1, sl.dc).iter().sum();
                    if ok > best16.0 {
                        best16 = (ok, slip);
                    }
                }
                let mut best32 = (0usize, 0.0f64, vec![]);
                for oi in -40..=40 {
                    let o = oi as f64 * 0.05;
                    let v = ok_2level(&dq, bt0 + o * s32, s32, 512, 2, sl.dc);
                    let ok: usize = v.iter().sum();
                    if ok > best32.0 {
                        best32 = (ok, o, v);
                    }
                }
                println!(
                    "      transitions {n} (half-3200-symbol units 0..11+: {hist:?}); as 1600/2 best {}/8 at slip {}; as 3200/2 best {}/16 {:?} at {:+.2} 3200-sym",
                    best16.0, best16.1, best32.0, best32.2, best32.1
                );
            }
            if do_sweep && ok_count(&sl) < 16 {
                let mut best = (0usize, 0.0f64, 0.0f64, 0.0f32);
                for frac in [0.125, 0.25, 0.5, 0.75, 1.0] {
                    let d = smooth(&raw, (fs / 1600.0 * frac).round() as usize);
                    for oi in -12..=12 {
                        let o = oi as f64 * 0.05;
                        for ri in 0..=8 {
                            let r = 0.45 + ri as f32 * 0.05;
                            let ok = ok_count(&slice(&d, t0 + ((blk * 256) as f64 + o) * s, s, level, r));
                            if ok > best.0 {
                                best = (ok, frac, o, r);
                            }
                        }
                    }
                }
                println!("      sweep best ok {} with filter {:.3} sym, timing {:+.2} sym, thr {:.2}", best.0, best.1, best.2, best.3);
            }
        }
    }
}

/// Error anatomy of one block: where phase A's wrong bits sit (symbol index,
/// value read, neighbours), from the corrected words.
#[allow(dead_code)]
pub fn anatomy(d: &[f32], t0: f64, s: f64, header_level: f32) -> String {
    let n = 256;
    let v: Vec<f32> = (0..n).map(|k| at(d, t0 + (k as f64 + 0.5) * s)).collect();
    let sl = slice(d, t0, s, header_level, 2.0 / 3.0);
    let mut out = String::new();
    for (w, word) in sl.phases[0].iter().enumerate() {
        if !word.0 {
            continue;
        }
        // Rebuild received vs corrected codewords.
        let rx = (0..32).fold(0u32, |a, b| a << 1 | ((v[b * 8 + w] - sl.dc) > 0.0) as u32);
        let fixed = pocsag::correct(rx).unwrap().0;
        let errs = rx ^ fixed;
        for b in 0..32 {
            if errs >> (31 - b) & 1 == 1 {
                let k = b * 8 + w;
                let nb = |i: i64| if i < 0 || i >= n as i64 { f32::NAN } else { v[i as usize] - sl.dc };
                out.push_str(&format!(
                    "    word {w} bit {b:2} -> symbol {k:3}: read {:+5.0} (prev {:+5.0}, next {:+5.0}); fine samples around it: {}\n",
                    v[k] - sl.dc,
                    nb(k as i64 - 1),
                    nb(k as i64 + 1),
                    (-4..=4)
                        .map(|q| format!("{:+5.0}", at(d, t0 + (k as f64 + 0.5 + q as f64 / 8.0) * s) - sl.dc))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
        }
    }
    out
}

/// Words passing BCH when `nsym` symbols from `t0` (step `s`) are sliced
/// 2-level and split into `nph` interleaved phases of 256 bits.
#[allow(dead_code)]
pub fn ok_2level(d: &[f32], t0: f64, s: f64, nsym: usize, nph: usize, dc: f32) -> Vec<usize> {
    let bits: Vec<u8> = (0..nsym).map(|k| ((at(d, t0 + (k as f64 + 0.5) * s) - dc) > 0.0) as u8).collect();
    (0..nph)
        .map(|p| {
            let ph: Vec<u8> = bits.iter().skip(p).step_by(nph).copied().collect();
            (0..8)
                .filter(|&w| {
                    let cw = (0..32).fold(0u32, |a, b| a << 1 | ph[b * 8 + w] as u32);
                    pocsag::correct(cw).is_some_and(|c| c.1 <= 1)
                })
                .count()
        })
        .collect()
}

/// Intervals between sign changes in a stretch, in 3200-baud symbols.
#[allow(dead_code)]
pub fn transitions(d: &[f32], fs: f64, from: f64, to: f64, dc: f32) -> (usize, Vec<usize>) {
    let mut hist = vec![0usize; 12];
    let (mut last, mut n, mut sign) = (from, 0usize, (at(d, from) - dc) > 0.0);
    let mut t = from;
    while t < to {
        let sg = (at(d, t) - dc) > 0.0;
        if sg != sign {
            sign = sg;
            n += 1;
            let units = ((t - last) / (fs / 3200.0) * 2.0).round() as usize;
            hist[units.min(11)] += 1;
            last = t;
        }
        t += 1.0;
    }
    (n, hist)
}
