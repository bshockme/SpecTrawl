//! Sync bank: sign-bit history at every sampling phase, matched
//! against a masked table of known sync words.
//!
//! Patterns are sign bits, oldest symbol first, newest in bit 0; a set bit
//! means a negative symbol. For 4-level protocols only the outer ±3 symbols
//! decide the sign, which is why masks drop the ±1 positions (NXDN).

#[derive(Debug, Clone, Copy)]
pub struct SyncEntry {
    pub protocol: &'static str,
    pub detail: &'static str,
    pub baud: f64,
    pub len: u8,
    pub pattern: u32,
    pub mask: u32,
    /// Also match the inverted pattern (discriminator polarity unknown).
    /// DMR and dPMR list both polarities explicitly instead.
    pub invertible: bool,
    /// 4-level protocol: sync symbols must all be outer (±3) levels and
    /// masked-out positions inner (±1). Binary protocols skip this check.
    pub four_level: bool,
    /// Symbols between consecutive sync positions (a burst, a frame or a
    /// batch): a second sync confirms the first only at a multiple of this,
    /// missed frames allowed. 0 when the protocol's grid isn't known.
    pub period: u32,
}

#[allow(clippy::too_many_arguments)]
const fn e(
    protocol: &'static str,
    detail: &'static str,
    baud: f64,
    len: u8,
    pattern: u32,
    mask: u32,
    invertible: bool,
    period: u32,
) -> SyncEntry {
    SyncEntry { protocol, detail, baud, len, pattern, mask, invertible, four_level: true, period }
}

const fn bin(protocol: &'static str, detail: &'static str, baud: f64, pattern: u32, invertible: bool, period: u32) -> SyncEntry {
    SyncEntry { protocol, detail, baud, len: 32, pattern, mask: 0xFFFF_FFFF, invertible, four_level: false, period }
}

/// Sync grids, in symbols. P25 Phase 1 frames (HDU 792, TDU 144, LDU 1728,
/// TDULC 432, TSDU 360 bits, status symbols included) are all multiples of
/// 72 bits. DMR bursts are 30 ms (264 bits + CACH) in every mode. P25 Phase
/// 2 has an ISCH every 30 ms slot. NXDN and dPMR frames are 80 ms (384 bits
/// at 2400 sym/s, 40 ms at NXDN96's 4800). YSF frames are 100 ms. POCSAG
/// batches are the sync plus 16 codewords.
const P25P1: u32 = 36;
const DMR: u32 = 144;
const P25P2: u32 = 180;
const NXDN: u32 = 192;
const DPMR: u32 = 192;
const YSF: u32 = 480;
const POCSAG: u32 = 544;

/// NXDN frame sync + LICH: the 18 valid values (mask drops ±1 symbols).
const NXDN_LICH: [u32; 18] = [
    0x2B0C8, 0x2B0E1, 0x2B0E5, 0x2B0C4, 0x2B08B, 0x2B087, 0x2B089, 0x2B085, 0x2B0A4, 0x2B04B, 0x2B047, 0x2B027, 0x2B049, 0x2B045, 0x2B079,
    0x2B075, 0x2B064, 0x2B025,
];

