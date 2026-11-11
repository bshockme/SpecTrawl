//! IEEE 802.15.4g SUN FSK frames (Wi-SUN FAN, the 902–928 MHz smart-meter mesh): binary FSK at 50–300 kb/s, each
//! frame a preamble of alternating bits, then a 16-bit start-of-frame delimiter that also says whether the PHY header
//! and payload are FEC-coded. The preamble and SFD together are 32 fixed bits: they identify the frame outright.
//!
//! On air (926 MHz, 2026-11-10, R2) the band's "Wideband" bursts were 5–12 ms 2FSK frames at 150 kb/s, ±45 kHz,
//! hopping a 400 kHz grid (902.4 + 0.4n MHz): Wi-SUN operating mode 3. Every one carried the preamble and SFD 0x6F4E.

use num_complex::Complex32;

/// The data rates SUN FSK defines (operating modes 1a–4a and their kin).
pub const RATES: [f64; 5] = [50e3, 100e3, 150e3, 200e3, 300e3];
/// SFD for uncoded frames (phySunFskSfd = 0).
pub const SFD_UNCODED: u16 = 0x904E;
/// SFD for FEC-coded frames (phySunFskSfd = 0).
pub const SFD_CODED: u16 = 0x6F4E;
/// Preamble bits that must precede the SFD: the standard sends at least 32, and a burst's first bits can be lost to
/// its rise. With the SFD that is 40 fixed bits: a 1.5 s look tries a few million positions, about 1 in 200 000 looks
/// matching by chance.
const PREAMBLE_BITS: usize = 24;

/// Frames found in a window.
#[derive(Debug, Clone, PartialEq)]
pub struct SunFsk {
    /// The data rate, bit/s.
    pub rate: f64,
    /// Frames with each SFD.
    pub coded: usize,
    pub uncoded: usize,
}

impl SunFsk {
    pub fn frames(&self) -> usize {
        self.coded + self.uncoded
    }

    pub fn label(&self) -> String {
        format!("802.15.4g FSK {:.0} kb/s", self.rate / 1e3)
    }
}

/// Searches `x` (complex baseband centred on the signal, at `fs`, channelized to about its width: the pool's windows
/// and slices are 1.25 × it) for SUN FSK frames at each rate the sample rate allows: the discriminator, averaged over
/// each bit at eight timing phases (the average is each rate's own filter), sliced at zero, either polarity (an
/// inverted spectrum turns the SFD into its complement). The rate with the most frames wins.
pub fn detect(x: &[Complex32], fs: f64) -> Option<SunFsk> {
    if x.len() < 256 {
        return None;
    }
    // The discriminator's running sum: any bit's average in one subtraction.
    let mut sum = Vec::with_capacity(x.len());
    sum.push(0.0f64);
    for w in x.windows(2) {
        sum.push(sum.last().unwrap() + f64::from((w[1] * w[0].conj()).arg()));
    }
    let mut best: Option<SunFsk> = None;
    for rate in RATES {
        let sps = fs / rate;
        if sps < 2.0 {
            continue;
        }
        // Frame starts in samples, from every phase and polarity.
        let (mut coded, mut uncoded): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        for k in 0..8 {
            let bits = slice(&sum, sps, sps * k as f64 / 8.0);
            for (i, sfd) in find(&bits) {
                let at = (i as f64 * sps) as usize;
                if sfd == SFD_CODED { coded.push(at) } else { uncoded.push(at) }
            }
        }
        // A frame found at several phases or polarities counts once.
        let distinct = |v: &mut Vec<usize>| {
            v.sort_unstable();
            v.dedup_by(|a, b| a.abs_diff(*b) < (64.0 * sps) as usize);
            v.len()
        };
        let found = SunFsk { rate, coded: distinct(&mut coded), uncoded: distinct(&mut uncoded) };
        if found.frames() > 0 && best.as_ref().is_none_or(|b| found.frames() > b.frames()) {
            best = Some(found);
        }
    }
    best
}

/// One bit per `sps` samples from `phase`: the sign of the discriminator (`sum` is its running sum) over the bit's
/// middle half.
fn slice(sum: &[f64], sps: f64, phase: f64) -> Vec<bool> {
    let len = sum.len() - 1;
    let n = ((len as f64 - phase) / sps) as usize;
    (0..n)
        .map(|i| {
            let a = ((phase + (i as f64 + 0.25) * sps) as usize).min(len - 1);
            let b = ((phase + (i as f64 + 0.75) * sps) as usize).clamp(a + 1, len);
            sum[b] - sum[a] > 0.0
        })
        .collect()
}

