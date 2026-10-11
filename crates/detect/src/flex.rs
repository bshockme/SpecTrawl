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
const HEADER_BAUD: f64 = 1600.0;
const SYNC2_S: f64 = 0.025;
const BLOCKS: usize = 11;
const WORDS: usize = 88;
/// Length of the data symbol filter, in 3200-baud symbols.
const DATA_FILTER: f64 = 0.25;

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
                    let (code, marker, tail) = ((v >> 48) as u16, (v >> 16) as u32, v as u16);
                    if let Some(s) = SPEEDS.iter().position(|s| s.0 == code).filter(|_| marker == MARKER && tail == !code) {
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
fn slice_block(d: &[f32], t0: f64, s: f64, n: usize, levels: u8, inv: bool, header_level: f32) -> Vec<Vec<u8>> {
    let mut v: Vec<f32> = (0..n).map(|k| at(d, t0 + (k as f64 + 0.5) * s)).collect();
    // The carrier drifts a few hundred Hz between blocks, and idle fill
    // makes the symbol mix lopsided (so no percentile midpoint): estimate
    // the offset and outer level decision-directed, from each symbol's
    // distance to its nearest ideal level.
    // Start from the header's level: idle fill can put nearly every symbol
    // on the inner levels, which would pass for outer ones.
    let mut outer = header_level;
    let mut dc = 0.0;
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
    let thr = outer * 2.0 / 3.0;
    // Data bits: bit a is the sign (positive = 1); with 4 levels, bit b is
    // set on the inner levels.
    let bits: Vec<(u8, u8)> = v.iter().map(|&x| ((x > 0.0) as u8, (x.abs() < thr) as u8)).collect();
    match (n, levels) {
        (256, 2) => vec![bits.iter().map(|b| b.0).collect()],
        (256, _) => vec![bits.iter().map(|b| b.0).collect(), bits.iter().map(|b| b.1).collect()],
        (_, 2) => vec![bits.iter().step_by(2).map(|b| b.0).collect(), bits.iter().skip(1).step_by(2).map(|b| b.0).collect()],
        _ => vec![
            bits.iter().step_by(2).map(|b| b.0).collect(),
            bits.iter().step_by(2).map(|b| b.1).collect(),
            bits.iter().skip(1).step_by(2).map(|b| b.0).collect(),
            bits.iter().skip(1).step_by(2).map(|b| b.1).collect(),
        ],
    }
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
    if (x.len() as f64) < fs * 0.1 {
        return Vec::new();
    }
    // The 1600-baud header is read through a one-symbol filter; the data
    // through a shorter one, which keeps the four levels further apart.
    let dh = discriminator(x, fs, 1.0);
    let d = discriminator(x, fs, DATA_FILTER);
    let sps = fs / HEADER_BAUD;
    let mut out = Vec::new();
    for (end, speed, inv) in find_sync1(&dh, fs) {
        let Some((cycle, frame)) = fiw(&dh, fs, end, inv) else { continue };
        let (_, baud, levels) = SPEEDS[speed];
        let s = fs / baud as f64;
        let n = (baud as f64 * 0.16).round() as usize;
        // First data symbol: after the FIW and sync 2.
        let t0 = end + 48.5 * sps + SYNC2_S * fs;
        if t0 + (BLOCKS * n) as f64 * s + 2.0 * s > (d.len() - 1) as f64 {
            break;
        }
        let nph = phase_names(baud, levels).len();
        // Full deviation, from the 1600-baud header after sync 1's start.
        let level = (0..112).map(|k| at(&dh, end - (63 - k) as f64 * sps).abs()).sum::<f32>() / 112.0;
        let mut phases: Vec<Vec<Option<u32>>> = vec![Vec::with_capacity(WORDS); nph];
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
        let locked = if (baud, levels) == (3200, 4) { sync2_timing(&d, end + 48.5 * sps, s, inv, level, 4.0) } else { None };
        let mut anchor = (locked.unwrap_or(0.0), 0usize);
        for (blk, ok_out) in block_ok.iter_mut().enumerate() {
            // Timing: the offset whose words need the fewest corrections.
            let span = if locked.is_some() {
                5
            } else if blk == 0 {
                30
            } else {
                5
            };
            let mut best: Option<(u32, f64, Vec<BlockWords>)> = None;
            // Nearest offsets first, so ties (idle fill decodes at several)
            // keep the timing where it is.
            for o in (0..=2 * span).map(|k| if k % 2 == 0 { k / 2 } else { -(k + 1) / 2 }) {
                let off = anchor.0 + o as f64 * 0.1;
                let mut cost = 0;
                let words: Vec<BlockWords> = slice_block(&d, t0 + ((blk * n) as f64 + off) * s, s, n, levels, inv, level)
                    .iter()
                    .map(|b| {
                        block_words(b).map(|w| {
                            let c = pocsag::correct(w);
                            // Corrected bits, or 9 for a word past repair.
                            cost += c.map_or(9, |c| c.1);
                            c
                        })
                    })
                    .collect();
                if best.as_ref().is_none_or(|b| cost < b.0) {
                    best = Some((cost, off, words));
                }
            }
            let (_, off, words) = best.expect("offset range is non-empty");
            let ok = words.iter().flatten().filter(|w| w.is_some()).count();
            *ok_out = ok as u8;
            block_off[blk] = off as f32;
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
        }
        let mut pages = Vec::new();
        for (ph, name) in phases.iter().zip(phase_names(baud, levels)) {
            pages.extend(phase_pages(ph, *name));
        }
        out.push(FlexFrame { at: end as usize, baud, levels, cycle, frame, block_ok, block_off, pages });
    }
    out
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
    if biw == 0 || biw == 0x1F_FFFF || (0..6).map(|k| biw >> (4 * k) & 0xF).sum::<u32>() & 0xF != 0xF {
        return out;
    }
    let aoff = ((biw >> 8 & 3) + 1) as usize;
    let voff = (biw >> 10 & 0x3F) as usize;
    let mut i = aoff;
    while i < voff {
        let vi = voff + i - aoff;
        let (Some(aw), Some(&Some(vec))) = (dw[i], dw.get(vi)) else {
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
                let (t, dmg) = numeric(dw, mw1, len, header, long, kind_bits == 7);
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
/// word and the message starts at `mw1`. A message that isn't the first
/// fragment (fragment ≠ 3) starts with a signature character, not text.
fn alpha(dw: &[Option<u32>], mw1: usize, len: usize, header: Option<u32>, long: bool) -> (&'static str, String, bool, bool, u8, u8) {
    let (header, first) = if long { (header, mw1) } else { (dw.get(mw1).copied().flatten(), mw1 + 1) };
    // Without a header the fragment number is unknown; 3 (a whole message)
    // keeps it out of the joiner, and `damaged` says it was guessed.
    let frag = header.map_or(3, |h| h >> 11 & 3);
    let continued = header.is_some_and(|h| h >> 10 & 1 == 1);
    let message_no = header.map_or(0, |h| (h >> 4 & 0x3F) as u8);
    let last = (mw1 + len).min(dw.len());
    let mut text = String::new();
    let mut damaged = header.is_none();
    for (k, w) in dw.iter().enumerate().take(last).skip(first) {
        let Some(w) = w else {
            text.push_str("???");
            damaged = true;
            continue;
        };
        for sh in [0, 7, 14] {
            if k == first && sh == 0 && frag != 3 {
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
/// numbered numeric). Long addresses start the message in the second
/// vector word.
fn numeric(dw: &[Option<u32>], mw1: usize, len: usize, header: Option<u32>, long: bool, numbered: bool) -> (String, bool) {
    const DIGITS: &[u8; 16] = b"0123456789 U -][";
    let mut words: Vec<Option<u32>> = Vec::new();
    if long {
        words.push(header);
    }
    words.extend(dw.iter().take((mw1 + len).min(dw.len())).skip(mw1).take(if long { len.saturating_sub(1) } else { len }));
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
mod tests {
    use super::*;

    /// A codeword for 21 data bits given LSB first.
    fn word(x: u32) -> u32 {
        pocsag::encode(x.reverse_bits() >> 11)
    }

    /// FM-modulates a FLEX 3200/4 frame with `phase_a` as phase A's words
    /// (other phases idle) at `fs`, with a carrier offset.
    fn modulate(phase_a: &[u32], cycle: u32, frame: u32, fs: f64, offset_hz: f64) -> Vec<Complex32> {
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
        // Sync 2: alternating.
        for k in 0..80 {
            push(&mut freq, if k % 2 == 0 { 4800.0 } else { -4800.0 }, 1.0 / 3200.0);
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

    fn alpha_words(text: &str) -> Vec<u32> {
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

    #[test]
    fn frame_with_short_and_long_pages() {
        let short_text = "SPECTRAWL FLEX TEST 123";
        let long_text = "LONG ADDRESS PAGE";
        let short_msg = alpha_words(short_text);
        let long_msg = alpha_words(long_text);
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
            word(5 << 4 | mw_short << 7 | (1 + short_msg.len() as u32) << 14),
            // Long alpha vector, then the header (fragment 3) word.
            word(5 << 4 | mw_long << 7 | (long_msg.len() as u32) << 14),
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
}
