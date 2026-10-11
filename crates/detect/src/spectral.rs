//! Identifies known wideband occupants from a power spectrum alone: FM
//! broadcast stations (and their HD Radio sidebands), digital TV channels
//! (ATSC 1.0 by its pilot, ATSC 3.0 by its flat block), and LTE/NR carriers.
//!
//! These are what the spectrum squelch hides so that unknown signals stand
//! out. Each test is shape-based and resolution-aware: it only runs when the
//! bins are fine enough to see the feature it relies on.

use crate::bandplan;

/// A spectrum to examine: dB per bin (any consistent scale), lowest frequency
/// first, and the noise floor per bin on the same scale.
pub struct Spectrum<'a> {
    pub db: &'a [f32],
    pub floor: &'a [f32],
    /// Frequency of the lower edge of bin 0.
    pub f0_hz: f64,
    pub bin_hz: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// An FM broadcast station; `hd` when HD Radio sidebands are present.
    FmBroadcast { hd: bool },
    /// A digital TV channel. `atsc1` when the ATSC 1.0 pilot is seen; else a
    /// flat block (ATSC 3.0, or ATSC 1.0 seen too coarsely to tell).
    Tv { channel: u16, atsc1: bool },
    /// An LTE/NR carrier of the given nominal bandwidth.
    Cellular { bw_mhz: f32 },
}

