//! ACARS (ARINC 618 plain old ACARS): the data link of the VHF air band. A ground station or an aircraft keys AM for a
//! fraction of a second and sends audio MSK at 2400 bit/s, its two tones 1200 and 2400 Hz. Characters are 7-bit ASCII
//! with odd parity, least significant bit first. A block is
//!
//! `pre-key (ones) + * SYN SYN SOH | mode, address (7), ack, label (2), block id [STX text] ETX|ETB | BCS (2) DEL`
//!
//! and its block check (BCS) is CRC-16 over everything from the mode to the ETX or ETB.
//!
//! Written from the protocol's description, not from any decoder's source.

use num_complex::Complex32;

use crate::carrier;

/// The bit rate and the two tones.
const BAUD: f64 = 2400.0;
const TONE_LO: f64 = 1200.0;
const TONE_HI: f64 = 2400.0;
/// The audio band kept for the tones: MSK's main lobe at 2400 bit/s around 1800 Hz is 0–3.6 kHz.
const BAND_LO: f64 = 300.0;
const BAND_HI: f64 = 3600.0;

const SYN: u8 = 0x16;
const SOH: u8 = 0x01;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const ETB: u8 = 0x17;

/// Longest block: 220 characters of text plus the header.
const MAX_CHARS: usize = 260;

/// One decoded block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub mode: char,
    /// The aircraft's registration, dots for padding (".N123AB").
    pub address: String,
    pub ack: char,
    pub label: String,
    pub block_id: char,
    /// Downlinks (from an aircraft: a block id that is a digit) start their text with a message number and the flight.
    pub text: String,
    /// The block ended with ETB: more blocks follow.
    pub more: bool,
    /// The BCS checked.
    pub crc_ok: bool,
    /// Characters whose parity failed.
    pub parity_errors: usize,
}

impl Block {
    /// The registration without its padding.
    pub fn registration(&self) -> &str {
        self.address.trim_start_matches('.')
    }

    /// From an aircraft.
    pub fn downlink(&self) -> bool {
        self.block_id.is_ascii_digit()
    }

    /// One line: "N365NB NW2320 H1: D20ANW2320#DFB…" (the text's first 40 characters, control characters as spaces).
    pub fn summary(&self) -> String {
        let text: String = self.text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(40).collect();
        let more = if self.text.chars().count() > 40 { "…" } else { "" };
        match self.flight() {
            Some(f) => format!("{} {f} {}: {}{more}", self.registration(), self.label, text.trim_end()),
            None => format!("{} {}: {}{more}", self.registration(), self.label, text.trim_end()),
        }
    }

    /// A downlink's flight, from its text (`M01A` then the flight, `AB1234`).
    pub fn flight(&self) -> Option<&str> {
        (self.downlink() && self.text.len() >= 10)
            .then(|| self.text[4..10].trim_matches(|c: char| c == '.' || c == ' '))
            .filter(|f| !f.is_empty())
    }
}

/// AM demodulated to audio at `fs`, with the carrier's level removed (a 20 ms moving mean): from the envelope of `x`
/// low-passed to ±5 kHz around the centre, or (`coherent`) in phase with the carrier. Neither always wins on air: the
/// envelope keeps what a transmitter's incidental phase modulation moves out of phase, the coherent detector holds up
/// where the envelope's noise does not (`None` when no carrier line is found).
fn audio(x: &[Complex32], fs: f64, coherent: bool) -> Option<Vec<f32>> {
    let level = if coherent {
        carrier::to_carrier(x, fs, carrier::line_near_centre(x, fs, CARRIER_REACH_HZ)?).0
    } else {
        lowpass(x, fs, 5000.0).iter().map(|v| v.norm()).collect()
    };
    let l = ((0.02 * fs) as usize).max(1);
    let mut out = Vec::with_capacity(level.len());
    let mut acc = 0.0f64;
    for i in 0..level.len() {
        acc += level[i] as f64;
        if i >= l {
            acc -= level[i - l] as f64;
        }
        out.push(level[i] - (acc / (i + 1).min(l) as f64) as f32);
    }
    Some(out)
}

