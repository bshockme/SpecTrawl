//! LTE downlink cell identification from a narrow slice of the carrier.
//!
//! Three stages, each cheap enough to run on a focus receiver every few
//! seconds, and each usable on its own:
//!
//! 1. **Numerology** (`cp_autocorrelation`): blind OFDM detection. Every OFDM
//!    symbol repeats its tail as a cyclic prefix, so the signal correlates
//!    with itself at a lag of one useful symbol (66.7 µs at 15 kHz subcarrier
//!    spacing, 33.3 µs at 30 kHz), with peaks once per symbol. Folding the
//!    lagged product over 1 ms (a whole number of symbols for both spacings)
//!    makes the peaks stand out of noise. No synchronisation needed; also
//!    gives the fractional carrier offset.
//! 2. **PSS/SSS** (`cell_sync`): the primary synchronisation signal is one
//!    of three Zadoff-Chu sequences on the central 62 subcarriers, sent every
//!    5 ms; the secondary one next to it carries the cell-identity group and
//!    tells subframe 0 from 5. Together: the physical cell ID (0..503),
//!    frame timing to a sample, and the carrier offset to tens of Hz.
//! 3. **MIB** (`decode_mib`): the broadcast channel in the central 72
//!    subcarriers of subframe 0 (slot 1, symbols 0..3), spread over 40 ms:
//!    the carrier's true bandwidth, the frame number and the antenna count.
//!    Tail-biting convolutional code, QPSK, transmit diversity on two ports
//!    (the common case) or one; four-port cells are recognised by their
//!    CRC mask only through the decoders that fail, so they stay at stage 2.
//!
//! Everything runs at 1.92 MS/s (128-point FFT, the central 1.08 MHz), which
//! is independent of the carrier's bandwidth: a 20 MHz carrier's sync block
//! fits a 1.4 MHz slice. `identify` resamples other rates.
//!
//! NR (5G) uses a different PSS (an m-sequence in a 240-subcarrier block that
//! need not sit at the carrier centre); only its numerology is recognised
//! here, which is enough to tell an NR carrier from an LTE one in the shared
//! low bands.

use num_complex::Complex32;
use rustfft::FftPlanner;
use std::f64::consts::PI;

/// Working sample rate: 128 subcarriers of 15 kHz.
pub const FS: f64 = 1.92e6;
/// FFT size at `FS`.
const N: usize = 128;
/// Normal cyclic prefix: 10 samples on the first symbol of a slot, 9 after.
const CP0: usize = 10;
const CP: usize = 9;
/// Samples per OFDM symbol (after the first of a slot), slot, subframe,
/// half-frame and frame at `FS`.
const SYM: usize = N + CP;
const SLOT: usize = 960;
const SUBFRAME: usize = 2 * SLOT;
const HALF_FRAME: usize = 5 * SUBFRAME;
const FRAME: usize = 10 * SUBFRAME;
/// Data part of symbol `l` of a slot starts at `10 + 137 l`.
fn symbol_start(l: usize) -> usize {
    CP0 + SYM * l
}
/// PSS: last symbol of slot 0 (the SSS is the symbol before).
const PSS_OFFSET: usize = CP0 + SYM * 6;

/// Zadoff-Chu roots of the three PSS sequences (N_ID_2 = 0, 1, 2).
const PSS_ROOTS: [u32; 3] = [25, 29, 34];

// --------------------------------------------------------------- Numerology

/// OFDM structure found by cyclic-prefix autocorrelation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Numerology {
    /// Subcarrier spacing: 15 (LTE, NR) or 30 (NR) kHz.
    pub scs_khz: u32,
    /// Peak normalised correlation, 0..1 (≈ SNR/(SNR+1) on a clean carrier).
    pub rho: f32,
    /// Carrier frequency offset modulo the subcarrier spacing, Hz.
    pub cfo_hz: f32,
}

/// Peak correlation below this is noise.
const MIN_RHO: f32 = 0.3;

/// Blind detection of LTE/NR OFDM numerology on `x` at `fs` (any rate from
/// about 1 MS/s up; the lag is chosen in samples). Uses up to 40 ms.
pub fn cp_autocorrelation(x: &[Complex32], fs: f64) -> Option<Numerology> {
    let take = x.len().min((0.040 * fs) as usize);
    let x = &x[..take];
    let mut best: Option<Numerology> = None;
    for scs in [15u32, 30] {
        let n_f = fs / (scs as f64 * 1e3);
        let n = n_f.round() as usize;
        if n < 16 || (n_f - n as f64).abs() > 0.02 * n_f {
            continue;
        }
        // Normal CP is 144/2048 of the useful symbol (160/2048 on the first).
        let l = ((n as f64 * 144.0 / 2048.0).round() as usize).max(2);
        let period = (fs * 1e-3).round() as usize;
        if x.len() < 2 * period + n + l {
            continue;
        }
        let folds = (x.len() - n - l) / period;
        let mut c = vec![Complex32::default(); period];
        let mut p = vec![0f32; period];
        for j in 0..folds {
            let base = j * period;
            // Running sums of y[i] = x[i] conj(x[i+n]) and the energies.
            let mut acc = Complex32::default();
            let mut e = 0f32;
            for i in 0..l {
                acc += x[base + i] * x[base + i + n].conj();
                e += x[base + i].norm_sqr() + x[base + i + n].norm_sqr();
            }
            for k in 0..period {
                c[k] += acc;
                p[k] += 0.5 * e;
                // Slide the window by one sample. (The last fold may run into
                // the following period; bounds were checked above.)
                let i0 = base + k;
                acc -= x[i0] * x[i0 + n].conj();
                e -= x[i0].norm_sqr() + x[i0 + n].norm_sqr();
                let i1 = i0 + l;
                acc += x[i1] * x[i1 + n].conj();
                e += x[i1].norm_sqr() + x[i1 + n].norm_sqr();
            }
        }
        let (mut k_best, mut rho) = (0usize, 0f32);
        for k in 0..period {
            let r = if p[k] > 0.0 { c[k].norm() / p[k] } else { 0.0 };
            if r > rho {
                rho = r;
                k_best = k;
            }
        }
        if rho < MIN_RHO {
            continue;
        }
        // Peaks must be narrow: OFDM gives one peak of ~l samples per symbol;
        // a tone or a constant gives a flat profile.
        let high = (0..period).filter(|&k| p[k] > 0.0 && c[k].norm() / p[k] > 0.5 * rho).count();
        let symbols = (1e-3 * scs as f64 * 1e3) as usize;
        if high > symbols * (2 * l + 2) {
            continue;
        }
        let cfo = (-c[k_best].arg() as f64 * fs / (2.0 * PI * n as f64)) as f32;
        let cand = Numerology { scs_khz: scs, rho, cfo_hz: cfo };
        if best.is_none_or(|b| cand.rho > b.rho) {
            best = Some(cand);
        }
    }
    best
}

// ---------------------------------------------------------------- Sequences