pub fn table() -> Vec<SyncEntry> {
    let mut v = vec![
        e("P25", "Phase 1", 4800.0, 24, 0x04CF5F, 0xFFFFFF, true, P25P1),
        e("P25", "Phase 2", 6000.0, 20, 0x0121DF, 0xFFFFF, true, P25P2),
        e("DMR", "MS voice", 4800.0, 24, 0x76286E, 0xFFFFFF, false, DMR),
        e("DMR", "BS voice", 4800.0, 24, 0x439B4D, 0xFFFFFF, false, DMR),
        e("DMR", "MS data", 4800.0, 24, 0x89D791, 0xFFFFFF, false, DMR),
        e("DMR", "BS data", 4800.0, 24, 0xBC64B2, 0xFFFFFF, false, DMR),
        e("DMR", "direct TS1 voice", 4800.0, 24, 0x21751F, 0xFFFFFF, false, DMR),
        e("DMR", "direct TS2 voice", 4800.0, 24, 0x6F8C23, 0xFFFFFF, false, DMR),
        e("DMR", "direct TS1 data", 4800.0, 24, 0xDE8AE0, 0xFFFFFF, false, DMR),
        e("DMR", "direct TS2 data", 4800.0, 24, 0x9073DC, 0xFFFFFF, false, DMR),
        e("YSF", "Fusion", 4800.0, 20, 0x84A52, 0xFFFFF, true, YSF),
        e("dPMR", "FS1", 2400.0, 24, 0x1F3485, 0xFFFFFF, false, DPMR),
        e("dPMR", "FS1 (inv)", 2400.0, 24, 0xE0CB7A, 0xFFFFFF, false, DPMR),
        e("X2-TDMA", "sync A (unverified)", 4800.0, 24, 0xA780FD, 0xFFFFFF, true, 0),
        e("X2-TDMA", "sync B (unverified)", 4800.0, 24, 0xD41473, 0xFFFFFF, true, 0),
        bin("ProVoice", "", 9600.0, 0x57125555, true, 0),
        bin("ProVoice", "", 9600.0, 0x57123333, true, 0),
        bin("ProVoice", "", 9600.0, 0x94D83523, true, 0),
        bin("ProVoice", "", 9600.0, 0x57126969, true, 0),
    ];
    // NXDN48 (2400 sym/s) and NXDN96 (4800 sym/s) share the sync.
    for &p in &NXDN_LICH {
        v.push(e("NXDN", "NXDN48", 2400.0, 18, p, 0x2FDFF, true, NXDN));
        v.push(e("NXDN", "NXDN96", 4800.0, 18, p, 0x2FDFF, true, NXDN));
    }
    // POCSAG batch sync codeword; binary FSK, so sign = bit.
    for baud in [512.0, 1200.0, 2400.0] {
        v.push(bin("POCSAG", "", baud, 0x7CD215D8, true, POCSAG));
    }
    v
}

/// Bit errors tolerated for a pattern of `len` symbols. Short patterns must
/// match exactly; 32-bit binary syncs allow 2.
fn tolerance(len: u8) -> u32 {
    if len >= 32 { 2 } else { 0 }
}

/// An amplitude-consistency check: every sync position must be
/// an outer symbol (magnitude near the median of the sync's magnitudes) and
/// every masked-out position inside the pattern an inner one. `vals[0]` is
/// the newest symbol.
fn levels_ok(e: &SyncEntry, vals: &[f32; 32]) -> bool {
    if !e.four_level {
        return true;
    }
    let mut outer: Vec<f32> = (0..e.len as usize).filter(|&i| e.mask >> i & 1 == 1).map(|i| vals[i].abs()).collect();
    if outer.is_empty() {
        return false;
    }
    let mid = outer.len() / 2;
    let med = *outer.select_nth_unstable_by(mid, f32::total_cmp).1;
    if med <= 0.0 {
        return false;
    }
    (0..e.len as usize).all(|i| {
        let m = vals[i].abs() / med;
        if e.mask >> i & 1 == 1 { m > 0.6 && m < 1.6 } else { m < 0.66 }
    })
}

#[derive(Debug, Clone)]
pub struct SyncHit {
    pub entry: SyncEntry,
    pub inverted: bool,
    pub phase: usize,
    /// Symbol index (at `entry.baud`) where the pattern ended.
    pub symbol: usize,
    pub errors: u32,
}

/// Samples per symbol searched (parallel phases).
pub const PHASES: usize = 10;

/// How the sync search reads a discriminator.
#[derive(Debug, Clone, Copy)]
pub struct SearchOpts {
    /// Boxcar smoothing before sampling, as a fraction of a symbol. A symbol-spaced discriminator
    /// ([`crate::features::symbol_discriminator`]) is already one symbol's average and takes 0.
    pub smooth: f64,
    /// Also accept a word whose signs are a few symbols off (a sixth of its length) when its symbols correlate with
    /// the pattern's levels at least this well (see [`soft_match`]): a weak channel's noise flips a symbol or two of
    /// nearly every sync, which an exact match never accepts.
    pub soft: Option<f32>,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self { smooth: 0.5, soft: None }
    }
}

/// Searches the FM discriminator output `disc` (sampled at `fs`, DC removed)
/// for every table entry. Returns all hits.
pub fn search(disc: &[f32], fs: f64, table: &[SyncEntry]) -> Vec<SyncHit> {
    search_with(disc, fs, table, SearchOpts::default())
}

