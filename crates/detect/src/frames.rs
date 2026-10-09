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
    /// Index into `dibits` of the first sync symbol.
    pub sync_start: usize,
    /// Outer (±3) level measured on the sync, in discriminator units (Hz).
    pub outer: f32,
}

/// Groups hits that are the same sync seen at neighbouring phases and
/// returns one frame per occurrence, sliced `before`/`after` symbols around
/// the sync. `disc` is the DC-removed discriminator at `fs`.
pub fn slice(disc: &[f32], fs: f64, hits: &[SyncHit], before: usize, after: usize) -> Vec<Frame> {
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
    let mut frames = Vec::new();
    for mut g in groups {
        g.sort_by_key(|h| (h.symbol, h.phase));
        let h = g[g.len() / 2];
        let e = &h.entry;
        let sym_len = fs / e.baud;
        let smoothed = boxcar(disc, ((sym_len) * 0.5).round().max(1.0) as usize);
        let first_sync = h.symbol + 1 - e.len as usize;
        let Some(start) = first_sync.checked_sub(before) else { continue };
        let n = before + e.len as usize + after;
        let vals: Vec<f32> = (start..start + n)
            .map(|k| {
                let t = (k as f64 + (h.phase as f64 + 0.5) / PHASES as f64) * sym_len;
                interp(&smoothed, t)
            })
            .collect();
        if vals.len() < n || (start + n) as f64 * sym_len >= smoothed.len() as f64 {
            continue;
        }
        // Levels from the sync's outer symbols: expected sign from the pattern.
        let sign = |i: usize| -> f32 {
            let bit = e.pattern >> (e.len as usize - 1 - i) & 1;
            let s = if bit == 1 { -1.0 } else { 1.0 };
            if h.inverted { -s } else { s }
        };
        let outer_idx: Vec<usize> = (0..e.len as usize).filter(|&i| e.mask >> (e.len as usize - 1 - i) & 1 == 1).collect();
        let outer = outer_idx.iter().map(|&i| vals[before + i] * sign(i)).sum::<f32>() / outer_idx.len() as f32;
        let centre = outer_idx.iter().map(|&i| vals[before + i] - sign(i) * outer).sum::<f32>() / outer_idx.len() as f32;
        if outer <= 0.0 {
            continue;
        }
        let thr = outer * 2.0 / 3.0;
        let dibits = vals
            .iter()
            .map(|&v| {
                let v = if h.inverted { -(v - centre) } else { v - centre };
                if v >= thr {
                    0b01
                } else if v >= 0.0 {
                    0b00
                } else if v >= -thr {
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
            sync_start: before,
            outer,
        });
    }
    frames.sort_by_key(|f| f.sync_start);
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