/// Zadoff-Chu PSS sequence for root `u`, 62 values (36.211 §6.11.1.1).
fn pss_sequence(u: u32) -> [Complex32; 62] {
    let mut d = [Complex32::default(); 62];
    for n in 0..62 {
        let ph = if n < 31 { -PI * u as f64 * (n * (n + 1)) as f64 / 63.0 } else { -PI * u as f64 * ((n + 1) * (n + 2)) as f64 / 63.0 };
        d[n] = Complex32::new(ph.cos() as f32, ph.sin() as f32);
    }
    d
}

/// FFT bin of the `i`th of the 62 sync subcarriers (DC left out).
fn sync_bin(i: usize) -> usize {
    let k: isize = if i < 31 { i as isize - 31 } else { i as isize - 30 };
    k.rem_euclid(N as isize) as usize
}

/// FFT bin of RE `k` (0..71) of the central six resource blocks.
fn pbch_bin(k: usize) -> usize {
    let f: isize = if k < 36 { k as isize - 36 } else { k as isize - 35 };
    f.rem_euclid(N as isize) as usize
}

/// Time-domain PSS symbol (128 samples, unit-energy per sample scale is
/// irrelevant for the normalised correlation).
fn pss_time(u: u32) -> Vec<Complex32> {
    let d = pss_sequence(u);
    let mut buf = vec![Complex32::default(); N];
    for (i, v) in d.iter().enumerate() {
        buf[sync_bin(i)] = *v;
    }
    FftPlanner::new().plan_fft_inverse(N).process(&mut buf);
    buf
}

/// m-sequence of length 31 from a recurrence over the last 5 bits (as ±1).
fn m_sequence(taps: &[usize]) -> [f32; 31] {
    let mut x = [0u8; 31];
    x[4] = 1;
    for i in 0..26 {
        x[i + 5] = taps.iter().map(|&t| x[i + t]).fold(0, |a, b| a ^ b);
    }
    let mut s = [0f32; 31];
    for i in 0..31 {
        s[i] = 1.0 - 2.0 * x[i] as f32;
    }
    s
}

/// (m0, m1) for a cell-identity group (36.211 table 6.11.2.1-1).
fn sss_m(n_id_1: u32) -> (usize, usize) {
    let q_p = n_id_1 / 30;
    let q = (n_id_1 + q_p * (q_p + 1) / 2) / 30;
    let m_p = n_id_1 + q * (q + 1) / 2;
    let m0 = m_p % 31;
    let m1 = (m0 + m_p / 31 + 1) % 31;
    (m0 as usize, m1 as usize)
}

/// SSS for (N_ID_1, N_ID_2) in subframe 0 (`second` false) or 5, 62 values.
fn sss_sequence(n_id_1: u32, n_id_2: u32, second: bool) -> [f32; 62] {
    let s = m_sequence(&[0, 2]);
    let c = m_sequence(&[0, 3]);
    let z = m_sequence(&[0, 1, 2, 4]);
    let (m0, m1) = sss_m(n_id_1);
    let n2 = n_id_2 as usize;
    let mut d = [0f32; 62];
    for n in 0..31 {
        let s0 = s[(n + m0) % 31];
        let s1 = s[(n + m1) % 31];
        let c0 = c[(n + n2) % 31];
        let c1 = c[(n + n2 + 3) % 31];
        let z0 = z[(n + m0 % 8) % 31];
        let z1 = z[(n + m1 % 8) % 31];
        if !second {
            d[2 * n] = s0 * c0;
            d[2 * n + 1] = s1 * c1 * z0;
        } else {
            d[2 * n] = s1 * c0;
            d[2 * n + 1] = s0 * c1 * z1;
        }
    }
    d
}

/// Gold sequence c(n), n < len (36.211 §7.2).
fn gold(c_init: u32, len: usize) -> Vec<u8> {
    const NC: usize = 1600;
    let total = NC + len + 31;
    let mut x1 = vec![0u8; total];
    let mut x2 = vec![0u8; total];
    x1[0] = 1;
    for i in 0..31 {
        x2[i] = ((c_init >> i) & 1) as u8;
    }
    for i in 0..total - 31 {
        x1[i + 31] = x1[i + 3] ^ x1[i];
        x2[i + 31] = x2[i + 3] ^ x2[i + 2] ^ x2[i + 1] ^ x2[i];
    }
    (0..len).map(|n| x1[n + NC] ^ x2[n + NC]).collect()
}

/// Cell-specific reference symbols for one port in slot `ns`, symbol `l`:
/// the 12 values on the central six resource blocks (sequence index
/// m' = 104..115, the same for every carrier bandwidth).
fn crs_values(pci: u32, ns: usize, l: usize) -> [Complex32; 12] {
    let c_init = (1 << 10) * (7 * (ns as u32 + 1) + l as u32 + 1) * (2 * pci + 1) + 2 * pci + 1;
    let c = gold(c_init, 2 * 116);
    let mut r = [Complex32::default(); 12];
    for m in 0..12 {
        let mp = m + 104;
        r[m] = Complex32::new(1.0 - 2.0 * c[2 * mp] as f32, 1.0 - 2.0 * c[2 * mp + 1] as f32) * std::f32::consts::FRAC_1_SQRT_2;
    }
    r
}

/// RE indices (0..71) of port `port`'s reference symbols in symbol `l` of
/// slot `ns` (ports 0 and 1: `l` 0 or 4; ports 2 and 3: `l` 1), for cell
/// `pci` (36.211 6.10.1.2).
fn crs_positions(pci: u32, port: usize, ns: usize, l: usize) -> [usize; 12] {
    let v = match (port, l) {
        (0, 0) | (1, 4) => 0,
        (0, _) | (1, _) => 3,
        (2, _) => 3 * (ns % 2),
        _ => 3 + 3 * (ns % 2),
    };
    let shift = (v + pci as usize % 6) % 6;
    std::array::from_fn(|m| 6 * m + shift)
}

// ------------------------------------------------------------------- Sync

/// Timing, identity and carrier offset of an LTE cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSync {
    /// Physical cell identity, 3·N_ID_1 + N_ID_2.
    pub pci: u32,
    pub n_id_1: u32,
    pub n_id_2: u32,
    /// Carrier frequency offset of the signal as given (positive: the cell
    /// is above the slice's centre), Hz.
    pub cfo_hz: f32,
    /// Sample index (at `FS`) where the first whole radio frame in the data
    /// starts.
    pub frame_start: usize,
    /// Normalised PSS correlation, 0..1.
    pub corr_peak: f32,
    /// Normalised SSS correlation (how clearly the group was told apart).
    pub sss_peak: f32,
}

/// Rotates `x` by `-f` Hz into `out`.
fn shift(x: &[Complex32], f: f64, out: &mut Vec<Complex32>) {
    out.clear();
    out.reserve(x.len());
    let w = -2.0 * PI * f / FS;
    let step = Complex32::new(w.cos() as f32, w.sin() as f32);
    let mut osc = Complex32::new(1.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        out.push(v * osc);
        osc *= step;
        if i % 4096 == 4095 {
            osc /= osc.norm();
        }
    }
}

