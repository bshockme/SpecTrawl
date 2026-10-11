//! GSM BCCH carrier detection: the frequency-correction burst (FCCH) and the
//! synchronisation burst (SCH).
//!
//! A GSM beacon transmits all the time, 270.833 kbit/s GMSK in a 200 kHz
//! channel. Its FCCH is 148 zero bits, which GMSK turns into a pure tone a
//! quarter of the bit rate (67.7 kHz) above the carrier, 0.546 ms long,
//! repeating every 10 TDMA frames (46.15 ms; 11 frames once per
//! 51-multiframe). One frame (1250 bits) after each FCCH comes the SCH: a
//! burst with a fixed 64-bit training sequence and 78 coded bits carrying
//! the BSIC (network and base-station colour codes) and the reduced frame
//! number (TS 45.002 §3.3.2, 45.003 §4.7, 44.018 §9.1.30).
//!
//! Detection: a bank of complex moving averages (one per candidate carrier
//! offset) finds the tone bursts; their spacing must fit the frame grid.
//! Then the SCH is demodulated MSK-style (mix by −R/4, one-bit average,
//! phase from the training correlation), Viterbi-decoded (K = 5, rate 1/2)
//! and checked against its 10-bit parity.
//!
//! No GSM is on the air where this was written (US networks are shut
//! down); everything here is verified on synthetic signals only.

use num_complex::Complex32;

/// GSM bit rate, bit/s (1625/6 kbit/s).
pub const BIT_RATE: f64 = 1_625_000.0 / 6.0;
/// Bits per TDMA frame (8 slots of 156.25).
const FRAME_BITS: f64 = 1250.0;
/// Bits in a burst (tails included).
const BURST_BITS: usize = 148;
/// Carrier offsets tried for the FCCH tone, Hz (± this, in `LANE_STEP_HZ`).
const CFO_SEARCH_HZ: f64 = 6_000.0;
const LANE_STEP_HZ: f64 = 1_200.0;
/// Coherent-sum ratio a tone must reach (a 148-bit window of data or noise
/// gives ~1/√148 ≈ 0.08).
const TONE_RATIO: f32 = 0.35;
/// Training-correlation quality the SCH must reach.
const SCH_MIN_CORR: f32 = 0.4;

/// The SCH extended training sequence (TS 45.002 §5.2.5), 64 bits.
const SCH_TRAINING: [u8; 64] = [
    1, 0, 1, 1, 1, 0, 0, 1, 0, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 0, 0,
    1, 0, 1, 0, 1, 1, 1, 0, 1, 1, 0, 0, 0, 0, 1, 1, 0, 1, 1,
];

/// What was found on a channel.
#[derive(Debug, Clone, PartialEq)]
pub struct GsmCell {
    /// Carrier offset of the beacon from the channel centre, Hz.
    pub cfo_hz: f64,
    /// FCCH bursts found on the frame grid.
    pub fcch_hits: usize,
    /// SCH bursts that decoded with a good parity.
    pub sch_ok: usize,
    /// (NCC, BCC) from the SCH.
    pub bsic: Option<(u8, u8)>,
    /// TDMA frame number of the first decoded SCH.
    pub frame_number: Option<u32>,
}

impl GsmCell {
    /// "GSM BCCH BSIC 2-5" once the SCH decoded, else "GSM (FCCH)".
    pub fn label(&self) -> String {
        match self.bsic {
            Some((ncc, bcc)) => format!("GSM BCCH BSIC {ncc}-{bcc}"),
            None => "GSM (FCCH)".to_string(),
        }
    }

    /// Identity for an emitter: "BSIC 2-5".
    pub fn id(&self) -> Option<String> {
        self.bsic.map(|(n, b)| format!("BSIC {n}-{b}"))
    }
}

