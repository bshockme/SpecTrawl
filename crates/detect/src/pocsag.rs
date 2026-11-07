//! POCSAG pager messages from sync-aided frames.
//!
//! A batch is a sync codeword (0x7CD215D8, found by the sync bank) and 16
//! codewords of 32 bits: 21 data bits (a flag, then 20), BCH(31,21) parity
//! and an even-parity bit. Address codewords (flag 0) carry 18 address bits
//! and 2 function bits; the low 3 address bits are the codeword's frame
//! (its pair position in the batch). Message codewords (flag 1) carry 20
//! bits of text: numeric (4-bit BCD, each digit sent LSB first) or
//! alphanumeric (7-bit ASCII, LSB first, packed across codewords).
//!
//! Bit errors are corrected by syndrome lookup (up to 2 per codeword), as
//! decoders since PDW have done. A message codeword past that is repaired
//! from the bits' reliabilities ([`chase`]); an address codeword never is.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::frames::Frame;

const IDLE: u32 = 0x7A89_C197;
/// BCH(31,21) generator x^10+x^9+x^8+x^6+x^5+x^3+1.
const GEN: u32 = 0x769;

/// BCH parity (10 bits) of the 21 data bits.
fn bch(data21: u32) -> u32 {
    let mut r = data21 << 10;
    for bit in (10..31).rev() {
        if r >> bit & 1 == 1 {
            r ^= GEN << (bit - 10);
        }
    }
    r & 0x3FF
}

/// A valid codeword for 21 data bits: data, BCH, even parity.
pub fn encode(data21: u32) -> u32 {
    let cw = (data21 & 0x1F_FFFF) << 11 | bch(data21 & 0x1F_FFFF) << 1;
    cw | (cw.count_ones() & 1)
}

fn syndrome(cw: u32) -> u32 {
    let data = cw >> 11;
    (cw >> 1 & 0x3FF) ^ bch(data)
}

/// Syndrome → error pattern for every 1- and 2-bit error in the 31 BCH bits.
fn table() -> &'static HashMap<u32, u32> {
    static T: OnceLock<HashMap<u32, u32>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = HashMap::new();
        for i in 1..32 {
            let e = 1u32 << i;
            t.entry(syndrome(e)).or_insert(e);
            for j in i + 1..32 {
                let e2 = e | 1 << j;
                t.entry(syndrome(e2)).or_insert(e2);
            }
        }
        t
    })
}

/// Corrects a received codeword. Returns it and the bits fixed, or None
/// if it can't be made valid within two bit errors over all 32 bits.
///
/// The extended code has distance 4, so two errors anywhere (BCH bits or
/// the parity bit) are corrected and three are only detected: a word that
/// needs two BCH corrections *and* a parity flip is as close to another
/// codeword as to this one, and is rejected rather than guessed.
pub fn correct(cw: u32) -> Option<(u32, u32)> {
    let s = syndrome(cw);
    let fixed = if s == 0 { cw } else { cw ^ *table().get(&s)? };
    // The parity bit: flip it if it is wrong.
    let fixed = if fixed.count_ones() & 1 == 1 { fixed ^ 1 } else { fixed };
    let errors = (fixed ^ cw).count_ones();
    (errors <= 2).then_some((fixed, errors))
}

/// Least reliable bits a soft repair tries flipping, and how many at once.
const CHASE_BITS: usize = 6;
const CHASE_FLIPS: u32 = 3;
/// Most bits a soft repair may change from the hard decisions.
const CHASE_CHANGES: u32 = 4;