/// PSS correlation at `n`: the two halves of the symbol separately, so a
/// carrier offset up to ±7.5 kHz costs little, and their phase difference
/// measures the offset. Returns (metric 0..1, half correlations).
fn pss_metric(x: &[Complex32], n: usize, t: &[Complex32], t_energy: f32) -> (f32, Complex32, Complex32) {
    let (mut c1, mut c2) = (Complex32::default(), Complex32::default());
    let mut e = 0f32;
    for i in 0..N / 2 {
        let v = x[n + i];
        c1 += v * t[i].conj();
        e += v.norm_sqr();
    }
    for i in N / 2..N {
        let v = x[n + i];
        c2 += v * t[i].conj();
        e += v.norm_sqr();
    }
    let m = if e > 0.0 { (c1.norm() + c2.norm()).powi(2) / (e * t_energy) } else { 0.0 };
    (m, c1, c2)
}

/// A PSS detection must reach this normalised correlation (noise peaks
/// over a half-frame search reach ~0.1).
const MIN_PSS: f32 = 0.2;
/// ...and the SSS this (random sequences score ~0.1 at best).
const MIN_SSS: f32 = 0.25;

/// Finds the PSS and SSS. `x` is at `FS`; `cfo_hint` is the fractional
/// offset from the cyclic prefix (None: search in 7.5 kHz steps). Needs at
/// least 5.1 ms of signal; uses up to 40 ms.
pub fn cell_sync(x: &[Complex32], cfo_hint: Option<f32>) -> Option<CellSync> {
    let take = x.len().min(4 * FRAME + SUBFRAME);
    let x = &x[..take];
    if x.len() < HALF_FRAME + SYM + N {
        return None;
    }
    let templates: Vec<Vec<Complex32>> = PSS_ROOTS.iter().map(|&u| pss_time(u)).collect();
    let t_energy: Vec<f32> = templates.iter().map(|t| t.iter().map(|v| v.norm_sqr()).sum()).collect();
    // Integer carrier offsets (multiples of the subcarrier spacing, or of
    // half of it without a hint) over ±60 kHz: the slice is centred on an
    // occupancy estimate that can be tens of kHz off.
    let hyps: Vec<f64> = match cfo_hint {
        Some(f) => (-4..=4).map(|k| f as f64 + 15e3 * k as f64).collect(),
        None => (-8..=8).map(|k| 7.5e3 * k as f64).collect(),
    };
    // Coarse: one half-frame (a PSS is certain to be inside) per hypothesis;
    // the next half-frames only if the first held nothing (a stretch that
    // starts before the carrier came up).
    let coarse_len = HALF_FRAME + N;
    let mut rot = Vec::new();
    let mut best = (0f32, 0f64, 0usize, 0usize); // metric, cfo, n_id_2, position
    for w in 0..(x.len() - N) / HALF_FRAME {
        let win = &x[w * HALF_FRAME..(w * HALF_FRAME + coarse_len).min(x.len())];
        for &h in &hyps {
            shift(win, h, &mut rot);
            for (id2, t) in templates.iter().enumerate() {
                for n in 0..rot.len() - N {
                    let (m, _, _) = pss_metric(&rot, n, t, t_energy[id2]);
                    if m > best.0 {
                        best = (m, h, id2, n);
                    }
                }
            }
        }
        if best.0 >= MIN_PSS {
            break;
        }
    }
    if best.0 < MIN_PSS {
        return None;
    }
    let (_, h, n_id_2, _) = best;
    // Fine: the chosen template over all the data, folded per half-frame.
    shift(x, h, &mut rot);
    let t = &templates[n_id_2];
    let occurrences = (rot.len() - N) / HALF_FRAME;
    let mut folded = vec![0f32; HALF_FRAME];
    for n in 0..occurrences * HALF_FRAME {
        folded[n % HALF_FRAME] += pss_metric(&rot, n, t, t_energy[n_id_2]).0;
    }
    let n0 = (0..HALF_FRAME).max_by(|&a, &b| folded[a].total_cmp(&folded[b]))?;
    // Carrier offset: the phase between the symbol halves at every occurrence.
    let mut ph = Complex32::default();
    let mut peak = 0f32;
    for j in 0..occurrences {
        let (m, c1, c2) = pss_metric(&rot, n0 + j * HALF_FRAME, t, t_energy[n_id_2]);
        ph += c2 * c1.conj();
        peak = peak.max(m);
    }
    let fine = ph.arg() as f64 * FS / (2.0 * PI * (N / 2) as f64);
    let mut cfo = h + fine;
    shift(x, cfo, &mut rot);
    // Residual from the cyclic prefixes of the whole stretch (±7.5 kHz range).
    if let Some(num) = cp_autocorrelation(&rot, FS)
        && num.scs_khz == 15
        && num.cfo_hz.abs() < 2e3
    {
        cfo += num.cfo_hz as f64;
        shift(x, cfo, &mut rot);
    }
    // SSS: the symbol before the PSS, equalised by the PSS. Occurrences
    // alternate between subframes 0 and 5; the two hypotheses are which
    // parity is subframe 0. Scores sum over every occurrence.
    let fft = FftPlanner::new().plan_fft_forward(N);
    let mut scores = vec![0f32; 168 * 2];
    let mut scored = 0usize;
    for j in 0..occurrences {
        let p = n0 + j * HALF_FRAME;
        if p < SYM {
            continue;
        }
        let mut pss_f = rot[p..p + N].to_vec();
        let mut sss_f = rot[p - SYM..p - SYM + N].to_vec();
        fft.process(&mut pss_f);
        fft.process(&mut sss_f);
        let d = pss_sequence(PSS_ROOTS[n_id_2]);
        let mut z = [Complex32::default(); 62];
        let mut e = 0f32;
        for i in 0..62 {
            let b = sync_bin(i);
            let hch = pss_f[b] * d[i].conj();
            z[i] = sss_f[b] * hch.conj();
            e += z[i].norm_sqr();
        }
        if e <= 0.0 {
            continue;
        }
        scored += 1;
        for n_id_1 in 0..168u32 {
            for hyp in 0..2usize {
                // Under hypothesis `hyp`, occurrence j is subframe 5 when
                // (j + hyp) is odd.
                let second = (j + hyp) % 2 == 1;
                let s = sss_sequence(n_id_1, n_id_2 as u32, second);
                let acc: Complex32 = z.iter().zip(s.iter()).map(|(a, b)| a * b).sum();
                scores[n_id_1 as usize * 2 + hyp] += acc.norm_sqr() / (e * 62.0);
            }
        }
    }
    if scored == 0 {
        return None;
    }
    let (mut sss_peak, mut n_id_1, mut hyp) = (0f32, 0u32, 0usize);
    for g in 0..168u32 {
        for h in 0..2usize {
            let s = scores[g as usize * 2 + h] / scored as f32;
            if s > sss_peak {
                (sss_peak, n_id_1, hyp) = (s, g, h);
            }
        }
    }
    if sss_peak < MIN_SSS {
        return None;
    }
    // Occurrence 0 is subframe 0 under hypothesis 0, subframe 5 under 1;
    // the PSS data part sits at PSS_OFFSET into its half-frame.
    let mut frame_start = n0 as isize - PSS_OFFSET as isize - if hyp == 1 { HALF_FRAME as isize } else { 0 };
    while frame_start < 0 {
        frame_start += FRAME as isize;
    }
    Some(CellSync {
        pci: 3 * n_id_1 + n_id_2 as u32,
        n_id_1,
        n_id_2: n_id_2 as u32,
        cfo_hz: cfo as f32,
        frame_start: frame_start as usize,
        corr_peak: peak,
        sss_peak,
    })
}

