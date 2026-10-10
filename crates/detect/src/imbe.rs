//! IMBE voice frames (P25 Phase 1, TIA-102.BABA): from the 144 bits a
//! voice frame occupies on air to the eight prioritized bit vectors u0..u7
//! the vocoder's parameters are unpacked from.
//!
//! Each frame is 72 dibits, de-interleaved per BABA Annex H into eight code
//! vectors: c0..c3 Golay (23,12), c4..c6 Hamming (15,11), c7 seven bits
//! uncoded. c1..c6 are also XORed with a pseudo-random sequence seeded by
//! u0 (BABA 7.4), so a c0 beyond repair garbles the rest; the error counts
//! catch that (BABA 7.6, 7.7).

use std::f64::consts::PI;

use crate::frames::Frame;
use crate::imbe_tables::{BLOCKS, DCT_BITS, GAIN_ALLOC, GAIN_LEVELS};
use crate::mbe::{self, Model, MAX_L};
use crate::p25::data_dibits;
use crate::p25_lc::golay23_decode;

/// Code vector lengths: c0..c7.
const LEN: [usize; 8] = [23, 23, 23, 23, 15, 15, 15, 7];

/// BABA Annex H: for each of the 72 dibits, the (vector, bit) carried by
/// its first and second bit. Bit numbers count from the LSB.
const INTERLEAVE: [[(u8, u8); 2]; 72] = [
    [(0, 22), (1, 21)],
    [(2, 20), (3, 19)],
    [(4, 10), (5, 1)],
    [(1, 20), (0, 21)],
    [(3, 18), (2, 19)],
    [(5, 0), (4, 9)],
    [(0, 20), (1, 19)],
    [(2, 18), (3, 17)],
    [(4, 8), (6, 14)],
    [(1, 18), (0, 19)],
    [(3, 16), (2, 17)],
    [(6, 13), (4, 7)],
    [(0, 18), (1, 17)],
    [(2, 16), (3, 15)],
    [(4, 6), (6, 12)],
    [(1, 16), (0, 17)],
    [(3, 14), (2, 15)],
    [(6, 11), (4, 5)],
    [(0, 16), (1, 15)],
    [(2, 14), (3, 13)],
    [(4, 4), (6, 10)],
    [(1, 14), (0, 15)],
    [(3, 12), (2, 13)],
    [(6, 9), (4, 3)],
    [(0, 14), (1, 13)],
    [(2, 12), (3, 11)],
    [(4, 2), (6, 8)],
    [(1, 12), (0, 13)],
    [(3, 10), (2, 11)],
    [(6, 7), (4, 1)],
    [(0, 12), (1, 11)],
    [(2, 10), (3, 9)],
    [(4, 0), (6, 6)],
    [(1, 10), (0, 11)],
    [(3, 8), (2, 9)],
    [(6, 5), (5, 14)],
    [(0, 10), (1, 9)],
    [(2, 8), (3, 7)],
    [(5, 13), (6, 4)],
    [(1, 8), (0, 9)],
    [(3, 6), (2, 7)],
    [(6, 3), (5, 12)],
    [(0, 8), (1, 7)],
    [(2, 6), (3, 5)],
    [(5, 11), (6, 2)],
    [(1, 6), (0, 7)],
    [(3, 4), (2, 5)],
    [(6, 1), (5, 10)],
    [(0, 6), (1, 5)],
    [(2, 4), (3, 3)],
    [(5, 9), (6, 0)],
    [(1, 4), (0, 5)],
    [(3, 2), (2, 3)],
    [(7, 6), (5, 8)],
    [(0, 4), (1, 3)],
    [(2, 2), (3, 1)],
    [(5, 7), (7, 5)],
    [(1, 2), (0, 3)],
    [(3, 0), (2, 1)],
    [(7, 4), (5, 6)],
    [(0, 2), (1, 1)],
    [(2, 0), (4, 14)],
    [(5, 5), (7, 3)],
    [(1, 0), (0, 1)],
    [(4, 13), (3, 22)],
    [(7, 2), (5, 4)],
    [(0, 0), (2, 22)],
    [(3, 21), (4, 12)],
    [(5, 3), (7, 1)],
    [(2, 21), (1, 22)],
    [(4, 11), (3, 20)],
    [(7, 0), (5, 2)],
];

/// Hamming (15,11) parity (BABA 7.3, gH): the 4 parity bits for each data
/// bit, data MSB first.
const HAMMING: [u16; 11] = [0xF, 0xE, 0xD, 0xC, 0xB, 0xA, 0x9, 0x7, 0x6, 0x5, 0x3];

