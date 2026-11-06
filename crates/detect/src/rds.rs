//! RDS / RBDS decoding from an FM broadcast multiplex (IEC 62106 / NRSC-4).
//!
//! The data rides on a 57 kHz suppressed subcarrier, locked to the third
//! harmonic of the 19 kHz stereo pilot, as biphase-coded, differentially
//! encoded BPSK at 1187.5 bit/s. Groups are four 26-bit blocks: 16 data bits
//! plus a 10-bit check word that also carries the block's position in the
//! group (offset words A, B, C/C', D).
//!
//! Pipeline: a PLL on the band-passed pilot gives the carrier phase; the
//! multiplex is mixed with three times that phase, low-passed to ±2.4 kHz and
//! resampled to eight samples per bit. Half a second at a time, the BPSK
//! phase is taken from the second moment of the baseband, the biphase pulse
//! is matched-filtered, bit timing is the sample phase with the most energy,
//! and the bits go through differential decoding into the block synchroniser
//! (syndrome matching, burst correction up to five bits, lock lost after a
//! run of bad blocks). Groups 0A/0B give the programme service name, 2A/2B
//! the radio text, 0A the alternative frequencies and 4A the clock.
//!
//! `Discriminator` turns channel IQ into the multiplex for the decoder.

use std::collections::HashMap;
use std::sync::OnceLock;

use num_complex::Complex32;

pub const BIT_RATE: f64 = 1187.5;
/// Baseband samples per bit after resampling.
const SPS: usize = 8;
const BB_RATE: f64 = BIT_RATE * SPS as f64;
const PILOT_HZ: f64 = 19_000.0;
/// Bits decided per block (a multiple of SPS keeps the timing phase global).
const BLOCK_BITS: usize = 594;
/// Generator polynomial x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1.
const GEN: u32 = 0x5B9;
/// Offset words A, B, C, C', D.
const OFFSETS: [u32; 5] = [0x0FC, 0x198, 0x168, 0x350, 0x1B4];
/// Bad blocks in a row before sync is dropped.
const MAX_BAD_RUN: u32 = 24;

/// FM discriminator: channel IQ in, multiplex in Hz of deviation out.
pub struct Discriminator {
    prev: Complex32,
    k: f32,
}

impl Discriminator {
    pub fn new(fs: f64) -> Self {
        Self { prev: Complex32::new(1.0, 0.0), k: (fs / (2.0 * std::f64::consts::PI)) as f32 }
    }

    pub fn process(&mut self, x: &[Complex32], out: &mut Vec<f32>) {
        out.reserve(x.len());
        for &s in x {
            out.push((s * self.prev.conj()).arg() * self.k);
            self.prev = s;
        }
    }
}