// -------------------------------------------------------------------- MIB

/// The master information block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mib {
    /// Downlink bandwidth in resource blocks: 6, 15, 25, 50, 75 or 100.
    pub bandwidth_rb: u16,
    /// PHICH duration: false normal, true extended.
    pub phich_extended: bool,
    /// PHICH resource: 0 = 1/6, 1 = 1/2, 2 = 1, 3 = 2.
    pub phich_resource: u8,
    /// System frame number, 0..1023.
    pub sfn: u16,
    /// Cell-specific antenna ports: 1, 2 or 4.
    pub antenna_ports: u8,
}

impl Mib {
    pub fn bandwidth_mhz(&self) -> f32 {
        match self.bandwidth_rb {
            6 => 1.4,
            15 => 3.0,
            25 => 5.0,
            50 => 10.0,
            75 => 15.0,
            _ => 20.0,
        }
    }
}

const BW_TABLE: [u16; 6] = [6, 15, 25, 50, 75, 100];

/// Column permutation of the rate-matching sub-block interleaver (36.212
/// table 5.1.4-2).
const PERM: [usize; 32] =
    [1, 17, 9, 25, 5, 21, 13, 29, 3, 19, 11, 27, 7, 23, 15, 31, 0, 16, 8, 24, 4, 20, 12, 28, 2, 18, 10, 26, 6, 22, 14, 30];

/// Order in which the 40 coded bits of one stream leave the sub-block
/// interleaver: original bit indices.
fn interleave_order() -> [usize; 40] {
    let mut out = [0usize; 40];
    let mut n = 0;
    for c in 0..32 {
        for r in 0..2 {
            let i = r * 32 + PERM[c];
            if i >= 24 {
                out[n] = i - 24;
                n += 1;
            }
        }
    }
    out
}

/// CRC-16 (D^16 + D^12 + D^5 + 1) over `bits`.
fn crc16(bits: &[u8]) -> u16 {
    let mut crc: u32 = 0;
    for &b in bits {
        let fb = ((crc >> 15) & 1) as u8 ^ b;
        crc = ((crc << 1) & 0xFFFF) ^ if fb == 1 { 0x1021 } else { 0 };
    }
    crc as u16
}

/// Convolutional code generators (133, 171, 165 octal), as tap masks over
/// [c_k, c_k-1, ..., c_k-6].
const GEN: [u8; 3] = [0o133, 0o171, 0o165];

fn conv_output(window: u8) -> [u8; 3] {
    // `window` bit 6 = c_k ... bit 0 = c_{k-6}.
    std::array::from_fn(|i| (window & GEN[i]).count_ones() as u8 & 1)
}

/// Tail-biting convolutional encoder, three output streams.
fn conv_encode(c: &[u8]) -> [Vec<u8>; 3] {
    let k = c.len();
    let mut out: [Vec<u8>; 3] = Default::default();
    for i in 0..k {
        let mut w = 0u8;
        for j in 0..7 {
            let idx = (i as isize - j as isize).rem_euclid(k as isize) as usize;
            w |= c[idx] << (6 - j);
        }
        let d = conv_output(w);
        for s in 0..3 {
            out[s].push(d[s]);
        }
    }
    out
}

/// Tail-biting Viterbi over soft bits `llr[k][s]` (positive = 0), trying
/// every start state and demanding it be the end state.
fn conv_decode(llr: &[[f32; 3]]) -> (Vec<u8>, f32) {
    let k = llr.len();
    let mut best = (f32::MIN, Vec::new());
    for start in 0..64u8 {
        // state = bits c_{k-1}..c_{k-6} (c_{k-1} in bit 5).
        let mut metric = [f32::MIN; 64];
        metric[start as usize] = 0.0;
        let mut prev = vec![[0u8; 64]; k];
        let mut bit = vec![[0u8; 64]; k];
        for i in 0..k {
            let mut next = [f32::MIN; 64];
            for s in 0..64u8 {
                if metric[s as usize] == f32::MIN {
                    continue;
                }
                for b in 0..2u8 {
                    let w = (b << 6) | s;
                    let d = conv_output(w);
                    let m: f32 = (0..3).map(|j| if d[j] == 0 { llr[i][j] } else { -llr[i][j] }).sum();
                    let ns = ((w >> 1) & 0x3F) as usize;
                    let cand = metric[s as usize] + m;
                    if cand > next[ns] {
                        next[ns] = cand;
                        prev[i][ns] = s;
                        bit[i][ns] = b;
                    }
                }
            }
            metric = next;
        }
        let end = start as usize;
        if metric[end] > best.0 {
            let mut bits = vec![0u8; k];
            let mut s = end;
            for i in (0..k).rev() {
                bits[i] = bit[i][s];
                s = prev[i][s] as usize;
            }
            best = (metric[end], bits);
        }
    }
    (best.1, best.0)
}

/// Encodes a MIB into the 1920 scrambled PBCH bits of one 40 ms period.
pub fn encode_pbch(mib: &Mib, pci: u32) -> Vec<u8> {
    let mut bits = Vec::with_capacity(40);
    let bw = BW_TABLE.iter().position(|&b| b == mib.bandwidth_rb).unwrap_or(3) as u8;
    for j in (0..3).rev() {
        bits.push((bw >> j) & 1);
    }
    bits.push(mib.phich_extended as u8);
    bits.push((mib.phich_resource >> 1) & 1);
    bits.push(mib.phich_resource & 1);
    let sfn_msb = (mib.sfn >> 2) as u8;
    for j in (0..8).rev() {
        bits.push((sfn_msb >> j) & 1);
    }
    bits.extend(std::iter::repeat_n(0, 10));
    let mask: u16 = match mib.antenna_ports {
        2 => 0xFFFF,
        4 => 0x5555,
        _ => 0,
    };
    let crc = crc16(&bits) ^ mask;
    for j in (0..16).rev() {
        bits.push(((crc >> j) & 1) as u8);
    }
    let streams = conv_encode(&bits);
    let order = interleave_order();
    let mut w = Vec::with_capacity(120);
    for s in 0..3 {
        for &i in &order {
            w.push(streams[s][i]);
        }
    }
    let scr = gold(pci, 1920);
    (0..1920).map(|i| w[i % 120] ^ scr[i]).collect()
}

/// Available PBCH REs (0..71) of slot-1 symbol `l`, in mapping order.
fn pbch_res(pci: u32, l: usize) -> Vec<usize> {
    let shift = (pci % 3) as usize;
    (0..72).filter(|k| l >= 2 || k % 3 != shift).collect()
}