/// Where `PREAMBLE_BITS` alternating bits end and an SFD follows (sent most significant bit first, in either polarity):
/// the bit position after the preamble, and which SFD.
fn find(bits: &[bool]) -> Vec<(usize, u16)> {
    let mut out = Vec::new();
    // Alternating bits ending at the current one.
    let mut run = 1usize;
    for j in 1..bits.len() {
        run = if bits[j] != bits[j - 1] { run + 1 } else { 1 };
        // The preamble's last bit is `j`; its SFD is the next 16. A longer preamble passes here at each of its bits.
        if run < PREAMBLE_BITS || j + 17 > bits.len() {
            continue;
        }
        let word = bits[j + 1..j + 17].iter().fold(0u16, |w, &b| (w << 1) | u16::from(b));
        for sfd in [SFD_CODED, SFD_UNCODED] {
            if word == sfd || word == !sfd {
                out.push((j + 1, sfd));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2FSK at `rate`, ±`dev` Hz, sampled at `fs`, with Gaussian-ish smoothing of the frequency, from `bits`.
    fn fsk(bits: &[bool], rate: f64, dev: f64, fs: f64) -> Vec<Complex32> {
        let sps = fs / rate;
        let n = (bits.len() as f64 * sps) as usize;
        let raw: Vec<f64> = (0..n).map(|k| if bits[((k as f64) / sps) as usize] { dev } else { -dev }).collect();
        let m = (sps / 2.0).max(1.0) as usize;
        let mut phase = 0.0f64;
        (0..n)
            .map(|k| {
                let lo = k.saturating_sub(m);
                let f = raw[lo..(k + m).min(n)].iter().sum::<f64>() / (k + m).min(n).saturating_sub(lo) as f64;
                phase += std::f64::consts::TAU * f / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex32> {
        let mut s = seed;
        let mut u = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        (0..n).map(|_| Complex32::new(u() * amp, u() * amp)).collect()
    }

    /// A burst in a window: 2 ms of nothing either side.
    fn burst(x: Vec<Complex32>) -> Vec<Complex32> {
        let quiet = vec![Complex32::default(); 1250];
        [quiet.clone(), x, quiet].concat()
    }

    /// The standard's shortest preamble (four octets of 0x55), the SFD and random bits.
    fn frame(sfd: u16, payload_bits: usize, seed: u64) -> Vec<bool> {
        let mut bits: Vec<bool> = (0..32).map(|i| i % 2 == 1).collect();
        bits.extend((0..16).rev().map(|i| (sfd >> i) & 1 == 1));
        let mut s = seed;
        bits.extend((0..payload_bits).map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s & 1 == 1
        }));
        bits
    }

    #[test]
    fn a_coded_frame_at_150_kbps_is_found_at_its_rate() {
        let fs = 625e3;
        let x: Vec<Complex32> =
            burst(fsk(&frame(SFD_CODED, 600, 7), 150e3, 37.5e3, fs)).iter().zip(noise(1 << 20, 0.3, 3)).map(|(a, b)| a + b).collect();
        let f = detect(&x, fs).expect("found");
        assert_eq!((f.rate, f.coded, f.uncoded), (150e3, 1, 0), "{f:?}");
        assert_eq!(f.label(), "802.15.4g FSK 150 kb/s");
    }

    #[test]
    fn an_uncoded_frame_at_50_kbps_is_found_and_inverted_spectra_too() {
        let fs = 625e3;
        let mut x = burst(fsk(&frame(SFD_UNCODED, 400, 9), 50e3, 25e3, fs));
        for v in &mut x {
            *v = v.conj();
        }
        let f = detect(&x, fs).expect("found");
        assert_eq!((f.rate, f.coded, f.uncoded), (50e3, 0, 1), "{f:?}");
    }

    #[test]
    fn fsk_without_the_sfd_and_noise_are_nothing() {
        let fs = 625e3;
        let mut s = 11u64;
        let bits: Vec<bool> = (0..2000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s & 1 == 1
            })
            .collect();
        assert_eq!(detect(&fsk(&bits, 150e3, 37.5e3, fs), fs), None);
        assert_eq!(detect(&noise(20_000, 1.0, 5), fs), None);
    }
}
