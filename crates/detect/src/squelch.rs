//! Sub-audible squelch signalling under analog FM: CTCSS tones and DCS
//! codes. Both identify an FM transmitter the way a NAC identifies a P25
//! site: a repeater keeps the same tone or code for years.
//!
//! **DCS** (Digital Coded Squelch, Motorola's DPL) is a 23-bit Golay word
//! sent continuously at 134.4 bit/s, NRZ, swinging the carrier about
//! ±0.5–1 kHz. Its 12 data bits are the octal code (9 bits, lowest first)
//! followed by the fixed bits 100 (so bit 11 is set); the 11 parity bits
//! follow. Bit 0 goes out first, so the stream is cyclic with period 23 and
//! the word's start is found from the fixed bits. The code is cyclic and
//! contains the all-ones word, so a complemented stream is another valid
//! code: the receiver's "inverted" code. The published list of 104
//! standard codes avoids codes that are rotations of each other, which is
//! how a rotation is picked when several fit. On a scanner the code is the
//! low buzz under an idle repeater.
//!
//! **CTCSS** is one tone from the EIA table (67.0–254.1 Hz) swinging the
//! carrier a few hundred hertz, under voice that transmitters high-pass at
//! 300 Hz.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

/// Golay(23,12) generator, x^11+x^10+x^6+x^5+x^4+x^2+1, applied with the
/// first-sent bit as bit 0. (On air the complement of code 205 decoded as
/// 263 under this convention, which is the published inversion pair; the
/// reciprocal polynomial, or the other bit order, fits nothing.)
const GOLAY: u32 = 0xC75;
/// The twelve data bits of a code word: the code and the fixed 100.
const FIXED: u32 = 0b100 << 9;
pub const DCS_BAUD: f64 = 134.4;
const WORD: usize = 23;

/// The 104 standard DCS codes (octal), as radios list them.
pub const DCS_STANDARD: [u16; 104] = [
    0o023, 0o025, 0o026, 0o031, 0o032, 0o036, 0o043, 0o047, 0o051, 0o053, 0o054, 0o065, 0o071, 0o072, 0o073, 0o074, 0o114, 0o115, 0o116,
    0o122, 0o125, 0o131, 0o132, 0o134, 0o143, 0o145, 0o152, 0o155, 0o156, 0o162, 0o165, 0o172, 0o174, 0o205, 0o212, 0o223, 0o225, 0o226,
    0o243, 0o244, 0o245, 0o246, 0o251, 0o252, 0o255, 0o261, 0o263, 0o265, 0o266, 0o271, 0o274, 0o306, 0o311, 0o315, 0o325, 0o331, 0o332,
    0o343, 0o346, 0o351, 0o356, 0o364, 0o365, 0o371, 0o411, 0o412, 0o413, 0o423, 0o431, 0o432, 0o445, 0o446, 0o452, 0o454, 0o455, 0o462,
    0o464, 0o465, 0o466, 0o503, 0o506, 0o516, 0o523, 0o526, 0o532, 0o546, 0o565, 0o606, 0o612, 0o624, 0o627, 0o631, 0o632, 0o654, 0o662,
    0o664, 0o703, 0o712, 0o723, 0o731, 0o732, 0o734, 0o743, 0o754,
];

/// Standard CTCSS tones (EIA), Hz.
pub const CTCSS: [f32; 50] = [
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8,
    136.5, 141.3, 146.2, 150.0, 151.4, 156.7, 159.8, 162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6,
    199.5, 203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Squelch {
    /// A standard CTCSS tone, Hz.
    Ctcss(f32),
    /// A DCS code (octal value) read with a 1 as positive deviation, and
    /// the code the same stream reads as with the opposite polarity.
    Dcs { code: u16, inverted: u16 },
}

impl Squelch {
    /// "CTCSS 100.0 Hz", "DCS 205".
    pub fn label(&self) -> String {
        match self {
            Squelch::Ctcss(t) => format!("CTCSS {t:.1} Hz"),
            Squelch::Dcs { code, .. } => format!("DCS {code:03o}"),
        }
    }
}

/// What `measure` found, with how much backed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub squelch: Squelch,
    /// DCS: whole words the stream held (each a repeat of the code). CTCSS: 1.
    pub words: usize,
}

