//! The HydraSDR RFOne's firmware sweep (`v1.0.2-sweepN`, ~/hydrasdr
//! `experiments/2026-11-04-sweep-fw`; SpecTrawl `docs/hydrasdr-sweep-mode.md`).
//!
//! The board retunes itself, waits for the R828D's PLL lock and sends only
//! blocks captured after it, each tagged with its step and frequency. The
//! stream is raw (20 MS/s real, IF at 5 MHz, spectrum inverted, unpacked);
//! each block becomes 4088 IQ samples at 10 MS/s here, the right way round,
//! so the engine sees ordinary hops. The sweep is started and stopped through
//! the sweep calls of the patched libhydrasdr (`hydrasdr_sweep_*`, ~/hydrasdr
//! `host/`); a stock library lacks them and the board then never sweeps.

use sdr_core::Complex32;

/// 16 KB: 8192 12-bit samples in u16 words.
pub const BLOCK_WORDS: usize = 8192;
/// Header and trailer, overwriting the block's first and last 8 words.
const TAG_WORDS: usize = 8;
/// Real samples per block that are samples.
pub const REAL_SAMPLES: usize = BLOCK_WORDS - 2 * TAG_WORDS;
/// IQ samples per block after the conversion (half the real rate).
pub const BLOCK_SAMPLES: usize = REAL_SAMPLES / 2;
/// The DMA counter's tick: one 8192-sample block at 20 MS/s.
pub const TICK_S: f64 = BLOCK_WORDS as f64 / 20e6;

/// Consecutive locked reads of R828D register 2 bit 6 before capture (0 turns
/// the check off: 1218 hops/s, but ~40 of 222 steps read up to 22 dB low).
pub const LOCK_READS: u8 = 2;
/// Lock polls (~0.1 ms each) before the board gives up and sends anyway.
pub const LOCK_TIMEOUT: u16 = 200;

/// `hydrasdr_sweep_params_t`: 16 bytes, naturally aligned, no padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepParams {
    /// Centre of step 0.
    pub f_start_hz: u32,
    pub f_step_hz: u32,
    pub n_steps: u16,
    /// Extra blocks discarded after lock.
    pub skip_blocks: u8,
    pub dwell_blocks: u8,
    pub lock_reads: u8,
    pub reserved: u8,
    pub lock_timeout: u16,
}

impl SweepParams {
    pub fn new(first_center_hz: u32, step_hz: u32, steps: u16, dwell: u8) -> Self {
        Self {
            f_start_hz: first_center_hz,
            f_step_hz: step_hz,
            n_steps: steps,
            skip_blocks: 0,
            dwell_blocks: dwell,
            lock_reads: LOCK_READS,
            reserved: 0,
            lock_timeout: LOCK_TIMEOUT,
        }
    }
}

/// A block's tags, when header and trailer agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub step: u16,
    /// Block within the step's dwell.
    pub block: u16,
    pub sweep: u32,
    /// The board's DMA block counter at the block (a device-side clock, `TICK_S` per tick).
    pub dma: u32,
    pub center_hz: u32,
    /// The board gave up waiting for lock on this step.
    pub lock_timed_out: bool,
}

/// What a block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Tagged(Tag),
    /// Header present but the trailer disagrees: the DMA overwrote the block while USB still read it.
    Torn,
    /// No header: normal-mode data, sent before the sweep starts or after it stops.
    Untagged,
}

/// The checks `hydrasdr_sweep_parse_block` makes, done here so they cost no
/// call into the library per block and are tested without it.
pub fn parse(w: &[u16]) -> Block {
    debug_assert_eq!(w.len(), BLOCK_WORDS);
    let (h, t) = (&w[..TAG_WORDS], &w[BLOCK_WORDS - TAG_WORDS..]);
    if h[0] != 0xA55A || h[1] != 0x5357 {
        return Block::Untagged;
    }
    if t[0] != 0x5AA5 || h[2] != t[2] || h[3] != t[3] || h[6] != t[6] || h[7] != t[7] {
        return Block::Torn;
    }
    let u32_at = |s: &[u16], i: usize| u32::from(s[i]) | (u32::from(s[i + 1]) << 16);
    Block::Tagged(Tag {
        step: h[2],
        block: h[3],
        sweep: u32_at(h, 4),
        dma: u32_at(t, 4),
        center_hz: u32_at(h, 6),
        lock_timed_out: t[1] & 0x8000 != 0,
    })
}