/// Looks for a GSM beacon in a channel of complex baseband at `fs` (the
/// channel should pass at least ±100 kHz). None when no FCCH grid is found.
pub fn detect(x: &[Complex32], fs: f64) -> Option<GsmCell> {
    if fs < 2.0 * BIT_RATE / 4.0 * 1.6 || x.len() as f64 / fs < 0.1 {
        return None;
    }
    let (hits, cfo) = fcch(x, fs)?;
    let mut cell = GsmCell { cfo_hz: cfo, fcch_hits: hits.len(), sch_ok: 0, bsic: None, frame_number: None };
    // The MSK de-rotation and a one-bit average, once for all SCH tries.
    let y = derotate(x, fs, BIT_RATE / 4.0 + cfo);
    let spb = fs / BIT_RATE;
    for h in &hits {
        let sch_start = *h + FRAME_BITS * spb;
        if sch_start + (BURST_BITS as f64 + 8.0) * spb >= y.len() as f64 {
            break;
        }
        if let Some((bsic, fn_)) = decode_sch(&y, spb, sch_start) {
            cell.sch_ok += 1;
            if cell.bsic.is_none() {
                cell.bsic = Some(bsic);
                cell.frame_number = Some(fn_);
            }
        }
    }
    Some(cell)
}

/// FCCH burst starts (fractional sample positions) that lie on the frame
/// grid, and the carrier offset measured on them.
fn fcch(x: &[Complex32], fs: f64) -> Option<(Vec<f64>, f64)> {
    let spb = fs / BIT_RATE;
    let w = (142.0 * spb).round() as usize;
    if x.len() < 4 * w {
        return None;
    }
    let n = x.len();
    // Moving average of |x| for normalisation.
    let mut mag = vec![0.0f32; n];
    let mut acc = 0.0f32;
    for i in 0..n {
        acc += x[i].norm();
        if i >= w {
            acc -= x[i - w].norm();
        }
        mag[i] = acc / w as f32;
    }
    // Best lane per sample: the complex moving average after mixing by
    // −(R/4 + lane offset), relative to the magnitude average.
    let lanes = (2.0 * CFO_SEARCH_HZ / LANE_STEP_HZ).round() as i32;
    let mut score = vec![0.0f32; n];
    let mut lane_of = vec![0i32; n];
    let mut mixed = vec![Complex32::default(); n];
    for k in -lanes / 2..=lanes / 2 {
        let f = BIT_RATE / 4.0 + k as f64 * LANE_STEP_HZ;
        let step = Complex32::from_polar(1.0, (-2.0 * std::f64::consts::PI * f / fs) as f32);
        let mut ph = Complex32::new(1.0, 0.0);
        for (m, v) in mixed.iter_mut().zip(x) {
            *m = v * ph;
            ph *= step;
            if m.re.is_nan() {
                break;
            }
        }
        // Renormalise the oscillator now and then (f32 drift).
        let mut acc = Complex32::default();
        for i in 0..n {
            acc += mixed[i];
            if i >= w {
                acc -= mixed[i - w];
            }
            if i % 4096 == 0 {
                // Re-sum occasionally to shed accumulated rounding.
                acc = mixed[i.saturating_sub(w - 1)..=i].iter().sum();
            }
            let s = if mag[i] > 0.0 { acc.norm() / w as f32 / mag[i] } else { 0.0 };
            if s > score[i] {
                score[i] = s;
                lane_of[i] = k;
            }
        }
    }
    // Bursts: runs above threshold; the run's best sample ends the window.
    let mut bursts: Vec<(f64, i32, usize)> = Vec::new(); // (start sample, lane, run length)
    let mut i = w;
    while i < n {
        if score[i] >= TONE_RATIO {
            let run_start = i;
            let (mut best, mut best_i) = (score[i], i);
            while i < n && score[i] >= TONE_RATIO {
                if score[i] > best {
                    best = score[i];
                    best_i = i;
                }
                i += 1;
            }
            let run = i - run_start;
            // A tone that stays on is a carrier, not an FCCH; a flicker
            // isn't one either. The run is centred on the burst (the
            // window slides through it), which places the start better
            // than the flat-topped peak does.
            if run < 3 * w && run >= w / 4 {
                // The window (w) overlaps the burst (148 bits) symmetrically
                // about the run's centre, which is at the burst start plus
                // (148 + w) / 2 window-end positions.
                let centre = (run_start + i) as f64 / 2.0;
                bursts.push((centre - (BURST_BITS as f64 * spb + w as f64) / 2.0, lane_of[best_i], run));
            }
        } else {
            i += 1;
        }
    }
    if bursts.len() < 2 {
        return None;
    }
    // Keep bursts whose distance to another is a whole number of frames,
    // at least 10 of them.
    let frame = FRAME_BITS * spb;
    let tol = 4.0 * spb;
    let on_grid: Vec<&(f64, i32, usize)> = bursts
        .iter()
        .filter(|a| {
            bursts.iter().any(|b| {
                let d = (a.0 - b.0).abs();
                let k = (d / frame).round();
                k >= 10.0 && (d - k * frame).abs() < tol
            })
        })
        .collect();
    if on_grid.len() < 2 {
        return None;
    }
    // Precise offset: lane plus the residual tone frequency inside each burst.
    let mut cfo = 0.0;
    for b in &on_grid {
        let f = BIT_RATE / 4.0 + b.1 as f64 * LANE_STEP_HZ;
        cfo += tone_residual(x, fs, f, (b.0 + 8.0 * spb) as usize, (132.0 * spb) as usize) + b.1 as f64 * LANE_STEP_HZ;
    }
    cfo /= on_grid.len() as f64;
    Some((on_grid.iter().map(|b| b.0).collect(), cfo))
}