/// Channel estimate for `port` over the 72 central REs of slot 1 of
/// subframe 0, from its reference symbols (ports 0 and 1: symbols 0 and 4;
/// ports 2 and 3: symbol 1), linearly interpolated between pilots and held
/// flat beyond the ends. Also a crude SNR from the difference between
/// neighbouring pilots (frequency selectivity counts as noise, so it is a
/// lower bound). `syms` are the FFTs of slot-1 symbols 0..=4.
fn channel(syms: &[Vec<Complex32>], pci: u32, port: usize) -> ([Complex32; 72], f32) {
    let symbols: &[usize] = if port < 2 { &[0, 4] } else { &[1] };
    let mut known: Vec<(usize, Complex32)> = Vec::new();
    let (mut power, mut noise, mut pairs) = (0f32, 0f32, 0usize);
    for &l in symbols {
        let vals = crs_values(pci, 1, l);
        let hs: Vec<(usize, Complex32)> =
            crs_positions(pci, port, 1, l).iter().enumerate().map(|(m, &k)| (k, syms[l][pbch_bin(k)] * vals[m].conj())).collect();
        for w in hs.windows(2) {
            noise += (w[0].1 - w[1].1).norm_sqr() / 2.0;
            pairs += 1;
        }
        power += hs.iter().map(|h| h.1.norm_sqr()).sum::<f32>();
        known.extend(hs);
    }
    known.sort_by_key(|(k, _)| *k);
    let mut h = [Complex32::default(); 72];
    for (k, hk) in h.iter_mut().enumerate() {
        let after = known.iter().position(|(kk, _)| *kk >= k);
        *hk = match after {
            Some(0) => known[0].1,
            None => known[known.len() - 1].1,
            Some(j) => {
                let (k0, h0) = known[j - 1];
                let (k1, h1) = known[j];
                let t = (k - k0) as f32 / (k1 - k0) as f32;
                h0 * (1.0 - t) + h1 * t
            }
        };
    }
    let snr_db = 10.0 * (power / known.len() as f32 / (noise / pairs.max(1) as f32).max(1e-12)).max(1e-6).log10();
    (h, snr_db)
}

/// Soft PBCH bits of one radio frame (480 values, positive = 0, weighted by
/// channel power), decoded assuming `ports` transmit antennas (1, 2 or 4).
/// `frame` is the index of the first sample of the frame in `x` (at `FS`,
/// CFO-corrected). Also returns the per-port CRS SNRs.
fn pbch_llrs(x: &[Complex32], frame: usize, pci: u32, ports: usize) -> Option<(Vec<f32>, Vec<f32>)> {
    let fft = FftPlanner::new().plan_fft_forward(N);
    let slot1 = frame + SLOT;
    if slot1 + symbol_start(4) + N > x.len() {
        return None;
    }
    let mut sym = |l: usize| -> Vec<Complex32> {
        let s = slot1 + symbol_start(l);
        let mut b = x[s..s + N].to_vec();
        fft.process(&mut b);
        b
    };
    let syms: Vec<Vec<Complex32>> = (0..5).map(&mut sym).collect();
    let est: Vec<([Complex32; 72], f32)> = (0..ports).map(|p| channel(&syms, pci, p)).collect();
    let h = |p: usize, k: usize| est[p].0[k];
    let snrs: Vec<f32> = est.iter().map(|e| e.1).collect();
    let mut llr = Vec::with_capacity(480);
    // Alamouti pair on REs (ka, kb) sent by ports (pa, pb): pa sends
    // (x0, x1), pb sends (-x1*, x0*).
    let alamouti = |l: usize, ka: usize, kb: usize, pa: usize, pb: usize, out: &mut Vec<f32>| {
        let (r0, r1) = (syms[l][pbch_bin(ka)], syms[l][pbch_bin(kb)]);
        let (h0, h1) = ((h(pa, ka) + h(pa, kb)) * 0.5, (h(pb, ka) + h(pb, kb)) * 0.5);
        let x0 = h0.conj() * r0 + h1 * r1.conj();
        let x1 = h0.conj() * r1 - h1 * r0.conj();
        out.extend([x0.re, x0.im, x1.re, x1.im]);
    };
    for l in 0..4 {
        let res = pbch_res(pci, l);
        match ports {
            1 => {
                for &k in &res {
                    let xh = syms[l][pbch_bin(k)] * h(0, k).conj();
                    llr.push(xh.re);
                    llr.push(xh.im);
                }
            }
            2 => {
                for pair in res.chunks(2) {
                    alamouti(l, pair[0], pair[1], 0, 1, &mut llr);
                }
            }
            _ => {
                // SFBC + FSTD: ports 0 and 2 on the first two REs of each
                // four, ports 1 and 3 on the other two.
                for quad in res.chunks(4) {
                    alamouti(l, quad[0], quad[1], 0, 2, &mut llr);
                    alamouti(l, quad[2], quad[3], 1, 3, &mut llr);
                }
            }
        }
    }
    Some((llr, snrs))
}

/// Decodes the MIB from the frames starting at `sync.frame_start` (any
/// number from 1 to 4 consecutive frames; more improve the soft combining).
pub fn decode_mib(x: &[Complex32], sync: &CellSync) -> Option<Mib> {
    decode_mib_impl(x, sync, None)
}

/// `decode_mib` with a line of diagnostics per step pushed to `log`: CRS
/// SNR per port and frame, and the Viterbi metric and CRC outcome of every
/// port-count and TTI-phase hypothesis.
pub fn decode_mib_debug(x: &[Complex32], sync: &CellSync, log: &mut Vec<String>) -> Option<Mib> {
    decode_mib_impl(x, sync, Some(log))
}