/// Soft-decision repair of a received codeword (Chase's second algorithm): flip each combination of up to three of the
/// six least reliable bits, correct what results, and keep the codeword whose changed bits are the least reliable in
/// sum. It may change at most four bits, each less reliable than `doubt`. A confident bit is never overruled: hard
/// correction alone passes about a quarter of random words, and this must not make noise look valid.
///
/// `rel[k]` is how reliable transmitted bit `k` (bit `31 - k` of `cw`) is: 0 on its decision threshold, about 1 on a
/// clean level. Returns the codeword and the bits changed, as [`correct`] does.
pub fn chase(cw: u32, rel: &[f32; 32], doubt: f32) -> Option<(u32, u32)> {
    let mut order: [usize; 32] = std::array::from_fn(|k| k);
    order.sort_by(|&a, &b| rel[a].total_cmp(&rel[b]));
    let weakest = &order[..CHASE_BITS];
    let mut best: Option<(f32, u32, u32)> = None;
    for mask in 0u32..1 << CHASE_BITS {
        if mask.count_ones() > CHASE_FLIPS {
            continue;
        }
        let flip = weakest.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).fold(0u32, |f, (_, &k)| f | 1 << (31 - k));
        let Some((fixed, _)) = correct(cw ^ flip) else { continue };
        let changed = fixed ^ cw;
        if changed.count_ones() > CHASE_CHANGES {
            continue;
        }
        let bits = (0..32).filter(|k| changed >> (31 - k) & 1 == 1);
        if bits.clone().any(|k| rel[k] >= doubt) {
            continue;
        }
        let cost: f32 = bits.map(|k| rel[k]).sum();
        if best.is_none_or(|b| cost < b.0) {
            best = Some((cost, fixed, changed.count_ones()));
        }
    }
    best.map(|(_, fixed, n)| (fixed, n))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub address: u32,
    pub function: u8,
    /// Decoded as numeric and as alphanumeric; `text` is the likelier one.
    pub numeric: String,
    pub alpha: String,
    pub text: String,
    /// Codeword bits corrected in this page (hard and soft).
    pub corrected: u32,
    /// Message codewords repaired from soft decisions.
    pub repaired: u32,
    /// Something failed after the page began (a codeword past repair, a
    /// noise batch, a missing batch), so its text may be cut short there.
    pub damaged: bool,
}

impl Page {
    /// One line for the monitor: address, function, flags, text.
    pub fn summary(&self) -> String {
        format!("{} ({}){}: {}", self.address, (b'A' + self.function) as char, if self.damaged { " [damaged]" } else { "" }, self.text)
    }
}