/// MSK's ambiguities a receiver can't resolve before the sync word (see [`soft_bits`]).
const VARIANTS: u8 = 16;

/// How far from the window's centre the carrier is looked for.
const CARRIER_REACH_HZ: f64 = 2500.0;

/// A windowed-sinc low-pass (Hamming), `cutoff` Hz, applied without delay (the filter is centred).
fn lowpass<T>(x: &[T], fs: f64, cutoff: f64) -> Vec<T>
where
    T: Copy + Default + std::ops::Add<Output = T> + std::ops::Mul<f32, Output = T>,
{
    let taps = ((4.0 * fs / cutoff) as usize) | 1;
    let m = (taps / 2) as isize;
    let fc = cutoff / fs;
    let h: Vec<f32> = (0..taps as isize)
        .map(|i| {
            let k = (i - m) as f64;
            let sinc = if k == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * k).sin() / (std::f64::consts::PI * k) };
            let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (taps - 1) as f64).cos();
            (sinc * w) as f32
        })
        .collect();
    (0..x.len() as isize)
        .map(|n| {
            let mut acc = T::default();
            for (i, hi) in h.iter().enumerate() {
                let j = n + i as isize - m;
                if j >= 0 && (j as usize) < x.len() {
                    acc = acc + x[j as usize] * *hi;
                }
            }
            acc
        })
        .collect()
}

/// The audio's analytic signal over the band (through an FFT), shifted down by the middle of the two tones: MSK at
/// baseband, its phase moving ±π/2 over each bit (up for the high tone).
fn baseband(a: &[f32], fs: f64) -> Vec<Complex32> {
    let n = a.len();
    let mut z: Vec<Complex32> = a.iter().map(|v| Complex32::new(*v, 0.0)).collect();
    if n < 2 {
        return z;
    }
    let mut planner = rustfft::FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut z);
    let bin = fs / n as f64;
    for (k, v) in z.iter_mut().enumerate() {
        let f = if k <= n / 2 { k as f64 * bin } else { -((n - k) as f64) * bin };
        *v *= if (BAND_LO..=BAND_HI).contains(&f) { 2.0 / n as f32 } else { 0.0 };
    }
    planner.plan_fft_inverse(n).process(&mut z);
    let w = -2.0 * std::f64::consts::PI * (TONE_LO + TONE_HI) / 2.0 / fs;
    for (k, v) in z.iter_mut().enumerate() {
        *v *= Complex32::from_polar(1.0, (w * k as f64).rem_euclid(std::f64::consts::TAU) as f32);
    }
    z
}

/// Where each bit starts (in samples) and the phase it moved, timed by the phase that makes the whole burst's bits most
/// decided, then re-timed every 64 bits within an eighth of a bit (the clocks drift a little; a pre-key's steady tone
/// has no transitions to time by, so it can't be timed on its own).
fn bit_timing(z: &[Complex32], fs: f64) -> Vec<(f64, f32)> {
    let l = fs / BAUD;
    let n = z.len();
    if (n as f64) < 32.0 * l {
        return Vec::new();
    }
    let mut c = vec![0.0f64; n + 1];
    for i in 1..n {
        c[i + 1] = c[i] + (z[i] * z[i - 1].conj()).arg() as f64;
    }
    let moved = |s: f64| -> Option<f32> {
        let (a, b) = (s.round() as isize, (s + l).round() as isize);
        (a >= 0 && (b as usize) <= n && b > a).then(|| (c[b as usize] - c[a as usize]) as f32)
    };
    let score = |start: f64, bits: usize| -> f64 { (0..bits).filter_map(|k| moved(start + k as f64 * l)).map(|v| v.abs() as f64).sum() };
    let total = (n as f64 / l) as usize;
    const STEPS: i32 = 32;
    let step = l / STEPS as f64;
    let mut start = (0..STEPS).map(|i| i as f64 * step).max_by(|a, b| score(*a, total).total_cmp(&score(*b, total))).unwrap_or(0.0);
    let mut out = Vec::with_capacity(total);
    while start + l <= n as f64 {
        // Ties go to the timing nearer the last.
        let here = start;
        start = (-4..=4)
            .map(|i| here + i as f64 * step)
            .max_by(|a, b| score(*a, 64).total_cmp(&score(*b, 64)).then((b - here).abs().total_cmp(&(a - here).abs())))
            .unwrap_or(here);
        for k in 0..64 {
            let s = start + k as f64 * l;
            match moved(s) {
                Some(v) => out.push((s, v)),
                None => return out,
            }
        }
        start += 64.0 * l;
    }
    out
}

