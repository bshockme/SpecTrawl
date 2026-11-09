//! FLEX pager messages, straight from channel IQ (FLEX switches speed
//! mid-frame, so it doesn't fit the fixed-baud sync bank).
//!
//! A frame (1.875 s, 128 per 4-minute cycle) starts at 1600 baud 2FSK:
//! bit sync, then sync 1 — `A` (a 16-bit speed code and 0xA6C6), `B`
//! (0xAAAA), then `A` inverted — then the frame information word (cycle and
//! frame number). After 25 ms of sync 2 come 11 blocks of 160 ms at the
//! frame's speed: 1600 or 3200 baud, 2 or 4 levels. Every block carries
//! 256 bits for each of 1, 2 or 4 phases (A-D), interleaved as 8 words of
//! 32 bits. Words are BCH(31,21) with even parity — the POCSAG code — and
//! their 21 data bits are sent least significant first.
//!
//! Each phase is its own 88-word stream: a block information word, then
//! addresses, then one vector per address (message type, start, length),
//! then message words. Long addresses (capcodes above 2 million) take two
//! address words and two vector words.
//!
//! Protocol facts from the FLEX spec as implemented by PDW and multimon-ng;
//! the code is SpecTrawl's own.
//!
//! Reception notes, from off-air captures of a 3200/4 system heard under a
//! second co-channel FLEX transmitter: symbol timing comes from sync 2
//! (whole-symbol slips pass BCH, the code being cyclic, so data alone can't
//! set it); the data is read through a quarter-symbol filter (a full-symbol
//! one closes the four-level eye noticeably); and two-bit corrections are
//! only trusted in blocks where every word decodes, a random word passing
//! with two flips about one time in four.

use num_complex::Complex32;

use crate::features;
use crate::pocsag;

/// Sync 1 speed codes: (code, baud, levels).
const SPEEDS: [(u16, u32, u8); 5] = [(0x870C, 1600, 2), (0xB068, 1600, 4), (0x7B18, 3200, 2), (0xDEA0, 3200, 4), (0x4C7C, 3200, 4)];
const MARKER: u32 = 0xA6C6_AAAA;
/// Bit errors allowed over the 64 bits of sync 1 (the frame information
/// word, BCH-protected, confirms the frame). multimon-ng allows three in
/// the marker and three more between the speed code and its complement.
const SYNC1_ERRORS: u32 = 3;
const HEADER_BAUD: f64 = 1600.0;
const SYNC2_S: f64 = 0.025;
const BLOCKS: usize = 11;
const WORDS: usize = 88;
/// Length of the data symbol filter, in 3200-baud symbols.
const DATA_FILTER: f64 = 0.25;
/// A bit's reliability scales with the envelope (relative to its block's median) to this power. Under simulcast
/// fading the cube recovered the most pages without more wrong ones: with no envelope (the distance from the threshold
/// alone), soft repair barely helps; below FM's threshold the discriminator's clicks grow faster than the noise.
const ENVELOPE_POWER: i32 = 3;
/// A block's timing is within this many symbols of the previous block's.
const TIMING_STEP: f64 = 0.3;

/// One phase's 8 words of a block: corrected word and bits fixed, or None
/// past repair.
type BlockWords = [Option<(u32, u32)>; 8];
/// The highest FLEX capcode (long addresses top out here).
const MAX_CAPCODE: u64 = 4_297_068_542;

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub capcode: u64,
    /// Phase 'A'..'D'.
    pub phase: char,
    /// "alpha", "numeric", "tone", "binary", "instruction", "secure".
    pub kind: &'static str,
    pub text: String,
    /// Some words of the message failed BCH and are shown as '?'.
    pub damaged: bool,
    /// Not the whole message: more fragments follow in later frames.
    pub continued: bool,
    /// Alphanumeric fragment number: 3 for the first (or only) fragment,
    /// then 0, 1, 2 cycling for continuations (`flex_join` puts them back
    /// together). 3 for other kinds.
    pub fragment: u8,
    /// Alphanumeric message number from the header (6 bits), 0 otherwise.
    pub message_no: u8,
}

impl Page {
    /// "007723728 A alpha: text" (capcodes zero-padded to 9 digits, as
    /// pager decoders print them).
    pub fn summary(&self) -> String {
        let flags = format!("{}{}", if self.damaged { " [damaged]" } else { "" }, if self.continued { " [continued]" } else { "" });
        format!("{:09} {} {}{flags}: {}", self.capcode, self.phase, self.kind, self.text)
    }
}

/// A frame's length: 128 frames in a 4-minute cycle.
pub const FRAME_SECS: f64 = 240.0 / 128.0;

/// A frame's length after the end of sync 1 (where [`FlexFrame::at`] points): the frame information word (32 bits at
/// 1600 bit/s), sync 2 (25 ms) and 11 blocks of 160 ms.
pub const FRAME_AFTER_SYNC1_SECS: f64 = 0.020 + 0.025 + 11.0 * 0.160;

#[derive(Debug, Clone)]
pub struct FlexFrame {
    /// Sample index of the end of sync 1.
    pub at: usize,
    pub baud: u32,
    pub levels: u8,
    pub cycle: u8,
    pub frame: u8,
    /// Words that passed BCH (after correction), per block, over all phases.
    pub block_ok: [u8; BLOCKS],
    /// Symbol timing used per block, in symbols from nominal.
    pub block_off: [f32; BLOCKS],
    /// Words the soft repair made good, over all phases (not in `block_ok`).
    pub soft_words: u16,
    pub pages: Vec<Page>,
}

impl FlexFrame {
    pub fn mode(&self) -> String {
        format!("{}/{}", self.baud, self.levels)
    }
}

/// Linear interpolation of `d` at fractional index `t`.
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

/// The 21 data bits of a received word (packed first bit = MSB), LSB first.
fn data21(cw: u32) -> u32 {
    (cw >> 11).reverse_bits() >> 11
}