/// RBJ biquad (direct form II transposed).
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn from_coeffs(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b: [(b0 / a0) as f32, (b1 / a0) as f32, (b2 / a0) as f32], a: [(a1 / a0) as f32, (a2 / a0) as f32], z: [0.0; 2] }
    }

    fn lowpass(cutoff: f64, fs: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * cutoff / fs;
        let alpha = w0.sin() * std::f64::consts::FRAC_1_SQRT_2;
        let c = w0.cos();
        Self::from_coeffs((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    fn bandpass(center: f64, fs: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * center / fs;
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        Self::from_coeffs(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    fn push(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// What has been decoded so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RdsInfo {
    /// Programme identification, once the same code has been seen twice.
    pub pi: Option<u16>,
    /// Programme service name (8 characters), once every segment has arrived.
    pub ps: Option<String>,
    /// Radio text, trimmed; partial until every segment has arrived.
    pub rt: Option<String>,
    pub rt_complete: bool,
    /// Programme type code (RBDS names via `pty_name`).
    pub pty: u8,
    pub tp: bool,
    pub ta: bool,
    /// Alternative frequencies, MHz, ascending.
    pub af_mhz: Vec<f64>,
    /// Clock time from group 4A, "YYYY-MM-DD HH:MM UTC±H".
    pub clock: Option<String>,
    /// Groups whose A and B blocks arrived (either corrected or clean).
    pub groups_ok: u32,
    /// Groups with A or B missing.
    pub groups_bad: u32,
    pub blocks_corrected: u32,
    pub synced: bool,
}

impl RdsInfo {
    /// US call sign for a North American PI code (NRSC-4), if it encodes one.
    pub fn call_sign(&self) -> Option<String> {
        self.pi.and_then(call_sign_from_pi)
    }

    /// RBDS programme type name.
    pub fn pty_name(&self) -> &'static str {
        pty_name(self.pty)
    }

    /// Short identity: the call sign, else the PI in hex.
    pub fn station(&self) -> Option<String> {
        self.call_sign().or_else(|| self.pi.map(|p| format!("PI {p:04X}")))
    }
}

const PTY_RBDS: [&str; 32] = [
    "",
    "News",
    "Information",
    "Sports",
    "Talk",
    "Rock",
    "Classic Rock",
    "Adult Hits",
    "Soft Rock",
    "Top 40",
    "Country",
    "Oldies",
    "Soft",
    "Nostalgia",
    "Jazz",
    "Classical",
    "Rhythm and Blues",
    "Soft R&B",
    "Foreign Language",
    "Religious Music",
    "Religious Talk",
    "Personality",
    "Public",
    "College",
    "Spanish Talk",
    "Spanish Music",
    "Hip Hop",
    "",
    "",
    "Weather",
    "Emergency Test",
    "Emergency",
];

pub fn pty_name(pty: u8) -> &'static str {
    PTY_RBDS[(pty & 31) as usize]
}

/// Three-letter call signs, in PI order from 0x9950 (NRSC-4-B table D.7).
const THREE_LETTER: [&str; 70] = [
    "KBW", "KCY", "KDB", "KDF", "KEX", "KFH", "KFI", "KGA", "KGO", "KGU", "KGW", "KGY", "KHQ", "KID", "KIT", "KJR", "KLO", "KLZ", "KMA",
    "KMJ", "KNX", "KOA", "KOB", "KOY", "KPQ", "KQV", "KSD", "KSL", "KUJ", "KUT", "KVI", "KWG", "KXL", "KXO", "KYW", "WBT", "WBZ", "WDZ",
    "WEW", "WGH", "WGL", "WGN", "WGR", "WGY", "WHA", "WHB", "WHK", "WHO", "WHP", "WIL", "WIP", "WIS", "WJR", "WJW", "WJZ", "WKY", "WLS",
    "WLW", "WMC", "WMT", "WOC", "WOI", "WOL", "WOR", "WOW", "WRR", "WSB", "WSM", "WWJ", "WWL",
];

/// NRSC-4 PI → call sign. Four-letter K/W calls are a base-26 count from
/// 0x1000 (K) and 0x54A8 (W); codes that would end in 00 are sent as AFxx,
/// and ones with a zero third nibble as A<n1><n3><n4>; 0x9950–0x9995 are the
/// three-letter calls. Anything else (nationally linked, Canada, Mexico)
/// has no call sign.
pub fn call_sign_from_pi(pi: u16) -> Option<String> {
    let mut pi = pi;
    if pi & 0xF000 == 0xA000 {
        pi = if pi & 0x0F00 == 0x0F00 { (pi & 0x00FF) << 8 } else { ((pi & 0x0F00) << 4) | (pi & 0x00FF) };
    }
    if (0x9950..0x9950 + THREE_LETTER.len() as u16).contains(&pi) {
        return Some(THREE_LETTER[(pi - 0x9950) as usize].to_string());
    }
    let (first, n) = if (0x1000..0x54A8).contains(&pi) {
        ('K', (pi - 0x1000) as u32)
    } else if (0x54A8..0x9950).contains(&pi) {
        ('W', (pi - 0x54A8) as u32)
    } else {
        return None;
    };
    if n >= 26 * 26 * 26 {
        return None;
    }
    let l = |k: u32| (b'A' + (k % 26) as u8) as char;
    Some(format!("{first}{}{}{}", l(n / 676), l(n / 26), l(n)))
}

/// Call sign → PI (the inverse of `call_sign_from_pi`, for tests and lookups).
pub fn pi_from_call_sign(call: &str) -> Option<u16> {
    let b = call.as_bytes();
    if b.len() == 3 {
        return THREE_LETTER.iter().position(|c| c.as_bytes() == b).map(|i| 0x9950 + i as u16);
    }
    if b.len() != 4 || !b[1..].iter().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let n: u32 = b[1..].iter().fold(0, |acc, &c| acc * 26 + (c - b'A') as u32);
    let pi = match b[0] {
        b'K' => 0x1000 + n,
        b'W' => 0x54A8 + n,
        _ => return None,
    } as u16;
    Some(if pi & 0x00FF == 0 {
        0xAF00 | (pi >> 8)
    } else if pi & 0x0F00 == 0 {
        0xA000 | ((pi & 0xF000) >> 4) | (pi & 0x00FF)
    } else {
        pi
    })
}

/// Remainder of a 26-bit block (MSB first) modulo the generator: equals the
/// offset word when the block is error-free.
fn syndrome(block: u32) -> u32 {
    let mut s = 0u32;
    for i in (0..26).rev() {
        s = (s << 1) | ((block >> i) & 1);
        if s & 0x400 != 0 {
            s ^= GEN;
        }
    }
    s
}

/// Check word for 16 data bits: the remainder of data·x^10, plus the offset.
pub fn check_word(data: u16, offset: u32) -> u32 {
    syndrome((data as u32) << 10) ^ offset
}

/// Error pattern for each syndrome (offset removed) of a burst of up to
/// five bits anywhere in the block; 0 where no such burst exists.
fn burst_table() -> &'static [u32; 1024] {
    static TABLE: OnceLock<[u32; 1024]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 1024];
        for len in 1..=5u32 {
            for inner in 0..(1u32 << len.saturating_sub(2)) {
                let pat = if len == 1 { 1 } else { 1 | (inner << 1) | (1 << (len - 1)) };
                for pos in 0..=(26 - len) {
                    let e = pat << pos;
                    let s = syndrome(e) as usize;
                    if t[s] == 0 {
                        t[s] = e;
                    }
                }
            }
        }
        t
    })
}

