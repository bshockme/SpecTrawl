//! The P25 half-rate vocoder (TIA-102.BABA-1), used by P25 Phase 2: 72
//! bits per 20 ms frame, 49 of them voice parameters. Synthesis is the
//! full-rate vocoder's (`mbe`).
//!
//! Each frame is 36 dibits, de-interleaved per Annex H into four code
//! vectors: c0 Golay (24,12), c1 Golay (23,12) XORed with a sequence
//! seeded by u0, c2 and c3 uncoded (11 and 14 bits).

use std::f64::consts::PI;

use crate::ambe_tables::{BLOCKS, GAIN, HOC1, HOC2, HOC3, HOC4, INTERLEAVE, PITCH, PRBA24, PRBA58, TONES, VUV};
use crate::mbe::{self, MAX_L, Model};
use crate::p25_lc::{golay_decode, golay23_decode};

/// One frame after error correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coded {
    /// u0, u1 12 bits; u2 11; u3 14.
    pub u: [u16; 4],
    /// Bits corrected in c0 (4: detected but not correctable) and c1.
    pub errors: [u8; 2],
}

/// The 23-bit modulation vector for u0 (BABA-1 5.3).
fn modulation(u0: u16) -> u32 {
    let mut pr = 16 * u0 as u32;
    let mut m = 0;
    for _ in 0..23 {
        pr = (173 * pr + 13849) & 0xFFFF;
        m = m << 1 | pr >> 15;
    }
    m
}

/// Error-corrects a frame's 72 bits (first in time first).
pub fn decode(bits: &[u8]) -> Coded {
    let mut c = [0u32; 4];
    for (s, pair) in INTERLEAVE.iter().enumerate() {
        for (k, &(v, b)) in pair.iter().enumerate() {
            c[v as usize] |= (bits[2 * s + k] as u32 & 1) << b;
        }
    }
    let (u0, e0) = golay_decode(c[0]).map(|(d, e)| (d as u16, e as u8)).unwrap_or(((c[0] >> 12) as u16, 4));
    let (u1, e1) = golay23_decode(c[1] ^ modulation(u0));
    Coded { u: [u0, u1 as u16, c[2] as u16, c[3] as u16], errors: [e0, e1 as u8] }
}

/// The 9 quantizer values b0..b8 (BABA-1 5.1, Tables 5-8).
pub fn unpack(u: &[u16; 4]) -> [u16; 9] {
    let bit = |w: u16, i: u32| w >> i & 1;
    let mut b = [0u16; 9];
    b[0] = (u[0] >> 8 & 0xF) << 3 | (u[3] >> 9 & 7);
    b[1] = (u[0] >> 4 & 0xF) << 1 | bit(u[3], 13);
    b[2] = (u[0] & 0xF) << 1 | bit(u[3], 12);
    b[3] = (u[1] >> 4 & 0xFF) << 1 | bit(u[3], 8);
    b[4] = (u[1] & 0xF) << 3 | (u[3] >> 5 & 7);
    b[5] = (u[2] >> 7 & 0xF) << 1 | bit(u[3], 4);
    b[6] = (u[2] >> 4 & 7) << 1 | bit(u[3], 3);
    b[7] = (u[2] >> 1 & 7) << 1 | bit(u[3], 2);
    b[8] = bit(u[2], 0) << 2 | (u[3] & 3);
    b
}

/// What a frame is (BABA-1 Table 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Voice,
    Erasure,
    Silence,
    Tone,
}

pub fn kind(u: &[u16; 4]) -> Kind {
    if u[0] >> 6 == 63 {
        return Kind::Tone;
    }
    match unpack(u)[0] {
        0..=119 => Kind::Voice,
        120..=123 => Kind::Erasure,
        124..=125 => Kind::Silence,
        _ => Kind::Tone,
    }
}

/// What became of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Decoded,
    Tone,
    Repeated,
    Muted,
}

/// A stateful half-rate decoder.
pub struct Decoder {
    /// Λl(−1): log2 amplitudes of the last voice frame (index 0 is Λ1).
    lambda: [f64; MAX_L],
    l_prev: usize,
    /// γ(−1), the last voice frame's gain.
    gamma: f64,
    /// The previous frame's parameters, for repeats.
    prev: Model,
    rate: f64,
    repeats: u32,
    enhancer: mbe::Enhancer,
    synth: mbe::Synth,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder {
            lambda: [1.0; MAX_L],
            l_prev: 15,
            gamma: 0.0,
            prev: Model::initial(),
            rate: 0.0,
            repeats: 0,
            enhancer: Default::default(),
            synth: Default::default(),
        }
    }
}

