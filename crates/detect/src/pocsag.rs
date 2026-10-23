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
//! decoders since PDW have done.

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
/// if it can't be made valid.
pub fn correct(cw: u32) -> Option<(u32, u32)> {
    let s = syndrome(cw);
    let fixed = if s == 0 { cw } else { cw ^ *table().get(&s)? };
    // The parity bit: flip it if only it is wrong.
    let fixed = if fixed.count_ones() & 1 == 1 { fixed ^ 1 } else { fixed };
    Some((fixed, (fixed ^ cw).count_ones()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub address: u32,
    pub function: u8,
    /// Decoded as numeric and as alphanumeric; `text` is the likelier one.
    pub numeric: String,
    pub alpha: String,
    pub text: String,
    /// Codeword bits corrected in this page.
    pub corrected: u32,
}

const NUMERIC: [char; 16] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', 'U', ' ', '-', ')', '('];

fn numeric(bits: &[u8]) -> String {
    bits.chunks_exact(4).map(|c| NUMERIC[(c[0] | c[1] << 1 | c[2] << 2 | c[3] << 3) as usize]).collect::<String>().trim_end().to_string()
}

fn alpha(bits: &[u8]) -> String {
    bits.chunks_exact(7)
        .map(|c| c.iter().enumerate().fold(0u8, |a, (i, b)| a | b << i))
        .filter(|&c| c != 0 && c != 0x04) // NUL padding, EOT
        .map(|c| if (0x20..0x7F).contains(&c) || c == b'\n' { c as char } else { '·' })
        .collect()
}

/// The 16 codewords after each frame's sync, as received (polarity already
/// corrected: a negative symbol is a 1 bit).
pub fn batch_codewords(f: &Frame) -> Option<[u32; 16]> {
    let start = f.sync_start + 32;
    let bits = f.soft.get(start..start + 512)?;
    let mut out = [0u32; 16];
    for (k, cw) in out.iter_mut().enumerate() {
        *cw = bits[k * 32..k * 32 + 32].iter().fold(0u32, |a, v| a << 1 | (*v < 0.0) as u32);
    }
    Some(out)
}

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
pub fn pages(frames: &[Frame]) -> Vec<Page> {
    let mut out = Vec::new();
    let mut cur: Option<(Page, Vec<u8>)> = None;
    let finish = |cur: &mut Option<(Page, Vec<u8>)>, out: &mut Vec<Page>| {
        if let Some((mut p, bits)) = cur.take() {
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
    let mut last_at = None;
    for f in frames.iter().filter(|f| f.protocol == "POCSAG") {
        // A gap of more than one batch (544 bits) ends any open page.
        if last_at.is_some_and(|a: usize| f.at_symbol > a + 544 + 8) {
            finish(&mut cur, &mut out);
        }
        last_at = Some(f.at_symbol);
        let Some(cws) = batch_codewords(f) else { continue };
        if cws.iter().filter(|&&raw| correct(raw).is_some_and(|c| c.1 == 0)).count() < CLEAN_PER_BATCH {
            finish(&mut cur, &mut out);
            continue;
        }
        for (k, &raw) in cws.iter().enumerate() {
            let Some((cw, fixed)) = correct(raw) else {
                // Uncorrectable: the page is broken here.
                finish(&mut cur, &mut out);
                continue;
            };
            if cw == IDLE {
                finish(&mut cur, &mut out);
                continue;
            }
            if cw >> 31 == 0 {
                finish(&mut cur, &mut out);
                let address = (cw >> 13 & 0x3_FFFF) << 3 | (k as u32 / 2);
                let function = (cw >> 11 & 3) as u8;
                cur = Some((
                    Page { address, function, numeric: String::new(), alpha: String::new(), text: String::new(), corrected: fixed },
                    Vec::new(),
                ));
            } else if let Some((p, bits)) = cur.as_mut() {
                p.corrected += fixed;
                bits.extend((0..20).rev().map(|i| (cw >> (11 + i) & 1) as u8));
            }
        }
    }
    finish(&mut cur, &mut out);
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
        // Two bit errors in one message codeword.
        cws[16] ^= 1 << 3 | 1 << 17;
        let p = pages(&frames_for(&cws));
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!((p[0].address, p[0].function), (addr, 3));
        assert_eq!(p[0].text, text);
        assert_eq!(p[0].corrected, 2);
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
}