/// Sub-audible signalling in `disc`, the instantaneous frequency (Hz) of a
/// contiguous stretch of carrier at `fs`. Needs about 0.4 s.
pub fn measure(disc: &[f32], fs: f64) -> Option<Measurement> {
    // Down to ~2 kHz: box average, then a windowed-sinc low-pass at 300 Hz.
    let dec = (fs / 2000.0).floor().max(1.0) as usize;
    let fd = fs / dec as f64;
    if disc.len() / dec < (0.35 * fd) as usize {
        return None;
    }
    let boxed: Vec<f32> = disc.chunks_exact(dec).map(|c| c.iter().sum::<f32>() / dec as f32).collect();
    let lp = lowpass(&boxed, fd, 300.0);
    if let Some(m) = dcs(&lp, fd) {
        return Some(m);
    }
    ctcss(&lp, fd).map(|t| Measurement { squelch: Squelch::Ctcss(t), words: 1 })
}

/// Windowed-sinc FIR low-pass, 65 taps, delay compensated (the output is
/// shorter by the taps).
fn lowpass(x: &[f32], fs: f64, cutoff_hz: f64) -> Vec<f32> {
    const TAPS: usize = 65;
    if x.len() <= TAPS {
        return Vec::new();
    }
    let fc = cutoff_hz / fs;
    let mid = (TAPS / 2) as f64;
    let mut h: Vec<f32> = (0..TAPS)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t) };
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos();
            (sinc * w) as f32
        })
        .collect();
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    x.windows(TAPS).map(|w| w.iter().zip(&h).map(|(a, b)| a * b).sum()).collect()
}

/// Least swing between the two levels, Hz: DCS deviation is 0.5–1 kHz by
/// design; less than this is a tone's ripple or noise.
const DCS_MIN_SWING: f32 = 350.0;
/// Fraction of bits that must equal the bit one word later.
const DCS_PERIODIC: f32 = 0.9;

fn dcs(lp: &[f32], fd: f64) -> Option<Measurement> {
    let mut s = lp.to_vec();
    let n = s.len();
    if n < 64 {
        return None;
    }
    let p10 = *s.select_nth_unstable_by(n / 10, f32::total_cmp).1;
    let p90 = *s.select_nth_unstable_by(n * 9 / 10, f32::total_cmp).1;
    if p90 - p10 < DCS_MIN_SWING {
        return None;
    }
    let thr = (p10 + p90) / 2.0;
    // Bit clock: the phase at which the samples sit farthest from the
    // threshold (mid-bit), out of 16.
    let period = fd / DCS_BAUD;
    let mut best = (f32::MIN, 0.0f64);
    for k in 0..16 {
        let ph = period * k as f64 / 16.0;
        let mut sum = 0.0f32;
        let mut cnt = 0usize;
        let mut t = ph;
        while (t as usize) < n {
            sum += (lp[t as usize] - thr).abs();
            cnt += 1;
            t += period;
        }
        let m = sum / cnt.max(1) as f32;
        if m > best.0 {
            best = (m, ph);
        }
    }
    let mut bits = Vec::new();
    let mut t = best.1;
    while (t as usize) < n {
        bits.push(lp[t as usize] > thr);
        t += period;
    }
    if bits.len() < 2 * WORD {
        return None;
    }
    let agree = bits.iter().zip(&bits[WORD..]).filter(|(a, b)| a == b).count() as f32 / (bits.len() - WORD) as f32;
    if agree < DCS_PERIODIC {
        return None;
    }
    // Fold onto one word by majority, then find the rotation and polarity
    // that pass the Golay check with the fixed bits in place.
    let mut votes = [0i32; WORD];
    for (i, b) in bits.iter().enumerate() {
        votes[i % WORD] += if *b { 1 } else { -1 };
    }
    let mut word = 0u32;
    for (i, v) in votes.iter().enumerate() {
        if *v > 0 {
            word |= 1 << i;
        }
    }
    let code = decode_word(word)?;
    let inverted = decode_word(!word & 0x7F_FFFF).unwrap_or(code);
    Some(Measurement { squelch: Squelch::Dcs { code, inverted }, words: bits.len() / WORD })
}

