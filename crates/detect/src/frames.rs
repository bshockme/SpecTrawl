//! Sync-aided symbol slicing: once a sync word is found, sample the symbols
//! around it at the sync's phase and slice them with levels taken from the
//! sync itself (levels set per sync, simplified).

use crate::sync::{PHASES, SyncHit};

/// One sync occurrence with the symbols around it.
#[derive(Debug, Clone)]
pub struct Frame {
    pub protocol: &'static str,
    pub detail: &'static str,
    pub pattern: u32,
    pub inverted: bool,
    /// Dibits (0..=3: 01=+3, 00=+1, 10=−1, 11=−3) starting `before` symbols
    /// before the first sync symbol.
    pub dibits: Vec<u8>,
    /// The same symbols as soft values: normalised so the outer levels are
    /// ±1 and the inner ±1/3 (polarity already corrected).
    pub soft: Vec<f32>,
    /// Index into `dibits` of the first sync symbol.
    pub sync_start: usize,
    /// Where the sync starts in the input, in symbols at the frame's rate
    /// (orders frames in time).
    pub at_symbol: usize,
    /// Outer (±3) level measured on the sync, in discriminator units (Hz).
    pub outer: f32,
}

/// How symbols are taken from the discriminator. The options are the
/// usual symbol-engine choices, with our own filter designs.
#[derive(Debug, Clone, Copy)]
pub struct SliceOpts {
    /// Boxcar smoothing before sampling, as a fraction of a symbol (a one-symbol
    /// boxcar is the usual matched filter).
    pub smooth: f64,
    /// Search each frame's sampling instant (±half a symbol) for the one
    /// whose symbols sit closest to the four levels, rather than keeping
    /// the sync hit's coarse phase.
    pub refine: bool,
    /// Low-pass the discriminator at this fraction of the symbol rate first
    /// (a brick wall at 0.625 × baud is a common choice).
    pub post_lpf: Option<f64>,
    /// Levels from the sync with the extreme symbols of each sign dropped.
    pub robust_levels: bool,
    /// Track the centre through the frame from the inner (±1) symbols.
    pub dc_track: bool,
    /// Boost isolated symbols (sign differs from both neighbours), which
    /// inter-symbol interference pulls toward the centre.
    pub isi_boost: bool,
}

impl Default for SliceOpts {
    /// Tuned on the real captures (`examples/slice_tune`): on the weak DMR
    /// control channel, slot-type errors 1.6 → 0.14 per frame together with
    /// a 4 kHz pre-filter; P25 NID errors 0.58 → 0.43.
    fn default() -> Self {
        Self { smooth: 0.75, refine: true, post_lpf: Some(0.625), robust_levels: true, dc_track: true, isi_boost: true }
    }
}

impl SliceOpts {
    /// The original slicer: the sync hit's phase, half-symbol smoothing,
    /// mean levels from the sync. For comparisons.
    pub fn plain() -> Self {
        Self { smooth: 0.5, refine: false, post_lpf: None, robust_levels: false, dc_track: false, isi_boost: false }
    }
}

/// Windowed-sinc (Blackman) low-pass with unity DC gain; `cutoff` in
/// cycles per sample.
fn lowpass_taps(cutoff: f64, len: usize) -> Vec<f32> {
    let m = (len - 1) as f64;
    let pi = std::f64::consts::PI;
    let mut h: Vec<f64> = (0..len)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (2.0 * pi * cutoff * x).sin() / (pi * x) };
            sinc * (0.42 - 0.5 * (2.0 * pi * i as f64 / m).cos() + 0.08 * (4.0 * pi * i as f64 / m).cos())
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h.into_iter().map(|v| v as f32).collect()
}

/// Zero-phase FIR: output aligned with the input.
fn fir(x: &[f32], h: &[f32]) -> Vec<f32> {
    let half = h.len() / 2;
    (0..x.len())
        .map(|i| {
            let lo = (i + half + 1).saturating_sub(h.len());
            let hi = (i + half).min(x.len() - 1);
            (lo..=hi).map(|j| h[i + half - j] * x[j]).sum()
        })
        .collect()
}