/// Fraction of a block's samples at either end of the ADC's range.
pub fn clipped_fraction(w: &[u16]) -> f32 {
    let s = &w[TAG_WORDS..BLOCK_WORDS - TAG_WORDS];
    s.iter().filter(|&&v| matches!(v & 0x0FFF, 0 | 4095)).count() as f32 / s.len() as f32
}

/// Raw block → IQ at half the rate: shift the 5 MHz IF (fs/4) to DC by
/// multiplying with j^n, which also undoes the inversion (the image the
/// shift lands at DC is the right way round), then a half-band low-pass and
/// decimation by 2. Each block is converted on its own (blocks of a sweep are
/// not contiguous), so the first and last few samples carry the filter's
/// edge; a windowed FFT barely sees them.
pub struct RawToIq {
    /// The half-band's centre tap and odd-offset taps (the rest are zero), scaled as libhydrasdr's float IQ is: a
    /// full-scale real tone becomes a complex tone of amplitude 0.5 (measured on air: the same carriers and floors
    /// within ±0.2 dB of the retune sweep's on 160 of 197 hops).
    centre: f32,
    odd: Vec<f32>,
    /// Signed odd-index samples, zero-padded at both ends.
    q: Vec<f32>,
    pad: usize,
}

/// Taps of the half-band: passband to ~4.6 MHz, stopband from ~5.4 MHz at
/// 20 MS/s (Kaiser, β 6.5: ~65 dB), so the 9 MHz the engine keeps of each hop
/// is alias-free.
const HALFBAND_TAPS: usize = 95;

impl Default for RawToIq {
    fn default() -> Self {
        Self::new()
    }
}

impl RawToIq {
    pub fn new() -> Self {
        let c = (HALFBAND_TAPS - 1) / 2;
        let h = kaiser_halfband(HALFBAND_TAPS, 6.5);
        // im[m] = Σ_j h[c + 2j + 1] · q[m − j − 1], j = −(c+1)/2 .. (c−1)/2.
        let half = c.div_ceil(2);
        let odd: Vec<f32> = (0..2 * half).map(|i| h[(c as isize + 2 * (i as isize - half as isize) + 1) as usize] as f32).collect();
        let pad = odd.len();
        Self { centre: h[c] as f32, odd, q: vec![0.0; REAL_SAMPLES / 2 + 2 * pad], pad }
    }

    /// Converts one tagged block's `REAL_SAMPLES` samples (words 8..8184) into `out` (`BLOCK_SAMPLES`).
    pub fn process(&mut self, words: &[u16], out: &mut Vec<Complex32>) {
        debug_assert_eq!(words.len(), REAL_SAMPLES);
        let x = |n: usize| (f32::from(words[n] & 0x0FFF) - 2048.0) / 2048.0;
        let half = REAL_SAMPLES / 2;
        // The block's mean (the ADC's DC offset) would land on the shifted IF.
        let mean = words.iter().map(|&w| f32::from(w & 0x0FFF)).sum::<f32>() / REAL_SAMPLES as f32;
        let mean = (mean - 2048.0) / 2048.0;
        let (pad, taps) = (self.pad, self.odd.len());
        for i in 0..half {
            // z[n] = x[n]·j^n; at odd n = 2i+1 that is ±j·x: + when i is even.
            let v = x(2 * i + 1) - mean;
            self.q[pad + i] = if i % 2 == 0 { v } else { -v };
        }
        out.clear();
        out.reserve(half);
        let lag = taps / 2;
        for m in 0..half {
            // Real part: the centre tap on z[2m] = x[2m]·(−1)^m.
            let v = (x(2 * m) - mean) * self.centre;
            let re = if m % 2 == 0 { v } else { -v };
            // q[m − j − 1] for j = −lag..lag−1, i.e. q[m + lag − 1 − i] for tap i.
            let base = pad + m + lag - 1;
            let mut im = 0.0f32;
            for (i, t) in self.odd.iter().enumerate() {
                im += t * self.q[base - i];
            }
            out.push(Complex32::new(re, im));
        }
    }
}