/// Golay remainder of a 23-bit word (bit 0 first sent).
fn syndrome(word: u32) -> u32 {
    let mut r = word;
    for i in (11..WORD).rev() {
        if r >> i & 1 == 1 {
            r ^= GOLAY << (i - 11);
        }
    }
    r & 0x7FF
}

/// The 23-bit code word of a DCS code (bit 0 first sent).
pub fn dcs_codeword(code: u16) -> u32 {
    // The data sits in the low bits (sent first) and the parity above it,
    // so the parity is the 11-bit value whose remainder cancels the data's
    // (the syndrome is linear, and a cyclic code lets any 12 consecutive
    // positions carry the data).
    let data = (code as u32 & 0x1FF) | FIXED;
    let want = syndrome(data);
    let parity = (0..1u32 << 11).find(|p| syndrome(p << 12) == want).expect("parity map is invertible");
    data | parity << 12
}

/// The code read with the opposite polarity: what a receiver set to
/// "inverted" sees.
pub fn dcs_inverse(code: u16) -> Option<u16> {
    decode_word(!dcs_codeword(code) & 0x7F_FFFF)
}

/// The code of a received 23-bit cyclic word, start unknown: of the
/// rotations that pass the Golay check with the fixed bits in place (a
/// cyclic code admits several), the standard one if any, else the lowest.
fn decode_word(word: u32) -> Option<u16> {
    let mut fits = Vec::new();
    for r in 0..WORD as u32 {
        let w = ((word >> r) | (word << (WORD as u32 - r))) & 0x7F_FFFF;
        if syndrome(w) == 0 && w >> 9 & 7 == 0b100 {
            fits.push((w & 0x1FF) as u16);
        }
    }
    fits.iter().copied().find(|c| DCS_STANDARD.contains(c)).or_else(|| fits.into_iter().min())
}

/// Least share of the 50–300 Hz power one line must hold, and the least it
/// must stand over the band's median bin.
const CTCSS_SHARE: f32 = 0.5;
const CTCSS_OVER_MEDIAN: f32 = 20.0;