/// A group's four blocks, where they decoded.
#[derive(Debug, Clone, Copy, Default)]
struct Group {
    blocks: [Option<u16>; 4],
    /// The third block carried offset C' (a version B group).
    cprime: bool,
}

/// Bit-level block synchroniser.
struct BlockSync {
    reg: u32,
    total: u64,
    locked: bool,
    /// Next block expected (0 A, 1 B, 2 C/C', 3 D).
    expect: usize,
    bits: u32,
    bad_run: u32,
    /// Last offset seen while acquiring: (block index, bit count then).
    pend: Option<(usize, u64)>,
    group: Group,
    corrected: u32,
    /// The previous block needed no correction: only then is a burst
    /// correction trusted (a random block passes correction 40% of the time).
    last_clean: bool,
}

impl BlockSync {
    fn new() -> Self {
        Self {
            reg: 0,
            total: 0,
            locked: false,
            expect: 0,
            bits: 0,
            bad_run: 0,
            pend: None,
            group: Group::default(),
            corrected: 0,
            last_clean: false,
        }
    }

    fn block_index(offset_idx: usize) -> usize {
        match offset_idx {
            0 => 0,
            1 => 1,
            2 | 3 => 2,
            _ => 3,
        }
    }

    /// Feeds one decoded bit; returns a group when its D block completes.
    fn push(&mut self, bit: bool) -> Option<Group> {
        self.reg = ((self.reg << 1) | bit as u32) & 0x3FF_FFFF;
        self.total += 1;
        if !self.locked {
            let s = syndrome(self.reg);
            if let Some(i) = OFFSETS.iter().position(|&o| o == s) {
                let blk = Self::block_index(i);
                if let Some((pb, pt)) = self.pend
                    && self.total - pt == 26
                    && blk == (pb + 1) % 4
                {
                    self.locked = true;
                    self.bits = 0;
                    self.bad_run = 0;
                    self.group = Group::default();
                    self.group.blocks[blk] = Some((self.reg >> 10) as u16);
                    self.group.cprime = i == 3;
                    self.expect = (blk + 1) % 4;
                    self.pend = None;
                    self.last_clean = true;
                    if blk == 3 {
                        // Locked on a D block: that group is incomplete.
                        self.group = Group::default();
                    }
                    return None;
                }
                self.pend = Some((blk, self.total));
            }
            return None;
        }
        self.bits += 1;
        if self.bits < 26 {
            return None;
        }
        self.bits = 0;
        let want: &[usize] = match self.expect {
            0 => &[0],
            1 => &[1],
            2 => &[2, 3],
            _ => &[4],
        };
        let s = syndrome(self.reg);
        let mut got: Option<(u16, usize)> = None;
        if let Some(&i) = want.iter().find(|&&i| OFFSETS[i] == s) {
            got = Some(((self.reg >> 10) as u16, i));
            self.last_clean = true;
        } else if self.last_clean {
            self.last_clean = false;
            for &i in want {
                let e = burst_table()[((s ^ OFFSETS[i]) & 0x3FF) as usize];
                if e != 0 {
                    got = Some((((self.reg ^ e) >> 10) as u16, i));
                    self.corrected += 1;
                    break;
                }
            }
        }
        match got {
            Some((data, i)) => {
                self.bad_run = 0;
                self.group.blocks[self.expect] = Some(data);
                if self.expect == 2 {
                    self.group.cprime = i == 3;
                }
            }
            None => {
                self.bad_run += 1;
                self.group.blocks[self.expect] = None;
                if self.bad_run >= MAX_BAD_RUN {
                    self.locked = false;
                    self.pend = None;
                    self.group = Group::default();
                    return None;
                }
            }
        }
        self.expect = (self.expect + 1) % 4;
        if self.expect == 0 {
            let g = self.group;
            self.group = Group::default();
            return Some(g);
        }
        None
    }
}