fn hamming15_parity(data: u16) -> u16 {
    (0..11).filter(|i| data >> (10 - i) & 1 == 1).fold(0, |p, i| p ^ HAMMING[i])
}

/// Corrects a received Hamming (15,11) word (data in bits 14..4): (11 data
/// bits, bits fixed). A syndrome matching no single error is left as is
/// and counted as one.
pub fn hamming15_decode(w: u16) -> (u16, u32) {
    let s = hamming15_parity(w >> 4) ^ (w & 0xF);
    if s == 0 {
        return (w >> 4, 0);
    }
    // Single errors: a parity bit's own syndrome is 1 << bit; a data bit's
    // is its row.
    match HAMMING.iter().position(|&r| r == s) {
        Some(i) => ((w >> 4) ^ 1 << (10 - i), 1),
        None => (w >> 4, 1),
    }
}

/// The modulation vectors m1..m6 (BABA 7.4) for u0: bit n of the
/// sequence is the top bit of pr(n), pr(0) = 16·u0, pr(n) = 173·pr(n−1) +
/// 13849 mod 65536. Each vector takes the next `LEN` bits, MSB first.
fn modulation(u0: u16) -> [u32; 8] {
    let mut m = [0u32; 8];
    let mut pr = 16 * u0 as u32;
    for (i, v) in m.iter_mut().enumerate().take(7).skip(1) {
        for _ in 0..LEN[i] {
            pr = (173 * pr + 13849) & 0xFFFF;
            *v = *v << 1 | pr >> 15;
        }
    }
    m
}

/// One IMBE frame after error correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coded {
    /// u0..u3 12 bits, u4..u6 11 bits, u7 7 bits.
    pub u: [u16; 8],
    /// Bits corrected in c0..c6 (ε0..ε6).
    pub errors: [u8; 7],
}

impl Coded {
    /// εT, the total corrected.
    pub fn total_errors(&self) -> u32 {
        self.errors.iter().map(|&e| e as u32).sum()
    }
}

/// The eight code vectors of a frame's 72 dibits, in air order.
pub fn deinterleave(dibits: &[u8]) -> [u32; 8] {
    let mut c = [0u32; 8];
    for (d, pair) in dibits.iter().zip(INTERLEAVE.iter()) {
        for (k, &(v, b)) in pair.iter().enumerate() {
            c[v as usize] |= ((d >> (1 - k) & 1) as u32) << b;
        }
    }
    c
}

/// Error-corrects a frame's 72 dibits.
pub fn decode(dibits: &[u8]) -> Coded {
    let mut c = deinterleave(dibits);
    let mut u = [0u16; 8];
    let mut errors = [0u8; 7];
    let (u0, e0) = golay23_decode(c[0]);
    u[0] = u0 as u16;
    errors[0] = e0 as u8;
    let m = modulation(u[0]);
    for i in 1..7 {
        c[i] ^= m[i];
        let (d, e) = if i < 4 {
            golay23_decode(c[i])
        } else {
            let (d, e) = hamming15_decode(c[i] as u16);
            (d as u32, e)
        };
        u[i] = d as u16;
        errors[i] = e as u8;
    }
    u[7] = c[7] as u16;
    Coded { u, errors }
}

/// Where the nine voice frames of an LDU sit in its data dibits (after the
/// NID, status symbols removed): two frames, then six pieces of link
/// control or encryption sync (20 dibits) each followed by a frame, then
/// the low speed data (16 dibits) and the last frame.
pub const LDU_VOICE: [usize; 9] = [0, 72, 164, 256, 348, 440, 532, 624, 712];

/// The nine voice frames of an LDU1 or LDU2 (sliced with at least ~880
/// dibits after the sync), error corrected.
pub fn ldu_frames(f: &Frame) -> Option<[Coded; 9]> {
    let d = data_dibits(f, 784)?;
    Some(LDU_VOICE.map(|at| decode(&d[at..at + 72])))
}

// ------------------------------------------------- parameter decoding ----

/// The quantizer values of a frame, unpacked from u0..u7 per BABA 7.1:
/// b0 (pitch), b1 (V/UV), b2 (gain), b3..b(L+1) (spectral amplitudes),
/// b(L+2) (sync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quantized {
    pub b: Vec<u16>,
    /// The bit count of each b.
    pub bits: Vec<u8>,
    pub l: usize,
    pub k: usize,
}