/// A Kaiser-windowed sinc with its cutoff at a quarter of the rate (so every
/// even offset from the centre is zero), unit DC gain.
fn kaiser_halfband(taps: usize, beta: f64) -> Vec<f64> {
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
        while term > 1e-12 * sum {
            term *= (x / (2.0 * k)).powi(2);
            sum += term;
            k += 1.0;
        }
        sum
    }
    let c = (taps - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let d = i as f64 - c;
            let sinc = if d == 0.0 { 0.5 } else { (std::f64::consts::PI * d / 2.0).sin() / (std::f64::consts::PI * d) };
            let r = d / c;
            sinc * i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0(beta)
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(step: u16, center_hz: u32, samples: &[u16], lock_word: u16) -> Vec<u16> {
        let mut w = vec![0u16; BLOCK_WORDS];
        w[TAG_WORDS..BLOCK_WORDS - TAG_WORDS].copy_from_slice(samples);
        let lo = |v: u32| v as u16;
        let hi = |v: u32| (v >> 16) as u16;
        w[..8].copy_from_slice(&[0xA55A, 0x5357, step, 0, 7, 0, lo(center_hz), hi(center_hz)]);
        w[BLOCK_WORDS - 8..].copy_from_slice(&[0x5AA5, lock_word, step, 0, 1234, 0, lo(center_hz), hi(center_hz)]);
        w
    }

    /// A tone at RF offset `off_hz` from the centre, as the raw stream carries it (inverted about the 5 MHz IF).
    fn tone(off_hz: f64, amp: f64) -> Vec<u16> {
        let f_if = 5e6 - off_hz;
        (0..REAL_SAMPLES).map(|n| (2048.0 + amp * (2.0 * std::f64::consts::PI * f_if / 20e6 * n as f64).cos()).round() as u16).collect()
    }

    #[test]
    fn tags_parse_and_a_torn_or_untagged_block_is_refused() {
        let w = tagged(3, 500_000_000, &tone(1e6, 100.0), 0x8005);
        let Block::Tagged(t) = parse(&w) else { panic!() };
        assert_eq!((t.step, t.block, t.sweep, t.dma, t.center_hz, t.lock_timed_out), (3, 0, 7, 1234, 500_000_000, true));
        let mut torn = w.clone();
        torn[BLOCK_WORDS - 6] ^= 1;
        assert_eq!(parse(&torn), Block::Torn);
        assert_eq!(parse(&vec![2048u16; BLOCK_WORDS]), Block::Untagged);
        // The C struct's layout: on a little-endian machine its bytes are the firmware's `<IIHBBBBH`.
        let p = SweepParams::new(28_000_000, 8_000_000, 222, 1);
        assert_eq!(std::mem::size_of::<SweepParams>(), 16);
        let bytes: [u8; 16] = unsafe { std::mem::transmute(p) };
        assert_eq!(bytes, [0x00, 0x3f, 0xab, 0x01, 0x00, 0x12, 0x7a, 0x00, 222, 0, 0, 1, 2, 0, 200, 0]);
    }

    /// Power per FFT bin of `iq` (zero-padded to 4096, Hann), DC-centred.
    fn spectrum(iq: &[Complex32]) -> Vec<f32> {
        let n = 4096;
        let mut x: Vec<rustfft::num_complex::Complex32> = (0..n)
            .map(|i| {
                let v = iq.get(i).copied().unwrap_or_default();
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
                rustfft::num_complex::Complex32::new(v.re * w, v.im * w)
            })
            .collect();
        rustfft::FftPlanner::new().plan_fft_forward(n).process(&mut x);
        (0..n).map(|k| x[(k + n / 2) % n].norm_sqr()).collect()
    }

    #[test]
    fn a_tone_lands_at_its_rf_offset_and_not_at_its_mirror() {
        let mut cv = RawToIq::new();
        let mut out = Vec::new();
        let bin = |off: f64| (2048.0 + off / (10e6 / 4096.0)).round() as usize;
        let db = |p: f32| 10.0 * p.log10();
        for off in [1_234_567.0, -3_100_000.0, 4_400_000.0] {
            cv.process(&tone(off, 800.0), &mut out);
            assert_eq!(out.len(), BLOCK_SAMPLES);
            let p = spectrum(&out);
            let peak = (0..p.len()).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap();
            assert!(peak.abs_diff(bin(off)) <= 1, "{off}: peak at bin {peak}, expected {}", bin(off));
            // The other side of the centre is where the inversion done wrong, or the half-band's alias, would put it.
            let mirror = p[bin(-off).saturating_sub(2)..bin(-off) + 3].iter().copied().fold(0.0, f32::max);
            assert!(db(p[peak]) - db(mirror) > 50.0, "{off}: mirror only {:.1} dB down", db(p[peak]) - db(mirror));
        }
        // libhydrasdr's scale: an 800-count cosine becomes a complex tone of 400/2048.
        cv.process(&tone(1e6, 800.0), &mut out);
        let amp = out.iter().skip(200).take(3000).map(|c| c.norm()).sum::<f32>() / 3000.0;
        assert!((amp - 400.0 / 2048.0).abs() < 0.005, "{amp}");
    }
}