/// Soft bits, coherently: MSK is offset QPSK, so at the end of bit `k` the phase sits on an axis, real and imaginary
/// in turn, and its sign there (a half-sine matched filter over the two bits either side) is the bit, the signs flipping
/// every two bits. The carrier's phase comes from the fourth power at those instants (which loses the modulation and
/// keeps the phase to a quarter turn), averaged over 32 bits either side; which quarter, and which axis comes first, is
/// what `variant` (0–15: the quarter, the axis bit 0 is read on, the flips' phase) picks and the sync word settles.
fn soft_bits(z: &[Complex32], timing: &[(f64, f32)], fs: f64, variant: u8) -> Vec<f32> {
    let l = fs / BAUD;
    let ends: Vec<f64> = timing.iter().map(|(s, _)| s + l).collect();
    let at = |t: f64| z.get(t.round() as usize).copied().unwrap_or_default();
    let fourth: Vec<Complex32> = ends.iter().map(|&t| at(t).powi(4) / at(t).norm().powi(3).max(1e-30)).collect();
    let mut prefix = vec![Complex32::new(0.0, 0.0); fourth.len() + 1];
    for (i, v) in fourth.iter().enumerate() {
        prefix[i + 1] = prefix[i] + v;
    }
    let half = (l.round() as isize).max(1);
    let (quarter, axis, flip) = (variant & 3, variant >> 2 & 1, variant >> 3 & 1);
    let mut theta_last = 0.0f32;
    ends.iter()
        .enumerate()
        .map(|(k, &t)| {
            let (a, b) = (k.saturating_sub(32), (k + 33).min(fourth.len()));
            let mut theta = (prefix[b] - prefix[a]).arg() / 4.0;
            // Unwrapped against the last, a quarter turn at a time, so the reference holds its quadrant.
            while theta - theta_last > std::f32::consts::FRAC_PI_4 {
                theta -= std::f32::consts::FRAC_PI_2;
            }
            while theta - theta_last < -std::f32::consts::FRAC_PI_4 {
                theta += std::f32::consts::FRAC_PI_2;
            }
            theta_last = theta;
            let rot = Complex32::from_polar(1.0, -(theta + quarter as f32 * std::f32::consts::FRAC_PI_2));
            let real = (k + axis as usize).is_multiple_of(2);
            let centre = t.round() as isize;
            let mut acc = 0.0f32;
            for i in -half..=half {
                let w = (std::f64::consts::FRAC_PI_2 * i as f64 / l).cos() as f32;
                let v = z.get((centre + i) as usize).copied().unwrap_or_default() * rot;
                acc += w * if real { v.re } else { v.im };
            }
            let sign = if ((k + flip as usize) / 2).is_multiple_of(2) { 1.0 } else { -1.0 };
            acc * sign
        })
        .collect()
}

fn byte_at(bits: &[bool], i: usize) -> Option<u8> {
    let b = bits.get(i..i + 8)?;
    Some(b.iter().enumerate().fold(0u8, |acc, (k, v)| acc | (u8::from(*v) << k)))
}