impl Kind {
    pub fn label(&self) -> String {
        match self {
            Kind::FmBroadcast { hd: true } => "FM broadcast + HD Radio".into(),
            Kind::FmBroadcast { hd: false } => "FM broadcast".into(),
            Kind::Tv { channel, atsc1: true } => format!("TV ch {channel} (ATSC 1.0)"),
            Kind::Tv { channel, atsc1: false } => format!("TV ch {channel} (digital)"),
            Kind::Cellular { bw_mhz } => format!("LTE/NR {bw_mhz} MHz"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Occupant {
    pub kind: Kind,
    pub center_hz: f64,
    /// Extent the occupant covers (including sidebands).
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Level above the floor, dB.
    pub snr_db: f32,
}

impl Spectrum<'_> {
    fn bin(&self, hz: f64) -> f64 {
        (hz - self.f0_hz) / self.bin_hz
    }

    fn covers(&self, lo: f64, hi: f64) -> bool {
        self.bin(lo) >= 0.0 && self.bin(hi) <= self.db.len() as f64
    }

    fn range(&self, lo: f64, hi: f64) -> std::ops::Range<usize> {
        let a = self.bin(lo).floor().max(0.0) as usize;
        let b = (self.bin(hi).ceil() as usize).min(self.db.len()).max(a + 1).min(self.db.len());
        a.min(b)..b
    }

    /// Mean power over [lo, hi), in dB above the floor (power-averaged).
    pub fn level(&self, lo: f64, hi: f64) -> f32 {
        let r = self.range(lo, hi);
        if r.is_empty() {
            return f32::MIN;
        }
        let n = r.len() as f64;
        let p: f64 = r.clone().map(|i| 10f64.powf((self.db[i] - self.floor[i]) as f64 / 10.0)).sum::<f64>() / n;
        (10.0 * p.log10()) as f32
    }

    /// Median dB above floor over [lo, hi): robust to narrow lines.
    pub fn median(&self, lo: f64, hi: f64) -> f32 {
        let r = self.range(lo, hi);
        let mut v: Vec<f32> = r.map(|i| self.db[i] - self.floor[i]).collect();
        if v.is_empty() {
            return f32::MIN;
        }
        let m = v.len() / 2;
        *v.select_nth_unstable_by(m, f32::total_cmp).1
    }

    /// Highest bin (dB above floor) over [lo, hi).
    pub fn peak(&self, lo: f64, hi: f64) -> f32 {
        self.range(lo, hi).map(|i| self.db[i] - self.floor[i]).fold(f32::MIN, f32::max)
    }

    /// Like `ripple`, but around the best straight line through the slice
    /// levels: a flat-topped signal seen through a tilted front end.
    pub fn ripple_detilted(&self, lo: f64, hi: f64, step: f64) -> f32 {
        let mut pts = Vec::new();
        let mut f = lo;
        while f + step <= hi + 1.0 {
            pts.push((f as f32, self.level(f, f + step)));
            f += step;
        }
        if pts.len() < 3 {
            return self.ripple(lo, hi, step);
        }
        let n = pts.len() as f32;
        let (mx, my) = (pts.iter().map(|p| p.0).sum::<f32>() / n, pts.iter().map(|p| p.1).sum::<f32>() / n);
        let sxy: f32 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let sxx: f32 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let res: Vec<f32> = pts.iter().map(|p| p.1 - (my + slope * (p.0 - mx))).collect();
        res.iter().copied().fold(f32::MIN, f32::max) - res.iter().copied().fold(f32::MAX, f32::min)
    }

    /// Spread of levels over [lo, hi) in `step` slices: max − min of their
    /// power means, dB. Small for flat-topped signals.
    pub fn ripple(&self, lo: f64, hi: f64, step: f64) -> f32 {
        let mut f = lo;
        let (mut mn, mut mx) = (f32::MAX, f32::MIN);
        while f + step <= hi + 1.0 {
            let l = self.level(f, f + step);
            mn = mn.min(l);
            mx = mx.max(l);
            f += step;
        }
        mx - mn
    }
}

/// All known occupants found in `s`, lowest first.
pub fn identify(s: &Spectrum) -> Vec<Occupant> {
    let mut out = Vec::new();
    out.extend(fm_broadcast(s));
    out.extend(tv(s));
    out.extend(cellular(s, &out));
    out.sort_by(|a, b| a.lo_hz.total_cmp(&b.lo_hz));
    out
}

// --------------------------------------------------------- FM broadcast ----

/// Analog FM needs this much above the floor in its central ±60 kHz.
const FM_MIN_DB: f32 = 10.0;
/// HD sidebands: each within this of the other and at least this far above
/// the floor, flat, with a clear drop just outside.
const HD_MIN_DB: f32 = 6.0;
const HD_MAX_RIPPLE_DB: f32 = 4.0;
const HD_EDGE_DROP_DB: f32 = 6.0;

fn fm_broadcast(s: &Spectrum) -> Vec<Occupant> {
    let mut out = Vec::new();
    // Needs a few bins per 200 kHz channel (a full-band survey has 54 kHz).
    if s.bin_hz > 60e3 {
        return out;
    }
    let lo = s.f0_hz + 200e3;
    let hi = s.f0_hz + s.db.len() as f64 * s.bin_hz - 200e3;
    for c in bandplan::fm_channels(lo, hi) {
        let core = s.level(c - 60e3, c + 60e3);
        // A station's energy is centred: the core beats the band 100-125 kHz out.
        let shoulder = s.level(c - 125e3, c - 100e3).max(s.level(c + 100e3, c + 125e3));
        let analog = core >= FM_MIN_DB && core > shoulder + 3.0;
        let hd = s.bin_hz <= 15e3 && hd_sidebands(s, c);
        if !(analog || hd) {
            continue;
        }
        let (lo, hi) = if hd { (c - 200e3, c + 200e3) } else { (c - 100e3, c + 100e3) };
        out.push(Occupant { kind: Kind::FmBroadcast { hd }, center_hz: c, lo_hz: lo, hi_hz: hi, snr_db: core });
    }
    out
}

/// HD Radio (hybrid IBOC) puts flat OFDM sidebands at 129-199 kHz either
/// side of the analog carrier, about 20 dB under it.
fn hd_sidebands(s: &Spectrum, c: f64) -> bool {
    if !s.covers(c - 230e3, c + 230e3) {
        return false;
    }
    let side = |sign: f64| {
        let (a, b) = (c + sign * 135e3, c + sign * 195e3);
        let (lo, hi) = (a.min(b), a.max(b));
        let level = s.level(lo, hi);
        // Just outside the outer edge (200-230 kHz out).
        let (o1, o2) = (c + sign * 205e3, c + sign * 230e3);
        let outside = s.level(o1.min(o2), o1.max(o2));
        let ripple = s.ripple(lo, hi, 15e3);
        (level, outside, ripple)
    };
    let (l, lo_out, lr) = side(-1.0);
    let (u, hi_out, ur) = side(1.0);
    l >= HD_MIN_DB
        && u >= HD_MIN_DB
        && (l - u).abs() < 6.0
        && lr < HD_MAX_RIPPLE_DB
        && ur < HD_MAX_RIPPLE_DB
        && l - lo_out >= HD_EDGE_DROP_DB
        && u - hi_out >= HD_EDGE_DROP_DB
}

// ------------------------------------------------------------------- TV ----

/// ATSC 1.0 pilot: this far above the floor and above the data next to it.
const ATSC_PILOT_MIN_DB: f32 = 6.0;
const ATSC_PILOT_CLEAR_DB: f32 = 6.0;
/// A channel without a pilot (ATSC 3.0) must be flat this far above the
/// floor, with at most this ripple across 750 kHz slices...
const TV_MIN_DB: f32 = 3.0;
const TV_MAX_RIPPLE_DB: f32 = 3.0;
/// ...and a dip at least this deep at one channel edge or the other.
const TV_EDGE_DB: f32 = 2.0;
/// ATSC 1.0 pilot: 309.44 kHz above the channel's lower edge.
const ATSC_PILOT_HZ: f64 = 309_440.6;

fn tv(s: &Spectrum) -> Vec<Occupant> {
    let mut out = Vec::new();
    if s.bin_hz > 120e3 {
        return out;
    }
    let span_lo = s.f0_hz;
    let span_hi = s.f0_hz + s.db.len() as f64 * s.bin_hz;
    let mut f = span_lo;
    while f < span_hi {
        let Some((ch, lo)) = bandplan::tv_channel(f) else {
            f += 1e6;
            continue;
        };
        f = lo + 6e6 + 1.0;
        if !s.covers(lo - 0.2e6, lo + 6.2e6) {
            continue;
        }
        let flat = s.median(lo + 0.7e6, lo + 5.3e6);
        // ATSC 1.0: the pilot is a line standing well clear of the data just
        // above it, even when the channel itself is only a few dB up (a
        // distant station: pilot +14 dB, data +3 dB at 4.7 kHz bins).
        let w = 2.0 * s.bin_hz.max(2e3);
        let pilot = s.peak(lo + ATSC_PILOT_HZ - w, lo + ATSC_PILOT_HZ + w);
        let above = s.median(lo + 0.4e6, lo + 1.4e6);
        if pilot >= ATSC_PILOT_MIN_DB && pilot - above >= ATSC_PILOT_CLEAR_DB && flat >= 1.0 {
            out.push(Occupant {
                kind: Kind::Tv { channel: ch, atsc1: true },
                center_hz: lo + 3e6,
                lo_hz: lo,
                hi_hz: lo + 6e6,
                snr_db: flat,
            });
            continue;
        }
        // Otherwise a flat block filling the channel (ATSC 3.0), with a dip at
        // a channel edge: ATSC 1.0 neighbours leave 0.3 MHz, ATSC 3.0 84 kHz.
        if flat < TV_MIN_DB || s.ripple(lo + 0.75e6, lo + 5.25e6, 0.75e6) > TV_MAX_RIPPLE_DB {
            continue;
        }
        let notch_lo = flat - s.median(lo - 0.1e6, lo + 0.08e6).min(s.median(lo - 0.25e6, lo - 0.05e6));
        let notch_hi = flat - s.median(lo + 5.92e6, lo + 6.1e6).min(s.median(lo + 6.05e6, lo + 6.25e6));
        if notch_lo < TV_EDGE_DB && notch_hi < TV_EDGE_DB {
            continue;
        }
        out.push(Occupant { kind: Kind::Tv { channel: ch, atsc1: false }, center_hz: lo + 3e6, lo_hz: lo, hi_hz: lo + 6e6, snr_db: flat });
    }
    out
}

// ------------------------------------------------------------- Cellular ----

/// LTE channel bandwidths and their occupied widths (90%; 1.4 MHz is 1.08).
const LTE_BW: [(f32, f64); 6] = [(1.4, 1.08e6), (3.0, 2.7e6), (5.0, 4.5e6), (10.0, 9.0e6), (15.0, 13.5e6), (20.0, 18.0e6)];
const CELL_MIN_DB: f32 = 4.0;
const CELL_MAX_RIPPLE_DB: f32 = 6.0;
/// A level change this big within 1 MHz separates two carriers.
const CELL_STEP_DB: f32 = 6.0;

/// Flat-topped blocks in cellular downlink allocations. The profile is the
/// level in 250 kHz steps, median-smoothed over 1.25 MHz so that a lightly
/// loaded carrier's reference-signal comb reads as one block. Runs above the
/// floor are split where the level steps (adjacent carriers of different
/// power, often different operators); each piece of 1 MHz or more is one
/// carrier (or a group of equal ones), labelled with the nearest LTE
/// bandwidth when its width matches one.
fn cellular(s: &Spectrum, found: &[Occupant]) -> Vec<Occupant> {
    let mut out = Vec::new();
    // A carrier can fill a narrow span edge to edge, leaving no floor to
    // judge it against: only look with 20 MHz or more in view.
    if s.bin_hz > 200e3 || s.db.len() as f64 * s.bin_hz < 20e6 {
        return out;
    }
    let step = 250e3f64.max(s.bin_hz);
    let n = ((s.db.len() as f64 * s.bin_hz) / step) as usize;
    let raw: Vec<f32> = (0..n).map(|k| s.level(s.f0_hz + k as f64 * step, s.f0_hz + (k + 1) as f64 * step)).collect();
    let lv: Vec<f32> = (0..n)
        .map(|k| {
            let mut w: Vec<f32> = raw[k.saturating_sub(2)..(k + 3).min(n)].to_vec();
            let m = w.len() / 2;
            *w.select_nth_unstable_by(m, f32::total_cmp).1
        })
        .collect();
    let mut k = 0;
    while k < n {
        if lv[k] < CELL_MIN_DB {
            k += 1;
            continue;
        }
        let start = k;
        while k < n && lv[k] >= CELL_MIN_DB {
            k += 1;
        }
        // Split at steps: where the level 1 MHz on differs by > CELL_STEP_DB.
        let mut cuts = vec![start];
        let mut j = start + 4;
        while j + 4 <= k {
            if (lv[j] - lv[j - 4]).abs() > CELL_STEP_DB {
                // Cut at the steepest point between j-4 and j.
                let c = (j - 3..=j).max_by(|&a, &b| (lv[a] - lv[a - 1]).abs().total_cmp(&(lv[b] - lv[b - 1]).abs())).unwrap();
                cuts.push(c);
                j = c + 4;
            } else {
                j += 1;
            }
        }
        cuts.push(k);
        let alone = cuts.len() == 2;
        for w in cuts.windows(2) {
            let lo = s.f0_hz + w[0] as f64 * step;
            let hi = s.f0_hz + w[1] as f64 * step;
            let width = hi - lo;
            let c = (lo + hi) / 2.0;
            // Pieces cut from a longer run under 2 MHz are skirts, not carriers.
            if width < if alone { 1e6 } else { 2e6 } || !bandplan::lookup(c).is_some_and(|b| b.service == bandplan::Service::CellularDown) {
                continue;
            }
            if found.iter().any(|o| o.lo_hz < hi && o.hi_hz > lo) {
                continue;
            }
            if s.ripple_detilted(lo + 0.1 * width, hi - 0.1 * width, (width / 5.0).max(step)) > CELL_MAX_RIPPLE_DB {
                continue;
            }
            let bw = LTE_BW
                .iter()
                .find(|(_, occ)| (width - occ).abs() <= (0.15 * occ).max(2.0 * step))
                .map(|b| b.0)
                .unwrap_or(((width / 1e5).round() / 10.0) as f32);
            out.push(Occupant { kind: Kind::Cellular { bw_mhz: bw }, center_hz: c, lo_hz: lo, hi_hz: hi, snr_db: s.median(lo, hi) });
        }
    }
    out
}

/// Level of a bin that was never measured (a hop a segmented sweep skips).
/// Anything at or below `EMPTY_DB + 1` is "no data", not a quiet bin.
pub const EMPTY_DB: f32 = -200.0;

/// True for a bin that holds no measurement (see `EMPTY_DB`).
#[inline]
pub fn is_empty_db(v: f32) -> bool {
    v <= EMPTY_DB + 1.0
}

/// A noise floor for a spectrum with no better estimate: the 20th
/// percentile of each `window`-wide block, interpolated linearly.
pub fn floor_estimate(db: &[f32], window_bins: usize) -> Vec<f32> {
    let n = db.len();
    let w = window_bins.clamp(16, n.max(16));
    let blocks = n.div_ceil(w);
    // Unmeasured bins (`EMPTY_DB`) don't count; a block with none measured
    // takes its nearest measured neighbour's floor.
    let mut fl: Vec<Option<f32>> = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let mut v: Vec<f32> = db[b * w..((b + 1) * w).min(n)].iter().copied().filter(|v| !is_empty_db(*v)).collect();
        if v.is_empty() {
            fl.push(None);
            continue;
        }
        let i = v.len() / 5;
        fl.push(Some(*v.select_nth_unstable_by(i, f32::total_cmp).1));
    }
    // A block filled by a wide signal reads high: cap each block at 3 dB
    // over the lower of its neighbours (the nearest measured one each side,
    // so a gap next to a signal doesn't lose the cap).
    let raw = fl.clone();
    for b in 0..blocks {
        let Some(own) = raw[b] else { continue };
        let l = (0..b).rev().find_map(|j| raw[j]).unwrap_or(own);
        let r = (b + 1..blocks).find_map(|j| raw[j]).unwrap_or(own);
        fl[b] = Some(own.min(l.min(r) + 3.0));
    }
    // Blocks before the first measured one take the first measured floor;
    // later unmeasured blocks carry the previous one forward.
    let mut last = fl.iter().flatten().next().copied().unwrap_or(EMPTY_DB);
    let fl: Vec<f32> = fl
        .iter()
        .map(|v| {
            if let Some(v) = v {
                last = *v;
            }
            last
        })
        .collect();
    (0..n)
        .map(|i| {
            let x = (i as f64 + 0.5) / w as f64 - 0.5;
            let a = x.floor().clamp(0.0, (blocks - 1) as f64) as usize;
            let b = (a + 1).min(blocks - 1);
            let t = (x - a as f64).clamp(0.0, 1.0) as f32;
            fl[a] + t * (fl[b] - fl[a])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat floor at 0 dB over `span` around `centre`, `bin` Hz per bin,
    /// with `shape(f) -> dB above floor` added (power sum).
    fn synth(centre: f64, span: f64, bin: f64, shape: impl Fn(f64) -> f32) -> (Vec<f32>, Vec<f32>, f64) {
        let n = (span / bin) as usize;
        let f0 = centre - span / 2.0;
        let db = (0..n)
            .map(|i| {
                let f = f0 + (i as f64 + 0.5) * bin;
                let s = shape(f);
                (10.0 * (1.0 + 10f64.powf(s as f64 / 10.0)).log10()) as f32
            })
            .collect();
        (db, vec![0.0; n], f0)
    }

    #[test]
    fn floor_ignores_unmeasured_bins() {
        // 2000 bins at -100 dB; 600..1400 never measured; a wide signal
        // sits right next to the gap.
        let mut db = vec![-100.0f32; 2000];
        for v in &mut db[600..1400] {
            *v = EMPTY_DB;
        }
        for v in &mut db[1400..1600] {
            *v = -70.0;
        }
        let fl = floor_estimate(&db, 100);
        for (i, f) in fl.iter().enumerate() {
            assert!((-101.0..-96.0).contains(f), "bin {i}: floor {f}");
        }
        // Nothing is identified from a gap edge.
        let s = Spectrum { db: &db, floor: &fl, f0_hz: 600e6, bin_hz: 54e3 };
        assert!(identify(&s).is_empty());
    }

    fn fm(c: f64, hd: bool) -> impl Fn(f64) -> f32 {
        move |f: f64| {
            let d = (f - c).abs();
            if d < 90e3 {
                40.0 - (d / 90e3 * 15.0) as f32
            } else if hd && (129e3..199e3).contains(&d) {
                20.0
            } else {
                -30.0
            }
        }
    }

    #[test]
    fn fm_with_and_without_hd() {
        let (db, fl, f0) = synth(102.0e6, 1e6, 610.0, |f| fm(102.1e6, true)(f).max(fm(101.7e6, false)(f)));
        let s = Spectrum { db: &db, floor: &fl, f0_hz: f0, bin_hz: 610.0 };
        let o = identify(&s);
        assert_eq!(o.len(), 2, "{o:?}");
        assert_eq!((o[0].center_hz, o[0].kind), (101.7e6, Kind::FmBroadcast { hd: false }));
        assert_eq!((o[1].center_hz, o[1].kind), (102.1e6, Kind::FmBroadcast { hd: true }));
    }

    fn atsc(lo: f64, pilot: bool) -> impl Fn(f64) -> f32 {
        move |f: f64| {
            let d = f - lo;
            // The synthetic spectrum samples bin centres: make the line a bin wide.
            if pilot && (d - ATSC_PILOT_HZ).abs() < 6e3 {
                40.0
            } else if (0.31e6..5.69e6).contains(&d) {
                25.0
            } else {
                -30.0
            }
        }
    }

    #[test]
    fn tv_channels_with_and_without_pilot() {
        // Channels 30 (566-572) with pilot and 31 (572-578) without, at 10 kHz bins.
        let (db, fl, f0) = synth(572e6, 20e6, 10e3, |f| atsc(566e6, true)(f).max(atsc(572e6, false)(f)));
        let s = Spectrum { db: &db, floor: &fl, f0_hz: f0, bin_hz: 10e3 };
        let o = identify(&s);
        let tv: Vec<_> = o.iter().map(|o| o.kind).collect();
        assert_eq!(tv, vec![Kind::Tv { channel: 30, atsc1: true }, Kind::Tv { channel: 31, atsc1: false }], "{o:?}");
    }

    #[test]
    fn lte_block_in_downlink_band_only() {
        let lte = |c: f64| move |f: f64| if (f - c).abs() < 4.5e6 { 15.0 } else { -30.0 };
        let (db, fl, f0) = synth(740e6, 30e6, 50e3, lte(737e6));
        let s = Spectrum { db: &db, floor: &fl, f0_hz: f0, bin_hz: 50e3 };
        let o = identify(&s);
        assert_eq!(o.len(), 1, "{o:?}");
        assert_eq!(o[0].kind, Kind::Cellular { bw_mhz: 10.0 });
        assert!((o[0].center_hz - 737e6).abs() < 100e3);
        // The same block in an uplink band isn't a downlink carrier.
        let (db, fl, f0) = synth(707e6, 30e6, 50e3, lte(707e6));
        let s = Spectrum { db: &db, floor: &fl, f0_hz: f0, bin_hz: 50e3 };
        assert!(identify(&s).iter().all(|o| !matches!(o.kind, Kind::Cellular { .. })));
    }
}
