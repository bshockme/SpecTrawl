//! FIR filter design.

/// Blackman-windowed sinc low-pass, normalised to unit DC gain (taps sum to 1).
/// `taps` should be odd so the filter is symmetric about a centre tap;
/// `cutoff_hz` is the one-sided cutoff at sample rate `fs`.
pub fn windowed_sinc(taps: usize, cutoff_hz: f64, fs: f64) -> Vec<f32> {
    let fc = cutoff_hz / fs;
    let m = (taps - 1) as f64;
    let mut h: Vec<f32> = (0..taps)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
            // Blackman window.
            let w =
                0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos() + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
            (sinc * w) as f32
        })
        .collect();
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|t| *t /= sum);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_sinc_has_unit_dc_gain_and_rejects_the_stopband() {
        let taps = windowed_sinc(65, 6_250.0, 78_125.0);
        assert_eq!(taps.len(), 65);
        let dc: f32 = taps.iter().sum();
        assert!((dc - 1.0).abs() < 1e-6);
        // Response at 25 kHz (adjacent channel): below -60 dB.
        let resp = |f: f64| -> f32 {
            let w = 2.0 * std::f64::consts::PI * f / 78_125.0;
            let (re, im) = taps
                .iter()
                .enumerate()
                .fold((0.0f64, 0.0f64), |(re, im), (i, &t)| (re + t as f64 * (w * i as f64).cos(), im - t as f64 * (w * i as f64).sin()));
            // |H(f)|, not Re H(f): the linear phase would halve the passband reading.
            (re * re + im * im).sqrt() as f32
        };
        assert!(20.0 * resp(25_000.0).log10() < -60.0);
        assert!((resp(1_000.0) - 1.0).abs() < 0.01);
    }
}