/// CRC-16 as ACARS's BCS is: CCITT's polynomial, bits reflected, from zero, over the characters as sent (parity bit
/// included) and then the BCS's two bytes, which leaves zero.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    crc
}

/// The characters of a block and whether its BCS checks, after correcting what parity and the BCS can: one flipped
/// bit in each of up to [`MAX_FIXES`] characters whose parity failed, the combination the BCS accepts.
fn correct(raw: Vec<u8>, bcs: [u8; 2]) -> (Vec<u8>, bool) {
    let ok = |r: &[u8]| {
        let mut v = r.to_vec();
        v.extend(bcs);
        crc16(&v) == 0
    };
    if ok(&raw) {
        return (raw, true);
    }
    let bad: Vec<usize> = (0..raw.len()).filter(|&i| raw[i].count_ones().is_multiple_of(2)).collect();
    if bad.is_empty() || bad.len() > MAX_FIXES {
        return (raw, false);
    }
    let mut r = raw.clone();
    let combos = 8usize.pow(bad.len() as u32);
    for c in 0..combos {
        let mut k = c;
        for &i in &bad {
            r[i] = raw[i] ^ (1 << (k % 8));
            k /= 8;
        }
        if ok(&r) {
            return (r, true);
        }
    }
    (raw, false)
}

/// Characters with failed parity that [`correct`] tries to fix (8 to the power of this many BCS checks).
const MAX_FIXES: usize = 3;

/// Bit errors allowed in SYN SYN SOH: the BCS still checks every block, and a weak burst often loses one there.
const SYNC_ERRORS: u32 = 2;

/// The blocks in a bit stream: each SYN SYN SOH starts one (to within [`SYNC_ERRORS`]).
fn blocks(bits: &[bool]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 24 <= bits.len() {
        let (Some(a), Some(b), Some(c)) = (byte_at(bits, i), byte_at(bits, i + 8), byte_at(bits, i + 16)) else { break };
        if (a ^ SYN).count_ones() + (b ^ SYN).count_ones() + (c ^ SOH).count_ones() > SYNC_ERRORS {
            i += 1;
            continue;
        }
        let mut j = i + 24;
        let mut raw = Vec::new();
        let mut end = None;
        while raw.len() < MAX_CHARS {
            let Some(b) = byte_at(bits, j) else { break };
            j += 8;
            raw.push(b);
            let c = b & 0x7F;
            if c == ETX || c == ETB {
                end = Some(c);
                break;
            }
        }
        let Some(end) = end else {
            i += 24;
            continue;
        };
        let (Some(b1), Some(b2)) = (byte_at(bits, j), byte_at(bits, j + 8)) else { break };
        let parity_errors = raw.iter().filter(|b| b.count_ones().is_multiple_of(2)).count();
        let (raw, crc_ok) = correct(raw, [b1, b2]);
        let chars: Vec<char> = raw.iter().map(|b| (b & 0x7F) as char).collect();
        if chars.len() >= 13 {
            let text = match chars.iter().position(|c| *c as u8 == STX) {
                Some(p) => chars[p + 1..chars.len() - 1].iter().collect(),
                None => String::new(),
            };
            out.push(Block {
                mode: chars[0],
                address: chars[1..8].iter().collect(),
                ack: chars[8],
                label: chars[9..11].iter().collect(),
                block_id: chars[11],
                text,
                more: end == ETB,
                crc_ok,
                parity_errors,
            });
        }
        i = j + 16;
    }
    out
}