/// The correlation of a sync word's symbols (`vals`, newest first) with `e`'s pattern at its outer levels: Σ s·v over
/// the masked positions, over √(n·Σ v²), so 1 for a clean word and about N(0, 1/n) for noise. The positions the mask
/// drops (a 4-level word's inner symbols) must be inner on average: under two thirds of the outer level. Negative
/// for the inverted word.
pub fn soft_match(e: &SyncEntry, vals: &[f32; 32]) -> f32 {
    let (mut c, mut energy, mut n, mut inner, mut ni) = (0.0f32, 0.0f32, 0usize, 0.0f32, 0usize);
    for i in 0..e.len as usize {
        let v = vals[i];
        if e.mask >> i & 1 == 1 {
            c += if e.pattern >> i & 1 == 1 { -v } else { v };
            energy += v * v;
            n += 1;
        } else {
            inner += v.abs();
            ni += 1;
        }
    }
    if n == 0 || energy <= 0.0 {
        return 0.0;
    }
    if e.four_level && ni > 0 && inner / ni as f32 >= 2.0 / 3.0 * c.abs() / n as f32 {
        return 0.0;
    }
    c / (n as f32 * energy).sqrt()
}

/// [`search`] with its options.
pub fn search_with(disc: &[f32], fs: f64, table: &[SyncEntry], opts: SearchOpts) -> Vec<SyncHit> {
    let mut hits = Vec::new();
    let mut bauds: Vec<f64> = table.iter().map(|e| e.baud).collect();
    bauds.sort_by(f64::total_cmp);
    bauds.dedup();
    for baud in bauds {
        let entries: Vec<&SyncEntry> = table.iter().filter(|e| e.baud == baud).collect();
        // Need at least ~2 samples per symbol to decide signs.
        if fs / baud < 2.0 {
            continue;
        }
        let smoothed = boxcar(disc, ((fs / baud) * opts.smooth).round().max(1.0) as usize);
        let sym_len = fs / baud;
        let nsym = ((smoothed.len() as f64 - 1.0) / sym_len).floor() as usize;
        for phase in 0..PHASES {
            let mut hist: u32 = 0;
            let mut vals = [0.0f32; 32];
            for k in 0..nsym {
                let t = (k as f64 + (phase as f64 + 0.5) / PHASES as f64) * sym_len;
                let v = interp(&smoothed, t);
                hist = (hist << 1) | (v < 0.0) as u32;
                vals.copy_within(0..31, 1);
                vals[0] = v;
                if k < 17 {
                    continue;
                }
                for e in &entries {
                    if k + 1 < e.len as usize {
                        continue;
                    }
                    let m = hist & e.mask;
                    let errs = ((m ^ e.pattern) & e.mask).count_ones();
                    let errs_inv = ((m ^ (!e.pattern & e.mask)) & e.mask).count_ones();
                    let tol = tolerance(e.len);
                    if errs <= tol && levels_ok(e, &vals) {
                        hits.push(SyncHit { entry: **e, inverted: false, phase, symbol: k, errors: errs });
                    } else if e.invertible && errs_inv <= tol && levels_ok(e, &vals) {
                        hits.push(SyncHit { entry: **e, inverted: true, phase, symbol: k, errors: errs_inv });
                    } else if let Some(min) = opts.soft {
                        let near = u32::from(e.len) / 6;
                        if errs <= near && soft_match(e, &vals) >= min {
                            hits.push(SyncHit { entry: **e, inverted: false, phase, symbol: k, errors: errs });
                        } else if e.invertible && errs_inv <= near && -soft_match(e, &vals) >= min {
                            hits.push(SyncHit { entry: **e, inverted: true, phase, symbol: k, errors: errs_inv });
                        }
                    }
                }
            }
        }
    }
    hits
}

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub protocol: &'static str,
    pub detail: &'static str,
    pub baud: f64,
    pub inverted: bool,
    /// Distinct sync occurrences on the protocol's grid (merged across
    /// neighbouring phases).
    pub count: usize,
    pub confidence: f32,
}