/// ω0, L and K for a pitch code b0 ≤ 207 (BABA eq. 46-48).
pub fn harmonics(b0: u16) -> (f64, usize, usize) {
    let w0 = 4.0 * PI / (b0 as f64 + 39.5);
    let l = (0.9254 * (PI / w0 + 0.25).floor()).floor() as usize;
    let k = if l <= 36 { l.div_ceil(3) } else { 12 };
    (w0, l, k)
}

/// b0, from u0 and u7.
pub fn pitch_code(u: &[u16; 8]) -> u16 {
    (u[0] >> 6) << 2 | (u[7] >> 1 & 3)
}

/// The order Figure 22 scans b3..b(L+1) in: row by row from bit 10 down,
/// left to right; (m, bit) for each allocated bit.
fn scan_order(bits: &[u8], l: usize) -> Vec<(usize, u8)> {
    (0..11u8).rev().flat_map(|r| (3..=l + 1).filter(move |&m| bits[m] > r).map(move |m| (m, r))).collect()
}

/// Unpacks u0..u7; None for a reserved pitch code (b0 > 207).
pub fn unpack(u: &[u16; 8]) -> Option<Quantized> {
    let b0 = pitch_code(u);
    if b0 > 207 {
        return None;
    }
    let (_, l, k) = harmonics(b0);
    let mut bits = vec![0u8; l + 3];
    bits[0] = 8;
    bits[1] = k as u8;
    bits[2] = 6;
    for (m, &(nb, _)) in GAIN_ALLOC[l - 9].iter().enumerate() {
        bits[3 + m] = nb;
    }
    bits[8..=l + 1].copy_from_slice(&DCT_BITS[l - 9][..l - 6]);
    bits[l + 2] = 1;
    // The bits in priority order: u0..u3 MSB first, then u4..u6 and the top
    // three of u7.
    let word = |i: usize| {
        let n = if i < 4 { 12 } else { 11 };
        (0..n).rev().map(move |j| u[i] >> j & 1)
    };
    let hi: Vec<u16> = (0..4).flat_map(word).collect();
    let lo: Vec<u16> = (4..7).flat_map(word).chain((4..7).rev().map(|j| u[7] >> j & 1)).collect();
    let mut b = vec![0u16; l + 3];
    b[0] = b0;
    let mut scan = scan_order(&bits, l).into_iter();
    // u0: b0 bits 7..2 (known already), b2 bits 5..3, then the scan.
    b[2] |= hi[6] << 5 | hi[7] << 4 | hi[8] << 3;
    for &v in &hi[9..] {
        let (m, r) = scan.next()?;
        b[m] |= v << r;
    }
    // u4 on: b1 MSB first, b2 bits 2 and 1, then the rest of the scan.
    for &v in &lo[..k] {
        b[1] = b[1] << 1 | v;
    }
    b[2] |= lo[k] << 2 | lo[k + 1] << 1;
    for &v in &lo[k + 2..] {
        let (m, r) = scan.next()?;
        b[m] |= v << r;
    }
    debug_assert!(scan.next().is_none());
    b[2] |= u[7] >> 3 & 1;
    b[l + 2] = u[7] & 1;
    Some(Quantized { b, bits, l, k })
}

/// Table 3: step size per bit count, in standard deviations.
const STEP: [f64; 11] = [0.0, 1.2, 0.85, 0.65, 0.40, 0.28, 0.15, 0.08, 0.04, 0.02, 0.01];
/// Table 4: the standard deviation of DCT coefficient k (2..=10).
const SIGMA: [f64; 11] = [0.0, 0.0, 0.307, 0.241, 0.207, 0.190, 0.179, 0.173, 0.165, 0.170, 0.170];

/// A uniform quantizer's level (eq. 68, 71).
fn level(b: u16, bits: u8, step: f64) -> f64 {
    if bits == 0 { 0.0 } else { step * (b as f64 - (1u32 << (bits - 1)) as f64 + 0.5) }
}