const NUMERIC: [char; 16] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', 'U', ' ', '-', ')', '('];

fn numeric(bits: &[u8]) -> String {
    bits.as_chunks::<4>()
        .0
        .iter()
        .map(|c| NUMERIC[(c[0] | c[1] << 1 | c[2] << 2 | c[3] << 3) as usize])
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn alpha(bits: &[u8]) -> String {
    bits.as_chunks::<7>()
        .0
        .iter()
        .map(|c| c.iter().enumerate().fold(0u8, |a, (i, b)| a | b << i))
        .filter(|&c| c != 0 && c != 0x04) // NUL padding, EOT
        .map(|c| if (0x20..0x7F).contains(&c) || c == b'\n' { c as char } else { '·' })
        .collect()
}

/// The 16 codewords after each frame's sync, as received (polarity already
/// corrected: a negative symbol is a 1 bit).
pub fn batch_codewords(f: &Frame) -> Option<[u32; 16]> {
    Some(batch_soft(f)?.map(|(cw, _)| cw))
}

/// The batch's codewords with each bit's reliability (its soft value's
/// distance from the threshold, about 1 on a clean level).
fn batch_soft(f: &Frame) -> Option<[(u32, [f32; 32]); 16]> {
    let start = f.sync_start + 32;
    let bits = f.soft.get(start..start + 512)?;
    Some(std::array::from_fn(|k| {
        let b = &bits[k * 32..k * 32 + 32];
        (b.iter().fold(0u32, |a, v| a << 1 | (*v < 0.0) as u32), std::array::from_fn(|j| b[j].abs()))
    }))
}

/// Soft repair: only bits less reliable than this may be overruled (FLEX's
/// threshold, which its simulcast tests measured).
const DOUBT: f32 = 0.6;

/// Codewords a batch must have that are valid as received for it to count
/// as signal: a random word is within two bits of a codeword about half the
/// time, so noise decodes freely with corrections, but is valid uncorrected
/// only once in 2048.
const CLEAN_PER_BATCH: usize = 2;

/// Pages from POCSAG frames (sliced with at least 512 symbols after the
/// sync), in order. A page runs from its address codeword to the next
/// address or idle codeword, across batches. A batch without
/// `CLEAN_PER_BATCH` uncorrected codewords is noise (on air, quiet channels
/// gave 40 to 100 such pages an hour) and ends any open page.
///
/// What a codeword is trusted with, measured under simulcast fading (the
/// `fading_experiment` test: 7200 pages at 512 to 2400 Bd). POCSAG has no
/// interleaving, so a fade takes out whole codewords, and a random word is
/// within two bits of a valid one a quarter of the time. Two-bit fixes, as
/// decoders since PDW make them, put 786 false pages beside 4549 right ones,
/// mostly pages under addresses nobody sent. So:
/// - an address may have one bit fixed, not two (an idle codeword two);
/// - a hard fix to a message codeword may not overrule a bit the slicer was
///   sure of (one at least [`DOUBT`] from the threshold), as the soft repair
///   may not. (Holding addresses and idle to that too cost 75 right pages
///   for 7 false ones fewer.)
///
/// Those leave 31 false pages and 4457 right. A message codeword past hard
/// correction is then repaired from the bits' reliabilities ([`chase`]), but
/// its guesses are wrong too often to pass as right (without interleaving,
/// a fade leaves every bit in the word doubtful), so a page with a repaired
/// codeword is marked damaged: its text runs on, flagged. An address is
/// never repaired: a page starts only from one that decoded hard.
pub fn pages(frames: &[Frame]) -> Vec<Page> {
    pages_with(frames, Trust::Strict)
}

/// How far [`pages_with`] trusts codewords: the rules [`pages`] uses, or the two-bit hard correction it replaced
/// (for comparison).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Trust {
    Strict,
    #[cfg(test)]
    TwoBit,
}

fn pages_with(frames: &[Frame], trust: Trust) -> Vec<Page> {
    let mut out = Vec::new();
    let mut cur: Option<(Page, Vec<u8>)> = None;
    let finish = |cur: &mut Option<(Page, Vec<u8>)>, out: &mut Vec<Page>, damaged: bool| {
        if let Some((mut p, bits)) = cur.take() {
            p.damaged |= damaged;
            p.numeric = numeric(&bits);
            p.alpha = alpha(&bits);
            // Alphanumeric pages read as mostly printable text; numeric
            // ones as mostly digits.
            let printable = p.alpha.chars().filter(|c| c.is_ascii_alphanumeric() || " .,:-/".contains(*c)).count();
            p.text = if bits.is_empty() {
                String::new()
            } else if p.function == 0 || printable * 10 < p.alpha.chars().count() * 8 {
                p.numeric.clone()
            } else {
                p.alpha.clone()
            };
            out.push(p);
        }
    };
    let strict = trust == Trust::Strict;
    // A fix overrules only doubtful bits.
    let doubtful = |changed: u32, rel: &[f32; 32]| (0..32).all(|j| changed >> (31 - j) & 1 == 0 || rel[j] < DOUBT);
    let mut last_at = None;
    for f in frames.iter().filter(|f| f.protocol == "POCSAG") {
        // A gap of more than one batch (544 bits) ends any open page.
        if last_at.is_some_and(|a: usize| f.at_symbol > a + 544 + 8) {
            finish(&mut cur, &mut out, true);
        }
        last_at = Some(f.at_symbol);
        let Some(cws) = batch_soft(f) else { continue };
        if cws.iter().filter(|(raw, _)| correct(*raw).is_some_and(|c| c.1 == 0)).count() < CLEAN_PER_BATCH {
            finish(&mut cur, &mut out, true);
            continue;
        }
        for (k, (raw, rel)) in cws.iter().enumerate() {
            let hard = correct(*raw)
                .filter(|&(cw, fixed)| !strict || if cw >> 31 == 1 { doubtful(cw ^ raw, rel) } else { cw == IDLE || fixed <= 1 });
            let soft = || (strict && cur.is_some()).then(|| chase(*raw, rel, DOUBT)).flatten().filter(|(cw, _)| cw >> 31 == 1);
            let (cw, fixed, repaired) = match hard {
                Some((cw, fixed)) => (cw, fixed, false),
                None => match soft() {
                    Some((cw, fixed)) => (cw, fixed, true),
                    None => {
                        // Past repair (perhaps an address we can't trust): the page is broken here.
                        finish(&mut cur, &mut out, true);
                        continue;
                    }
                },
            };
            if cw == IDLE {
                finish(&mut cur, &mut out, false);
                continue;
            }
            if cw >> 31 == 0 {
                finish(&mut cur, &mut out, false);
                let address = (cw >> 13 & 0x3_FFFF) << 3 | (k as u32 / 2);
                let function = (cw >> 11 & 3) as u8;
                cur = Some((
                    Page {
                        address,
                        function,
                        numeric: String::new(),
                        alpha: String::new(),
                        text: String::new(),
                        corrected: fixed,
                        repaired: 0,
                        damaged: false,
                    },
                    Vec::new(),
                ));
            } else if let Some((p, bits)) = cur.as_mut() {
                p.corrected += fixed;
                p.repaired += repaired as u32;
                p.damaged |= repaired;
                bits.extend((0..20).rev().map(|i| (cw >> (11 + i) & 1) as u8));
            }
        }
    }
    finish(&mut cur, &mut out, false);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_sync_are_codewords() {
        assert_eq!(correct(IDLE), Some((IDLE, 0)));
        assert_eq!(correct(0x7CD2_15D8), Some((0x7CD2_15D8, 0)));
        assert_eq!(encode(IDLE >> 11), IDLE);
    }

    #[test]
    fn corrects_two_errors() {
        let cw = encode(0x12345);
        assert_eq!(correct(cw ^ 1 << 7 ^ 1 << 20), Some((cw, 2)));
        assert_eq!(correct(cw ^ 1 << 30), Some((cw, 1)));
        // One BCH error plus the parity bit: still two in all.
        assert_eq!(correct(cw ^ 1 << 7 ^ 1), Some((cw, 2)));
        assert_eq!(correct(cw ^ 1), Some((cw, 1)));
    }

    #[test]
    fn chase_repairs_errors_on_doubtful_bits_only() {
        let cw = encode(0x12345);
        let mut rel = [1.0f32; 32];
        // Four errors, past hard correction, on the four least reliable bits.
        let hit = [3usize, 9, 17, 28].iter().fold(cw, |w, &k| w ^ 1 << (31 - k));
        for k in [3, 9, 17, 28] {
            rel[k] = 0.2;
        }
        rel[12] = 0.3;
        rel[22] = 0.35;
        assert_eq!(correct(hit), None);
        assert_eq!(chase(hit, &rel, 0.6), Some((cw, 4)));
        // One of the errors on a bit the slicer was sure of: not repaired, not guessed.
        rel[28] = 0.9;
        let found = chase(hit, &rel, 0.6);
        assert!(found.is_none_or(|(w, _)| w != cw), "{found:?}");
        // Clean words pass unchanged.
        assert_eq!(chase(cw, &[1.0; 32], 0.6), Some((cw, 0)));
    }

    #[test]
    fn chase_rarely_makes_noise_valid() {
        // Random words with random reliabilities: hard correction passes about a quarter. The soft repair, with every
        // bit allowed to be overruled, may pass more; with the strict doubt used for addresses, few beyond those.
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let (mut hard, mut strict) = (0, 0);
        let n = 20_000;
        for _ in 0..n {
            let w = next() as u32;
            let rel: [f32; 32] = std::array::from_fn(|_| (next() % 1000) as f32 / 1000.0 * 2.0);
            hard += correct(w).is_some() as usize;
            strict += chase(w, &rel, 0.6).is_some() as usize;
        }
        let (hard, strict) = (hard as f64 / n as f64, strict as f64 / n as f64);
        assert!((0.2..0.3).contains(&hard), "{hard}");
        assert!(strict < 0.25, "hard {hard}, strict soft {strict}");
    }

    #[test]
    fn rejects_three_errors_instead_of_guessing() {
        // Every valid word, three bits away in every arrangement that the
        // syndrome table would repair as two BCH errors plus parity: the
        // extended code can only detect that, so correct() must refuse.
        for data in [0u32, 0x12345, 0x1F_FFFF, IDLE >> 11] {
            let cw = encode(data);
            for (a, b) in [(1, 2), (1, 31), (7, 20), (12, 13)] {
                let hit = cw ^ 1 << a ^ 1 << b ^ 1;
                assert_eq!(correct(hit), None, "data {data:#x} bits {a},{b},0 accepted as {:?}", correct(hit));
            }
        }
        // The review's case: the zero word with bits 1, 2 and 5 flipped
        // used to come back as 0x00900027, a different address.
        assert_eq!(correct(0x26), None);
    }

    /// Builds frames for a sequence of codewords (sync every 16).
    fn frames_for(cws: &[u32]) -> Vec<Frame> {
        cws.chunks(16)
            .enumerate()
            .map(|(b, batch)| {
                let mut soft: Vec<f32> = (0..32).rev().map(|i| if 0x7CD2_15D8u32 >> i & 1 == 1 { -1.0 } else { 1.0 }).collect();
                for &cw in batch.iter().chain(std::iter::repeat(&IDLE)).take(16) {
                    soft.extend((0..32).rev().map(|i| if cw >> i & 1 == 1 { -1.0 } else { 1.0 }));
                }
                Frame {
                    protocol: "POCSAG",
                    detail: "",
                    pattern: 0x7CD2_15D8,
                    inverted: false,
                    dibits: Vec::new(),
                    soft,
                    sync_start: 0,
                    at_symbol: b * 544,
                    outer: 1.0,
                }
            })
            .collect()
    }

    fn alpha_codewords(text: &str) -> Vec<u32> {
        let bits: Vec<u32> = text.bytes().flat_map(|c| (0..7).map(move |i| (c >> i & 1) as u32)).collect();
        bits.chunks(20).map(|c| encode(1 << 20 | c.iter().chain(std::iter::repeat(&0)).take(20).fold(0, |a, b| a << 1 | b))).collect()
    }

    #[test]
    fn alpha_page_across_batches() {
        // Address 1234567 sits in frame 7 (its low 3 bits): codeword slot 14.
        let addr = 1_234_567u32;
        let mut cws = vec![IDLE; 14];
        cws.push(encode((addr >> 3) << 2 | 3));
        let text = "TEST PAGE 42: units respond to Main St";
        cws.extend(alpha_codewords(text));
        cws.push(IDLE);
        // Two bit errors in one message codeword (the second batch's first), on bits the slicer doubted: wrong side
        // of the threshold, near it.
        let mut frames = frames_for(&cws);
        let flip = |frames: &mut Vec<Frame>, bit: usize, level: f32| {
            let v = &mut frames[1].soft[32 + 31 - bit];
            *v = -v.signum() * level;
        };
        flip(&mut frames, 3, 0.2);
        flip(&mut frames, 17, 0.3);
        let p = pages(&frames);
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!((p[0].address, p[0].function), (addr, 3));
        assert_eq!(p[0].text, text);
        assert_eq!((p[0].corrected, p[0].damaged), (2, false));
        // The same errors on bits the slicer was sure of: not a fix but a guess, so the page is cut there, damaged.
        let mut frames = frames_for(&cws);
        flip(&mut frames, 3, 0.9);
        flip(&mut frames, 17, 0.9);
        let p = pages(&frames);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].damaged && text.starts_with(&p[0].text) && p[0].text.len() < text.len(), "{p:?}");
    }

    #[test]
    fn noise_batches_make_no_pages() {
        // Random bits pass BCH with two corrections half the time; a batch
        // of them used to yield tone pages and short garbage texts.
        let mut x = 0x2545_F491u32;
        let mut frames = frames_for(&[IDLE; 16]);
        for f in frames.iter_mut() {
            for v in f.soft.iter_mut().skip(32) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *v = if x & 1 == 1 { 1.0 } else { -1.0 };
            }
        }
        let noise: Vec<Frame> = (0..40).flat_map(|_| frames.clone()).collect();
        assert!(pages(&noise).is_empty());
    }

    #[test]
    fn numeric_page() {
        let addr = 8u32;
        let digits = "5551234";
        // Pagers pad numeric messages with spaces (0xC).
        let mut bits: Vec<u32> = digits.bytes().flat_map(|d| (0..4).map(move |i| ((d - b'0') >> i & 1) as u32)).collect();
        while bits.len() % 20 != 0 {
            bits.extend([0, 0, 1, 1]);
        }
        let mut cws = vec![IDLE, IDLE, encode((addr >> 3) << 2)];
        cws.extend(bits.chunks(20).map(|c| encode(1 << 20 | c.iter().chain(std::iter::repeat(&0)).take(20).fold(0, |a, b| a << 1 | b))));
        let p = pages(&frames_for(&cws));
        assert_eq!(p[0].address, addr | 1);
        assert_eq!(p[0].text, "5551234");
    }

    /// A transmission of `n` alpha pages, each in its own frame slot, as codewords: (the codewords, the pages sent as
    /// (address, text)). Pages run across batches as real ones do.
    fn pages_sent(n: u32) -> (Vec<u32>, Vec<(u32, String)>) {
        let mut cws = Vec::new();
        let mut sent = Vec::new();
        for k in 0..n {
            // Each address in the frame slot its low bits name, after idle fill to reach it.
            let address = 200_000 + 7_919 * k;
            let slot = (address & 7) as usize * 2;
            while cws.len() % 16 != slot {
                cws.push(IDLE);
            }
            cws.push(encode((address >> 3) << 2 | 3));
            let text = format!("PAGE {k} FADING TEST {}", 1000 + 37 * k);
            cws.extend(alpha_codewords(&text));
            sent.push((address, text));
        }
        cws.push(IDLE);
        while cws.len() % 16 != 0 {
            cws.push(IDLE);
        }
        (cws, sent)
    }

    /// FM-modulates POCSAG at `baud`, ±4.5 kHz (a 1 bit low): carrier, the 576-bit preamble, then a sync and 16
    /// codewords per batch, then carrier.
    fn modulate(cws: &[u32], baud: f64, fs: f64) -> Vec<num_complex::Complex32> {
        let mut bits: Vec<bool> = (0..576).map(|i| i % 2 == 0).collect();
        for batch in cws.chunks(16) {
            bits.extend((0..32).rev().map(|i| 0x7CD2_15D8u32 >> i & 1 == 1));
            bits.extend(batch.iter().flat_map(|cw| (0..32).rev().map(move |i| cw >> i & 1 == 1)));
        }
        let lead = (0.05 * fs) as usize;
        let n = lead * 2 + (bits.len() as f64 * fs / baud) as usize;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let b = ((i as f64 - lead as f64) * baud / fs).floor();
                let dev = if b < 0.0 || b as usize >= bits.len() {
                    0.0
                } else if bits[b as usize] {
                    -4500.0
                } else {
                    4500.0
                };
                phase += std::f64::consts::TAU * dev / fs;
                num_complex::Complex32::from_polar(1.0, phase as f32)
            })
            .collect()
    }

    /// (right, damaged, false): a page is right when its address was sent and its text is what was sent, damaged
    /// when its address was sent and it says so, false otherwise (an address nobody sent, or other text unmarked).
    fn score(pages: &[Page], sent: &[(u32, String)]) -> (usize, usize, usize) {
        let (mut right, mut damaged, mut wrong) = (0, 0, 0);
        for p in pages {
            match sent.iter().find(|(a, _)| *a == p.address) {
                Some((_, t)) if !p.damaged && p.text == *t => right += 1,
                Some(_) if p.damaged => damaged += 1,
                _ => wrong += 1,
            }
        }
        (right, damaged, wrong)
    }

    #[test]
    fn a_clean_transmission_decodes_every_page() {
        let fs = 39_062.5;
        let (cws, sent) = pages_sent(5);
        let p = pages(&frames(&modulate(&cws, 1200.0, fs), fs));
        assert_eq!(score(&p, &sent), (5, 0, 0), "{p:?}");
        assert!(p.iter().all(|p| p.repaired == 0 && p.corrected == 0));
    }

    /// (right, damaged, false) over eight faded transmissions of six pages, by two-bit fixes and by the strict rules.
    fn faded_scores(baud: f64, beat: f64, ratio: f32, snr_db: f64) -> ((usize, usize, usize), (usize, usize, usize)) {
        let fs = 39_062.5;
        let (cws, sent) = pages_sent(6);
        let clean = modulate(&cws, baud, fs);
        let (mut two, mut strict) = ((0, 0, 0), (0, 0, 0));
        for seed in 0..8 {
            let fr = frames(&faded(&clean, fs, beat, ratio, snr_db, seed), fs);
            add(&mut two, score(&pages_with(&fr, Trust::TwoBit), &sent));
            add(&mut strict, score(&pages_with(&fr, Trust::Strict), &sent));
        }
        (two, strict)
    }

    #[test]
    fn deep_fades_make_no_false_pages() {
        // 2400 Bd under a 30 Hz beat at 0.7: fades take out whole codewords, and two-bit fixes read pages under
        // addresses nobody sent out of them.
        let (two, strict) = faded_scores(2400.0, 30.0, 0.7, 10.0);
        assert!(two.2 >= 15, "two-bit {two:?}: the channel no longer tests anything");
        assert!(strict.2 <= 1, "strict {strict:?} (two-bit {two:?})");
    }

    #[test]
    fn the_strict_rules_keep_the_pages_a_fade_spares() {
        let (two, strict) = faded_scores(1200.0, 8.0, 0.5, 6.0);
        assert!(strict.0 + 4 >= two.0, "strict {strict:?}, two-bit {two:?}");
        assert!(strict.2 <= two.2 && strict.2 <= 2, "strict {strict:?}, two-bit {two:?}");
    }

    #[test]
    #[ignore = "an experiment: two-bit hard correction against the strict rules, page by page outcome"]
    fn fading_experiment() {
        let fs = 39_062.5;
        let (cws, sent) = pages_sent(6);
        let (mut two, mut strict) = ((0, 0, 0), (0, 0, 0));
        for baud in [512.0, 1200.0, 2400.0] {
            let clean = modulate(&cws, baud, fs);
            for (beat, ratio) in [(0.0, 0.0), (8.0, 0.3), (8.0, 0.5), (15.0, 0.5), (30.0, 0.7)] {
                for snr in [0.0, 3.0, 6.0, 10.0] {
                    let (mut a, mut b) = ((0, 0, 0), (0, 0, 0));
                    for seed in 0..20 {
                        let fr = frames(&faded(&clean, fs, beat, ratio, snr, seed), fs);
                        add(&mut a, score(&pages_with(&fr, Trust::TwoBit), &sent));
                        add(&mut b, score(&pages_with(&fr, Trust::Strict), &sent));
                    }
                    println!("{baud:4} Bd beat {beat:2}×{ratio} {snr:4} dB, right/damaged/false of 120: two-bit {a:?}, strict {b:?}");
                    add(&mut two, a);
                    add(&mut strict, b);
                }
            }
        }
        println!("all: two-bit {two:?}, strict {strict:?}");
    }

    fn add(t: &mut (usize, usize, usize), s: (usize, usize, usize)) {
        *t = (t.0 + s.0, t.1 + s.1, t.2 + s.2);
    }

    fn frames(x: &[num_complex::Complex32], fs: f64) -> Vec<Frame> {
        crate::pocsag_frames(x, fs)
    }

    /// The signal under simulcast fading (a second transmitter `beat` Hz off at `ratio` of its amplitude) and noise.
    fn faded(x: &[num_complex::Complex32], fs: f64, beat: f64, ratio: f32, snr_db: f64, seed: u64) -> Vec<num_complex::Complex32> {
        // (The noise generator sets the seed's low bit: odd seeds differ.)
        crate::flex::simulcast::simulcast(x, fs, beat, 40e-6, ratio, snr_db, 2 * seed + 1)
    }

    #[test]
    fn scratch_slices() {
        // Real slices (POCSAG_SLICES: stems, comma-separated), two-bit fixes against the strict rules, without text.
        let Some(list) = std::env::var_os("POCSAG_SLICES") else { return };
        for stem in list.to_string_lossy().split(',') {
            let meta = std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap();
            let v: serde_json::Value = serde_json::from_str(&meta).unwrap();
            let fs = v["global"]["core:sample_rate"].as_f64().unwrap();
            let x = crate::acars::tests::read_slice(std::path::Path::new(&format!("{stem}.sigmf-data")), &meta);
            let fr = frames(&x, fs);
            let c = crate::classify(&x, fs, 15e3);
            println!("  classify: {:?}, {} messages", c.protocol, c.fields.map_or(0, |f| f.messages.len()));
            for f in fr.iter().filter(|f| f.protocol == "POCSAG") {
                let h: String = batch_codewords(f)
                    .map_or(String::new(), |c| c.iter().map(|&w| correct(w).map_or('x', |c| (b'0' + c.1 as u8) as char)).collect());
                println!("  batch at symbol {}: {h}", f.at_symbol);
            }
            for (name, trust) in [("two-bit", Trust::TwoBit), ("strict", Trust::Strict)] {
                let p: Vec<String> = pages_with(&fr, trust)
                    .iter()
                    .map(|p| {
                        format!(
                            "{} corr {} rep {}{} {} chars",
                            p.address,
                            p.corrected,
                            p.repaired,
                            if p.damaged { " dmg" } else { "" },
                            p.text.len()
                        )
                    })
                    .collect();
                println!("{stem} {name}: {p:?}");
            }
        }
    }
}