struct Group {
    protocol: &'static str,
    detail: &'static str,
    baud: f64,
    inverted: bool,
    len: u8,
    period: u32,
    /// Symbol index of each distinct occurrence.
    at: Vec<usize>,
}

/// How a second sync confirms the first (DESIGN §7.1: "two-sync confirmation within one frame period + 3%").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Confirm {
    /// A whole number of grid periods apart, any number, ±(3 symbols + 0.5 % of the spacing). The rule before the
    /// rebuild's step 6; at that slack, two syncs more than ~0.6 s apart are always "on" P25's 36-symbol grid.
    AnyPeriods,
    /// Like `AnyPeriods`, ±(3 symbols + 0.05 % of the spacing): a transmitter's symbol clock is within tens of ppm of
    /// ours, not 5000.
    AnyPeriodsTight,
    /// On the grid, ±(3 symbols + 0.05 %), and no further apart than the protocol's longest sync-to-sync gap + 3 %; a
    /// transmission's count is its longest chain of such links. An entry with no known grid needs six syncs evenly
    /// spaced on one of their own.
    #[default]
    WithinFrame,
}

/// The longest gap between consecutive syncs of one transmission, in symbols: P25 Phase 1's LDU (864); P25 Phase 2's
/// 720-symbol fragment (its syncs sit at 360 and 540 of it, so consecutive syncs are 180 or 540 apart: measured on two
/// outbound channels on air, 2026-11-05, with 720 where one was missed); a DMR voice superframe on one slot (sync in
/// burst A of six, the slots interleaved: 12 bursts of 144); NXDN, dPMR, YSF and POCSAG sync every frame or batch, so
/// two of their periods (one missed).
pub fn max_gap(protocol: &str, period: u32) -> u32 {
    match protocol {
        "P25" if period == 36 => 864,
        "P25" if period == 180 => 720,
        "DMR" => 12 * 144,
        _ => 2 * period,
    }
}

/// Whether two sync positions `a` and `b` symbols into the stream can belong to one transmission on a grid of `period`
/// symbols under `rule`: a whole number of periods apart, give or take the sync search's symbol resolution and a little
/// clock error. Any spacing fits an unknown grid.
fn on_grid(a: usize, b: usize, period: u32, protocol: &str, rule: Confirm) -> bool {
    if period == 0 {
        return true;
    }
    let d = a.abs_diff(b) as f64;
    let k = (d / period as f64).round();
    let slack = match rule {
        Confirm::AnyPeriods => 0.005,
        Confirm::AnyPeriodsTight | Confirm::WithinFrame => 0.0005,
    };
    let near = rule != Confirm::WithinFrame || d <= 1.03 * max_gap(protocol, period) as f64;
    k >= 1.0 && near && (d - k * period as f64).abs() <= 3.0 + slack * d
}

/// The most occurrences in `at` on one grid that two of them define (their spacing, at least `min_gap` symbols): the
/// first pair, plus each other occurrence a whole number of that spacing from the first, ±3 symbols + 0.05 %.
fn own_grid(at: &mut [usize], min_gap: usize) -> usize {
    at.sort_unstable();
    let mut best = usize::from(!at.is_empty());
    for i in 0..at.len() {
        for j in i + 1..at.len() {
            let gap = at[j] - at[i];
            if gap < min_gap {
                continue;
            }
            let n = at
                .iter()
                .filter(|&&b| {
                    let d = b.abs_diff(at[i]) as f64;
                    let k = (d / gap as f64).round();
                    (d - k * gap as f64).abs() <= 3.0 + 0.0005 * d
                })
                .count();
            best = best.max(n);
        }
    }
    best
}

/// Picks the protocol with the most distinct sync occurrences on its grid.
/// Hits within a few symbols of each other (neighbouring phases seeing the
/// same word) count once. A protocol needs a second sync to be
/// believed, and the second must sit where the protocol's framing puts it
/// (a multiple of the burst/frame/batch period after the first, missed
/// frames allowed): two syncs at an arbitrary spacing are what noise and
/// analog voice produce, and confirm nothing.
pub fn best(hits: &[SyncHit]) -> Option<SyncResult> {
    best_with(hits, Confirm::default())
}