/// Streaming RDS decoder over the multiplex.
pub struct RdsDecoder {
    fs: f64,
    pilot_bp: Biquad,
    /// Pilot PLL: phase, nominal step, integrator.
    theta: f64,
    step: f64,
    integ: f64,
    /// Pilot amplitude estimate (in-phase product, low-passed), for a
    /// level-independent loop.
    amp: f32,
    lp: [[Biquad; 2]; 2],
    /// Resampler to `BB_RATE`: position of the next output in input samples.
    rpos: f64,
    last: Complex32,
    /// Baseband at `BB_RATE`, the last SPS samples of the previous block first.
    bb: Vec<Complex32>,
    rot: Complex32,
    phase: Option<usize>,
    prev_e: bool,
    sync: BlockSync,
    pi_votes: HashMap<u16, u32>,
    ps: [Option<[u8; 2]>; 4],
    /// The last complete PS assembly; a name is published once it is seen
    /// twice in a row, so a wrongly corrected segment can't put a bad
    /// character into a name (on air: "Star1 02" for "Star 102").
    ps_last: Option<String>,
    rt: [Option<u8>; 64],
    rt_ab: Option<bool>,
    rt_len: usize,
    info: RdsInfo,
}

impl RdsDecoder {
    /// `fs`: rate of the multiplex samples given to `push` (≥ 130 kHz so the
    /// 57 kHz subcarrier and its sidebands are inside Nyquist).
    pub fn new(fs: f64) -> Self {
        let lp = Biquad::lowpass(2_400.0, fs);
        Self {
            fs,
            pilot_bp: Biquad::bandpass(PILOT_HZ, fs, 25.0),
            theta: 0.0,
            step: 2.0 * std::f64::consts::PI * PILOT_HZ / fs,
            integ: 0.0,
            amp: 0.0,
            lp: [[lp; 2]; 2],
            rpos: 0.0,
            last: Complex32::default(),
            bb: vec![Complex32::default(); SPS],
            rot: Complex32::new(1.0, 0.0),
            phase: None,
            prev_e: false,
            sync: BlockSync::new(),
            pi_votes: HashMap::new(),
            ps: [None; 4],
            ps_last: None,
            rt: [None; 64],
            rt_ab: None,
            rt_len: 64,
            info: RdsInfo::default(),
        }
    }

    pub fn info(&self) -> &RdsInfo {
        &self.info
    }

    /// Feeds multiplex samples (any scale; the pilot sets the level).
    pub fn push(&mut self, mpx: &[f32]) {
        // Second-order loop, ~20 Hz bandwidth, critically damped.
        let wn = 2.0 * std::f64::consts::PI * 20.0 / self.fs;
        let (kp, ki) = (1.414 * wn, wn * wn);
        let rstep = self.fs / BB_RATE;
        for &m in mpx {
            let p = self.pilot_bp.push(m);
            let (s, c) = self.theta.sin_cos();
            // In-phase product tracks the pilot's amplitude; the quadrature
            // product, divided by it, is the phase error in radians (small).
            self.amp += 0.002 * (p * c as f32 * 2.0 - self.amp);
            // (Either sign of `amp` is a stable lock; the two differ by π,
            // which the BPSK phase estimate absorbs.)
            let err = if self.amp.abs() > 1e-6 { (-(p * s as f32) * 2.0 / self.amp).clamp(-1.0, 1.0) as f64 } else { 0.0 };
            self.integ += ki * err;
            self.theta += self.step + kp * err + self.integ;
            if self.theta > 2.0 * std::f64::consts::PI {
                self.theta -= 2.0 * std::f64::consts::PI;
            }
            // Mix with the third harmonic and low-pass to the data band.
            let (s3, c3) = (3.0 * self.theta).sin_cos();
            let mut i = m * c3 as f32;
            let mut q = -m * s3 as f32;
            for f in &mut self.lp[0] {
                i = f.push(i);
            }
            for f in &mut self.lp[1] {
                q = f.push(q);
            }
            let z = Complex32::new(i, q);
            while self.rpos <= 1.0 {
                self.bb.push(self.last + (z - self.last) * self.rpos as f32);
                self.rpos += rstep;
            }
            self.rpos -= 1.0;
            self.last = z;
        }
        while self.bb.len() >= SPS + BLOCK_BITS * SPS {
            self.decode_block();
        }
    }