/// Discriminator, centred and smoothed over `frac` of a 3200-baud symbol.
fn discriminator(x: &[Complex32], fs: f64, frac: f64) -> Vec<f32> {
    let d = features::discriminator(x, fs);
    if d.is_empty() {
        return d;
    }
    let mut s = d.clone();
    s.sort_by(f32::total_cmp);
    let mid = s[s.len() / 2];
    let n = ((fs / 3200.0 * frac).round() as usize).max(1);
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

/// Sync 1 hits: (sample index of the last bit of `A` inverted, speed index,
/// inverted), at most one per 4 header bits.
fn find_sync1(d: &[f32], fs: f64) -> Vec<(f64, usize, bool)> {
    let sps = fs / HEADER_BAUD;
    let mut hits: Vec<(f64, usize, bool)> = Vec::new();
    for p in 0..8 {
        let mut t = sps * p as f64 / 8.0;
        let mut w = 0u64;
        let mut n = 0;
        while t < (d.len() - 1) as f64 {
            // Header bits: a 1 is negative deviation.
            w = w << 1 | (at(d, t) < 0.0) as u64;
            n += 1;
            if n >= 64 {
                for inv in [false, true] {
                    let v = if inv { !w } else { w };
                    // The speed whose full 64-bit pattern is nearest, if near enough.
                    let best = SPEEDS
                        .iter()
                        .enumerate()
                        .map(|(s, (code, _, _))| ((v ^ ((*code as u64) << 48 | (MARKER as u64) << 16 | (!code) as u64)).count_ones(), s))
                        .min()
                        .filter(|(e, _)| *e <= SYNC1_ERRORS);
                    if let Some((_, s)) = best {
                        hits.push((t, s, inv));
                    }
                }
            }
            t += sps;
        }
    }
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    // The sampling phases that matched span the bit: their mean is its centre.
    let mut groups: Vec<(f64, usize, bool, f64, usize)> = Vec::new();
    for h in hits {
        match groups.last_mut() {
            Some(g) if h.0 - g.0 < sps * 4.0 => {
                g.3 += h.0;
                g.4 += 1;
            }
            _ => groups.push((h.0, h.1, h.2, h.0, 1)),
        }
    }
    groups.into_iter().map(|g| (g.3 / g.4 as f64, g.1, g.2)).collect()
}

/// Zero deviation at a frame, from its sync 1 (speed `code`, last bit at `end`): midway between the medians of the
/// bits sent positive and those sent negative.
fn sync1_centre(d: &[f32], end: f64, sps: f64, code: u16, inv: bool) -> f32 {
    let pattern = (code as u64) << 48 | (MARKER as u64) << 16 | (!code) as u64;
    let (mut pos, mut neg) = (Vec::with_capacity(64), Vec::with_capacity(64));
    for k in 0..64 {
        let v = at(d, end - (63 - k) as f64 * sps);
        // A header 1 is negative deviation.
        if (pattern >> (63 - k) & 1 == 1) != inv { neg.push(v) } else { pos.push(v) }
    }
    let median = |v: &mut Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    (median(&mut pos) + median(&mut neg)) / 2.0
}

/// Frame information word: (cycle, frame) if it checks out.
fn fiw(d: &[f32], fs: f64, end: f64, inv: bool) -> Option<(u8, u8)> {
    let sps = fs / HEADER_BAUD;
    // After `A` inverted: 0xA6C6 inverted (16 bits), then the FIW. Its
    // bits, like the data's, are 1 for positive deviation — the opposite
    // of how sync 1's patterns are written.
    let raw = (0..32).fold(0u32, |a, k| a << 1 | ((at(d, end + (17 + k) as f64 * sps) > 0.0) != inv) as u32);
    let (cw, _) = pocsag::correct(raw)?;
    let x = data21(cw);
    // The checksum makes the nibbles (and the top bit) sum to 0xF.
    let sum = (0..6).map(|k| x >> (4 * k) & 0xF).sum::<u32>();
    (sum & 0xF == 0xF).then_some(((x >> 4 & 0xF) as u8, (x >> 8 & 0x7F) as u8))
}

/// Sync 2 at 3200 baud, 4 levels, in symbols: a comma (the outer levels
/// alternating, 32 symbols), an 8-symbol word, the comma inverted and a
/// second 8-symbol word. `+`/`-` are the outer levels, `p`/`n` the inner.
/// Taken from off-air frames (it is the same in every clean one; the data
/// grid lines up with it to a tenth of a symbol).
const SYNC2_3200_4: &[u8; 80] = b"+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-p+pn+-n--+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-n-+np+p";

/// Symbol timing from sync 2, in symbols from nominal: the offset within
/// ±`span` symbols where the discriminator best matches the pattern
/// (`start` is the nominal first sync 2 symbol, `s` samples per symbol).
/// The comma pins the timing within a symbol; the two words break the
/// comma's two-symbol period, so whole-symbol slips are told apart too —
/// something the data can't do, the BCH code being cyclic.
/// None when the match is too poor to trust (a collision, or a fade).
fn sync2_timing(d: &[f32], start: f64, s: f64, inv: bool, level: f32, span: f64) -> Option<f64> {
    let tmpl: Vec<f32> = SYNC2_3200_4
        .iter()
        .map(|&c| match c {
            b'+' => 3.0,
            b'p' => 1.0,
            b'n' => -1.0,
            _ => -3.0,
        })
        .map(|v| if inv { -v } else { v })
        .collect();
    let steps = (span / 0.05).round() as i32;
    let mut best = (f32::MIN, 0.0);
    for k in -steps..=steps {
        let off = k as f64 * 0.05;
        // The pattern has zero mean, so a carrier offset adds nothing.
        let c: f32 = tmpl.iter().enumerate().map(|(i, &v)| v * at(d, start + (i as f64 + 0.5 + off) * s)).sum();
        if c > best.0 {
            best = (c, off);
        }
    }
    // Ideal: Σ v² · level / 3.
    let ideal = tmpl.iter().map(|v| v * v).sum::<f32>() * level / 3.0;
    (best.0 > 0.5 * ideal).then_some(best.1)
}

/// Slices one block into each phase's 256 bits.
#[allow(clippy::too_many_arguments)]
fn slice_block(
    d: &[f32],
    env: &[f32],
    t0: f64,
    s: f64,
    n: usize,
    levels: u8,
    inv: bool,
    header_centre: f32,
    header_level: f32,
    thr_ratio: f32,
) -> Vec<(Vec<u8>, Vec<f32>)> {
    let mut v: Vec<f32> = (0..n).map(|k| at(d, t0 + (k as f64 + 0.5) * s)).collect();
    // The carrier drifts a few hundred Hz between blocks, and idle fill
    // makes the symbol mix lopsided (so no percentile midpoint): estimate
    // the offset and outer level decision-directed, from each symbol's
    // distance to its nearest ideal level.
    // Start from the header's centre and level: idle fill can put nearly
    // every symbol on the inner levels, which would pass for outer ones.
    let mut outer = header_level;
    let mut dc = header_centre;
    for _ in 0..4 {
        let (mut resid, mut out_sum, mut out_n) = (0.0, 0.0, 0);
        for &x in &v {
            let u = x - dc;
            let level = if levels == 4 && u.abs() < outer * 2.0 / 3.0 { outer / 3.0 } else { outer };
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
    for x in v.iter_mut() {
        *x = if inv { dc - *x } else { *x - dc };
    }
    // The envelope at each symbol, relative to the block's median.
    let e: Vec<f32> = (0..n).map(|k| at(env, t0 + (k as f64 + 0.5) * s)).collect();
    let mut sorted = e.clone();
    sorted.sort_by(f32::total_cmp);
    let usual = sorted[n / 2].max(1e-12);
    let e: Vec<f32> = e.iter().map(|&e| (e / usual).min(1.0)).collect();
    let thr = outer * thr_ratio;
    // Data bits: bit a is the sign (positive = 1); with 4 levels, bit b is
    // set on the inner levels. Each bit's reliability: its distance from its threshold, in units of a clean level's distance (1 on a clean
    // level), scaled by the envelope there to `ENVELOPE_POWER`. In a fade (a simulcast beat's null) the discriminator
    // jumps, so a bit there can sit far from its threshold and still be a guess.
    let (ra, rb) = if levels == 4 { (3.0 / outer, 3.0 / outer) } else { (1.0 / outer, 0.0) };
    let bits: Vec<(u8, u8, f32, f32)> = v
        .iter()
        .zip(&e)
        .map(|(&x, &e)| {
            let e = e.powi(ENVELOPE_POWER);
            ((x > 0.0) as u8, (x.abs() < thr) as u8, x.abs() * ra * e, (x.abs() - thr).abs() * rb * e)
        })
        .collect();
    let a = |b: &(u8, u8, f32, f32)| (b.0, b.2);
    let b = |b: &(u8, u8, f32, f32)| (b.1, b.3);
    let take = |it: &mut dyn Iterator<Item = (u8, f32)>| -> (Vec<u8>, Vec<f32>) { it.unzip() };
    match (n, levels) {
        (256, 2) => vec![take(&mut bits.iter().map(a))],
        (256, _) => vec![take(&mut bits.iter().map(a)), take(&mut bits.iter().map(b))],
        (_, 2) => vec![take(&mut bits.iter().step_by(2).map(a)), take(&mut bits.iter().skip(1).step_by(2).map(a))],
        _ => vec![
            take(&mut bits.iter().step_by(2).map(a)),
            take(&mut bits.iter().step_by(2).map(b)),
            take(&mut bits.iter().skip(1).step_by(2).map(a)),
            take(&mut bits.iter().skip(1).step_by(2).map(b)),
        ],
    }
}

/// The envelope, smoothed like the data. A block's reliabilities divide it by the block's median.
fn envelope(x: &[Complex32], fs: f64, frac: f64) -> Vec<f32> {
    let n = ((fs / 3200.0 * frac).round() as usize).max(1);
    let a: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let mut out = vec![0.0; a.len()];
    let mut acc = 0.0f32;
    for i in 0..a.len() + n / 2 {
        if i < a.len() {
            acc += a[i];
        }
        if i >= n {
            acc -= a[i - n];
        }
        if i >= n / 2 && i - n / 2 < a.len() {
            out[i - n / 2] = acc / n as f32;
        }
    }
    out
}

/// De-interleaves a phase's 256 bit reliabilities as [`block_words`] does its bits.
fn block_rels(rel: &[f32]) -> [[f32; 32]; 8] {
    std::array::from_fn(|w| std::array::from_fn(|b| rel[b * 8 + w]))
}

/// De-interleaves a phase's 256 block bits into 8 words: bit `b` of word
/// `w` is the block's bit `b * 8 + w`.
fn block_words(bits: &[u8]) -> [u32; 8] {
    let mut out = [0u32; 8];
    for (w, o) in out.iter_mut().enumerate() {
        *o = (0..32).fold(0u32, |a, b| a << 1 | bits[b * 8 + w] as u32);
    }
    out
}

/// Phase names for a speed: 1600/2 → A; 1600/4 → A, B; 3200/2 → A, C;
/// 3200/4 → A, B, C, D.
fn phase_names(baud: u32, levels: u8) -> &'static [char] {
    match (baud, levels) {
        (1600, 2) => &['A'],
        (1600, _) => &['A', 'B'],
        (_, 2) => &['A', 'C'],
        _ => &['A', 'B', 'C', 'D'],
    }
}

/// A frame header: sync 1 and a valid frame information word.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    /// Sample index of the end of sync 1.
    pub at: usize,
    pub baud: u32,
    pub levels: u8,
    pub cycle: u8,
    pub frame: u8,
}

impl Header {
    pub fn mode(&self) -> String {
        format!("{}/{}", self.baud, self.levels)
    }
}

/// Frame headers only: enough to identify FLEX from ~0.1 s of signal.
pub fn headers(x: &[Complex32], fs: f64) -> Vec<Header> {
    if (x.len() as f64) < fs * 0.1 {
        return Vec::new();
    }
    let d = discriminator(x, fs, 1.0);
    find_sync1(&d, fs)
        .into_iter()
        .filter_map(|(end, speed, inv)| {
            let (cycle, frame) = fiw(&d, fs, end, inv)?;
            let (_, baud, levels) = SPEEDS[speed];
            Some(Header { at: end as usize, baud, levels, cycle, frame })
        })
        .collect()
}

/// Decodes every complete FLEX frame in a channel (complex baseband centred
/// on the FLEX carrier, at least ~16 kHz sample rate). Frames cut off by
/// the end of `x` are left out.
pub fn decode(x: &[Complex32], fs: f64) -> Vec<FlexFrame> {
    decode_with(x, fs, true)
}

/// [`decode`], with the soft repair on or off (off: hard decisions alone, for comparing them).
fn decode_with(x: &[Complex32], fs: f64, soft: bool) -> Vec<FlexFrame> {
    if (x.len() as f64) < fs * 0.1 {
        return Vec::new();
    }
    // The 1600-baud header is read through a one-symbol filter; the data
    // through a shorter one, which keeps the four levels further apart.
    let dh = discriminator(x, fs, 1.0);
    // The data filter is a fraction of the symbol: twice as long at 1600 baud.
    let d32 = discriminator(x, fs, DATA_FILTER);
    let mut d16: Option<Vec<f32>> = None;
    // Longer filters, made when a weak block first asks for them, keyed by
    // length in 3200-baud symbols.
    let mut longer: Vec<(f64, Vec<f32>)> = Vec::new();
    let env = envelope(x, fs, 1.0);
    let sps = fs / HEADER_BAUD;
    let mut out = Vec::new();
    for (end, speed, inv) in find_sync1(&dh, fs) {
        let Some((cycle, frame)) = fiw(&dh, fs, end, inv) else { continue };
        let (_, baud, levels) = SPEEDS[speed];
        let d: &[f32] = if baud == 1600 { d16.get_or_insert_with(|| discriminator(x, fs, DATA_FILTER * 2.0)) } else { &d32 };
        let s = fs / baud as f64;
        let n = (baud as f64 * 0.16).round() as usize;
        // First data symbol: after the FIW and sync 2.
        let t0 = end + 48.5 * sps + SYNC2_S * fs;
        // The blocks the buffer holds, with two symbols to spare past each and room for its timing search (three
        // symbols in block 0, under one after): past the buffer's end `at` repeats the last sample, which slices as a
        // constant word, and the all-zero word is a valid codeword. A transmitter may stop once the frame's last used
        // block is sent (on air at 929.6125 MHz many 3200/4 transmissions are 0.42 s, a header and two blocks), so a
        // slice of one holds no more. The rest are missing, not failed: no words and nothing for the soft repair.
        let fits = |b: usize| t0 + (((b + 1) * n) as f64 + 2.0 + if b == 0 { 3.0 } else { 1.0 }) * s <= (d.len() - 1) as f64;
        let present = (0..BLOCKS).take_while(|&b| fits(b)).count();
        if present == 0 {
            continue;
        }
        let nph = phase_names(baud, levels).len();
        // The frame's centre, from sync 1, whose bits are known: midway between its positive and its negative bits
        // (medians, as a simulcast beat's nulls throw the discriminator about). The buffer's median, which
        // `discriminator` takes off, is not the centre: idle fill and the four levels' mix put it kilohertz off, which
        // in slices from 931.3375 MHz sliced every data symbol of a frame to the wrong level while its header read.
        let centre = sync1_centre(&dh, end, sps, SPEEDS[speed].0, inv);
        // Full deviation, from the 1600-baud header after sync 1's start (two levels: an offset short of the deviation
        // leaves the mean distance from zero as it is).
        let level = (0..112).map(|k| at(&dh, end - (63 - k) as f64 * sps).abs()).sum::<f32>() / 112.0;
        let mut phases: Vec<Vec<Option<u32>>> = vec![Vec::with_capacity(WORDS); nph];
        // Every word as sliced, with its bits' reliabilities, for the soft repair once the frame is read.
        let mut sliced: Vec<Vec<(u32, [f32; 32])>> = vec![Vec::with_capacity(WORDS); nph];
        let mut block_ok = [0u8; BLOCKS];
        let mut block_off = [0f32; BLOCKS];
        // Symbol timing (in symbols from nominal). The header places it
        // within about half a symbol; sync 2 pins it to a tenth, and
        // clocks hold it over the frame. Each block still looks half a
        // symbol either side: under a co-channel signal the best sampling
        // point moves about that much from block to block. Without a lock
        // (other speeds, whose sync 2 isn't known here) the first block
        // searches wide for the data start and later ones follow the
        // best-decoding block so far: idle fill decodes equally well at
        // several offsets and would let a tracking loop wander.
        let locked = if (baud, levels) == (3200, 4) { sync2_timing(d, end + 48.5 * sps, s, inv, level, 4.0) } else { None };
        let mut anchor = (locked.unwrap_or(0.0), 0usize);
        let mut previous: Option<f64> = None;
        for (blk, ok_out) in block_ok.iter_mut().enumerate() {
            if blk >= present {
                for p in &mut phases {
                    p.extend([None; 8]);
                }
                continue;
            }
            // Timing: the offset whose words need the fewest corrections.
            let span = if locked.is_some() {
                5
            } else if blk == 0 {
                30
            } else {
                5
            };
            // Slices the block at `off` through `d` with threshold `thr`:
            // (corrections, words needing at most one, words, the words as sliced).
            let slice = |d: &[f32], off: f64, thr: f32| {
                let mut cost = 0;
                let mut good = 0;
                let cut = slice_block(d, &env, t0 + ((blk * n) as f64 + off) * s, s, n, levels, inv, centre, level, thr);
                let words: Vec<BlockWords> = cut
                    .iter()
                    .map(|(b, _)| {
                        block_words(b).map(|w| {
                            let c = pocsag::correct(w);
                            // Corrected bits, or 9 for a word past repair.
                            cost += c.map_or(9, |c| c.1);
                            good += c.is_some_and(|c| c.1 <= 1) as usize;
                            c
                        })
                    })
                    .collect();
                let raw: Vec<[(u32, [f32; 32]); 8]> = cut
                    .iter()
                    .map(|(b, r)| {
                        let (w, r) = (block_words(b), block_rels(r));
                        std::array::from_fn(|i| (w[i], r[i]))
                    })
                    .collect();
                (cost, good, words, raw)
            };
            // Every offset, nearest first, so ties (idle fill decodes at several) keep the timing where it is.
            let tried: Vec<_> = (0..=2 * span)
                .map(|k| if k % 2 == 0 { k / 2 } else { -(k + 1) / 2 })
                .map(|o| {
                    let off = anchor.0 + o as f64 * 0.1;
                    let (cost, _, words, raw) = slice(d, off, 2.0 / 3.0);
                    (cost, off, words, raw)
                })
                .collect();
            // The fewest corrections, among offsets near the last block's: a symbol from it is the neighbouring
            // symbol, and at 3200 baud another phase's, whose idle fill decodes without a correction while the right
            // timing's data may need some for a fade.
            let near = |off: f64| previous.is_none_or(|p| (off - p).abs() <= TIMING_STEP + 1e-9);
            let (_, mut off, mut words, mut raw) =
                tried.into_iter().filter(|t| near(t.1)).reduce(|a, b| if b.0 < a.0 { b } else { a }).expect("the nearest offset is near");
            let mut ok = words.iter().flatten().filter(|w| w.is_some()).count();
            if ok < nph * 8 {
                // A weak block (a fade, noise): the short filter and the
                // nominal thresholds are not always the best read of it.
                // Try longer filters and other thresholds around the chosen
                // timing; keep what gives the most words within one
                // correction (the only ones a weak block keeps).
                let mut good = words.iter().flatten().filter(|w| w.is_some_and(|c| c.1 <= 1)).count();
                let base = off;
                let sym32 = 3200.0 / baud as f64;
                for frac in [DATA_FILTER * sym32, DATA_FILTER * 2.0 * sym32, DATA_FILTER * 3.0 * sym32] {
                    if !longer.iter().any(|(f, _)| *f == frac) {
                        longer.push((frac, discriminator(x, fs, frac)));
                    }
                    let dl = &longer.iter().find(|(f, _)| *f == frac).unwrap().1;
                    for thr in [0.55f32, 2.0 / 3.0, 0.8] {
                        for o in -3..=3 {
                            let (_, g, w, r) = slice(dl, base + o as f64 * 0.1, thr);
                            if g > good {
                                good = g;
                                words = w;
                                raw = r;
                                off = base + o as f64 * 0.1;
                            }
                        }
                    }
                }
                ok = words.iter().flatten().filter(|w| w.is_some()).count();
            }
            *ok_out = ok as u8;
            block_off[blk] = off as f32;
            previous = Some(off);
            if locked.is_none() && ok > anchor.1 {
                anchor = (off, ok);
            }
            // A block with a word past repair has more wrong with it than
            // the odd bit error (a collision, a fade): 2-bit corrections
            // there are more often wrong than right (a random word passes
            // with two about a quarter of the time), so keep only words
            // that needed at most one.
            let weak = ok < nph * 8;
            for (p, w) in phases.iter_mut().zip(&words) {
                p.extend(w.iter().map(|w| w.filter(|c| !weak || c.1 <= 1).map(|c| data21(c.0))));
            }
            for (p, r) in sliced.iter_mut().zip(raw) {
                p.extend(r);
            }
        }
        let mut soft_words = 0;
        for (ph, raw) in phases.iter_mut().zip(&sliced).filter(|_| soft) {
            soft_words += soft_repair(ph, raw);
        }
        let mut pages = Vec::new();
        for (ph, name) in phases.iter().zip(phase_names(baud, levels)) {
            pages.extend(phase_pages(ph, *name));
        }
        out.push(FlexFrame { at: end as usize, baud, levels, cycle, frame, block_ok, block_off, soft_words, pages });
    }
    out
}

/// Soft repair: only bits less reliable than this may be overruled ([`pocsag::chase`]). The simulcast tests measured
/// it: bolder, and miscorrected text outnumbers what more repairs recover.
const DOUBT: f32 = 0.6;

/// Repairs, in one phase's words, those hard correction could not (or, in a weak block, would not) trust, from the
/// bits' reliabilities. Returns the words repaired.
///
/// Never the address words: a soft-repaired address put the right text under a capcode nobody sent (18 of 100 pages
/// in the simulcast tests at 4 dB), and a wrong address is a page that was never sent. A page starts only from an
/// address that decoded hard; the repair completes its vector and text. The block information word and the vectors
/// are repaired too: each has a checksum besides.
fn soft_repair(dw: &mut [Option<u32>], raw: &[(u32, [f32; 32])]) -> u16 {
    let mut repaired = 0;
    let mut fix = |i: usize, dw: &mut [Option<u32>]| {
        if dw[i].is_none()
            && let Some((cw, _)) = raw.get(i).and_then(|(w, r)| pocsag::chase(*w, r, DOUBT))
        {
            dw[i] = Some(data21(cw));
            repaired += 1;
        }
    };
    fix(0, dw);
    // The address words, from the block information word (every word may be one when it failed).
    let (aoff, voff) = match dw[0].filter(|&b| checks(b)) {
        Some(biw) => (((biw >> 8 & 3) + 1) as usize, (biw >> 10 & 0x3F) as usize),
        None => (1, dw.len()),
    };
    for i in 1..dw.len() {
        if !(aoff..voff).contains(&i) {
            fix(i, dw);
        }
    }
    repaired
}

/// The 4-bit checksum of a block information or vector word (as of the frame information word): its six nibbles
/// (21 data bits, the top one alone) sum to 0xF. Every alphanumeric and numeric vector heard on air checks.
fn checks(w: u32) -> bool {
    (0..6).map(|k| w >> (4 * k) & 0xF).sum::<u32>() & 0xF == 0xF
}

/// A message word, or None for one that failed, or that is idle fill: no message holds one, so an idle word where a
/// message's should be was read from another phase (a whole-symbol slip, which BCH can't see).
fn message(w: Option<u32>) -> Option<u32> {
    w.filter(|&w| w != 0x1F_FFFF)
}

fn is_long(aw: u32) -> bool {
    aw < 0x8001 || (aw > 0x1E_0000 && aw < 0x1F_0001) || aw > 0x1F_7FFE
}

/// The pages in one phase's 88 data words (None: failed BCH).
fn phase_pages(dw: &[Option<u32>], phase: char) -> Vec<Page> {
    let mut out = Vec::new();
    let Some(biw) = dw[0] else { return out };
    // Idle, or a block info word whose checksum (as the FIW's: nibbles
    // sum to 0xF) fails.
    if biw == 0 || biw == 0x1F_FFFF || !checks(biw) {
        return out;
    }
    let aoff = ((biw >> 8 & 3) + 1) as usize;
    let voff = (biw >> 10 & 0x3F) as usize;
    let mut i = aoff;
    while i < voff {
        let vi = voff + i - aoff;
        // A vector whose checksum fails is not the one sent (a word miscorrected past it), and would read the
        // wrong words as the page.
        let (Some(aw), Some(&Some(vec))) = (dw[i], dw.get(vi).filter(|v| v.is_some_and(checks))) else {
            i += 1;
            continue;
        };
        let long = is_long(aw);
        let capcode = if long {
            let Some(Some(aw2)) = dw.get(i + 1) else {
                i += 2;
                continue;
            };
            (((aw2 ^ 0x1F_FFFF) as u64) << 15) + 2_068_480 + aw as u64
        } else {
            (aw - 0x8000) as u64
        };
        // Long addresses carry the message header in their second vector
        // word; short ones in the message's first word.
        let header = if long { dw.get(vi + 1).copied().flatten() } else { None };
        let kind_bits = vec >> 4 & 7;
        let mw1 = (vec >> 7 & 0x7F) as usize;
        let len = (vec >> 14 & 0x7F) as usize;
        let (kind, text, damaged, continued, fragment, message_no) = match kind_bits {
            5 => alpha(dw, mw1, len, header, long),
            3 | 4 | 7 => {
                // Numeric vectors carry a 3-bit length (words beyond the first).
                let (t, dmg) = numeric(dw, mw1, (vec >> 14 & 7) as usize, header, long, kind_bits == 7);
                ("numeric", t, dmg, false, 3, 0)
            }
            2 => ("tone", String::new(), false, false, 3, 0),
            6 => ("binary", String::new(), false, false, 3, 0),
            1 => ("instruction", String::new(), false, false, 3, 0),
            _ => ("secure", String::new(), false, false, 3, 0),
        };
        if capcode <= MAX_CAPCODE {
            out.push(Page { capcode, phase, kind, text, damaged, continued, fragment, message_no });
        }
        i += if long { 2 } else { 1 };
    }
    out
}

/// Alphanumeric text: three 7-bit characters per word (bits 0-6, 7-13,
/// 14-20). The message header (fragment number, continuation) is the first
/// message word for short addresses; for long ones it's the second vector
/// word and the message starts at `mw1`. Either way the vector's length
/// counts the header word. A first fragment (fragment 3) opens with a
/// signature character, not text; continuations start with text at once
/// (as multimon-ng reads them, and as seen on air).
fn alpha(dw: &[Option<u32>], mw1: usize, len: usize, header: Option<u32>, long: bool) -> (&'static str, String, bool, bool, u8, u8) {
    let (header, first) = if long { (header, mw1) } else { (dw.get(mw1).copied().flatten(), mw1 + 1) };
    let header = message(header);
    // Without a header the fragment number is unknown; 3 (a whole message)
    // keeps it out of the joiner, and `damaged` says it was guessed.
    let frag = header.map_or(3, |h| h >> 11 & 3);
    let continued = header.is_some_and(|h| h >> 10 & 1 == 1);
    let message_no = header.map_or(0, |h| (h >> 4 & 0x3F) as u8);
    let last = (mw1 + len - usize::from(long)).min(dw.len());
    let mut text = String::new();
    let mut damaged = header.is_none();
    for (k, w) in dw.iter().enumerate().take(last).skip(first) {
        let Some(w) = message(*w) else {
            text.push_str("???");
            damaged = true;
            continue;
        };
        for sh in [0, 7, 14] {
            if k == first && sh == 0 && frag == 3 {
                continue;
            }
            match (w >> sh & 0x7F) as u8 {
                0x03 | 0 => {}
                c @ (0x20..=0x7E | b'\n') => text.push(c as char),
                b'\r' => text.push('\n'),
                _ => text.push('·'),
            }
        }
    }
    ("alpha", text.trim_end().to_string(), damaged, continued, frag as u8, message_no)
}

/// Numeric digits: 4 bits each, LSB first, packed across words. The first
/// 2 bits are a checksum (10 bits of checksum and message number for
/// numbered numeric). The message is `len` + 1 words: for a short address
/// from `mw1`; for a long one the second vector word (`header`) then `len`
/// words from `mw1` (as multimon-ng reads them). Fill digits (0xC) show as
/// spaces: on air they separate the groups of a phone number.
fn numeric(dw: &[Option<u32>], mw1: usize, len: usize, header: Option<u32>, long: bool, numbered: bool) -> (String, bool) {
    const DIGITS: &[u8; 16] = b"0123456789 U -][";
    let mut words: Vec<Option<u32>> = Vec::new();
    let from = if long {
        words.push(header);
        mw1
    } else {
        words.push(dw.get(mw1).copied().flatten());
        mw1 + 1
    };
    words.extend(dw.iter().take((from + len).min(dw.len())).skip(from));
    words.iter_mut().for_each(|w| *w = message(*w));
    let damaged = words.iter().any(|w| w.is_none());
    let bits: Vec<u8> = words.iter().flat_map(|w| (0..21).map(move |b| w.map_or(0, |w| (w >> b & 1) as u8))).collect();
    let skip = if numbered { 10 } else { 2 };
    let text: String = bits[skip.min(bits.len())..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| DIGITS[(c[0] | c[1] << 1 | c[2] << 2 | c[3] << 3) as usize] as char)
        .collect();
    (text.trim().to_string(), damaged)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A codeword for 21 data bits given LSB first.
    pub(super) fn word(x: u32) -> u32 {
        pocsag::encode(x.reverse_bits() >> 11)
    }

    /// A block information or vector word (its low nibble zero) with its checksum.
    pub(super) fn checked(x: u32) -> u32 {
        word(x | (0xF - ((0..6).map(|k| x >> (4 * k) & 0xF).sum::<u32>() & 0xF)))
    }

    /// FM-modulates a FLEX 3200/4 frame with `phase_a` as phase A's words
    /// (other phases idle) at `fs`, with a carrier offset.
    pub(super) fn modulate(phase_a: &[u32], cycle: u32, frame: u32, fs: f64, offset_hz: f64) -> Vec<Complex32> {
        let mut freq: Vec<f64> = Vec::new();
        // An absolute timeline, so symbol lengths don't round cumulatively.
        let mut clock = 0.0f64;
        let mut push = |f: &mut Vec<f64>, dev: f64, dur_s: f64| {
            clock += dur_s;
            f.resize((clock * fs).round() as usize, dev);
        };
        // A little carrier, then 1600-baud header: bit sync, sync 1, FIW.
        push(&mut freq, 0.0, 0.05);
        let mut hdr: Vec<u8> = (0..32).map(|k| (k % 2) as u8).collect();
        let fiw = {
            let x = cycle << 4 | frame << 8;
            let sum = (0..6).map(|k| x >> (4 * k) & 0xF).sum::<u32>();
            word(x | (0xF - (sum & 0xF)) & 0xF)
        };
        // (Header bits go out as 1 = negative; the FIW is 1 = positive.)
        for (v, nb) in [(0xDEA0u32, 16), (0xA6C6, 16), (0xAAAA, 16), (!0xDEA0u32 & 0xFFFF, 16), (!0xA6C6u32 & 0xFFFF, 16), (!fiw, 32)] {
            hdr.extend((0..nb).rev().map(|i| (v >> i & 1) as u8));
        }
        for b in hdr {
            push(&mut freq, if b == 1 { -4800.0 } else { 4800.0 }, 1.0 / 1600.0);
        }
        // Sync 2, as 3200/4 transmitters send it.
        for &c in SYNC2_3200_4 {
            let dev = match c {
                b'+' => 4800.0,
                b'p' => 1600.0,
                b'n' => -1600.0,
                _ => -4800.0,
            };
            push(&mut freq, dev, 1.0 / 3200.0);
        }
        // 11 blocks, 4 phases interleaved.
        let idle = word(0x1F_FFFF);
        for blk in 0..BLOCKS {
            let w = |p: usize, k: usize| if p == 0 { phase_a.get(blk * 8 + k).copied().unwrap_or(idle) } else { idle };
            let bit = |p: usize, j: usize| (w(p, j % 8) >> (31 - j / 8) & 1) as u8;
            for k in 0..512 {
                let (pa, pb) = if k % 2 == 0 { (0, 1) } else { (2, 3) };
                let (a, b) = (bit(pa, k / 2), bit(pb, k / 2));
                let dev = match (a, b) {
                    (1, 0) => 4800.0,
                    (1, 1) => 1600.0,
                    (0, 1) => -1600.0,
                    _ => -4800.0,
                };
                push(&mut freq, dev, 1.0 / 3200.0);
            }
        }
        push(&mut freq, 0.0, 0.05);
        // Smooth the steps (a transmitter's pre-modulation filter), then FM.
        let m = (fs / 3200.0 / 3.0).round() as usize;
        let mut ph = 0.0f64;
        let mut acc = 0.0;
        let mut rng = 0x1234_5678u32;
        let mut noise = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
        };
        (0..freq.len())
            .map(|i| {
                acc += freq[i] - if i >= m { freq[i - m] } else { 0.0 };
                ph += 2.0 * std::f64::consts::PI * (acc / m as f64 + offset_hz) / fs;
                Complex32::new(ph.cos() as f32 + noise(), ph.sin() as f32 + noise())
            })
            .collect()
    }

    pub(super) fn alpha_words(text: &str) -> Vec<u32> {
        let mut c: Vec<u32> = text.bytes().map(u32::from).collect();
        while !c.len().is_multiple_of(3) {
            c.push(3);
        }
        c.chunks(3).map(|c| word(c[0] | c[1] << 7 | c[2] << 14)).collect()
    }

    #[test]
    fn data21_reverses() {
        assert_eq!(data21(word(0x12345)), 0x12345);
        assert_eq!(data21(word(1)), 1);
    }

    /// A transmitter that stops after the blocks it used (on air, 0.42 s 3200/4 transmissions): its slice holds the
    /// header, those blocks, and then noise. The blocks sent decode; the rest are missing, and nothing comes of them.
    #[test]
    fn a_frame_cut_short_decodes_the_blocks_sent() {
        let (a, sent) = super::simulcast::pages_frame(2);
        assert!(a.len() <= 3 * 8, "the pages fit in three blocks");
        let fs = 62_500.0;
        let full = modulate(&a, 4, 33, fs, 350.0);
        let block = (0.16 * fs) as usize;
        let cut = |kept: usize| {
            let mut x = full[..full.len() - (0.05 * fs) as usize - (BLOCKS - kept) * block].to_vec();
            let mut rng = 0x9E37_79B9u32;
            let mut noise = || {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            };
            // The slice's post-roll: 150 ms of the channel with nobody on it.
            x.extend((0..(0.15 * fs) as usize).map(|_| Complex32::new(noise(), noise())));
            x
        };
        let fr = decode(&cut(4), fs);
        assert_eq!(fr.len(), 1);
        let f = &fr[0];
        assert_eq!((f.mode().as_str(), f.cycle, f.frame), ("3200/4", 4, 33));
        assert!(f.block_ok[..4].iter().all(|&n| n == 32) && f.block_ok[4..].iter().all(|&n| n == 0), "{:?}", f.block_ok);
        let got: Vec<(u64, String)> = f.pages.iter().map(|p| (p.capcode, p.text.clone())).collect();
        assert_eq!(got, sent);
        assert!(f.pages.iter().all(|p| !p.damaged));
        // Cut sooner, inside the messages: what decodes was sent, and a page whose words are missing says so.
        for kept in 1..4 {
            for f in decode(&cut(kept), fs) {
                for p in &f.pages {
                    let s = sent.iter().find(|s| s.0 == p.capcode).unwrap_or_else(|| panic!("{kept} blocks: {p:?} was never sent"));
                    assert!(p.damaged || p.text == s.1, "{kept} blocks: {p:?}");
                }
            }
        }
    }

    #[test]
    fn frame_with_short_and_long_pages() {
        let short_text = "SPECTRAWL FLEX TEST 123";
        let long_text = "LONG ADDRESS PAGE";
        // A first fragment's text opens with a signature character.
        let short_msg = alpha_words(&format!("${short_text}"));
        let long_msg = alpha_words(&format!("${long_text}"));
        // Phase A: BIW (address at 1, vectors from 4), short address,
        // long address (2 words), then vectors, then messages.
        let aoff = 1u32;
        let voff = 4u32;
        let mw_short = 7u32;
        let mw_long = mw_short + 1 + short_msg.len() as u32;
        let capcode_long = 8_148_262u64;
        let aw1 = 17_702u32;
        let aw2 = ((((capcode_long - 2_068_480 - aw1 as u64) >> 15) as u32) ^ 0x1F_FFFF) & 0x1F_FFFF;
        let biw = (aoff - 1) << 8 | voff << 10;
        let mut a = vec![
            word(biw | (0xF - ((0..6).map(|k| biw >> (4 * k) & 0xF).sum::<u32>() & 0xF))),
            word(0x8000 + 1_234_567),
            word(aw1),
            word(aw2),
            // Short alpha vector: header word + message words.
            checked(5 << 4 | mw_short << 7 | (1 + short_msg.len() as u32) << 14),
            // Long alpha vector (its length counts the header word, which
            // follows as the second vector word).
            checked(5 << 4 | mw_long << 7 | (1 + long_msg.len() as u32) << 14),
            word(3 << 11),
        ];
        a.push(word(3 << 11)); // short message header: fragment 3
        a.extend(&short_msg);
        a.extend(&long_msg);
        let fs = 62_500.0;
        let x = modulate(&a, 4, 33, fs, 350.0);
        let fr = decode(&x, fs);
        assert_eq!(fr.len(), 1);
        let f = &fr[0];
        assert_eq!((f.mode().as_str(), f.cycle, f.frame), ("3200/4", 4, 33));
        assert!(f.block_ok.iter().all(|&n| n == 32), "{:?}", f.block_ok);
        assert_eq!(f.pages.len(), 2, "{:?}", f.pages);
        assert_eq!((f.pages[0].capcode, f.pages[0].kind, f.pages[0].text.as_str()), (1_234_567, "alpha", short_text));
        assert_eq!((f.pages[1].capcode, f.pages[1].text.as_str()), (capcode_long, long_text));
        assert!(f.pages.iter().all(|p| p.phase == 'A' && !p.damaged));

        // The classifier identifies it from the header alone, too.
        let c = crate::classify(&x[..(0.4 * fs) as usize], fs, 16e3);
        assert_eq!((c.protocol.as_deref(), c.label.as_str()), (Some("FLEX"), "FLEX 3200/4"), "{c:?}");
        let c = crate::classify(&x, fs, 16e3);
        assert_eq!(c.fields.unwrap().messages[0], format!("001234567 A alpha: {short_text}"));
    }

    /// Numeric message words: 2 skipped bits, then 4-bit digits LSB first,
    /// packed LSB first across 21-bit words, padded with fill (0xC).
    fn numeric_words(digits: &str) -> Vec<u32> {
        const T: &[u8; 16] = b"0123456789 U -][";
        let mut bits: Vec<u32> = vec![0, 0];
        for c in digits.bytes() {
            let v = T.iter().position(|&t| t == c).unwrap() as u32;
            bits.extend((0..4).map(|b| v >> b & 1));
        }
        while !bits.len().is_multiple_of(21) {
            let n = bits.len();
            bits.extend((0..4).map(|b| 0xCu32 >> b & 1).take(21 - n % 21));
        }
        bits.chunks(21).map(|c| word(c.iter().enumerate().fold(0, |a, (i, b)| a | b << i))).collect()
    }

    #[test]
    fn numeric_pages_use_the_three_bit_length() {
        // On air (929.6125) numeric pages came out as the number followed by
        // a tail of fill and the next page's words: the vector's length is
        // only 3 bits (at most 7 words beyond the first), the rest of the
        // field is not length.
        let digits = "937 527-9895";
        let msg = numeric_words(digits);
        assert_eq!(msg.len(), 3);
        let capcode_long = 8_007_940u64;
        // The first address word carries the capcode's low 15 bits.
        let aw1 = ((capcode_long - 2_068_480) % 32_768) as u32;
        let aw2 = ((((capcode_long - 2_068_480 - aw1 as u64) >> 15) as u32) ^ 0x1F_FFFF) & 0x1F_FFFF;
        // BIW: addresses from 1, vectors from 4. Short address, long address.
        let biw = 0 << 8 | 4 << 10;
        let mw_short = 7u32;
        let mw_long = mw_short + msg.len() as u32;
        let junk = 0b1011u32 << 17;
        let a = [
            vec![
                word(biw | (0xF - ((0..6).map(|k| biw >> (4 * k) & 0xF).sum::<u32>() & 0xF))),
                word(0x8000 + 4_320_054 - 4_000_000),
                word(aw1),
                word(aw2),
                // Short numeric: 3 words from mw1 (length 2 = two more).
                checked(3 << 4 | mw_short << 7 | 2 << 14 | junk),
                // Long numeric: the first word is the second vector word, then 2 from mw1.
                checked(3 << 4 | mw_long << 7 | 2 << 14 | junk),
                msg[0],
            ],
            msg.clone(),
            msg[1..].to_vec(),
        ]
        .concat();
        let fs = 62_500.0;
        let fr = decode(&modulate(&a, 2, 5, fs, 0.0), fs);
        assert_eq!(fr.len(), 1);
        let p = &fr[0].pages;
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!((p[0].capcode, p[0].kind, p[0].text.as_str()), (320_054, "numeric", digits));
        assert_eq!((p[1].capcode, p[1].kind, p[1].text.as_str()), (capcode_long, "numeric", digits));
        assert!(!p[0].damaged && !p[1].damaged);
    }

    #[test]
    fn continuation_fragment_keeps_its_first_character() {
        // Fragment 0 of a long message: no signature, the text starts at
        // once (on air, the "W" of "WITH TOOL MAKER" was being dropped).
        let text = "WITH TOOL MAKER";
        let msg = alpha_words(text);
        let biw = 0 << 8 | 2 << 10;
        let a = [
            vec![
                word(biw | (0xF - ((0..6).map(|k| biw >> (4 * k) & 0xF).sum::<u32>() & 0xF))),
                word(0x8000 + 1_234_567),
                checked(5 << 4 | 3 << 7 | (1 + msg.len() as u32) << 14),
                word(0 << 11),
            ],
            msg,
        ]
        .concat();
        let fs = 62_500.0;
        let fr = decode(&modulate(&a, 1, 2, fs, 0.0), fs);
        assert_eq!(fr.len(), 1);
        let p = &fr[0].pages;
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!((p[0].fragment, p[0].continued, p[0].text.as_str()), (0, false, text));
    }
}