impl Decoder {
    /// Decodes one frame into 160 samples (16-bit scale).
    pub fn decode(&mut self, c: &Coded) -> ([f64; mbe::N], Outcome) {
        let total = c.errors[0] as u32 + c.errors[1] as u32;
        self.rate = 0.95 * self.rate + 0.001064 * total as f64;
        let k = kind(&c.u);
        let bad = k == Kind::Erasure || c.errors[0] >= 4 || (c.errors[0] >= 2 && total >= 6);
        let (model, mut outcome) = match (bad, k) {
            (true, _) => (None, Outcome::Repeated),
            (false, Kind::Tone) => match tone(&c.u) {
                Some(m) => (Some(m), Outcome::Tone),
                None => (None, Outcome::Repeated),
            },
            (false, _) => (Some(self.model(&unpack(&c.u), k == Kind::Voice)), Outcome::Decoded),
        };
        self.repeats = if model.is_none() { self.repeats + 1 } else { 0 };
        let model = model.unwrap_or_else(|| self.prev.clone());
        self.prev = model.clone();
        // BABA-1 5.7: mute on a high error rate or a fourth repeat in a row.
        if self.rate > 0.096 || self.repeats >= 4 {
            outcome = Outcome::Muted;
            return (self.synth.mute(), outcome);
        }
        let err = mbe::Errors { total, e4: 0, rate: self.rate };
        let mut enhanced = if outcome == Outcome::Tone { model } else { self.enhancer.apply(&model, err) };
        // As for IMBE: repeats fade rather than buzz.
        if self.repeats > 0 {
            let g = 0.5f64.powi(self.repeats as i32);
            enhanced.m.iter_mut().for_each(|m| *m *= g);
        }
        (self.synth.synth(&enhanced), outcome)
    }

    /// The model parameters of a voice or silence frame (BABA-1 4.1-4.4).
    /// Only voice frames update the predictor's state.
    fn model(&mut self, b: &[u16; 9], voice: bool) -> Model {
        let (w0, l) = if voice {
            let (l, f) = PITCH[b[0] as usize];
            (2.0 * PI * f, l as usize)
        } else {
            (2.0 * PI / 32.0, 14)
        };
        let mut m = Model { w0, l, voiced: [false; MAX_L], m: [0.0; MAX_L] };
        if voice {
            for i in 1..=l {
                let j = ((16.0 * i as f64 * w0 / (2.0 * PI)) as usize).min(7);
                m.voiced[i] = VUV[b[1] as usize][j];
            }
        }
        // Gain, PRBA vector and the four DCT blocks.
        let gamma = GAIN[b[2] as usize] + 0.5 * self.gamma;
        let mut g = [0.0; 9];
        g[2..5].copy_from_slice(&PRBA24[b[3] as usize]);
        g[5..9].copy_from_slice(&PRBA58[b[4] as usize]);
        let alpha = |k: usize| if k == 1 { 1.0 } else { 2.0 };
        let r: Vec<f64> =
            (1..=8).map(|i| (1..=8).map(|k| alpha(k) * g[k] * (PI * (k - 1) as f64 * (i as f64 - 0.5) / 8.0).cos()).sum()).collect();
        let hoc = [&HOC1[b[5] as usize], &HOC2[b[6] as usize], &HOC3[b[7] as usize], &HOC4[b[8] as usize]];
        let mut t = Vec::with_capacity(l);
        for (i, &ji) in BLOCKS[l - 9].iter().enumerate() {
            let ji = ji as usize;
            let mut c = vec![0.0; ji + 1];
            c[1] = (r[2 * i] + r[2 * i + 1]) / 2.0;
            c[2] = (r[2 * i] - r[2 * i + 1]) / (2.0 * 2f64.sqrt());
            // Higher order coefficients 3..=6 from the HOC vector (eq. 37).
            let n = ji.min(6).saturating_sub(2);
            c[3..3 + n].copy_from_slice(&hoc[i][..n]);
            for j in 1..=ji {
                t.push((1..=ji).map(|k| alpha(k) * c[k] * (PI * (k - 1) as f64 * (j as f64 - 0.5) / ji as f64).cos()).sum());
            }
        }
        // Λl = Tl + prediction from the last voice frame + Γ (eq. 40-45).
        let lp = self.l_prev;
        let prev = |i: usize| self.lambda[i.clamp(1, lp) - 1];
        let pred: Vec<f64> = (1..=l)
            .map(|i| {
                let kl = lp as f64 / l as f64 * i as f64;
                let (f, d) = (kl.floor() as usize, kl.fract());
                (1.0 - d) * prev(f) + d * prev(f + 1)
            })
            .collect();
        let mean_pred = pred.iter().sum::<f64>() / l as f64;
        let big_gamma = gamma - 0.5 * (l as f64).log2() - t.iter().sum::<f64>() / l as f64;
        let mut lambda = [0.0; MAX_L];
        for i in 1..=l {
            lambda[i - 1] = t[i - 1] + 0.65 * (pred[i - 1] - mean_pred) + big_gamma;
            let a = lambda[i - 1].exp2();
            m.m[i] = if m.voiced[i] { a } else { 0.2046 / w0.sqrt() * a };
        }
        if voice {
            self.lambda = lambda;
            self.l_prev = l;
            self.gamma = gamma;
        }
        m
    }
}