    /// Decides the bits of one block of baseband and feeds the synchroniser.
    fn decode_block(&mut self) {
        let n = BLOCK_BITS * SPS;
        let data: Vec<Complex32> = self.bb[..SPS + n].to_vec();
        // BPSK phase from the second moment (ambiguous by π: keep the
        // rotation nearer the previous one; differential coding absorbs
        // the rest).
        let p: Complex32 = data[SPS..].iter().map(|z| z * z).sum();
        let ang = -p.arg() / 2.0;
        let mut rot = Complex32::new(ang.cos(), ang.sin());
        if (rot - self.rot).norm_sqr() > (rot + self.rot).norm_sqr() {
            rot = -rot;
        }
        self.rot = rot;
        let x: Vec<f32> = data.iter().map(|z| (z * rot).re).collect();
        // Matched filter for the biphase pulse (+ over the first half of a
        // bit, − over the second), output aligned to the end of the bit.
        let y: Vec<f32> = (SPS..SPS + n)
            .map(|k| {
                let mut acc = 0.0;
                for j in 0..SPS {
                    let v = x[k - j];
                    acc += if j < SPS / 2 { v } else { -v };
                }
                acc
            })
            .collect();
        let mut energy = [0.0f32; SPS];
        for (k, v) in y.iter().enumerate() {
            energy[k % SPS] += v.abs();
        }
        let best = (0..SPS).max_by(|&a, &b| energy[a].total_cmp(&energy[b])).unwrap_or(0);
        let phase = match self.phase {
            // Locked: let the sampling point drift a little per block.
            Some(prev) if self.sync.locked => {
                let d = ((best + SPS + SPS / 2 - prev) % SPS) as isize - SPS as isize / 2;
                ((prev as isize + d.clamp(-1, 1)).rem_euclid(SPS as isize)) as usize
            }
            _ => best,
        };
        self.phase = Some(phase);
        let mut groups = Vec::new();
        let mut k = phase;
        while k < n {
            let e = y[k] > 0.0;
            let d = e ^ self.prev_e;
            self.prev_e = e;
            if let Some(g) = self.sync.push(d) {
                groups.push(g);
            }
            k += SPS;
        }
        self.bb.drain(..n);
        for g in groups {
            self.group(g);
        }
        self.info.synced = self.sync.locked;
        self.info.blocks_corrected = self.sync.corrected;
    }