/// The prediction residuals T1..TL (BABA 6.4): the gain vector and higher
/// order DCT coefficients, inverse transformed block by block.
fn residuals(q: &Quantized) -> Vec<f64> {
    let l = q.l;
    let alpha = |k: usize| if k == 1 { 1.0 } else { 2.0 };
    // The transformed gain vector G1..G6, then its inverse DCT R1..R6.
    let mut g = [0.0; 7];
    g[1] = GAIN_LEVELS[q.b[2] as usize];
    for m in 3..=7 {
        g[m - 1] = level(q.b[m], q.bits[m], GAIN_ALLOC[l - 9][m - 3].1);
    }
    let r: Vec<f64> = (1..=6)
        .map(|i| (1..=6).map(|m| alpha(m) * g[m] * (PI * (m - 1) as f64 * (i as f64 - 0.5) / 6.0).cos()).sum())
        .collect();
    let mut t = Vec::with_capacity(l);
    let mut bm = 8;
    for (i, &ji) in BLOCKS[l - 9].iter().enumerate() {
        let ji = ji as usize;
        let mut c = vec![0.0; ji + 1];
        c[1] = r[i];
        for (k, ck) in c.iter_mut().enumerate().skip(2) {
            *ck = level(q.b[bm], q.bits[bm], STEP[q.bits[bm] as usize] * SIGMA[k]);
            bm += 1;
        }
        for j in 1..=ji {
            t.push((1..=ji).map(|k| alpha(k) * c[k] * (PI * (k - 1) as f64 * (j as f64 - 0.5) / ji as f64).cos()).sum());
        }
    }
    t
}

/// What became of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Decoded,
    /// Too damaged: the previous frame's parameters again.
    Repeated,
    /// The channel is too bad: comfort noise.
    Muted,
}

/// A stateful IMBE decoder: error-corrected frames in, 8 kHz speech out.
pub struct Decoder {
    /// log2 M̃l(−1), the unenhanced amplitudes the predictor works from
    /// (index 0 is M̃0 = 1).
    log_m: [f64; MAX_L],
    /// The previous frame's parameters, for repeats.
    prev: Model,
    /// εR.
    rate: f64,
    /// Frames repeated in a row.
    repeats: u32,
    enhancer: mbe::Enhancer,
    synth: mbe::Synth,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder { log_m: [0.0; MAX_L], prev: Model::initial(), rate: 0.0, repeats: 0, enhancer: Default::default(), synth: Default::default() }
    }
}

impl Decoder {
    /// Decodes one frame into 160 samples (16-bit scale).
    pub fn decode(&mut self, c: &Coded) -> ([f64; mbe::N], Outcome) {
        let total = c.total_errors();
        self.rate = 0.95 * self.rate + 0.000365 * total as f64;
        let garbled = c.errors[0] >= 2 && total as f64 >= 10.0 + 40.0 * self.rate;
        let (model, outcome) = match unpack(&c.u).filter(|_| !garbled) {
            Some(q) => (self.model(&q), Outcome::Decoded),
            None => (self.prev.clone(), Outcome::Repeated),
        };
        self.prev = model.clone();
        if self.rate > 0.0875 {
            return (self.synth.mute(), Outcome::Muted);
        }
        let err = mbe::Errors { total, e4: c.errors[4] as u32, rate: self.rate };
        let mut enhanced = self.enhancer.apply(&model, err);
        // Repeating a frame for long holds one sound as a buzz (BABA only
        // mutes on the error rate). Each repeat in a row halves the level;
        // after three the synthesis fades to silence until a good frame.
        self.repeats = if outcome == Outcome::Repeated { self.repeats + 1 } else { 0 };
        if self.repeats > 0 {
            let g = if self.repeats > 3 { 0.0 } else { 0.5f64.powi(self.repeats as i32) };
            enhanced.m.iter_mut().for_each(|m| *m *= g);
        }
        (self.synth.synth(&enhanced), outcome)
    }