fn decode_mib_impl(x: &[Complex32], sync: &CellSync, mut log: Option<&mut Vec<String>>) -> Option<Mib> {
    let pci = sync.pci;
    let frames: Vec<usize> = (0..4).map(|j| sync.frame_start + j * FRAME).filter(|&f| f + SLOT + symbol_start(4) + N <= x.len()).collect();
    if frames.is_empty() {
        if let Some(l) = log.as_deref_mut() {
            l.push("no whole frame in the data".into());
        }
        return None;
    }
    let scr = gold(pci, 1920);
    let order = interleave_order();
    let mut found: Option<Mib> = None;
    for ports in [2usize, 4, 1] {
        let per_frame: Vec<(Vec<f32>, Vec<f32>)> = frames.iter().filter_map(|&f| pbch_llrs(x, f, pci, ports)).collect();
        if per_frame.is_empty() {
            continue;
        }
        if let Some(l) = log.as_deref_mut() {
            for (j, (_, snrs)) in per_frame.iter().enumerate() {
                let s: Vec<String> = snrs.iter().enumerate().map(|(p, v)| format!("p{p} {v:.1} dB")).collect();
                l.push(format!("{ports} ports, frame {j}: CRS SNR {}", s.join(", ")));
            }
        }
        for phase in 0..4usize {
            // Coded-bit soft values: 3 streams x 40, combined over the
            // frames that share frame 0's 40 ms TTI (the next TTI carries a
            // different payload: SFN + 4, another CRC), so under this
            // phase hypothesis only frames j < 4 - phase are added.
            let mut soft = vec![[0f32; 3]; 40];
            let mut energy = 0f32;
            for (j, (llr, _)) in per_frame.iter().enumerate().take(4 - phase) {
                let seg = phase + j;
                for i in 0..480 {
                    let g = seg * 480 + i;
                    let v = if scr[g] == 1 { -llr[i] } else { llr[i] };
                    let w = g % 120;
                    let (stream, pos) = (w / 40, order[w % 40]);
                    soft[pos][stream] += v;
                    energy += v.abs();
                }
            }
            let (bits, metric) = conv_decode(&soft);
            let mask: u16 = match ports {
                2 => 0xFFFF,
                4 => 0x5555,
                _ => 0,
            };
            let crc = crc16(&bits[..24]);
            let got = bits[24..40].iter().fold(0u16, |a, &b| (a << 1) | b as u16);
            let ok = crc ^ mask == got;
            if let Some(l) = log.as_deref_mut() {
                l.push(format!(
                    "{ports} ports, phase {phase}: metric {:.3} of {:.3} ({:.0}%), crc {}",
                    metric,
                    energy,
                    100.0 * metric / energy.max(1e-9),
                    if ok { "ok" } else { "bad" }
                ));
            }
            if !ok {
                continue;
            }
            let bw = ((bits[0] << 2) | (bits[1] << 1) | bits[2]) as usize;
            if bw >= BW_TABLE.len() {
                continue;
            }
            let sfn_msb = bits[6..14].iter().fold(0u16, |a, &b| (a << 1) | b as u16);
            let mib = Mib {
                bandwidth_rb: BW_TABLE[bw],
                phich_extended: bits[3] == 1,
                phich_resource: (bits[4] << 1) | bits[5],
                sfn: (sfn_msb << 2) | phase as u16,
                antenna_ports: ports as u8,
            };
            if log.is_none() {
                return Some(mib);
            }
            found.get_or_insert(mib);
        }
    }
    found
}

// --------------------------------------------------------------- Identify

/// What was learned about a cellular carrier.
#[derive(Debug, Clone, PartialEq)]
pub struct LteCell {
    pub numerology: Numerology,
    /// LTE synchronisation, when the PSS/SSS were found.
    pub sync: Option<CellSync>,
    pub mib: Option<Mib>,
}

impl LteCell {
    /// "LTE 10 MHz PCI 217 (2 ports)", "LTE PCI 217", "NR 30 kHz OFDM" or
    /// "15 kHz OFDM" (LTE or NR, no LTE sync found).
    pub fn label(&self) -> String {
        match (&self.sync, &self.mib) {
            (Some(s), Some(m)) => format!(
                "LTE {} MHz PCI {} ({} port{})",
                m.bandwidth_mhz(),
                s.pci,
                m.antenna_ports,
                if m.antenna_ports == 1 { "" } else { "s" }
            ),
            (Some(s), None) => format!("LTE PCI {}", s.pci),
            (None, _) if self.numerology.scs_khz == 30 => "NR 30 kHz OFDM".into(),
            (None, _) => "15 kHz OFDM".into(),
        }
    }
}

/// Resamples `x` from `fs_in` to `FS` (windowed sinc, 16 taps).
pub fn resample(x: &[Complex32], fs_in: f64) -> Vec<Complex32> {
    if (fs_in - FS).abs() < 1.0 {
        return x.to_vec();
    }
    let ratio = fs_in / FS;
    let r = (FS / fs_in).min(1.0);
    let n_out = ((x.len() as f64 - 1.0) / ratio).floor().max(0.0) as usize;
    const TAPS: isize = 8;
    let mut out = Vec::with_capacity(n_out);
    for m in 0..n_out {
        let t = m as f64 * ratio;
        let c = t.floor() as isize;
        let mut acc = Complex32::default();
        for n in c - TAPS + 1..=c + TAPS {
            if n < 0 || n as usize >= x.len() {
                continue;
            }
            let d = (t - n as f64) * r;
            let s = if d.abs() < 1e-9 { 1.0 } else { (PI * d).sin() / (PI * d) };
            let w = 0.5 + 0.5 * (PI * (t - n as f64) / TAPS as f64).cos();
            acc += x[n as usize] * ((s * r * w) as f32);
        }
        out.push(acc);
    }
    out
}

/// Runs the three stages on a slice of a carrier (any sample rate at or
/// above ~1.5 MS/s; at least 1.1 MHz around the carrier centre, ideally
/// 40 ms or more).
pub fn identify(x: &[Complex32], fs: f64) -> Option<LteCell> {
    let x = resample(x, fs);
    let numerology = cp_autocorrelation(&x, FS)?;
    if numerology.scs_khz != 15 {
        return Some(LteCell { numerology, sync: None, mib: None });
    }
    let Some(sync) = cell_sync(&x, Some(numerology.cfo_hz)) else {
        return Some(LteCell { numerology, sync: None, mib: None });
    };
    let mut rot = Vec::new();
    shift(&x, sync.cfo_hz as f64, &mut rot);
    let mib = decode_mib(&rot, &sync);
    Some(LteCell { numerology, sync: Some(sync), mib })
}

/// `identify` with the MIB stage's diagnostics pushed to `log`.
pub fn identify_debug(x: &[Complex32], fs: f64, log: &mut Vec<String>) -> Option<LteCell> {
    let x = resample(x, fs);
    let numerology = cp_autocorrelation(&x, FS)?;
    if numerology.scs_khz != 15 {
        return Some(LteCell { numerology, sync: None, mib: None });
    }
    let Some(sync) = cell_sync(&x, Some(numerology.cfo_hz)) else {
        log.push("no PSS/SSS sync".into());
        return Some(LteCell { numerology, sync: None, mib: None });
    };
    let mut rot = Vec::new();
    shift(&x, sync.cfo_hz as f64, &mut rot);
    let mib = decode_mib_debug(&rot, &sync, log);
    Some(LteCell { numerology, sync: Some(sync), mib })
}

// ------------------------------------------------------------------ Synth

/// A synthetic LTE downlink at `FS`, for tests and examples: PSS/SSS, CRS
/// on the configured ports, PBCH with the given MIB, and random QPSK on the
/// other central REs.
pub mod synth {
    use super::*;

    pub struct Cell {
        pub pci: u32,
        pub mib: Mib,
        /// Random QPSK filling the rest of the central 72 subcarriers.
        pub fill: bool,
    }