/// Frequency of the tone in `x[start..start+len]` relative to `f_hz`, from
/// the phase of a lagged autocorrelation after mixing `f_hz` away.
fn tone_residual(x: &[Complex32], fs: f64, f_hz: f64, start: usize, len: usize) -> f64 {
    let lag = ((fs / 20_000.0).round() as usize).clamp(1, len / 4);
    let step = -2.0 * std::f64::consts::PI * f_hz / fs;
    let mut acc = Complex32::default();
    let end = (start + len).min(x.len());
    for i in start + lag..end {
        let a = x[i] * Complex32::from_polar(1.0, (step * i as f64) as f32);
        let b = x[i - lag] * Complex32::from_polar(1.0, (step * (i - lag) as f64) as f32);
        acc += a * b.conj();
    }
    acc.arg() as f64 / (2.0 * std::f64::consts::PI * lag as f64) * fs
}

/// Mixes `f_hz` away and averages over one bit: MSK de-rotation, after which
/// the sample at a bit's centre is ±(carrier phase) for bit 0/1.
fn derotate(x: &[Complex32], fs: f64, f_hz: f64) -> Vec<Complex32> {
    let step = -2.0 * std::f64::consts::PI * f_hz / fs;
    let w = (fs / BIT_RATE).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(x.len());
    let mut acc = Complex32::default();
    let mut buf: Vec<Complex32> = Vec::with_capacity(x.len());
    for (i, v) in x.iter().enumerate() {
        let m = v * Complex32::from_polar(1.0, (step * i as f64) as f32);
        buf.push(m);
        acc += m;
        if i >= w {
            acc -= buf[i - w];
        }
        out.push(acc / w as f32);
    }
    // The average lags by half a window; shift back so index = bit centre.
    let half = w / 2;
    out.drain(..half);
    out.extend(std::iter::repeat_n(Complex32::default(), half));
    out
}

fn at(y: &[Complex32], pos: f64) -> Complex32 {
    let i = pos.floor();
    if i < 0.0 || i as usize + 1 >= y.len() {
        return Complex32::default();
    }
    let f = (pos - i) as f32;
    let i = i as usize;
    y[i] * (1.0 - f) + y[i + 1] * f
}