#[cfg(test)]
pub(crate) mod simulcast {
    //! Pages under simulcast fading: two transmitters a few hertz apart beat, and the nulls between them put bursts of
    //! errors into every block. Pages are known, so a page is right, damaged (its words failed and show as '?'), or
    //! false (an address nobody sent, or text that differs from what was sent without being marked damaged).
    use super::tests::{alpha_words, checked, modulate, word};
    use super::*;

    /// A phase A with `n` short alpha pages: (the words, the pages as (capcode, text)).
    pub(super) fn pages_frame(n: u32) -> (Vec<u32>, Vec<(u64, String)>) {
        let sent: Vec<(u64, String)> = (0..n).map(|k| (100_000 + 7_919 * k as u64, format!("PAGE {k} SIMULCAST"))).collect();
        (frame_of(&sent), sent)
    }

    /// A phase A carrying `pages` (capcode, text) as short-address alpha pages.
    pub(super) fn frame_of(pages: &[(u64, String)]) -> Vec<u32> {
        let n = pages.len() as u32;
        let (aoff, voff) = (1u32, 1 + n);
        let biw = (aoff - 1) << 8 | voff << 10;
        let mut a = vec![word(biw | (0xF - ((0..6).map(|k| biw >> (4 * k) & 0xF).sum::<u32>() & 0xF)))];
        let mut msgs = Vec::new();
        for (capcode, text) in pages {
            a.push(word(0x8000 + *capcode as u32));
            msgs.push(alpha_words(&format!("${text}")));
        }
        let mut mw = 1 + 2 * n;
        for m in &msgs {
            a.push(checked(5 << 4 | mw << 7 | (1 + m.len() as u32) << 14));
            mw += 1 + m.len() as u32;
        }
        for m in &msgs {
            a.push(word(3 << 11));
            a.extend(m);
        }
        assert!(a.len() <= WORDS);
        a
    }