/// Groups hits that are the same sync seen at neighbouring phases and
/// returns one frame per occurrence, sliced `before`/`after` symbols around
/// the sync. `disc` is the DC-removed discriminator at `fs`.
pub fn slice(disc: &[f32], fs: f64, hits: &[SyncHit], before: usize, after: usize) -> Vec<Frame> {
    slice_with(disc, fs, hits, before, after, SliceOpts::default())
}

pub fn slice_with(disc: &[f32], fs: f64, hits: &[SyncHit], before: usize, after: usize, opts: SliceOpts) -> Vec<Frame> {
    // Group by (entry, symbol ±2, polarity); use the middle phase of each group.
    let mut groups: Vec<Vec<&SyncHit>> = Vec::new();
    for h in hits {
        match groups.iter_mut().find(|g| {
            let f = g[0];
            f.entry.pattern == h.entry.pattern
                && f.entry.baud == h.entry.baud
                && f.inverted == h.inverted
                && f.symbol.abs_diff(h.symbol) <= 2
        }) {
            Some(g) => g.push(h),
            None => groups.push(vec![h]),
        }
    }
    // One smoothed copy per symbol rate.
    let mut smoothed: Vec<(f64, Vec<f32>)> = Vec::new();
    let mut frames = Vec::new();
    for mut g in groups {
        g.sort_by_key(|h| (h.symbol, h.phase));
        let h = g[g.len() / 2];
        let e = &h.entry;
        let sym_len = fs / e.baud;
        if !smoothed.iter().any(|s| s.0 == e.baud) {
            let src = match opts.post_lpf {
                // ~8 symbols of taps: steep enough, cheap enough.
                Some(f) => fir(disc, &lowpass_taps(f * e.baud / fs, ((8.0 * sym_len) as usize) | 1)),
                None => disc.to_vec(),
            };
            smoothed.push((e.baud, boxcar(&src, (sym_len * opts.smooth).round().max(1.0) as usize)));
        }
        let sm = &smoothed.iter().find(|s| s.0 == e.baud).unwrap().1;
        let first_sync = h.symbol + 1 - e.len as usize;
        let Some(start) = first_sync.checked_sub(before) else { continue };
        let n = before + e.len as usize + after;
        if (start + n + 1) as f64 * sym_len >= sm.len() as f64 {
            continue;
        }
        let sample = |offset: f64| -> Vec<f32> {
            (start..start + n).map(|k| interp(sm, (k as f64 + (h.phase as f64 + 0.5) / PHASES as f64 + offset) * sym_len)).collect()
        };
        // Expected sign of each sync symbol from the pattern.
        let sign = |i: usize| -> f32 {
            let bit = e.pattern >> (e.len as usize - 1 - i) & 1;
            let s = if bit == 1 { -1.0 } else { 1.0 };
            if h.inverted { -s } else { s }
        };
        let outer_idx: Vec<usize> = (0..e.len as usize).filter(|&i| e.mask >> (e.len as usize - 1 - i) & 1 == 1).collect();
        // Levels from the sync's outer symbols.
        let levels = |vals: &[f32]| -> (f32, f32) {
            if opts.robust_levels {
                // Each sign's sync symbols with the extremes dropped (3 each
                // side); `sign` already folds in inversion.
                let side = |want: f32| -> Option<f32> {
                    let mut v: Vec<f32> = outer_idx.iter().filter(|&&i| sign(i) == want).map(|&i| vals[before + i]).collect();
                    v.sort_by(f32::total_cmp);
                    let drop = (v.len() / 5).min(3);
                    let v = &v[drop..v.len() - drop];
                    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
                };
                if let (Some(p), Some(n)) = (side(1.0), side(-1.0)) {
                    return ((p - n) / 2.0, (p + n) / 2.0);
                }
            }
            let outer = outer_idx.iter().map(|&i| vals[before + i] * sign(i)).sum::<f32>() / outer_idx.len() as f32;
            let centre = outer_idx.iter().map(|&i| vals[before + i] - sign(i) * outer).sum::<f32>() / outer_idx.len() as f32;
            (outer, centre)
        };
        let norm = |vals: &[f32], outer: f32, centre: f32| -> Vec<f32> {
            vals.iter().map(|&v| if h.inverted { -(v - centre) } else { v - centre } / outer).collect()
        };
        let mut vals = sample(0.0);
        let (mut outer, mut centre) = levels(&vals);
        if opts.refine {
            // Eye opening: mean distance of each symbol to its nearest level.
            let cost = |soft: &[f32]| -> f32 {
                soft.iter()
                    .map(|v| [-1.0f32, -1.0 / 3.0, 1.0 / 3.0, 1.0].iter().map(|l| (v - l).abs()).fold(f32::MAX, f32::min))
                    .sum::<f32>()
                    / soft.len() as f32
            };
            let mut best = if outer > 0.0 { cost(&norm(&vals, outer, centre)) } else { f32::MAX };
            for k in -9..=9 {
                let off = k as f64 * 0.05;
                if k == 0 {
                    continue;
                }
                let v = sample(off);
                let (o, c) = levels(&v);
                if o <= 0.0 {
                    continue;
                }
                let cst = cost(&norm(&v, o, c));
                if cst < best {
                    (best, vals, outer, centre) = (cst, v, o, c);
                }
            }
        }
        if outer <= 0.0 {
            continue;
        }
        let mut soft = norm(&vals, outer, centre);
        if opts.dc_track {
            // Walk out from the sync both ways; inner symbols (|v| < 2/3) pull
            // the centre toward their ideal ±1/3. DC is tracked on inner
            // symbols only: the outer ones are compressed by the deviation.
            let s0 = before + e.len as usize;
            for order in [(s0..soft.len()).collect::<Vec<_>>(), (0..before).rev().collect()] {
                let mut dc = 0.0f32;
                for i in order {
                    soft[i] -= dc;
                    let v = soft[i];
                    if v.abs() < 2.0 / 3.0 {
                        dc += 0.1 * (v - if v >= 0.0 { 1.0 / 3.0 } else { -1.0 / 3.0 });
                    }
                }
            }
        }
        if opts.isi_boost {
            // Isolated symbols against symbols inside runs.
            let n = soft.len();
            let isolated = |s: &[f32], i: usize| s[i].signum() != s[i - 1].signum() && s[i].signum() != s[i + 1].signum();
            let (mut iso, mut ni, mut run, mut nr) = (0.0f32, 0, 0.0f32, 0);
            for i in 1..n - 1 {
                if isolated(&soft, i) {
                    iso += soft[i].abs();
                    ni += 1;
                } else if soft[i].signum() == soft[i - 1].signum() && soft[i].signum() == soft[i + 1].signum() {
                    run += soft[i].abs();
                    nr += 1;
                }
            }
            if ni > 4 && nr > 4 {
                let k = ((run / nr as f32) / (iso / ni as f32)).clamp(1.0, 1.3);
                let orig = soft.clone();
                for i in 1..n - 1 {
                    if isolated(&orig, i) {
                        soft[i] *= k;
                    }
                }
            }
        }
        let dibits = soft
            .iter()
            .map(|&v| {
                if v >= 2.0 / 3.0 {
                    0b01
                } else if v >= 0.0 {
                    0b00
                } else if v >= -2.0 / 3.0 {
                    0b10
                } else {
                    0b11
                }
            })
            .collect();
        frames.push(Frame {
            protocol: e.protocol,
            detail: e.detail,
            pattern: e.pattern,
            inverted: h.inverted,
            dibits,
            soft,
            sync_start: before,
            at_symbol: first_sync,
            outer,
        });
    }
    frames.sort_by_key(|f| f.at_symbol);
    frames
}

pub(crate) fn boxcar(x: &[f32], n: usize) -> Vec<f32> {
    if n <= 1 {
        return x.to_vec();
    }
    let mut out = vec![0.0; x.len()];
    let mut acc = 0.0f32;
    for i in 0..x.len() {
        acc += x[i];
        if i >= n {
            acc -= x[i - n];
        }
        out[i.saturating_sub(n / 2)] = acc / n.min(i + 1) as f32;
    }
    out
}

pub(crate) fn interp(x: &[f32], t: f64) -> f32 {
    let i = t.floor() as usize;
    if i + 1 >= x.len() {
        return x[x.len() - 1];
    }
    let f = (t - i as f64) as f32;
    x[i] * (1.0 - f) + x[i + 1] * f
}