    /// The spectral amplitudes, from the residuals and the previous frame's
    /// (eq. 75-79), and V/UV per harmonic (eq. 50-51).
    fn model(&mut self, q: &Quantized) -> Model {
        let (w0, l, k) = harmonics(q.b[0]);
        let t = residuals(q);
        let lp = self.prev.l;
        let rho = match l {
            0..=15 => 0.4,
            16..=24 => 0.03 * l as f64 - 0.05,
            _ => 0.7,
        };
        let prev = |i: usize| self.log_m[i.min(lp)];
        let pred: Vec<f64> = (1..=l)
            .map(|i| {
                let kl = lp as f64 / l as f64 * i as f64;
                let (f, d) = (kl.floor() as usize, kl.fract());
                (1.0 - d) * prev(f) + d * prev(f + 1)
            })
            .collect();
        let mean = pred.iter().sum::<f64>() / l as f64;
        let mut log_m = [0.0; MAX_L];
        let mut m = Model { w0, l, voiced: [false; MAX_L], m: [0.0; MAX_L] };
        for i in 1..=l {
            log_m[i] = t[i - 1] + rho * (pred[i - 1] - mean);
            m.m[i] = log_m[i].exp2();
            let kappa = if i <= 36 { i.div_ceil(3) } else { 12 };
            m.voiced[i] = q.b[1] >> (k - kappa) & 1 == 1;
        }
        self.log_m = log_m;
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BABA section 10's example: L = 16, K = 6. Tables 6 and 7 give the
    /// allocation, Tables 8-10 where the bits go. (Its ω0 = 2π/35.125 gives
    /// L = 15 by eq. 47, whose inner floor the 9..=56 range confirms; b0 =
    /// 32 is the first code with L = 16.)
    #[test]
    fn unpack_matches_encoding_example() {
        let b0 = 32;
        assert_eq!(harmonics(0).1, 9);
        assert_eq!(harmonics(207).1, 56);
        assert_eq!(harmonics(b0).1, 16);
        assert_eq!(harmonics(b0).2, 6);
        assert_eq!(&DCT_BITS[16 - 9][..10], &[6, 6, 5, 4, 4, 3, 3, 3, 3, 2]);
        assert_eq!(GAIN_ALLOC[16 - 9].map(|x| x.0), [6, 6, 6, 5, 5]);
        let pitch = |u: &mut [u16; 8]| {
            u[0] |= (b0 >> 2) << 6;
            u[7] |= (b0 & 3) << 1;
        };
        // Table 8: u0's last three bits are the top bits of b3, b4, b5; u1
        // starts with the top bits of b8 and b9.
        let mut u = [0u16; 8];
        pitch(&mut u);
        u[0] |= 0b101;
        u[1] = 0b11 << 10;
        let q = unpack(&u).unwrap();
        assert_eq!((q.b[3], q.b[4], q.b[5], q.b[8], q.b[9]), (0b100000, 0, 0b100000, 0b100000, 0b100000));
        // Table 9: u4 is b1, then b2's bits 2 and 1, then bit 1 of b5..b7.
        let mut u = [0u16; 8];
        pitch(&mut u);
        u[4] = 0b101_1011_0101; // b1 101101, b2 bits 10, then 101
        let q = unpack(&u).unwrap();
        assert_eq!((q.b[1], q.b[2]), (0b101101, 0b100));
        assert_eq!((q.b[5], q.b[6], q.b[7]), (0b10, 0, 0b10));
    }

    #[test]
    fn interleave_covers_every_bit_once() {
        let mut seen = [0u32; 8];
        for pair in INTERLEAVE {
            for (v, b) in pair {
                assert!((b as usize) < LEN[v as usize]);
                assert_eq!(seen[v as usize] >> b & 1, 0, "c{v}({b}) twice");
                seen[v as usize] |= 1 << b;
            }
        }
        for (v, s) in seen.iter().enumerate() {
            assert_eq!(*s, (1u32 << LEN[v]) - 1, "c{v}");
        }
    }

    /// Encodes u0..u7 the way BABA 7.3-7.5 describes, for round trips.
    fn encode(u: [u16; 8]) -> Vec<u8> {
        let golay = |d: u16| (d as u32) << 11 | crate::p25_lc::golay_encode(d as u32) >> 1 & 0x7FF;
        let m = modulation(u[0]);
        let mut c = [0u32; 8];
        for i in 0..8 {
            c[i] = match i {
                0..4 => golay(u[i]),
                4..7 => (u[i] as u32) << 4 | hamming15_parity(u[i]) as u32,
                _ => u[i] as u32,
            } ^ m[i];
        }
        INTERLEAVE.iter().map(|p| p.iter().fold(0u8, |a, &(v, b)| a << 1 | (c[v as usize] >> b & 1) as u8)).collect()
    }

    #[test]
    fn round_trip_with_errors() {
        let u = [0xABC, 0x123, 0xFED, 0x456, 0x7FF, 0x2AA, 0x555, 0x5A];
        let mut d = encode(u);
        assert_eq!(decode(&d), Coded { u, errors: [0; 7] });
        // Three errors in c0, one in each Hamming word.
        for s in [0, 6, 12, 2, 38, 11] {
            d[s] ^= 0b10;
        }
        let got = decode(&d);
        assert_eq!(got.u, u);
        assert_eq!(got.errors[0], 3);
        assert!(got.errors[4..7].iter().all(|&e| e == 1), "{:?}", got.errors);
    }

    #[test]
    fn hamming_corrects_any_single_error() {
        for data in [0u16, 0x7FF, 0x2A5, 0x412] {
            let w = data << 4 | hamming15_parity(data);
            for b in 0..15 {
                assert_eq!(hamming15_decode(w ^ 1 << b), (data, 1));
            }
        }
    }
}