/// Demodulates and decodes the SCH burst expected to start near sample
/// `start` of the de-rotated signal. Returns ((NCC, BCC), frame number).
fn decode_sch(y: &[Complex32], spb: f64, start: f64) -> Option<((u8, u8), u32)> {
    // Timing: the training sequence (bits 42..106) against ±8 bits in
    // quarter-bit steps.
    let train: Vec<f32> = SCH_TRAINING.iter().map(|b| if *b == 0 { 1.0 } else { -1.0 }).collect();
    let mut best: Option<(f32, f64, Complex32)> = None;
    let steps = (8.0 * 4.0) as i32;
    for q in -steps..=steps {
        let t0 = start + q as f64 * spb / 4.0;
        let mut corr = Complex32::default();
        let mut energy = 0.0f32;
        for (i, t) in train.iter().enumerate() {
            let r = at(y, t0 + (42.0 + i as f64 + 0.5) * spb);
            corr += r * *t;
            energy += r.norm_sqr();
        }
        if energy <= 0.0 {
            continue;
        }
        let q = corr.norm_sqr() / (energy * 64.0);
        if best.is_none_or(|b| q > b.0) {
            best = Some((q, t0, corr));
        }
    }
    let (quality, t0, corr) = best?;
    if quality < SCH_MIN_CORR {
        return None;
    }
    let rot = corr.conj() / corr.norm();
    // Soft data bits: 39 before the training, 39 after; positive = 0.
    let mut soft = Vec::with_capacity(78);
    for i in 3..42 {
        soft.push((at(y, t0 + (i as f64 + 0.5) * spb) * rot).re);
    }
    for i in 106..145 {
        soft.push((at(y, t0 + (i as f64 + 0.5) * spb) * rot).re);
    }
    let bits = viterbi(&soft);
    if !parity_ok(&bits) {
        return None;
    }
    Some(sch_fields(&bits[..25]))
}

/// Rate-1/2, K = 5 convolutional decoder (G0 = 1+D³+D⁴, G1 = 1+D+D³+D⁴),
/// 39 input bits with a zero tail; soft inputs positive for 0.
fn viterbi(soft: &[f32]) -> Vec<u8> {
    const N: usize = 39;
    const INF: f32 = f32::MAX / 4.0;
    let mut metric = [INF; 16];
    metric[0] = 0.0;
    let mut back = vec![[0u8; 16]; N];
    for k in 0..N {
        let mut next = [INF; 16];
        for s in 0..16usize {
            if metric[s] >= INF {
                continue;
            }
            for u in 0..2u8 {
                // State holds the last four inputs u(k-1)..u(k-4) (bit 0 = u(k-1)).
                let u1 = (s & 1) as u8;
                let u3 = ((s >> 2) & 1) as u8;
                let u4 = ((s >> 3) & 1) as u8;
                let c0 = u ^ u3 ^ u4;
                let c1 = u ^ u1 ^ u3 ^ u4;
                // Branch cost: distance from the soft values.
                let cost =
                    metric[s] + if c0 == 1 { soft[2 * k] } else { -soft[2 * k] } + if c1 == 1 { soft[2 * k + 1] } else { -soft[2 * k + 1] };
                let ns = ((s << 1) | u as usize) & 0xF;
                if cost < next[ns] {
                    next[ns] = cost;
                    back[k][ns] = s as u8;
                }
            }
        }
        metric = next;
    }
    // Terminated: end in state 0.
    let mut s = 0usize;
    let mut out = vec![0u8; N];
    for k in (0..N).rev() {
        out[k] = (s & 1) as u8;
        s = back[k][s] as usize;
    }
    out
}

/// g(D) = D¹⁰ + D⁸ + D⁶ + D⁵ + D⁴ + D² + 1.
const SCH_CRC_POLY: u32 = 0b101_0111_0101;

/// Remainder of (d(0..25) · D¹⁰ + p(0..10)) / g(D), as a 10-bit value.
fn crc_remainder(bits: &[u8]) -> u32 {
    let mut reg = 0u32;
    for &b in &bits[..35] {
        reg = (reg << 1) | b as u32;
        if reg & (1 << 10) != 0 {
            reg ^= SCH_CRC_POLY;
        }
    }
    reg & 0x3FF
}