    /// The pages of the committed synthetic slice (`testdata/small/flex_synthetic_929.6125`), by frame: invented
    /// capcodes and text, as no real pager traffic may be committed.
    pub(crate) const SYNTHETIC_PAGES: [&[(u64, &str)]; 4] = [
        &[(1_234_567, "SYNTHETIC FLEX TEST PAGE ONE"), (1_357_913, "CALL 555 0100 RE: TEST")],
        &[(1_246_802, "MAINTENANCE WINDOW 0200-0400")],
        &[(1_234_567, "SECOND PAGE TO THE FIRST CAPCODE")],
        &[(1_470_369, "END OF SYNTHETIC SLICE")],
    ];

    /// The synthetic slice: four FLEX 3200/4 frames (cycle 3, frames 40-43) carrying [`SYNTHETIC_PAGES`], 0.2 s into a
    /// 10 s slice at 50 kS/s, through a mild second transmitter (4 Hz beat, 30 us, 0.35) and noise 22 dB down, with
    /// the channel's noise alone before and after.
    pub(crate) fn synthetic_slice() -> (Vec<Complex32>, f64) {
        let fs = 50_000.0;
        let mut x = vec![Complex32::new(0.0, 0.0); (0.2 * fs) as usize];
        for (k, pages) in SYNTHETIC_PAGES.iter().enumerate() {
            let pages: Vec<(u64, String)> = pages.iter().map(|(c, t)| (*c, t.to_string())).collect();
            x.extend(modulate(&frame_of(&pages), 3, 40 + k as u32, fs, 0.0));
        }
        x.resize((10.0 * fs) as usize, Complex32::new(0.0, 0.0));
        (simulcast(&x, fs, 4.0, 30e-6, 0.35, 22.0, 2027), fs)
    }

