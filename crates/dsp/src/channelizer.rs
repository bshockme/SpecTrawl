//! Fast-convolution channelizer: one narrow channel cut from the bins of the
//! shared overlap-save FFT (N points, hop L = N/2) at a low output rate.

use std::f64::consts::PI;
use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::fir::windowed_sinc;

/// N: the shared forward FFT size the engine uses.
pub const FFT_SIZE: usize = 65_536;
/// L: the overlap-save hop.
pub const HOP: usize = FFT_SIZE / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignError {
    /// The channel needs more than N/4 bins.
    TooWide,
    /// The channel does not fit inside the usable part of the span.
    OutsideSpan,
    /// Non-positive bandwidth or rate, or N not a power of two.
    BadSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDesign {
    pub n: usize,
    pub k: usize,
    /// c = round(offset_hz / bin_hz).
    pub center_bin: i64,
    /// offset_hz − c·bin_hz.
    pub residual_hz: f64,
    /// fs·K/N.
    pub out_rate: f64,
    /// K/2.
    pub out_per_block: usize,
    /// H: K-point FFT of the zero-padded taps.
    pub response: Vec<Complex32>,
}

/// Designs a channel `bandwidth_hz` wide at `offset_hz` from the centre of an
/// `n`-point FFT of input at `fs`, with an output rate of at least `min_rate`
/// (and at least 1.6× the bandwidth).
pub fn design_channel(
    n: usize,
    fs: f64,
    usable_fraction: f64,
    offset_hz: f64,
    bandwidth_hz: f64,
    min_rate: f64,
) -> Result<ChannelDesign, DesignError> {
    if bandwidth_hz <= 0.0 || min_rate <= 0.0 || !n.is_power_of_two() || !(fs > 0.0 && usable_fraction > 0.0 && usable_fraction <= 1.0) {
        return Err(DesignError::BadSpec);
    }
    let need = min_rate.max(1.6 * bandwidth_hz);
    let mut k = 64usize;
    while fs * k as f64 / (n as f64) < need {
        if k > n / 4 {
            return Err(DesignError::TooWide);
        }
        k *= 2;
    }
    if k > n / 4 {
        return Err(DesignError::TooWide);
    }
    let out_rate = fs * k as f64 / n as f64;
    if offset_hz.abs() + out_rate / 2.0 > usable_fraction * fs / 2.0 {
        return Err(DesignError::OutsideSpan);
    }
    let bin_hz = fs / n as f64;
    let center_bin = (offset_hz / bin_hz).round() as i64;
    let residual_hz = offset_hz - center_bin as f64 * bin_hz;

    let taps = windowed_sinc(k / 2 + 1, bandwidth_hz / 2.0, out_rate);
    let mut response = vec![Complex32::default(); k];
    for (r, t) in response.iter_mut().zip(&taps) {
        *r = Complex32::new(*t, 0.0);
    }
    FftPlanner::new().plan_fft_forward(k).process(&mut response);

    Ok(ChannelDesign { n, k, center_bin, residual_hz, out_rate, out_per_block: k / 2, response })
}

/// Extracts one designed channel from successive shared-FFT blocks. Stateless
/// between blocks: a block back-filled from history equals the same block live.
pub struct Extractor {
    design: ChannelDesign,
    ifft: Arc<dyn Fft<f32>>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl Extractor {
    pub fn new(design: ChannelDesign) -> Extractor {
        let ifft = FftPlanner::new().plan_fft_inverse(design.k);
        let scratch = vec![Complex32::default(); ifft.get_inplace_scratch_len()];
        let buf = vec![Complex32::default(); design.k];
        Extractor { design, ifft, buf, scratch }
    }

    pub fn design(&self) -> &ChannelDesign {
        &self.design
    }

    /// `bins`: the N-point forward FFT (natural order) of input samples `n0 .. n0 + N`;
    /// writes the K/2 output samples for input positions `n0 + L .. n0 + N` into `out`.
    pub fn extract(&mut self, bins: &[Complex32], n0: u64, out: &mut [Complex32]) {
        let d = &self.design;
        let (n, k) = (d.n, d.k);
        assert_eq!(bins.len(), n, "extract: bins length must equal the design's FFT size");
        assert_eq!(out.len(), k / 2, "extract: out length must equal out_per_block (K/2)");
        let half = k as i64 / 2;
        for (j, y) in self.buf.iter_mut().enumerate() {
            let m = if (j as i64) < half { j as i64 } else { j as i64 - k as i64 };
            *y = bins[(d.center_bin + m).rem_euclid(n as i64) as usize] * d.response[j];
        }
        self.ifft.process_with_scratch(&mut self.buf, &mut self.scratch);

        // Mixer phase at the block start: the bin gather mixes from local index 0.
        let c = d.center_bin.rem_euclid(n as i64) as u64;
        let p = (c * (n0 % n as u64)) % n as u64;
        let rot = Complex32::from_polar(1.0 / n as f32, (-2.0 * PI * p as f64 / n as f64) as f32);
        // Residual mixer from the absolute input position.
        let (l, step) = ((n / 2) as u64, (n / k) as u64);
        let fs = d.out_rate * n as f64 / k as f64;
        for (i, (o, y)) in out.iter_mut().zip(&self.buf[k / 2..]).enumerate() {
            let pos = n0 + l + step * i as u64;
            let cycles = (d.residual_hz * pos as f64 / fs).rem_euclid(1.0);
            *o = y * rot * Complex32::from_polar(1.0, (-2.0 * PI * cycles) as f32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrum::SharedFft;
    use num_complex::Complex32;

    #[test]
    fn design_picks_the_rate_and_refuses_bad_specs() {
        let d = design_channel(65_536, 10e6, 0.9, 1.0e6, 12_500.0, 48_000.0).unwrap();
        assert_eq!((d.k, d.out_per_block), (512, 256));
        assert_eq!(d.out_rate, 78_125.0);
        assert_eq!(d.center_bin, 6554); // 1.0e6 / 152.587890625 = 6553.6
        assert!((d.residual_hz - (1.0e6 - 6554.0 * 10e6 / 65_536.0)).abs() < 1e-6);
        let narrow = design_channel(65_536, 10e6, 0.9, -2.0e6, 6_250.0, 8_000.0).unwrap();
        assert_eq!((narrow.k, narrow.out_rate), (128, 19_531.25));
        assert_eq!(design_channel(65_536, 10e6, 0.9, 0.0, 3.0e6, 48e3), Err(DesignError::TooWide));
        assert_eq!(design_channel(65_536, 10e6, 0.9, 4.49e6, 12_500.0, 48e3), Err(DesignError::OutsideSpan));
        assert_eq!(design_channel(65_536, 10e6, 0.9, 0.0, 0.0, 48e3), Err(DesignError::BadSpec));
    }

    #[test]
    fn a_nan_or_zero_rate_is_a_bad_spec() {
        assert_eq!(design_channel(65_536, f64::NAN, 0.9, 0.0, 12_500.0, 48e3), Err(DesignError::BadSpec));
        assert_eq!(design_channel(65_536, 0.0, 0.9, 0.0, 12_500.0, 48e3), Err(DesignError::BadSpec));
        assert_eq!(design_channel(65_536, 10e6, 0.0, 0.0, 12_500.0, 48e3), Err(DesignError::BadSpec));
        assert_eq!(design_channel(65_536, 10e6, 1.5, 0.0, 12_500.0, 48e3), Err(DesignError::BadSpec));
    }

    /// Twelve consecutive hops of a tone exactly on the channel centre, plus a
    /// neighbour 25 kHz up at the same level: the output is a steady DC with no
    /// step at block joins and the neighbour gone.
    fn steady_output_for(center_bin: i64) {
        let (n, fs) = (65_536usize, 10e6);
        let bin = fs / n as f64;
        let offset = center_bin as f64 * bin + 50.0; // residual 50 Hz
        let d = design_channel(n, fs, 0.9, offset, 12_500.0, 48_000.0).unwrap();
        assert_eq!(d.center_bin, center_bin);
        let l = n / 2;
        let tone = |f: f64, i: u64| Complex32::from_polar(0.5, (2.0 * std::f64::consts::PI * f * i as f64 / fs) as f32);
        let input: Vec<Complex32> = (0..(14 * l) as u64).map(|i| tone(offset, i) + tone(offset + 25e3, i)).collect();
        let mut fft = SharedFft::new(n);
        let mut bins = vec![Complex32::default(); n];
        let mut ex = Extractor::new(d.clone());
        let mut out = Vec::new();
        for b in 1..13u64 {
            let n0 = (b - 1) * l as u64;
            fft.forward(&input[n0 as usize..n0 as usize + n], &mut bins);
            let mut block = vec![Complex32::default(); d.out_per_block];
            ex.extract(&bins, n0, &mut block);
            if b >= 3 {
                out.extend(block);
            } // skip filter settling
        }
        let mean = out.iter().sum::<Complex32>() / out.len() as f32;
        assert!((mean.norm() - 0.5).abs() < 0.005, "gain {}", mean.norm());
        let ripple = out.iter().map(|y| (y - mean).norm()).fold(0.0f32, f32::max);
        assert!(ripple < 0.01, "ripple {ripple}: a block-phase step or a leaking neighbour");
    }

    #[test]
    fn even_centre_bin_is_steady_across_blocks() {
        steady_output_for(6554);
    }
    #[test]
    fn odd_centre_bin_is_steady_across_blocks() {
        steady_output_for(6555);
    }
    #[test]
    fn negative_odd_centre_bin_is_steady_across_blocks() {
        steady_output_for(-13_107);
    }
}