/// The ten parity bits for 25 information bits: the remainder, inverted
/// (so that the whole word leaves an all-ones remainder).
fn parity(info: &[u8]) -> [u8; 10] {
    let mut w: Vec<u8> = info[..25].to_vec();
    w.extend([0u8; 10]);
    let r = crc_remainder(&w) ^ 0x3FF;
    std::array::from_fn(|i| ((r >> (9 - i)) & 1) as u8)
}

fn parity_ok(bits: &[u8]) -> bool {
    bits.len() >= 39 && crc_remainder(bits) == 0x3FF && bits[35..39] == [0, 0, 0, 0]
}

/// BSIC and frame number from the 25 information bits (octets filled
/// least-significant bit first, TS 44.018 figure 9.1.30.1).
fn sch_fields(d: &[u8]) -> ((u8, u8), u32) {
    let byte = |n: usize| -> u32 { (0..8).filter(|k| n * 8 + k < 25).map(|k| (d[n * 8 + k] as u32) << k).sum() };
    let (b0, b1, b2, b3) = (byte(0), byte(1), byte(2), byte(3));
    let bsic = (b0 >> 2) & 0x3F;
    let t1 = (b0 & 3) << 9 | b1 << 1 | (b2 >> 7);
    let t2 = (b2 >> 2) & 0x1F;
    let t3p = (b2 & 3) << 1 | (b3 >> 7);
    let t3 = 10 * t3p + 1;
    let fn_ = 51 * 26 * t1 + 51 * ((t3 + 26 - t2) % 26) + t3;
    (((bsic >> 3) as u8, (bsic & 7) as u8), fn_)
}

/// The 25 information bits for a BSIC and frame number (the inverse of
/// `sch_fields`), for tests and simulators.
pub fn sch_info(ncc: u8, bcc: u8, frame_number: u32) -> [u8; 25] {
    let t1 = frame_number / (26 * 51);
    let t2 = frame_number % 26;
    let t3 = frame_number % 51;
    let t3p = (t3 - 1) / 10;
    let bsic = ((ncc & 7) << 3 | (bcc & 7)) as u32;
    let b0 = bsic << 2 | (t1 >> 9) & 3;
    let b1 = (t1 >> 1) & 0xFF;
    let b2 = (t1 & 1) << 7 | t2 << 2 | (t3p >> 1) & 3;
    let b3 = (t3p & 1) << 7;
    let mut d = [0u8; 25];
    for (n, b) in [b0, b1, b2, b3].into_iter().enumerate() {
        for k in 0..8 {
            if n * 8 + k < 25 {
                d[n * 8 + k] = ((b >> k) & 1) as u8;
            }
        }
    }
    d
}

/// The 78 coded SCH bits for 25 information bits.
pub fn sch_encode(info: &[u8; 25]) -> [u8; 78] {
    let mut u = Vec::with_capacity(39);
    u.extend_from_slice(info);
    u.extend(parity(info));
    u.extend([0u8; 4]);
    let g = |k: isize| -> u8 { if k < 0 { 0 } else { u[k as usize] } };
    let mut c = [0u8; 78];
    for k in 0..39isize {
        c[2 * k as usize] = g(k) ^ g(k - 3) ^ g(k - 4);
        c[2 * k as usize + 1] = g(k) ^ g(k - 1) ^ g(k - 3) ^ g(k - 4);
    }
    c
}

/// Bits of an SCH burst (148, tails included) from its coded bits.
pub fn sch_burst(coded: &[u8; 78]) -> [u8; BURST_BITS] {
    let mut b = [0u8; BURST_BITS];
    b[3..42].copy_from_slice(&coded[..39]);
    b[42..106].copy_from_slice(&SCH_TRAINING);
    b[106..145].copy_from_slice(&coded[39..]);
    b
}