    /// Writes the synthetic slice as SigMF to `$FLEX_SYNTH_OUT` (a path without extension):
    ///
    ///     FLEX_SYNTH_OUT=testdata/small/flex_synthetic_929.6125 cargo test -p detect --lib flex::simulcast::write_the_synthetic_slice -- --ignored
    #[test]
    #[ignore]
    fn write_the_synthetic_slice() {
        let Some(stem) = std::env::var_os("FLEX_SYNTH_OUT") else { return };
        let stem = stem.to_string_lossy().to_string();
        let (x, fs) = synthetic_slice();
        let bytes: Vec<u8> = x.iter().flat_map(|v| v.re.to_le_bytes().into_iter().chain(v.im.to_le_bytes())).collect();
        std::fs::write(format!("{stem}.sigmf-data"), bytes).unwrap();
        let meta = format!(
            "{{\n  \"annotations\": [],\n  \"captures\": [\n    {{\n      \"core:frequency\": 929612500.0,\n      \"core:sample_start\": 0\n    }}\n  ],\n  \"global\": {{\n    \"core:datatype\": \"cf32_le\",\n    \"core:description\": \"synthetic FLEX 3200/4: four frames, five invented pages, a 4 Hz simulcast beat and noise 22 dB down (detect::flex::simulcast::synthetic_slice)\",\n    \"core:recorder\": \"SpecTrawl detect tests\",\n    \"core:sample_rate\": {fs:.1},\n    \"core:version\": \"1.2.0\"\n  }}\n}}\n"
        );
        std::fs::write(format!("{stem}.sigmf-meta"), meta).unwrap();
    }