    /// `frames` radio frames starting at SFN `mib.sfn & !3` (so the TTI
    /// phase is 0 at the start), 19200 samples each.
    pub fn frames(cell: &Cell, frames: usize, seed: u64) -> Vec<Complex32> {
        let mut rng = seed | 1;
        let mut rnd = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let ifft = FftPlanner::new().plan_fft_inverse(N);
        let ports = cell.mib.antenna_ports as usize;
        let pbch = encode_pbch(&cell.mib, cell.pci);
        let (n_id_2, n_id_1) = (cell.pci % 3, cell.pci / 3);
        let pss = pss_sequence(PSS_ROOTS[n_id_2 as usize]);
        let mut out = Vec::with_capacity(frames * FRAME);
        for f in 0..frames {
            let phase = f % 4;
            for ns in 0..20 {
                for l in 0..7 {
                    // Per-port frequency grids.
                    let mut grid = vec![vec![Complex32::default(); N]; ports];
                    let mut used = [false; 72];
                    // Reference symbols: ports 0/1 in symbols 0 and 4, ports
                    // 2/3 in symbol 1; every port leaves the others' pilot
                    // REs empty.
                    let crs_ports: Vec<usize> = match l {
                        0 | 4 => (0..ports.min(2)).collect(),
                        1 if ports == 4 => vec![2, 3],
                        _ => Vec::new(),
                    };
                    for &p in &crs_ports {
                        let vals = crs_values(cell.pci, ns, l);
                        for (m, &k) in crs_positions(cell.pci, p, ns, l).iter().enumerate() {
                            grid[p][pbch_bin(k)] = vals[m];
                            used[k] = true;
                        }
                    }
                    for &p in &crs_ports {
                        for &k in crs_positions(cell.pci, p ^ 1, ns, l).iter() {
                            used[k] = true;
                        }
                    }
                    let sync_slot = (ns == 0 || ns == 10) && (l == 5 || l == 6);
                    if sync_slot {
                        let second = ns == 10;
                        let s = sss_sequence(n_id_1, n_id_2, second);
                        for i in 0..62 {
                            let v = if l == 6 { pss[i] } else { Complex32::new(s[i], 0.0) };
                            // Sync signals go out on every port alike (scaled).
                            for g in grid.iter_mut() {
                                g[sync_bin(i)] = v / (ports as f32).sqrt();
                            }
                            // RE index of sync subcarrier i within the 72 (which skip DC, as `sync_bin` does).
                            let k = i + 5;
                            used[k] = true;
                        }
                        // The 5 guard subcarriers either side of the sync
                        // signals are empty.
                        for k in (0..5).chain(67..72) {
                            used[k] = true;
                        }
                    }
                    let pbch_sym = ns == 1 && l < 4;
                    if pbch_sym {
                        let res = pbch_res(cell.pci, l);
                        // Bit offset of this symbol in the frame's 480.
                        let before: usize = (0..l).map(|ll| pbch_res(cell.pci, ll).len() * 2).sum();
                        let bits = &pbch[phase * 480 + before..phase * 480 + before + res.len() * 2];
                        let qpsk: Vec<Complex32> = bits
                            .chunks(2)
                            .map(|b| Complex32::new(1.0 - 2.0 * b[0] as f32, 1.0 - 2.0 * b[1] as f32) * std::f32::consts::FRAC_1_SQRT_2)
                            .collect();
                        let s2 = std::f32::consts::FRAC_1_SQRT_2;
                        // Alamouti pair (x0, x1) on REs (ka, kb) from ports (pa, pb).
                        let mut pair =
                            |grid: &mut Vec<Vec<Complex32>>, ka: usize, kb: usize, pa: usize, pb: usize, x0: Complex32, x1: Complex32| {
                                grid[pa][pbch_bin(ka)] = x0 * s2;
                                grid[pa][pbch_bin(kb)] = x1 * s2;
                                grid[pb][pbch_bin(ka)] = -x1.conj() * s2;
                                grid[pb][pbch_bin(kb)] = x0.conj() * s2;
                                used[ka] = true;
                                used[kb] = true;
                            };
                        match ports {
                            1 => {
                                for (i, &k) in res.iter().enumerate() {
                                    grid[0][pbch_bin(k)] = qpsk[i];
                                    used[k] = true;
                                }
                            }
                            2 => {
                                for (i, p) in res.chunks(2).enumerate() {
                                    pair(&mut grid, p[0], p[1], 0, 1, qpsk[2 * i], qpsk[2 * i + 1]);
                                }
                            }
                            _ => {
                                for (i, q) in res.chunks(4).enumerate() {
                                    pair(&mut grid, q[0], q[1], 0, 2, qpsk[4 * i], qpsk[4 * i + 1]);
                                    pair(&mut grid, q[2], q[3], 1, 3, qpsk[4 * i + 2], qpsk[4 * i + 3]);
                                }
                            }
                        }
                    }
                    if cell.fill {
                        for k in 0..72 {
                            if !used[k] {
                                let r = rnd();
                                let v = Complex32::new(1.0 - 2.0 * (r & 1) as f32, 1.0 - 2.0 * ((r >> 1) & 1) as f32)
                                    * std::f32::consts::FRAC_1_SQRT_2;
                                grid[0][pbch_bin(k)] = v / (ports as f32).sqrt();
                                if ports > 1 {
                                    let r = rnd();
                                    grid[1][pbch_bin(k)] = Complex32::new(1.0 - 2.0 * (r & 1) as f32, 1.0 - 2.0 * ((r >> 1) & 1) as f32)
                                        * std::f32::consts::FRAC_1_SQRT_2
                                        / (ports as f32).sqrt();
                                }
                            }
                        }
                    }
                    // Sum the ports through one (flat, equal) channel.
                    let mut td = vec![Complex32::default(); N];
                    for g in &mut grid {
                        ifft.process(g);
                        for (a, b) in td.iter_mut().zip(g.iter()) {
                            *a += b;
                        }
                    }
                    let cp = if l == 0 { CP0 } else { CP };
                    out.extend_from_slice(&td[N - cp..]);
                    out.extend_from_slice(&td);
                }
            }
        }
        out
    }