/// [`best`] under a given confirmation rule.
pub fn best_with(hits: &[SyncHit], rule: Confirm) -> Option<SyncResult> {
    let mut groups: Vec<Group> = Vec::new();
    for h in hits {
        let key = (h.entry.protocol, h.entry.detail, h.entry.baud, h.inverted);
        let g = match groups.iter_mut().find(|g| (g.protocol, g.detail, g.baud, g.inverted) == key) {
            Some(g) => g,
            None => {
                groups.push(Group {
                    protocol: key.0,
                    detail: key.1,
                    baud: key.2,
                    inverted: key.3,
                    len: h.entry.len,
                    period: h.entry.period,
                    at: Vec::new(),
                });
                groups.last_mut().unwrap()
            }
        };
        if !g.at.iter().any(|&s| s.abs_diff(h.symbol) <= 3) {
            g.at.push(h.symbol);
        }
    }
    // A group's count is the most occurrences that agree with any one of
    // them about the grid (the anchor itself included).
    let counted = groups.into_iter().map(|mut g| {
        let count = match rule {
            // Within one frame, a long transmission is a chain: each sync confirmed by the one before it.
            Confirm::WithinFrame if g.period > 0 => {
                g.at.sort_unstable();
                let mut best = usize::from(!g.at.is_empty());
                let mut chain = vec![1usize; g.at.len()];
                for j in 1..g.at.len() {
                    for i in (0..j).rev() {
                        if on_grid(g.at[i], g.at[j], g.period, g.protocol, rule) {
                            chain[j] = chain[j].max(chain[i] + 1);
                        }
                        if g.at[j] - g.at[i] > 2 * max_gap(g.protocol, g.period) as usize {
                            break;
                        }
                    }
                    best = best.max(chain[j]);
                }
                best
            }
            // No known grid: the syncs must make one of their own, evenly spaced at least a sync word apart (chance
            // matches in analog voice fall anywhere: 8 of 300 FM voice windows confirmed "ProVoice" without this).
            Confirm::WithinFrame => own_grid(&mut g.at, g.len as usize),
            _ => {
                g.at.iter()
                    .map(|&a| 1 + g.at.iter().filter(|&&b| b != a && on_grid(a, b, g.period, g.protocol, rule)).count())
                    .max()
                    .unwrap_or(0)
            }
        };
        (g, count)
    });
    // An entry whose frame grid is unknown (period 0: the unverified
    // ProVoice and X2-TDMA patterns) can't use the grid to reject chance
    // pairs, so it needs a third occurrence instead: on a 40 s window of
    // floor the ProVoice patterns matched twice.
    let (g, count) = counted
        .filter(|(g, count)| {
            *count
                >= if g.period > 0 {
                    2
                } else if rule == Confirm::WithinFrame {
                    6
                } else {
                    3
                }
        })
        .max_by_key(|(g, count)| (*count, g.len))?;
    let confidence = match count {
        2 => 0.8,
        3..=4 => 0.9,
        _ => 0.97,
    };
    Some(SyncResult { protocol: g.protocol, detail: g.detail, baud: g.baud, inverted: g.inverted, count, confidence })
}