    #[test]
    fn the_synthetic_slice_decodes_every_page_it_carries() {
        let (x, fs) = synthetic_slice();
        let frames = decode(&x, fs);
        let got: Vec<(u64, &str)> = frames.iter().flat_map(|f| f.pages.iter().map(|p| (p.capcode, p.text.as_str()))).collect();
        let sent: Vec<(u64, &str)> = SYNTHETIC_PAGES.iter().flat_map(|f| f.iter().copied()).collect();
        assert_eq!(got, sent, "{frames:?}");
    }

    /// The signal plus a second copy `beat_hz` off, `delay_s` late, at `ratio` of its amplitude, plus noise at
    /// `snr_db` (over the whole channel).
    pub(crate) fn simulcast(x: &[Complex32], fs: f64, beat_hz: f64, delay_s: f64, ratio: f32, snr_db: f64, seed: u64) -> Vec<Complex32> {
        let d = (delay_s * fs).round() as usize;
        let mut s = seed | 1;
        let mut gauss = move || {
            let mut u = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 11) as f64 / (1u64 << 53) as f64
            };
            let (a, b) = (u().max(1e-12), u());
            (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
        };
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        (0..x.len())
            .map(|i| {
                let two = if i >= d {
                    x[i - d] * Complex32::from_polar(ratio, (std::f64::consts::TAU * beat_hz * i as f64 / fs) as f32)
                } else {
                    Complex32::new(0.0, 0.0)
                };
                x[i] + two + Complex32::new((gauss() * sigma) as f32, (gauss() * sigma) as f32)
            })
            .collect()
    }

    /// (right, damaged, false) over the frames.
    pub(super) fn score(frames: &[FlexFrame], sent: &[(u64, String)]) -> (usize, usize, usize) {
        let (mut right, mut damaged, mut wrong) = (0, 0, 0);
        for p in frames.iter().flat_map(|f| &f.pages) {
            match sent.iter().find(|(c, _)| *c == p.capcode) {
                Some((_, t)) if !p.damaged && p.text == *t => right += 1,
                Some(_) if p.damaged => damaged += 1,
                _ => wrong += 1,
            }
        }
        (right, damaged, wrong)
    }

    #[test]
    fn soft_repair_recovers_pages_under_simulcast_without_inventing_text() {
        let fs = 39_062.5;
        let (a, sent) = pages_frame(6);
        let clean = modulate(&a, 2, 17, fs, 120.0);
        let (mut hard, mut soft) = ((0, 0, 0), (0, 0, 0));
        let seeds = 8;
        for seed in 0..seeds {
            let x = simulcast(&clean, fs, 30.0, 40e-6, 0.9, 11.0, 1 + seed);
            let (h, s) = (score(&decode_with(&x, fs, false), &sent), score(&decode(&x, fs), &sent));
            hard = (hard.0 + h.0, hard.1 + h.1, hard.2 + h.2);
            soft = (soft.0 + s.0, soft.1 + s.1, soft.2 + s.2);
        }
        let n = seeds as usize * sent.len();
        // Hard decisions lose most pages to the beat's nulls (as on air at 931.3375 MHz); the soft repair gets them back.
        assert!(hard.0 * 3 < n, "hard {hard:?} of {n}");
        assert!(soft.0 * 10 >= n * 7, "soft {soft:?} of {n} (hard {hard:?})");
        assert!(soft.2 <= hard.2 + 1, "soft {soft:?} gets text wrong beyond hard {hard:?}");
    }

    #[test]
    fn soft_repair_never_puts_a_page_under_an_address_nobody_sent() {
        let fs = 39_062.5;
        let (a, sent) = pages_frame(6);
        let clean = modulate(&a, 2, 17, fs, 120.0);
        let mut pages = 0;
        for (seed, snr) in (0..12).zip([4.0, 5.0, 6.0].into_iter().cycle()) {
            let x = simulcast(&clean, fs, 15.0, 40e-6, 0.85, snr, 100 + seed);
            for p in decode(&x, fs).iter().flat_map(|f| &f.pages) {
                pages += 1;
                assert!(sent.iter().any(|(c, _)| *c == p.capcode), "{p:?}");
            }
        }
        assert!(pages > 0);
    }

    #[test]
    #[ignore = "an experiment: prints hard and soft decoding side by side"]
    fn simulcast_experiment() {
        let fs = 39_062.5;
        let (a, sent) = pages_frame(6);
        let clean = modulate(&a, 2, 17, fs, 120.0);
        for (beat, ratio) in [(8.0, 0.7), (15.0, 0.85), (30.0, 0.9)] {
            for snr in [8.0, 11.0, 14.0, 20.0] {
                let (mut h, mut so, mut words) = ((0, 0, 0), (0, 0, 0), (0, 0, 0usize));
                for seed in 0..20 {
                    let x = simulcast(&clean, fs, beat, 40e-6, ratio, snr, 1 + seed);
                    let (fh, fsoft) = (decode_with(&x, fs, false), decode_with(&x, fs, true));
                    let (a, b) = (score(&fh, &sent), score(&fsoft, &sent));
                    h = (h.0 + a.0, h.1 + a.1, h.2 + a.2);
                    so = (so.0 + b.0, so.1 + b.1, so.2 + b.2);
                    words.0 += fh.iter().flat_map(|f| f.block_ok).map(|n| n as usize).sum::<usize>();
                    words.1 += fsoft.iter().map(|f| f.soft_words as usize).sum::<usize>();
                    words.2 += fh.len() * BLOCKS * 32;
                }
                println!(
                    "beat {beat:4} Hz ×{ratio}, SNR {snr:4} dB: hard right/damaged/false {h:?}, soft {so:?} of {}; words {}/{} +{} soft",
                    20 * sent.len(),
                    words.0,
                    words.2,
                    words.1
                );
            }
        }
    }
}