/// Every ACARS block in `x`, complex baseband at `fs` centred on the carrier, at least ±4 kHz wide: the blocks whose
/// BCS checks, through either AM detector and any of MSK's phase ambiguities; else (nothing checked) the attempt with
/// the fewest parity errors, unchecked, so a caller can see a burst was there.
pub fn decode(x: &[Complex32], fs: f64) -> Vec<Block> {
    if fs < 9600.0 {
        return Vec::new();
    }
    let mut ok: Vec<Block> = Vec::new();
    let mut best: Vec<Block> = Vec::new();
    for coherent in [false, true] {
        let Some(a) = audio(x, fs, coherent) else { continue };
        let z = baseband(&a, fs);
        let timing = bit_timing(&z, fs);
        for variant in 0..VARIANTS {
            let bits: Vec<bool> = soft_bits(&z, &timing, fs, variant).iter().map(|v| *v > 0.0).collect();
            let found = blocks(&bits);
            for b in found.iter().filter(|b| b.crc_ok) {
                if !ok.contains(b) {
                    ok.push(b.clone());
                }
            }
            let errors = |v: &[Block]| v.iter().map(|b| b.parity_errors).sum::<usize>();
            if !found.is_empty() && (best.is_empty() || errors(&found) < errors(&best)) {
                best = found;
            }
        }
    }
    if ok.is_empty() { best } else { ok }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a slice's SigMF data file: cf32_le (slices since 2026-11-07) or ci16_le (earlier ones), as `meta` says.
    fn read_slice(path: &std::path::Path, meta: &str) -> Vec<Complex32> {
        let raw = std::fs::read(path).unwrap();
        if meta.contains("\"cf32_le\"") {
            raw.as_chunks::<8>()
                .0
                .iter()
                .map(|c| Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]])))
                .collect()
        } else {
            raw.as_chunks::<4>()
                .0
                .iter()
                .map(|c| {
                    Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0, i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0)
                })
                .collect()
        }
    }

    /// Scratch: every slice in `ACARS_DIR` (the slice tier's layout), decoded.
    #[test]
    fn scratch_dir() {
        let Some(dir) = std::env::var_os("ACARS_DIR") else { return };
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sigmf-data"))
            .collect();
        files.sort();
        for f in files {
            let meta = std::fs::read_to_string(f.with_extension("sigmf-meta")).unwrap();
            let rate: f64 = meta.split("\"core:sample_rate\":").nth(1).unwrap().split([',', '\n']).next().unwrap().trim().parse().unwrap();
            let hz: f64 = meta.split("\"core:frequency\":").nth(1).unwrap().split([',', '\n']).next().unwrap().trim().parse().unwrap();
            let x = read_slice(&f, &meta);
            let t0 = std::time::Instant::now();
            let blocks = decode(&x, rate);
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            if !blocks.is_empty() || (129e6..132e6).contains(&hz) {
                let z = baseband(&audio(&x, rate, false).unwrap(), rate);
                let timing = bit_timing(&z, rate);
                let soft = soft_bits(&z, &timing, rate, 0);
                let mut sorted: Vec<f32> = soft.iter().map(|v| v.abs()).collect();
                sorted.sort_by(f32::total_cmp);
                let q = |p: usize| {
                    sorted.get(sorted.len() * p / 100).copied().unwrap_or(0.0)
                        / sorted.get(sorted.len() / 2).copied().unwrap_or(1.0).max(1e-9)
                };
                // How decided the noncoherent tones are: |moved| / (π/2), median.
                let mut mv: Vec<f32> = timing.iter().map(|(_, v)| v.abs() / std::f32::consts::FRAC_PI_2).collect();
                mv.sort_by(f32::total_cmp);
                eprintln!(
                    "{} {:.4} MHz {:.2} s ({ms:.0} ms): soft 5% {:.2} 10% {:.2}, tone median {:.2}",
                    f.file_name().unwrap().to_string_lossy(),
                    hz / 1e6,
                    x.len() as f64 / rate,
                    q(5),
                    q(10),
                    mv.get(mv.len() / 2).copied().unwrap_or(0.0)
                );
                for b in blocks {
                    eprintln!("   crc {} parity {} | {}", b.crc_ok, b.parity_errors, b.summary());
                }
            }
        }
    }
}