    /// Delays by `delay` samples (zeros in front), applies a carrier offset
    /// and adds white noise at `snr_db` relative to the signal power.
    pub fn impair(x: &[Complex32], delay: usize, cfo_hz: f64, snr_db: f32, seed: u64) -> Vec<Complex32> {
        let mut rng = seed | 1;
        let mut gauss = move || {
            let mut u = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng >> 11) as f64 / (1u64 << 53) as f64
            };
            let (u1, u2) = (u().max(1e-12), u());
            ((-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()) as f32
        };
        let power: f32 = x.iter().map(|v| v.norm_sqr()).sum::<f32>() / x.len() as f32;
        let sigma = (power / 10f32.powf(snr_db / 10.0) / 2.0).sqrt();
        let mut out = Vec::with_capacity(x.len() + delay);
        for i in 0..x.len() + delay {
            let s = if i >= delay { x[i - delay] } else { Complex32::default() };
            let ph = 2.0 * PI * cfo_hz * i as f64 / FS;
            let rot = Complex32::new(ph.cos() as f32, ph.sin() as f32);
            out.push(s * rot + Complex32::new(gauss() * sigma, gauss() * sigma));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::synth::{Cell, frames, impair};
    use super::*;

    fn cell(pci: u32, ports: u8) -> Cell {
        Cell { pci, mib: Mib { bandwidth_rb: 50, phich_extended: false, phich_resource: 1, sfn: 0x2A4, antenna_ports: ports }, fill: true }
    }

    #[test]
    fn sequences_have_the_right_shape() {
        assert_eq!(sss_m(0), (0, 1));
        assert_eq!(sss_m(1), (1, 2));
        assert_eq!(sss_m(29), (29, 30));
        assert_eq!(sss_m(30), (0, 2));
        // Every group has a distinct (m0, m1).
        let mut seen = std::collections::HashSet::new();
        for g in 0..168 {
            assert!(seen.insert(sss_m(g)), "duplicate for group {g}");
        }
        // PSS correlation between roots is low.
        let a = pss_time(25);
        let b = pss_time(29);
        let cross: Complex32 = a.iter().zip(&b).map(|(x, y)| x * y.conj()).sum();
        let auto: f32 = a.iter().map(|x| x.norm_sqr()).sum();
        assert!(cross.norm() / auto < 0.3);
    }

    #[test]
    fn crc_and_conv_round_trip() {
        let bits: Vec<u8> = (0..24).map(|i| ((i * 7 + 3) % 5 == 0) as u8).collect();
        let crc = crc16(&bits);
        let mut all = bits.clone();
        for j in (0..16).rev() {
            all.push(((crc >> j) & 1) as u8);
        }
        let streams = conv_encode(&all);
        let soft: Vec<[f32; 3]> = (0..40).map(|i| std::array::from_fn(|s| 1.0 - 2.0 * streams[s][i] as f32)).collect();
        assert_eq!(conv_decode(&soft).0, all);
        // A few flipped bits still decode.
        let mut noisy = soft.clone();
        noisy[3][0] = -noisy[3][0];
        noisy[17][2] = -noisy[17][2];
        noisy[30][1] = -noisy[30][1];
        assert_eq!(conv_decode(&noisy).0, all);
    }

    #[test]
    fn numerology_from_a_clean_carrier_and_none_from_noise() {
        let x = frames(&cell(301, 2), 2, 5);
        let y = impair(&x, 0, 1234.0, 10.0, 9);
        let n = cp_autocorrelation(&y, FS).expect("ofdm");
        assert_eq!(n.scs_khz, 15);
        assert!(n.rho > 0.6, "rho {}", n.rho);
        assert!((n.cfo_hz - 1234.0).abs() < 150.0, "cfo {}", n.cfo_hz);
        // The same samples played at twice the rate are a 30 kHz carrier.
        let n2 = cp_autocorrelation(&y, 2.0 * FS).expect("ofdm");
        assert_eq!(n2.scs_khz, 30);
        let noise = impair(&vec![Complex32::new(1.0, 0.0); 40 * SUBFRAME], 0, 0.0, -40.0, 3);
        assert!(cp_autocorrelation(&noise, FS).is_none());
    }

    #[test]
    fn finds_pci_timing_and_offset() {
        for (pci, cfo, delay, snr) in [(217u32, 2600.0f64, 1234usize, 10.0f32), (0, -18_400.0, 7, 5.0), (503, 300.0, 19_000, 15.0)] {
            let x = frames(&cell(pci, 2), 3, 11);
            let y = impair(&x, delay, cfo, snr, 17);
            let hint = cp_autocorrelation(&y, FS).map(|n| n.cfo_hz);
            let s = cell_sync(&y, hint).unwrap_or_else(|| panic!("sync {pci} cfo {cfo} delay {delay} snr {snr} hint {hint:?}"));
            assert_eq!(s.pci, pci, "pci");
            assert!((s.cfo_hz as f64 - cfo).abs() < 50.0, "cfo {} vs {cfo}", s.cfo_hz);
            let expect = delay % FRAME;
            assert!((s.frame_start as isize - expect as isize).abs() <= 1, "frame start {} vs {expect}", s.frame_start);
        }
    }

    #[test]
    fn survives_low_snr() {
        // 0 dB in the 1.08 MHz slice: sync and MIB over four frames.
        let x = frames(&cell(130, 2), 4, 43);
        let y = impair(&x, 2500, 700.0, 0.0, 47);
        let got = identify(&y, FS).expect("cell");
        assert_eq!(got.sync.expect("sync").pci, 130);
        assert_eq!(got.mib.map(|m| m.bandwidth_rb), Some(50));
    }

    #[test]
    fn decodes_the_mib_on_one_two_and_four_ports() {
        for ports in [4u8, 2, 1] {
            let c = cell(42, ports);
            let x = frames(&c, 4, 23);
            let y = impair(&x, 500, -900.0, 8.0, 29);
            let got = identify(&y, FS).expect("cell");
            let s = got.sync.expect("sync");
            assert_eq!(s.pci, 42);
            let m = got.mib.expect("mib");
            assert_eq!(m.bandwidth_rb, 50);
            assert_eq!(m.phich_resource, 1);
            assert!(!m.phich_extended);
            assert_eq!(m.antenna_ports, ports);
            // The data starts at TTI phase 0, 500 samples in: the first
            // whole frame is the first one generated.
            assert_eq!(m.sfn, 0x2A4);
            assert_eq!(got.label(), format!("LTE 10 MHz PCI 42 ({ports} port{})", if ports == 1 { "" } else { "s" }));
        }
    }

    #[test]
    fn single_frame_and_resampled_input_still_decode() {
        let c = cell(99, 2);
        let x = frames(&c, 4, 31);
        // 12 ms starting mid-TTI: one whole frame, phase 2.
        let y = impair(&x[2 * FRAME - 300..3 * FRAME + 400], 0, 400.0, 12.0, 37);
        let got = identify(&y, FS).expect("cell");
        let m = got.mib.expect("mib");
        assert_eq!(m.sfn, 0x2A4 + 2);
        // The same signal captured at 2.0 MS/s (what a 10 MS/s DDC gives).
        let y2 = impair(&x, 0, 400.0, 12.0, 41);
        let up = resample_to(&y2, FS, 2.0e6);
        let got = identify(&up, 2.0e6).expect("cell");
        assert_eq!(got.sync.expect("sync").pci, 99);
        assert_eq!(got.mib.expect("mib").bandwidth_rb, 50);
    }

    /// Test helper: arbitrary-ratio resampler (same kernel as `resample`).
    fn resample_to(x: &[Complex32], fs_in: f64, fs_out: f64) -> Vec<Complex32> {
        let ratio = fs_in / fs_out;
        let r = (fs_out / fs_in).min(1.0);
        let n_out = ((x.len() as f64 - 1.0) / ratio).floor() as usize;
        (0..n_out)
            .map(|m| {
                let t = m as f64 * ratio;
                let c = t.floor() as isize;
                let mut acc = Complex32::default();
                for n in c - 7..=c + 8 {
                    if n < 0 || n as usize >= x.len() {
                        continue;
                    }
                    let d = (t - n as f64) * r;
                    let s = if d.abs() < 1e-9 { 1.0 } else { (PI * d).sin() / (PI * d) };
                    let w = 0.5 + 0.5 * (PI * (t - n as f64) / 8.0).cos();
                    acc += x[n as usize] * ((s * r * w) as f32);
                }
                acc
            })
            .collect()
    }
}