    fn group(&mut self, g: Group) {
        let (Some(pi), Some(b)) = (g.blocks[0], g.blocks[1]) else {
            self.info.groups_bad += 1;
            return;
        };
        let votes = self.pi_votes.entry(pi).or_default();
        *votes += 1;
        let votes = *votes;
        let best = self.pi_votes.iter().max_by_key(|(_, n)| **n).map(|(p, _)| *p);
        if votes >= 2 && best == Some(pi) && self.info.pi != Some(pi) {
            // A new (or first) station: start the texts over.
            self.info.pi = Some(pi);
            self.ps = [None; 4];
            self.rt = [None; 64];
            self.rt_ab = None;
            self.info.ps = None;
            self.info.rt = None;
            self.info.af_mhz.clear();
        }
        if self.info.pi != Some(pi) {
            self.info.groups_bad += 1;
            return;
        }
        self.info.groups_ok += 1;
        let gtype = (b >> 12) as u8;
        let version_b = b & 0x800 != 0 || g.cprime;
        self.info.tp = b & 0x400 != 0;
        self.info.pty = ((b >> 5) & 0x1F) as u8;
        match (gtype, version_b) {
            (0, _) => {
                self.info.ta = b & 0x10 != 0;
                let seg = (b & 3) as usize;
                if let Some(d) = g.blocks[3] {
                    self.ps[seg] = Some([(d >> 8) as u8, d as u8]);
                    if self.ps.iter().all(|s| s.is_some()) {
                        let s: String = self.ps.iter().flatten().flatten().map(|&c| rds_char(c)).collect();
                        if self.ps_last.as_deref() == Some(s.as_str()) {
                            self.info.ps = Some(s);
                        } else {
                            self.ps_last = Some(s);
                        }
                    }
                }
                if !version_b && let Some(c) = g.blocks[2] {
                    for code in [(c >> 8) as u8, c as u8] {
                        if (1..=204).contains(&code) {
                            let mhz = 87.5 + 0.1 * code as f64;
                            let mhz = (mhz * 10.0).round() / 10.0;
                            if !self.info.af_mhz.iter().any(|f| (*f - mhz).abs() < 0.01) {
                                self.info.af_mhz.push(mhz);
                                self.info.af_mhz.sort_by(|a, b| a.total_cmp(b));
                            }
                        }
                    }
                }
            }
            (2, vb) => {
                let ab = b & 0x10 != 0;
                if self.rt_ab != Some(ab) {
                    self.rt_ab = Some(ab);
                    self.rt = [None; 64];
                    self.rt_len = 64;
                }
                let seg = (b & 0xF) as usize;
                let chars: Vec<(usize, u8)> = if vb {
                    g.blocks[3].map(|d| vec![(seg * 2, (d >> 8) as u8), (seg * 2 + 1, d as u8)]).unwrap_or_default()
                } else {
                    let mut v = Vec::new();
                    if let Some(c) = g.blocks[2] {
                        v.push((seg * 4, (c >> 8) as u8));
                        v.push((seg * 4 + 1, c as u8));
                    }
                    if let Some(d) = g.blocks[3] {
                        v.push((seg * 4 + 2, (d >> 8) as u8));
                        v.push((seg * 4 + 3, d as u8));
                    }
                    v
                };
                for (i, c) in chars {
                    if i < 64 {
                        self.rt[i] = Some(c);
                        if c == 0x0D {
                            self.rt_len = self.rt_len.min(i);
                        }
                    }
                }
                let len = self.rt_len.min(if vb { 32 } else { 64 });
                let complete = self.rt[..len].iter().all(|c| c.is_some());
                let text: String = self.rt[..len].iter().map(|c| c.map_or(' ', rds_char)).collect();
                let text = text.trim_end().to_string();
                self.info.rt_complete = complete;
                if !text.is_empty() {
                    self.info.rt = Some(text);
                }
            }
            (4, false) => {
                if let (Some(c), Some(d)) = (g.blocks[2], g.blocks[3]) {
                    let mjd = (((b & 3) as u32) << 15) | (c as u32 >> 1);
                    let hour = (((c & 1) as u32) << 4) | (d as u32 >> 12);
                    let minute = (d >> 6) & 0x3F;
                    let off = (d & 0x1F) as i32 * if d & 0x20 != 0 { -1 } else { 1 };
                    if mjd > 15_000 && hour < 24 && minute < 60 {
                        let (y, m, day) = mjd_to_ymd(mjd);
                        let (oh, om) = (off / 2, (off % 2).abs() * 30);
                        let sign = if off < 0 { '-' } else { '+' };
                        let tz = if om == 0 { format!("UTC{sign}{}", oh.abs()) } else { format!("UTC{sign}{}:{om:02}", oh.abs()) };
                        self.info.clock = Some(format!("{y:04}-{m:02}-{day:02} {hour:02}:{minute:02} {tz}"));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Modified Julian Day to calendar date.
fn mjd_to_ymd(mjd: u32) -> (u32, u32, u32) {
    let mjd = mjd as f64;
    let yp = ((mjd - 15_078.2) / 365.25).floor();
    let mp = ((mjd - 14_956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let d = mjd - 14_956.0 - (yp * 365.25).floor() - (mp * 30.6001).floor();
    let k = if mp == 14.0 || mp == 15.0 { 1.0 } else { 0.0 };
    ((1900.0 + yp + k) as u32, (mp - 1.0 - k * 12.0) as u32, d as u32)
}

/// RDS character set: ASCII in 0x20–0x7E; the rest shown as '?'.
fn rds_char(c: u8) -> char {
    if (0x20..0x7F).contains(&c) { c as char } else { '?' }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes groups into the differential, biphase-coded bit waveform.
    struct Encoder {
        prev: bool,
    }

    impl Encoder {
        fn block(data: u16, offset: u32) -> u32 {
            ((data as u32) << 10) | check_word(data, offset)
        }

        /// Bits (MSB first) of a group; `cprime` for version B.
        fn group_bits(&mut self, blocks: [u16; 4], cprime: bool) -> Vec<bool> {
            let offs = [OFFSETS[0], OFFSETS[1], if cprime { OFFSETS[3] } else { OFFSETS[2] }, OFFSETS[4]];
            let mut out = Vec::new();
            for (d, o) in blocks.iter().zip(offs) {
                let b = Self::block(*d, o);
                for i in (0..26).rev() {
                    let bit = (b >> i) & 1 == 1;
                    let e = bit ^ self.prev;
                    self.prev = e;
                    out.push(e);
                }
            }
            out
        }
    }

    fn b_block(gtype: u16, version_b: bool, tp: bool, pty: u16, low: u16) -> u16 {
        (gtype << 12) | ((version_b as u16) << 11) | ((tp as u16) << 10) | (pty << 5) | (low & 0x1F)
    }

    fn station_groups(pi: u16, ps: &str, rt: &str) -> Vec<([u16; 4], bool)> {
        let ps = ps.as_bytes();
        let mut rt: Vec<u8> = rt.as_bytes().to_vec();
        rt.push(0x0D);
        rt.resize(64, b' ');
        let mut v = Vec::new();
        for rep in 0..3 {
            for seg in 0..4u16 {
                let af = if seg == 0 { (225u16 << 8) | 0x38 } else { 0xCDCD };
                let d = ((ps[seg as usize * 2] as u16) << 8) | ps[seg as usize * 2 + 1] as u16;
                v.push(([pi, b_block(0, false, true, 5, (1 << 4) * (rep % 2) | seg), af, d], false));
            }
            for seg in 0..16u16 {
                let i = seg as usize * 4;
                let c = ((rt[i] as u16) << 8) | rt[i + 1] as u16;
                let d = ((rt[i + 2] as u16) << 8) | rt[i + 3] as u16;
                v.push(([pi, b_block(2, false, true, 5, seg), c, d], false));
            }
            // A 0B group (PI again in block 3) now and then.
            let d = ((ps[0] as u16) << 8) | ps[1] as u16;
            v.push(([pi, b_block(0, true, true, 5, 0), pi, d], true));
        }
        v
    }

    /// A multiplex at `fs`: pilot, a mono tone, a 38 kHz difference signal,
    /// the RDS subcarrier at phase `phi` from the pilot's third harmonic,
    /// and white noise.
    fn mpx(bits: &[bool], fs: f64, phi: f64, noise: f32, seed: &mut u32) -> Vec<f32> {
        let n = (bits.len() as f64 / BIT_RATE * fs) as usize;
        let mut lp = [Biquad::lowpass(2_400.0, fs); 2];
        let mut rnd = move || {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        (0..n)
            .map(|k| {
                let t = k as f64 / fs;
                let bit_pos = t * BIT_RATE;
                let i = (bit_pos as usize).min(bits.len() - 1);
                let half = if bit_pos.fract() < 0.5 { 1.0 } else { -1.0 };
                let mut sym = if bits[i] { half } else { -half } as f32;
                for f in &mut lp {
                    sym = f.push(sym);
                }
                let pilot = 0.09 * (2.0 * std::f64::consts::PI * PILOT_HZ * t).cos();
                let rds = 0.03 * sym as f64 * (2.0 * std::f64::consts::PI * 3.0 * PILOT_HZ * t + phi).cos();
                let audio = 0.4 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin();
                let diff = 0.2 * (2.0 * std::f64::consts::PI * 2_500.0 * t).sin() * (2.0 * std::f64::consts::PI * 2.0 * PILOT_HZ * t).cos();
                ((pilot + rds + audio + diff) as f32 + noise * rnd()) * 75_000.0
            })
            .collect()
    }

    fn decode(m: &[f32], fs: f64) -> RdsInfo {
        let mut dec = RdsDecoder::new(fs);
        for chunk in m.chunks((fs * 0.01) as usize) {
            dec.push(chunk);
        }
        dec.info().clone()
    }

    #[test]
    fn check_words_are_offsets_when_clean_and_bursts_correct() {
        for (i, &o) in OFFSETS.iter().enumerate() {
            let b = Encoder::block(0xBEEF, o);
            assert_eq!(syndrome(b), o, "offset {i}");
            // A 3-bit burst in the middle: corrected back.
            let e = 0b101 << 11;
            let s = syndrome(b ^ e) ^ o;
            assert_eq!(burst_table()[s as usize], e);
        }
    }

    #[test]
    fn decodes_pi_ps_and_rt_from_a_synthetic_multiplex() {
        for (fs, phi) in [(240_000.0, 0.0), (250_000.0, std::f64::consts::FRAC_PI_2)] {
            let mut enc = Encoder { prev: false };
            let bits: Vec<bool> =
                station_groups(0x4A5B, "TEST  FM", "HELLO FROM SPECTRAWL").iter().flat_map(|(b, c)| enc.group_bits(*b, *c)).collect();
            let mut seed = 7;
            let m = mpx(&bits, fs, phi, 0.02, &mut seed);
            let info = decode(&m, fs);
            assert_eq!(info.pi, Some(0x4A5B), "fs {fs}");
            assert_eq!(info.ps.as_deref(), Some("TEST  FM"), "fs {fs}");
            assert_eq!(info.rt.as_deref(), Some("HELLO FROM SPECTRAWL"), "fs {fs}");
            assert!(info.rt_complete);
            assert!(info.tp);
            assert_eq!(info.pty_name(), "Rock");
            assert_eq!(info.af_mhz, vec![93.1]);
            assert!(info.groups_ok >= 40, "{} groups ok, {} bad", info.groups_ok, info.groups_bad);
            assert!(info.groups_bad <= 2, "{} groups bad", info.groups_bad);
        }
    }

    #[test]
    fn tolerates_noise() {
        // Noise at 0.12 of full deviation across 120 kHz is roughly 6 dB SNR
        // in the 4.8 kHz RDS band; the station should still be named.
        let fs = 240_000.0;
        let mut enc = Encoder { prev: false };
        let bits: Vec<bool> =
            station_groups(0x4A5B, "TEST  FM", "HELLO FROM SPECTRAWL").iter().flat_map(|(b, c)| enc.group_bits(*b, *c)).collect();
        let mut seed = 5;
        let m = mpx(&bits, fs, 1.0, 0.12, &mut seed);
        let info = decode(&m, fs);
        assert_eq!(info.pi, Some(0x4A5B), "{info:?}");
        assert_eq!(info.ps.as_deref(), Some("TEST  FM"), "{info:?}");
        eprintln!("noise 0.12: {} ok, {} bad, {} corrected", info.groups_ok, info.groups_bad, info.blocks_corrected);
    }

    #[test]
    fn decodes_through_the_discriminator() {
        let fs = 240_000.0;
        let mut enc = Encoder { prev: false };
        let bits: Vec<bool> =
            station_groups(0x5F0A, "KXYZ 101", "A SONG - AN ARTIST").iter().flat_map(|(b, c)| enc.group_bits(*b, *c)).collect();
        let mut seed = 11;
        let m = mpx(&bits, fs, 0.3, 0.0, &mut seed);
        // FM-modulate the multiplex (in Hz) and demodulate it again.
        let mut ph = 0.0f64;
        let iq: Vec<Complex32> = m
            .iter()
            .map(|&f| {
                ph += 2.0 * std::f64::consts::PI * f as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut disc = Discriminator::new(fs);
        let mut out = Vec::new();
        disc.process(&iq, &mut out);
        let info = decode(&out, fs);
        assert_eq!(info.pi, Some(0x5F0A));
        assert_eq!(info.ps.as_deref(), Some("KXYZ 101"));
        assert_eq!(info.rt.as_deref(), Some("A SONG - AN ARTIST"));
    }

    #[test]
    fn noise_alone_decodes_nothing() {
        let fs = 240_000.0;
        let mut seed = 3u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let m: Vec<f32> = (0..(fs * 6.0) as usize).map(|_| rnd() * 50_000.0).collect();
        let info = decode(&m, fs);
        assert_eq!(info.pi, None);
        assert_eq!(info.groups_ok, 0);
    }

    #[test]
    fn call_signs_round_trip() {
        for call in ["KAAA", "KXYZ", "KZZZ", "WAAA", "WXYZ", "WZZZ", "KBW", "WWL", "WAAZ"] {
            let pi = pi_from_call_sign(call).unwrap();
            assert_eq!(call_sign_from_pi(pi).as_deref(), Some(call), "{call} → {pi:04X}");
        }
        assert_eq!(call_sign_from_pi(0x1000).as_deref(), Some("KAAA"));
        assert_eq!(pi_from_call_sign("KAAA"), Some(0xAF10));
        assert_eq!(pi_from_call_sign("WAAA"), Some(0x54A8));
        assert_eq!(call_sign_from_pi(0x9950).as_deref(), Some("KBW"));
        // Codes whose plain form would end in 00 or have a zero third
        // nibble go out as AFxx / A<n1><n3><n4>.
        let zero_tail = (0x1000u16..0x54A8).find(|p| p & 0xFF == 0).unwrap();
        let call = call_sign_from_pi(zero_tail).unwrap();
        let sent = pi_from_call_sign(&call).unwrap();
        assert_eq!(sent & 0xFF00, 0xAF00);
        assert_eq!(call_sign_from_pi(sent).as_deref(), Some(call.as_str()));
        let zero_mid = (0x1100u16..0x54A8).find(|p| p & 0x0F00 == 0 && p & 0xFF != 0).unwrap();
        let call = call_sign_from_pi(zero_mid).unwrap();
        let sent = pi_from_call_sign(&call).unwrap();
        assert_eq!(sent & 0xF000, 0xA000);
        assert_eq!(call_sign_from_pi(sent).as_deref(), Some(call.as_str()));
        assert_eq!(call_sign_from_pi(0xB201), None);
    }

    #[test]
    fn mjd_converts() {
        assert_eq!(mjd_to_ymd(51_544), (2000, 1, 1));
        assert_eq!(mjd_to_ymd(60_000), (2023, 2, 25));
    }
}