fn ctcss(lp: &[f32], fd: f64) -> Option<f32> {
    if lp.len() < 512 {
        return None;
    }
    let n = lp.len();
    let mean = lp.iter().sum::<f32>() / n as f32;
    // Hann window, zero-padded four times for a fine peak.
    let m = (n * 4).next_power_of_two();
    let mut buf = vec![Complex32::new(0.0, 0.0); m];
    for (i, v) in lp.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
        buf[i] = Complex32::new((v - mean) * w, 0.0);
    }
    FftPlanner::new().plan_fft_forward(m).process(&mut buf);
    let bin = fd / m as f64;
    let pw: Vec<f32> = buf[..m / 2].iter().map(|c| c.norm_sqr()).collect();
    let (lo, hi) = ((50.0 / bin) as usize, ((300.0 / bin) as usize).min(pw.len()));
    let (a, b) = ((60.0 / bin) as usize, ((260.0 / bin) as usize).min(pw.len()));
    if b <= a + 2 || hi <= lo {
        return None;
    }
    let (k, &pk) = pw[a..b].iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1)).map(|(i, v)| (i + a, v))?;
    let mut band = pw[lo..hi].to_vec();
    let mid = band.len() / 2;
    let med = *band.select_nth_unstable_by(mid, f32::total_cmp).1;
    if pk < CTCSS_OVER_MEDIAN * med.max(1e-30) {
        return None;
    }
    // Parabolic peak, and the line's share as ±1.5 Hz around it (the
    // window's main lobe at 0.5 s is ±4 Hz, so this is the lobe's core).
    let (y0, y1, y2) = (pw[k - 1].ln(), pw[k].ln(), pw[k + 1].ln());
    let d = 0.5 * (y0 - y2) / (y0 - 2.0 * y1 + y2).min(-1e-9);
    let f = (k as f64 + d as f64) * bin;
    let half = ((2.0 * n as f64 / m as f64 / bin) as usize).max(2);
    let line: f32 = pw[k.saturating_sub(half)..(k + half + 1).min(pw.len())].iter().sum();
    let total: f32 = pw[lo..hi].iter().sum();
    if line < CTCSS_SHARE * total {
        return None;
    }
    CTCSS.iter().copied().find(|t| (*t as f64 - f).abs() < 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NRZ bits of a code, repeated, as an instantaneous-frequency series.
    fn dcs_disc(code: u16, dev: f32, fs: f64, secs: f64, invert: bool, seed: u64) -> Vec<f32> {
        let w = dcs_codeword(code);
        let mut s = seed | 1;
        let mut noise = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        (0..(fs * secs) as usize)
            .map(|i| {
                let bit = (i as f64 / fs * DCS_BAUD) as usize % WORD;
                let one = (w >> bit & 1 == 1) != invert;
                (if one { dev } else { -dev }) + 600.0 * noise()
            })
            .collect()
    }

    #[test]
    fn codewords_pass_their_own_check() {
        for c in DCS_STANDARD {
            let w = dcs_codeword(c);
            assert_eq!(syndrome(w), 0, "{c:03o}");
            assert_eq!(decode_word(w), Some(c), "{c:03o}");
            // Any rotation decodes to the same code.
            assert_eq!(decode_word((w << 7 | w >> 16) & 0x7F_FFFF), Some(c), "{c:03o}");
        }
    }

    #[test]
    fn inversion_pairs_match_the_published_table() {
        for (a, b) in [(0o023, 0o047), (0o205, 0o263), (0o025, 0o244), (0o026, 0o464), (0o031, 0o627), (0o032, 0o051)] {
            assert_eq!(dcs_inverse(a), Some(b), "{a:03o}");
            assert_eq!(dcs_inverse(b), Some(a), "{b:03o}");
        }
    }

    #[test]
    fn decodes_a_noisy_dcs_stream_at_two_rates() {
        for fs in [48_000.0, 62_500.0] {
            let d = dcs_disc(0o023, 800.0, fs, 0.6, false, 7);
            let m = measure(&d, fs).expect("dcs");
            assert_eq!(m.squelch, Squelch::Dcs { code: 0o023, inverted: 0o047 }, "{fs}");
            assert!(m.words >= 3, "{m:?}");
            let d = dcs_disc(0o023, 800.0, fs, 0.6, true, 9);
            assert_eq!(measure(&d, fs).unwrap().squelch, Squelch::Dcs { code: 0o047, inverted: 0o023 });
        }
    }

    #[test]
    fn finds_a_ctcss_tone_under_voice() {
        let fs = 48_000.0;
        let mut s = 0x9E37_79B9u64;
        let mut noise = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        // Voice stand-in: wideband noise at ±2 kHz rms over the tone.
        let d: Vec<f32> = (0..(fs * 1.0) as usize)
            .map(|i| 500.0 * (2.0 * std::f32::consts::PI * 100.0 * i as f32 / fs as f32).sin() + 2000.0 * noise())
            .collect();
        assert_eq!(measure(&d, fs).map(|m| m.squelch), Some(Squelch::Ctcss(100.0)));
    }

    #[test]
    fn data_and_noise_carry_no_squelch() {
        let fs = 48_000.0;
        let mut s = 0xDEAD_BEEFu64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        // Random 2FSK at 1200 Bd, ±1 kHz.
        let bits: Vec<bool> = (0..2000).map(|_| rnd() & 1 == 1).collect();
        let d: Vec<f32> =
            (0..(fs * 0.8) as usize).map(|i| if bits[(i as f64 / fs * 1200.0) as usize] { 1000.0 } else { -1000.0 }).collect();
        assert_eq!(measure(&d, fs), None);
        let d: Vec<f32> = (0..(fs * 0.8) as usize).map(|_| ((rnd() >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 6000.0).collect();
        assert_eq!(measure(&d, fs), None);
    }
}