fn boxcar(x: &[f32], n: usize) -> Vec<f32> {
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

fn interp(x: &[f32], t: f64) -> f32 {
    let i = t.floor() as usize;
    if i + 1 >= x.len() {
        return x[x.len() - 1];
    }
    let f = (t - i as f64) as f32;
    x[i] * (1.0 - f) + x[i + 1] * f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discriminator-like signal: symbols held for `sps` samples.
    fn disc_from_symbols(symbols: &[f32], sps: usize) -> Vec<f32> {
        symbols.iter().flat_map(|&s| std::iter::repeat_n(s, sps)).collect()
    }

    /// Symbols from sign bits (oldest first), outer levels only.
    fn sync_symbols(pattern: u32, len: u8) -> Vec<f32> {
        (0..len).rev().map(|b| if pattern >> b & 1 == 1 { -3.0 } else { 3.0 }).collect()
    }

    fn noise_symbols(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                [-3.0, -1.0, 1.0, 3.0][(x >> 60) as usize & 3]
            })
            .collect()
    }

    #[test]
    fn finds_dmr_bs_voice_twice() {
        let mut syms = noise_symbols(200, 1);
        for _ in 0..2 {
            syms.extend(sync_symbols(0x439B4D, 24));
            syms.extend(noise_symbols(120, 7));
        }
        let fs = 4800.0 * 10.0;
        let disc = disc_from_symbols(&syms, 10);
        let r = best(&search(&disc, fs, &table())).expect("hit");
        assert_eq!((r.protocol, r.detail, r.count), ("DMR", "BS voice", 2));
        assert!(r.confidence >= 0.8);
    }

    #[test]
    fn finds_inverted_p25_at_fractional_sps() {
        let mut syms = noise_symbols(100, 3);
        for _ in 0..3 {
            syms.extend(sync_symbols(!0x04CF5F & 0xFFFFFF, 24));
            syms.extend(noise_symbols(300, 9));
        }
        // 50 kHz / 4800 baud = 10.42 samples per symbol: resample by repeating.
        let fs = 50_000.0;
        let disc: Vec<f32> = (0..((syms.len() as f64) * fs / 4800.0) as usize).map(|i| syms[((i as f64) * 4800.0 / fs) as usize]).collect();
        let r = best(&search(&disc, fs, &table())).expect("hit");
        assert_eq!((r.protocol, r.inverted), ("P25", true));
        assert!(r.count >= 3);
    }

    #[test]
    fn finds_nxdn48_with_inner_symbols_at_masked_positions() {
        // FSW+LICH 0x2B0C8 over 18 symbols; mask 0x2FDFF clears bits 9 and 16,
        // which are ±1 symbols in the real word.
        let pat = 0x2B0C8u32;
        let sync: Vec<f32> = (0..18u32)
            .rev()
            .map(|b| {
                let neg = pat >> b & 1 == 1;
                let mag = if b == 9 || b == 16 { 1.0 } else { 3.0 };
                if neg { -mag } else { mag }
            })
            .collect();
        let mut syms = noise_symbols(100, 5);
        for i in 0..3 {
            syms.extend(&sync);
            syms.extend(noise_symbols(174, 11 + i));
        }
        let disc = disc_from_symbols(&syms, 10);
        let r = best(&search(&disc, 24_000.0, &table())).expect("hit");
        assert_eq!((r.protocol, r.detail), ("NXDN", "NXDN48"));
        assert!(r.count >= 3);
    }

    fn hit(entry: SyncEntry, symbol: usize) -> SyncHit {
        SyncHit { entry, inverted: false, phase: 5, symbol, errors: 0 }
    }

    /// Two syncs are only a confirmation on the protocol's grid (the
    /// review's case: DMR syncs 101 symbols apart were accepted at 0.8).
    #[test]
    fn syncs_off_the_frame_grid_dont_confirm() {
        let dmr = table().into_iter().find(|e| e.protocol == "DMR" && e.detail == "BS voice").unwrap();
        assert!(best(&[hit(dmr, 100), hit(dmr, 201)]).is_none());
        // One burst, six bursts (a voice superframe), with a symbol of slop.
        let r = best(&[hit(dmr, 100), hit(dmr, 244)]).expect("one burst apart");
        assert_eq!(r.count, 2);
        let r = best(&[hit(dmr, 100), hit(dmr, 100 + 6 * 144 + 1), hit(dmr, 100 + 12 * 144 - 2)]).expect("superframes apart");
        assert_eq!(r.count, 3);
        // Three on the grid plus a stray: the stray doesn't count.
        let r = best(&[hit(dmr, 100), hit(dmr, 157), hit(dmr, 388), hit(dmr, 1252)]).unwrap();
        assert_eq!(r.count, 3);
        // P25 Phase 1: a TDU (72 symbols) after an LDU (864) is on the grid, 101 is not.
        let p25 = table().into_iter().find(|e| e.protocol == "P25" && e.detail == "Phase 1").unwrap();
        assert_eq!(best(&[hit(p25, 0), hit(p25, 864), hit(p25, 936)]).unwrap().count, 3);
        assert!(best(&[hit(p25, 0), hit(p25, 101)]).is_none());
        // POCSAG batches, 544 bits: consecutive batches chain with a little clock error; a sync 20 batches on is
        // another stretch (DESIGN §7.1: within one frame + 3 %), so it doesn't add to the count.
        let pocsag = table().into_iter().find(|e| e.protocol == "POCSAG" && e.baud == 1200.0).unwrap();
        assert_eq!(best(&[hit(pocsag, 10), hit(pocsag, 555), hit(pocsag, 10 + 2 * 544 + 2)]).unwrap().count, 3);
        assert_eq!(best(&[hit(pocsag, 10), hit(pocsag, 554), hit(pocsag, 10 + 20 * 544 + 40)]).unwrap().count, 2);
        // P25 Phase 2 outbound: syncs 180 and 540 apart in turn (and 720 over a missed one) chain as one transmission.
        let p2 = table().into_iter().find(|e| e.protocol == "P25" && e.detail == "Phase 2").unwrap();
        assert_eq!(best(&[hit(p2, 100), hit(p2, 280), hit(p2, 820), hit(p2, 1000), hit(p2, 1720), hit(p2, 1900)]).unwrap().count, 6);
        // Under the old slack (0.5 % of the spacing), any two P25 syncs ~0.6 s apart were "on" its 36-symbol grid.
        assert!(best_with(&[hit(p25, 0), hit(p25, 3000)], Confirm::AnyPeriods).is_some());
        assert!(best(&[hit(p25, 0), hit(p25, 3000)]).is_none());
    }

    #[test]
    fn a_grid_of_their_own_confirms_an_entry_without_one_only_at_six() {
        let pv = table().into_iter().find(|e| e.protocol == "ProVoice").unwrap();
        let even: Vec<SyncHit> = (0..6).map(|k| hit(pv, 50 + k * 400)).collect();
        assert_eq!(best(&even).unwrap().count, 6);
        assert!(best(&even[..5]).is_none(), "five is not enough");
        let scattered: Vec<SyncHit> = [50, 333, 790, 1410, 2002, 2600, 3333].into_iter().map(|s| hit(pv, s)).collect();
        assert!(best(&scattered).is_none(), "seven at no common spacing");
    }

    #[test]
    fn a_sync_with_two_symbols_flipped_is_found_only_by_the_soft_match() {
        let p25 = table().into_iter().find(|e| e.protocol == "P25" && e.detail == "Phase 1").unwrap();
        // Two weak symbols on the wrong side of zero, as noise leaves them; the rest at ±3 with a little spread.
        let mut sync = sync_symbols(p25.pattern, 24);
        sync[5] *= -0.4;
        sync[17] *= -0.3;
        for (i, v) in sync.iter_mut().enumerate() {
            *v *= 1.0 + 0.1 * ((i % 3) as f32 - 1.0);
        }
        let mut syms = noise_symbols(100, 3);
        for _ in 0..3 {
            syms.extend(&sync);
            syms.extend(noise_symbols(336, 9));
        }
        let disc = disc_from_symbols(&syms, 10);
        let fs = 48_000.0;
        assert!(best(&search(&disc, fs, std::slice::from_ref(&p25))).is_none(), "the exact search takes no flipped word");
        let soft = SearchOpts { smooth: 0.0, soft: Some(0.8) };
        let r = best(&search_with(&disc, fs, std::slice::from_ref(&p25), soft)).expect("soft hits");
        assert_eq!((r.protocol, r.count), ("P25", 3));
        // The correlation itself: 1 for a clean word, −1 inverted, near 0 for random symbols.
        let vals = |w: &[f32]| -> [f32; 32] {
            let mut v = [0.0f32; 32];
            for (i, x) in w.iter().rev().enumerate() {
                v[i] = *x;
            }
            v
        };
        let clean = sync_symbols(p25.pattern, 24);
        assert!((soft_match(&p25, &vals(&clean)) - 1.0).abs() < 1e-6);
        assert!((soft_match(&p25, &vals(&clean.iter().map(|v| -v).collect::<Vec<_>>())) + 1.0).abs() < 1e-6);
        assert!(soft_match(&p25, &vals(&sync)) > 0.8);
        assert!(soft_match(&p25, &vals(&noise_symbols(24, 5))).abs() < 0.6);
    }

    #[test]
    fn random_symbols_dont_confirm_anything() {
        let syms = noise_symbols(20_000, 42);
        let disc = disc_from_symbols(&syms, 10);
        let r = best(&search(&disc, 48_000.0, &table()));
        assert!(r.as_ref().is_none_or(|r| r.count < 2), "{r:?}");
    }
}