/// A tone frame's model (BABA-1 7.2, 8): the tone index ID (sent four
/// times) and amplitude AD select a fundamental and the one or two
/// harmonics carrying the tone.
fn tone(u: &[u16; 4]) -> Option<Model> {
    let id = (u[1] >> 4) as u8;
    let ad = ((u[0] & 0x3F) << 1 | (u[3] >> 4 & 1)) as f64;
    let &(_, f0, l1, l2) = TONES.iter().find(|t| t.0 == id)?;
    let w0 = 2.0 * PI * f0 / 8000.0;
    let l = ((3812.5 / f0) as usize).clamp(1, MAX_L - 1);
    let mut m = Model { w0, l, voiced: [false; MAX_L], m: [0.0; MAX_L] };
    let amp = if id == 255 { 0.0 } else { 16384.0 * 10f64.powf(0.03555 * (ad - 127.0)) };
    for h in [l1 as usize, l2 as usize] {
        if (1..=l).contains(&h) {
            m.voiced[h] = true;
            m.m[h] = amp;
        }
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p25_lc::golay_encode;

    /// Encodes u0..u3 per BABA-1 5.2-5.4.
    fn encode(u: [u16; 4]) -> Vec<u8> {
        let c = [golay_encode(u[0] as u32), ((golay_encode(u[1] as u32) >> 1) ^ modulation(u[0])), u[2] as u32, u[3] as u32];
        INTERLEAVE.iter().flat_map(|p| p.iter().map(|&(v, b)| (c[v as usize] >> b & 1) as u8)).collect()
    }

    #[test]
    fn interleave_covers_every_bit_once() {
        let lens = [24, 23, 11, 14];
        let mut seen = [0u32; 4];
        for pair in INTERLEAVE {
            for (v, b) in pair {
                assert_eq!(seen[v as usize] >> b & 1, 0);
                seen[v as usize] |= 1 << b;
            }
        }
        for (v, s) in seen.iter().enumerate() {
            assert_eq!(*s, (1u32 << lens[v]) - 1);
        }
    }

    #[test]
    fn round_trip_with_errors() {
        let u = [0xA5C, 0x3F1, 0x5A5, 0x2DB4];
        let mut bits = encode(u);
        assert_eq!(decode(&bits), Coded { u, errors: [0, 0] });
        // Two errors in c0 and two in c1 (first bits of dibits 0, 2 are c0;
        // dibit 1's first bit is c1, and so on per Annex H).
        let pos: Vec<usize> = (0..72).filter(|&i| INTERLEAVE[i / 2][i % 2].0 == 0).take(2).collect();
        let pos1: Vec<usize> = (0..72).filter(|&i| INTERLEAVE[i / 2][i % 2].0 == 1).take(2).collect();
        for &p in pos.iter().chain(&pos1) {
            bits[p] ^= 1;
        }
        let got = decode(&bits);
        assert_eq!(got.u, u);
        assert_eq!(got.errors, [2, 2]);
    }

    #[test]
    fn unpack_places_every_bit() {
        // Setting each u bit alone sets exactly one b bit, and all 49 are
        // reached.
        let widths = [7, 5, 5, 9, 7, 5, 4, 4, 3];
        let lens = [12, 12, 11, 14];
        let mut reached = std::collections::HashSet::new();
        for (v, &n) in lens.iter().enumerate() {
            for bit in 0..n {
                let mut u = [0u16; 4];
                u[v] = 1 << bit;
                let b = unpack(&u);
                let set: Vec<(usize, u32)> =
                    b.iter().enumerate().flat_map(|(i, &x)| (0..16).filter(move |j| x >> j & 1 == 1).map(move |j| (i, j))).collect();
                assert_eq!(set.len(), 1, "u{v} bit {bit}");
                assert!(set[0].1 < widths[set[0].0]);
                reached.insert(set[0]);
            }
        }
        assert_eq!(reached.len(), 49);
    }

    #[test]
    fn tones_decode_to_their_frequency() {
        // DTMF "1": 1209 + 697 Hz.
        let mut u = [63 << 6 | 0x3F, 129 << 4 | (129 >> 4), 0, 0];
        u[3] = 1 << 4;
        assert_eq!(kind(&u), Kind::Tone);
        let m = tone(&u).unwrap();
        let f = |h: usize| h as f64 * m.w0 * 8000.0 / (2.0 * PI);
        let hs: Vec<usize> = (1..=m.l).filter(|&h| m.voiced[h]).collect();
        let fs: Vec<f64> = hs.iter().map(|&h| f(h)).collect();
        assert!(fs.iter().any(|&x| (x - 697.0).abs() < 15.0) && fs.iter().any(|&x| (x - 1209.0).abs() < 15.0), "{fs:?}");
    }
}
