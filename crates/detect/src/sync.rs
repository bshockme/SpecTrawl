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
}

const fn e(protocol: &'static str, detail: &'static str, baud: f64, len: u8, pattern: u32, mask: u32, invertible: bool) -> SyncEntry {
    SyncEntry { protocol, detail, baud, len, pattern, mask, invertible, four_level: true }
}

const fn bin(protocol: &'static str, detail: &'static str, baud: f64, pattern: u32, invertible: bool) -> SyncEntry {
    SyncEntry { protocol, detail, baud, len: 32, pattern, mask: 0xFFFF_FFFF, invertible, four_level: false }
}

/// NXDN frame sync + LICH: the 18 valid values (mask drops ±1 symbols).
const NXDN_LICH: [u32; 18] = [
    0x2B0C8, 0x2B0E1, 0x2B0E5, 0x2B0C4, 0x2B08B, 0x2B087, 0x2B089, 0x2B085, 0x2B0A4, 0x2B04B, 0x2B047, 0x2B027, 0x2B049,
    0x2B045, 0x2B079, 0x2B075, 0x2B064, 0x2B025,
];

pub fn table() -> Vec<SyncEntry> {
    let mut v = vec![
        e("P25", "Phase 1", 4800.0, 24, 0x04CF5F, 0xFFFFFF, true),
        e("P25", "Phase 2", 6000.0, 20, 0x0121DF, 0xFFFFF, true),
        e("DMR", "MS voice", 4800.0, 24, 0x76286E, 0xFFFFFF, false),
        e("DMR", "BS voice", 4800.0, 24, 0x439B4D, 0xFFFFFF, false),
        e("DMR", "MS data", 4800.0, 24, 0x89D791, 0xFFFFFF, false),
        e("DMR", "BS data", 4800.0, 24, 0xBC64B2, 0xFFFFFF, false),
        e("DMR", "direct TS1 voice", 4800.0, 24, 0x21751F, 0xFFFFFF, false),
        e("DMR", "direct TS2 voice", 4800.0, 24, 0x6F8C23, 0xFFFFFF, false),
        e("DMR", "direct TS1 data", 4800.0, 24, 0xDE8AE0, 0xFFFFFF, false),
        e("DMR", "direct TS2 data", 4800.0, 24, 0x9073DC, 0xFFFFFF, false),
        e("YSF", "Fusion", 4800.0, 20, 0x84A52, 0xFFFFF, true),
        e("dPMR", "FS1", 2400.0, 24, 0x1F3485, 0xFFFFFF, false),
        e("dPMR", "FS1 (inv)", 2400.0, 24, 0xE0CB7A, 0xFFFFFF, false),
        e("X2-TDMA", "sync A (unverified)", 4800.0, 24, 0xA780FD, 0xFFFFFF, true),
        e("X2-TDMA", "sync B (unverified)", 4800.0, 24, 0xD41473, 0xFFFFFF, true),
        bin("ProVoice", "", 9600.0, 0x57125555, true),
        bin("ProVoice", "", 9600.0, 0x57123333, true),
        bin("ProVoice", "", 9600.0, 0x94D83523, true),
        bin("ProVoice", "", 9600.0, 0x57126969, true),
    ];
    // NXDN48 (2400 sym/s) and NXDN96 (4800 sym/s) share the sync.
    for &p in &NXDN_LICH {
        v.push(e("NXDN", "NXDN48", 2400.0, 18, p, 0x2FDFF, true));
        v.push(e("NXDN", "NXDN96", 4800.0, 18, p, 0x2FDFF, true));
    }
    // POCSAG batch sync codeword; binary FSK, so sign = bit.
    for baud in [512.0, 1200.0, 2400.0] {
        v.push(bin("POCSAG", "", baud, 0x7CD215D8, true));
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

/// Searches the FM discriminator output `disc` (sampled at `fs`, DC removed)
/// for every table entry. Returns all hits.
pub fn search(disc: &[f32], fs: f64, table: &[SyncEntry]) -> Vec<SyncHit> {
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
        let smoothed = boxcar(disc, ((fs / baud) * 0.5).round().max(1.0) as usize);
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
    /// Distinct sync occurrences (merged across neighbouring phases).
    pub count: usize,
    pub confidence: f32,
}

/// Picks the protocol with the most distinct sync occurrences. Hits within a
/// few symbols of each other (neighbouring phases seeing the same word) count
/// once. One occurrence of a long pattern is a weak result; two or more is
/// confirmation (a second sync within a frame period).
pub fn best(hits: &[SyncHit]) -> Option<SyncResult> {
    let mut groups: Vec<(&'static str, &'static str, f64, bool, Vec<usize>, u8)> = Vec::new();
    for h in hits {
        let key = (h.entry.protocol, h.entry.detail, h.entry.baud, h.inverted);
        let g = match groups.iter_mut().find(|g| (g.0, g.1, g.2, g.3) == key) {
            Some(g) => g,
            None => {
                groups.push((key.0, key.1, key.2, key.3, Vec::new(), h.entry.len));
                groups.last_mut().unwrap()
            }
        };
        if !g.4.iter().any(|&s| s.abs_diff(h.symbol) <= 3) {
            g.4.push(h.symbol);
        }
    }
    // A single occurrence is only trusted for amplitude-checked 4-level syncs;
    // binary syncs (tolerating bit errors, no level check) need two.
    let g = groups
        .into_iter()
        .filter(|g| g.4.len() >= 2 || table().iter().any(|e| e.protocol == g.0 && e.detail == g.1 && e.four_level))
        .max_by_key(|g| (g.4.len(), g.5))?;
    let count = g.4.len();
    let confidence = match (count, g.5) {
        (1, l) if l >= 24 => 0.6,
        (1, _) => 0.4,
        (2, _) => 0.85,
        _ => 0.97,
    };
    Some(SyncResult { protocol: g.0, detail: g.1, baud: g.2, inverted: g.3, count, confidence })
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
        assert!(r.confidence > 0.8);
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

    #[test]
    fn random_symbols_dont_confirm_anything() {
        let syms = noise_symbols(20_000, 42);
        let disc = disc_from_symbols(&syms, 10);
        let r = best(&search(&disc, 48_000.0, &table()));
        assert!(r.as_ref().is_none_or(|r| r.count < 2), "{r:?}");
    }
}