/// A GMSK (BT = 0.3) modulator for tests and simulators: bits at
/// `BIT_RATE`, complex baseband at `fs`, carrier offset `cfo_hz`.
pub fn gmsk(bits: &[u8], fs: f64, cfo_hz: f64) -> Vec<Complex32> {
    use std::f64::consts::PI;
    let spb = fs / BIT_RATE;
    // Frequency pulse: rect(T) convolved with the Gaussian, sampled at fs
    // over ±2 bits; normalised so a bit contributes π/2 of phase.
    let bt = 0.3;
    let sigma = (2.0f64.ln()).sqrt() / (2.0 * PI * bt); // in bits
    let span = 2.0;
    let m = (span * spb).ceil() as isize;
    let gauss = |t: f64| (-t * t / (2.0 * sigma * sigma)).exp();
    // Pulse sampled on the output grid, convolution with the bit rect done numerically.
    let pulse: Vec<f64> = (-m..=m)
        .map(|i| {
            let t = i as f64 / spb; // in bits
            let steps = 16;
            (0..steps).map(|s| gauss(t - (-0.5 + (s as f64 + 0.5) / steps as f64))).sum::<f64>() / steps as f64
        })
        .collect();
    let norm: f64 = pulse.iter().sum::<f64>();
    let pulse: Vec<f64> = pulse.iter().map(|p| p / norm * (PI / 2.0)).collect();
    // Differentially encoded symbols α = 1 − 2(b_i ⊕ b_{i−1}), b_{−1} = 1.
    let mut prev = 1u8;
    let alpha: Vec<f64> = bits
        .iter()
        .map(|&b| {
            let d = b ^ prev;
            prev = b;
            1.0 - 2.0 * d as f64
        })
        .collect();
    let n = (bits.len() as f64 * spb) as usize;
    let mut dphi = vec![0.0f64; n];
    for (i, a) in alpha.iter().enumerate() {
        let c = ((i as f64 + 0.5) * spb).round() as isize;
        for (j, p) in pulse.iter().enumerate() {
            let k = c + j as isize - m;
            if k >= 0 && (k as usize) < n {
                dphi[k as usize] += a * p;
            }
        }
    }
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(n);
    for (i, d) in dphi.iter().enumerate() {
        phase += d;
        let ph = phase + 2.0 * PI * cfo_hz * i as f64 / fs;
        out.push(Complex32::new(ph.cos() as f32, ph.sin() as f32));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bit(&mut self) -> u8 {
            (self.next() >> 60 & 1) as u8
        }
        fn gauss(&mut self) -> f32 {
            let u: f32 = (0..4).map(|_| (self.next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5).sum();
            u * (3.0f32).sqrt() // ~N(0,1)
        }
    }

    /// A BCCH carrier: `frames` TDMA frames starting at frame `first_fn`,
    /// FCCH/SCH in slot 0 of the right frames, random bursts elsewhere.
    fn beacon_bits(first_fn: u32, frames: usize, ncc: u8, bcc: u8, rng: &mut Rng) -> Vec<u8> {
        let mut bits = Vec::with_capacity(frames * 1250);
        for f in 0..frames {
            let fn_ = first_fn + f as u32;
            let t3 = fn_ % 51;
            for slot in 0..8 {
                let burst: Vec<u8> = if slot == 0 && t3 % 10 == 0 && t3 <= 40 {
                    vec![0u8; BURST_BITS]
                } else if slot == 0 && t3 % 10 == 1 && t3 <= 41 {
                    sch_burst(&sch_encode(&sch_info(ncc, bcc, fn_))).to_vec()
                } else {
                    (0..BURST_BITS).map(|_| rng.bit()).collect()
                };
                bits.extend(burst);
                // Guard period: 8.25 bits; keep the modulator running on random bits.
                bits.extend((0..8).map(|_| rng.bit()));
                if slot == 3 || slot == 7 {
                    bits.push(rng.bit()); // the .25s add up to one bit per four slots
                }
            }
        }
        bits
    }

    fn add_noise(x: &mut [Complex32], snr_db: f32, rng: &mut Rng) {
        let sigma = 10f32.powf(-snr_db / 20.0) / 2f32.sqrt();
        for v in x.iter_mut() {
            *v += Complex32::new(rng.gauss() * sigma, rng.gauss() * sigma);
        }
    }

    #[test]
    fn sch_coding_round_trips_and_parity_catches_errors() {
        // Frame numbers of SCH frames (T3 in 1, 11, 21, 31, 41): the reduced
        // frame number is exact only for those.
        for (ncc, bcc, fn_) in [(2u8, 5u8, 123_421u32), (0, 0, 1), (7, 7, 2_715_598), (4, 6, 51 * 26 * 9 + 41)] {
            let info = sch_info(ncc, bcc, fn_);
            assert_eq!(sch_fields(&info), ((ncc, bcc), fn_));
            let coded = sch_encode(&info);
            let soft: Vec<f32> = coded.iter().map(|b| if *b == 0 { 1.0 } else { -1.0 }).collect();
            let bits = viterbi(&soft);
            assert!(parity_ok(&bits));
            assert_eq!(&bits[..25], &info[..]);
            // A couple of flipped coded bits are corrected...
            let mut s2 = soft.clone();
            s2[10] = -s2[10];
            s2[41] = -s2[41];
            let bits = viterbi(&s2);
            assert!(parity_ok(&bits) && bits[..25] == info[..]);
        }
        // ...and a wrong word fails parity.
        let mut info = sch_info(3, 1, 500);
        let mut bits = info.to_vec();
        bits.extend(parity(&info));
        bits.extend([0u8; 4]);
        assert!(parity_ok(&bits));
        info[4] ^= 1;
        let mut bad = info.to_vec();
        bad.extend(&bits[25..]);
        assert!(!parity_ok(&bad));
    }

    #[test]
    fn beacon_is_found_with_bsic_and_frame_number() {
        let fs = 500_000.0;
        for (seed, cfo, snr, delay, first_fn) in
            [(1u64, 350.0f64, 20.0f32, 777usize, 100u32), (2, -2_900.0, 12.0, 3_333, 51 * 26 * 7 + 38), (3, 4_400.0, 8.0, 12_345, 2_000)]
        {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
            let bits = beacon_bits(first_fn, 60, 2, 5, &mut rng);
            let mut x = gmsk(&bits, fs, cfo);
            // Random start inside the stream.
            x.drain(..delay);
            add_noise(&mut x, snr, &mut rng);
            let cell = detect(&x, fs).unwrap_or_else(|| panic!("no beacon found (seed {seed})"));
            assert!((cell.cfo_hz - cfo).abs() < 100.0, "cfo {} vs {cfo} (seed {seed})", cell.cfo_hz);
            assert!(cell.fcch_hits >= 4, "fcch hits {} (seed {seed})", cell.fcch_hits);
            assert_eq!(cell.bsic, Some((2, 5)), "seed {seed}: {cell:?}");
            assert!(cell.sch_ok >= 2, "sch ok {} (seed {seed})", cell.sch_ok);
            let fn_ = cell.frame_number.unwrap();
            // The first decoded SCH is in one of the stream's frames.
            assert!(fn_ > first_fn && fn_ < first_fn + 60, "frame number {fn_} (seed {seed})");
            assert_eq!(cell.label(), "GSM BCCH BSIC 2-5");
        }
    }

    #[test]
    fn nothing_on_noise_or_fm() {
        let fs = 500_000.0;
        let mut rng = Rng(42);
        let mut noise = vec![Complex32::default(); (0.6 * fs) as usize];
        add_noise(&mut noise, 0.0, &mut rng);
        assert_eq!(detect(&noise, fs), None);
        // Wideband FM of a 1 kHz tone, ±75 kHz, plus noise.
        let mut phase = 0.0f64;
        let mut fm: Vec<Complex32> = (0..(0.6 * fs) as usize)
            .map(|i| {
                let t = i as f64 / fs;
                phase += 2.0 * std::f64::consts::PI * 75e3 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin() / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        add_noise(&mut fm, 20.0, &mut rng);
        assert_eq!(detect(&fm, fs), None);
        // A steady carrier right at the FCCH tone frequency isn't a beacon.
        let cw: Vec<Complex32> = (0..(0.6 * fs) as usize)
            .map(|i| Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * (BIT_RATE / 4.0) * i as f64 / fs) as f32))
            .collect();
        assert_eq!(detect(&cw, fs), None);
    }
}
