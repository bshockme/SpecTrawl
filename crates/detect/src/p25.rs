//! P25 Phase 1 trunking signalling blocks (TSBKs) from TSDU frames.
//!
//! After the frame sync and NID, a TSDU carries up to three TSBKs. Each is
//! 98 dibits on air: interleaved, ½-rate trellis coded, carrying 12 bytes
//! (opcode, manufacturer ID, 8 argument bytes, CRC-CCITT). Status symbols
//! sit after every 35 dibits of the frame, counted from the start of sync.
//!
//! The coding details (interleave direction, trellis table orientation, CRC
//! init/final XOR) are parameters so they can be confirmed against real
//! captures: only the right combination makes the CRCs check.

use crate::frames::Frame;

/// Dibit positions of the 98-dibit interleave (4 groups of 26/24).
const INTERLEAVE: [usize; 98] = [
    0, 1, 8, 9, 16, 17, 24, 25, 32, 33, 40, 41, 48, 49, 56, 57, 64, 65, 72, 73, 80, 81, 88, 89, 96, 97, //
    2, 3, 10, 11, 18, 19, 26, 27, 34, 35, 42, 43, 50, 51, 58, 59, 66, 67, 74, 75, 82, 83, 90, 91, //
    4, 5, 12, 13, 20, 21, 28, 29, 36, 37, 44, 45, 52, 53, 60, 61, 68, 69, 76, 77, 84, 85, 92, 93, //
    6, 7, 14, 15, 22, 23, 30, 31, 38, 39, 46, 47, 54, 55, 62, 63, 70, 71, 78, 79, 86, 87, 94, 95,
];

/// ½-rate trellis: output nibble for (state = previous input dibit, input).
const TRELLIS: [[u8; 4]; 4] = [[0x2, 0xC, 0x1, 0xF], [0xE, 0x0, 0xD, 0x3], [0x9, 0x7, 0xA, 0x4], [0x5, 0xB, 0x6, 0x8]];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coding {
    /// true: received dibit `i` goes to position `INTERLEAVE[i]`;
    /// false: position `i` comes from received dibit `INTERLEAVE[i]`.
    pub scatter: bool,
    /// Index the trellis table as [input][state] instead of [state][input].
    pub transpose: bool,
    pub crc_init: u16,
    pub crc_xorout: u16,
}

/// The combination confirmed on real captures (see `tests/real_captures.rs`).
pub const CODING: Coding = Coding { scatter: true, transpose: false, crc_init: 0x0000, crc_xorout: 0xFFFF };

/// `n` data dibits after the NID, status symbols removed (they sit after
/// every 35 dibits, counted from the first sync dibit). None if `f.dibits`
/// doesn't reach that far.
pub fn data_dibits(f: &Frame, n: usize) -> Option<Vec<u8>> {
    data_symbols(&f.dibits, f.sync_start, n)
}

/// The soft values of the same symbols as `data_dibits`.
pub fn data_soft(f: &Frame, n: usize) -> Option<Vec<f32>> {
    data_symbols(&f.soft, f.sync_start, n)
}

fn data_symbols<T: Copy>(v: &[T], sync_start: usize, n: usize) -> Option<Vec<T>> {
    let mut out = Vec::with_capacity(n);
    let mut idx = 24; // frame dibit index (0 = first sync dibit)
    let mut nid = 0;
    while out.len() < n {
        if (idx + 1) % 36 != 0 {
            if nid < 32 {
                nid += 1;
            } else {
                out.push(*v.get(sync_start + idx)?);
            }
        }
        idx += 1;
    }
    Some(out)
}

/// The (up to three) 98-dibit TSBK blocks of a TSDU frame, status symbols
/// removed. `f.dibits` must extend far enough past the sync.
pub fn tsbk_blocks(f: &Frame) -> Vec<[u8; 98]> {
    blocks_of(&f.dibits, f.sync_start)
}

/// The same blocks' soft symbol values (outer levels ±1, inner ±1/3).
pub fn tsbk_soft_blocks(f: &Frame) -> Vec<[f32; 98]> {
    blocks_of(&f.soft, f.sync_start)
}

fn blocks_of<T: Copy + Default>(v: &[T], base: usize) -> Vec<[T; 98]> {
    let mut out = Vec::new();
    let mut idx = 24; // frame dibit index (0 = first sync dibit)
    // Skip the NID: 32 data dibits plus the status symbol inside it.
    let mut taken = 0;
    while taken < 32 {
        if (idx + 1) % 36 != 0 {
            taken += 1;
        }
        idx += 1;
    }
    for _ in 0..3 {
        let mut b = [T::default(); 98];
        let mut n = 0;
        while n < 98 {
            if (idx + 1) % 36 != 0 {
                match v.get(base + idx) {
                    Some(&d) => b[n] = d,
                    None => return out,
                }
                n += 1;
            }
            idx += 1;
        }
        out.push(b);
    }
    out
}

fn deinterleave<T: Copy + Default>(rx: &[T; 98], scatter: bool) -> [T; 98] {
    let mut out = [T::default(); 98];
    for i in 0..98 {
        if scatter {
            out[INTERLEAVE[i]] = rx[i];
        } else {
            out[i] = rx[INTERLEAVE[i]];
        }
    }
    out
}

fn trellis(state: usize, input: usize, transpose: bool) -> u8 {
    if transpose { TRELLIS[input][state] } else { TRELLIS[state][input] }
}

/// Viterbi decode of 49 received nibbles (98 dibits) into 48 dibits.
/// Returns the 12 bytes and the path metric (bit errors along the best path).
fn viterbi(d: &[u8; 98], transpose: bool) -> ([u8; 12], u32) {
    let nib: Vec<u8> = d.chunks(2).map(|p| p[0] << 2 | p[1]).collect();
    const INF: u32 = u32::MAX / 2;
    let mut metric = [0, INF, INF, INF]; // encoder starts in state 0
    let mut back: Vec<[(u8, u8); 4]> = Vec::with_capacity(49); // (prev state, input) per new state
    for &rx in &nib {
        let mut next = [INF; 4];
        let mut from = [(0u8, 0u8); 4];
        for s in 0..4 {
            if metric[s] >= INF {
                continue;
            }
            for i in 0..4 {
                let m = metric[s] + (trellis(s, i, transpose) ^ rx).count_ones();
                // The new state is the input just sent.
                if m < next[i] {
                    next[i] = m;
                    from[i] = (s as u8, i as u8);
                }
            }
        }
        metric = next;
        back.push(from);
    }
    // The last input is a flush (0), so the path ends in state 0.
    let mut s = 0usize;
    let best = metric[0];
    let mut inputs = [0u8; 49];
    for t in (0..49).rev() {
        let (p, i) = back[t][s];
        inputs[t] = i;
        s = p as usize;
    }
    let mut bytes = [0u8; 12];
    for (k, &dib) in inputs[..48].iter().enumerate() {
        bytes[k / 4] |= dib << (6 - 2 * (k % 4));
    }
    (bytes, best)
}

/// The soft value a dibit is sent at: 01 = +3, 00 = +1, 10 = −1, 11 = −3, scaled to ±1.
const LEVEL: [f32; 4] = [1.0 / 3.0, 1.0, -1.0 / 3.0, -1.0];

/// [`viterbi`] on soft symbol values: each branch costs the squared distance of its two symbols from the levels its
/// nibble is sent at, so a symbol near a threshold counts for little either way. Returns the bytes and the hard bit
/// errors along the chosen path.
fn viterbi_soft(v: &[f32; 98], transpose: bool) -> ([u8; 12], u32) {
    const INF: f32 = f32::MAX / 4.0;
    let cost = |nib: u8, a: f32, b: f32| {
        (a.clamp(-1.5, 1.5) - LEVEL[(nib >> 2) as usize]).powi(2) + (b.clamp(-1.5, 1.5) - LEVEL[(nib & 3) as usize]).powi(2)
    };
    let mut metric = [0.0, INF, INF, INF];
    let mut back: Vec<[(u8, u8); 4]> = Vec::with_capacity(49);
    for p in v.chunks(2) {
        let mut next = [INF; 4];
        let mut from = [(0u8, 0u8); 4];
        for s in 0..4 {
            if metric[s] >= INF {
                continue;
            }
            for i in 0..4 {
                let m = metric[s] + cost(trellis(s, i, transpose), p[0], p[1]);
                if m < next[i] {
                    next[i] = m;
                    from[i] = (s as u8, i as u8);
                }
            }
        }
        metric = next;
        back.push(from);
    }
    let mut s = 0usize;
    let mut inputs = [0u8; 49];
    for t in (0..49).rev() {
        let (p, i) = back[t][s];
        inputs[t] = i;
        s = p as usize;
    }
    // Hard errors along the path, for the same meaning of `trellis_errors`.
    let mut errors = 0;
    let mut state = 0usize;
    for (t, &i) in inputs.iter().enumerate() {
        let sent = trellis(state, i as usize, transpose);
        let hard = |x: f32| LEVEL.iter().enumerate().min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs())).unwrap().0 as u8;
        errors += (sent ^ (hard(v[2 * t]) << 2 | hard(v[2 * t + 1]))).count_ones();
        state = i as usize;
    }
    let mut bytes = [0u8; 12];
    for (k, &dib) in inputs[..48].iter().enumerate() {
        bytes[k / 4] |= dib << (6 - 2 * (k % 4));
    }
    (bytes, errors)
}

/// CRC-CCITT (poly 0x1021) over the first 10 bytes, MSB first.
fn crc_ccitt(bytes: &[u8], init: u16, xorout: u16) -> u16 {
    let mut crc = init;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { crc << 1 ^ 0x1021 } else { crc << 1 };
        }
    }
    crc ^ xorout
}

/// A decoded TSBK: raw bytes plus whether its CRC checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tsbk {
    pub bytes: [u8; 12],
    pub crc_ok: bool,
    pub trellis_errors: u32,
}

impl Tsbk {
    pub fn last_block(&self) -> bool {
        self.bytes[0] & 0x80 != 0
    }
    pub fn opcode(&self) -> u8 {
        self.bytes[0] & 0x3F
    }
    pub fn mfid(&self) -> u8 {
        self.bytes[1]
    }
    fn arg(&self, bit: usize, len: usize) -> u64 {
        // Bits of bytes 2..10 (64 bits), MSB first.
        let v = u64::from_be_bytes(self.bytes[2..10].try_into().unwrap());
        (v >> (64 - bit - len)) & ((1u64 << len) - 1)
    }
}

pub fn decode_block(rx: &[u8; 98], c: Coding) -> Tsbk {
    let (bytes, trellis_errors) = viterbi(&deinterleave(rx, c.scatter), c.transpose);
    let crc = crc_ccitt(&bytes[..10], c.crc_init, c.crc_xorout);
    Tsbk { bytes, crc_ok: crc == u16::from_be_bytes([bytes[10], bytes[11]]), trellis_errors }
}

/// [`decode_block`] from soft symbol values.
pub fn decode_soft_block(rx: &[f32; 98], c: Coding) -> Tsbk {
    let (bytes, trellis_errors) = viterbi_soft(&deinterleave(rx, c.scatter), c.transpose);
    let crc = crc_ccitt(&bytes[..10], c.crc_init, c.crc_xorout);
    Tsbk { bytes, crc_ok: crc == u16::from_be_bytes([bytes[10], bytes[11]]), trellis_errors }
}

/// TSBKs of a TSDU frame, in order, stopping after the last-block flag.
///
/// A block whose hard decision fails its CRC is decoded again from its soft values, kept if that passes.
pub fn tsbks(f: &Frame, c: Coding) -> Vec<Tsbk> {
    let mut out = Vec::new();
    let soft = tsbk_soft_blocks(f);
    for (k, b) in tsbk_blocks(f).iter().enumerate() {
        let mut t = decode_block(b, c);
        if !t.crc_ok
            && let Some(again) = soft.get(k).map(|v| decode_soft_block(v, c)).filter(|t| t.crc_ok)
        {
            t = again;
        }
        let last = t.last_block();
        out.push(t);
        if last {
            break;
        }
    }
    out
}

/// Channel: 4-bit identifier (band plan) + 12-bit channel number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Channel {
    pub iden: u8,
    pub number: u16,
}

impl Channel {
    fn from(v: u64) -> Self {
        Self { iden: (v >> 12) as u8, number: (v & 0xFFF) as u16 }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.iden, self.number)
    }
}

/// A band plan entry from an identifier update: f = base + spacing × number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandPlan {
    pub iden: u8,
    pub base_hz: f64,
    pub spacing_hz: f64,
    pub tx_offset_hz: f64,
    pub bandwidth_hz: f64,
    /// TDMA time slots per carrier (1 for FDMA plans). For TDMA plans the
    /// channel number counts slots: carrier = number / slots.
    pub slots: u8,
}

impl BandPlan {
    pub fn freq(&self, ch: Channel) -> f64 {
        self.base_hz + self.spacing_hz * (ch.number / self.slots.max(1) as u16) as f64
    }

    /// TDMA slot of a channel number (0 for FDMA).
    pub fn slot(&self, ch: Channel) -> u8 {
        (ch.number % self.slots.max(1) as u16) as u8
    }
}

/// The messages we interpret (standard MFID 0x00). Others are kept raw.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    GroupGrant { channel: Channel, group: u16, source: u32, options: u8 },
    GroupGrantUpdate { a: (Channel, u16), b: Option<(Channel, u16)> },
    IdenUpdate(BandPlan),
    RfssStatus { system: u16, rfss: u8, site: u8, channel: Channel },
    NetworkStatus { wacn: u32, system: u16, channel: Channel },
    AdjacentSite { system: u16, rfss: u8, site: u8, channel: Channel },
    Other { opcode: u8, mfid: u8 },
}

pub fn opcode_name(op: u8) -> &'static str {
    match op {
        0x00 => "GRP_V_CH_GRANT",
        0x02 => "GRP_V_CH_GRANT_UPDT",
        0x03 => "GRP_V_CH_GRANT_UPDT_EXP",
        0x04 => "UU_V_CH_GRANT",
        0x06 => "UU_V_CH_GRANT_UPDT",
        0x20 => "ACK_RSP_FNE",
        0x28 => "GRP_AFF_RSP",
        0x2C => "U_REG_RSP",
        0x2F => "U_DE_REG_ACK",
        0x30 => "SYNC_BCST",
        0x33 => "IDEN_UP_TDMA",
        0x34 => "IDEN_UP_VU",
        0x35 => "TIME_DATE_ANN",
        0x39 => "SCCB",
        0x3A => "RFSS_STS_BCST",
        0x3B => "NET_STS_BCST",
        0x3C => "ADJ_STS_BCST",
        0x3D => "IDEN_UP",
        _ => "?",
    }
}

pub fn parse(t: &Tsbk) -> Message {
    let (op, mfid) = (t.opcode(), t.mfid());
    if mfid != 0x00 {
        return Message::Other { opcode: op, mfid };
    }
    match op {
        0x00 => Message::GroupGrant {
            options: t.arg(0, 8) as u8,
            channel: Channel::from(t.arg(8, 16)),
            group: t.arg(24, 16) as u16,
            source: t.arg(40, 24) as u32,
        },
        0x02 => {
            let a = (Channel::from(t.arg(0, 16)), t.arg(16, 16) as u16);
            let b = (Channel::from(t.arg(32, 16)), t.arg(48, 16) as u16);
            Message::GroupGrantUpdate { a, b: (b != a).then_some(b) }
        }
        // IDEN_UP (700/800/900 MHz): iden 4, bw 9, tx offset sign 1 + 8, spacing 10, base 32.
        0x3D => {
            let spacing = t.arg(22, 10) as f64 * 125.0;
            let off = t.arg(14, 8) as f64 * 250e3 * if t.arg(13, 1) == 1 { 1.0 } else { -1.0 };
            Message::IdenUpdate(BandPlan {
                iden: t.arg(0, 4) as u8,
                bandwidth_hz: t.arg(4, 9) as f64 * 125.0,
                tx_offset_hz: off,
                spacing_hz: spacing,
                base_hz: t.arg(32, 32) as f64 * 5.0,
                slots: 1,
            })
        }
        // IDEN_UP_TDMA: iden 4, channel type 4, tx offset sign 1 + 13,
        // spacing 10, base 32. Channel type sets the slots per carrier.
        0x33 => {
            let spacing = t.arg(22, 10) as f64 * 125.0;
            let off = t.arg(9, 13) as f64 * spacing * if t.arg(8, 1) == 1 { 1.0 } else { -1.0 };
            let (bw, slots) = match t.arg(4, 4) {
                0 => (6.25e3, 1),
                1 => (12.5e3, 1),
                2 => (6.25e3, 1),
                3 => (12.5e3, 2),
                4 => (25e3, 4),
                5 => (12.5e3, 2),
                _ => (0.0, 1),
            };
            Message::IdenUpdate(BandPlan {
                iden: t.arg(0, 4) as u8,
                bandwidth_hz: bw,
                tx_offset_hz: off,
                spacing_hz: spacing,
                base_hz: t.arg(32, 32) as f64 * 5.0,
                slots,
            })
        }
        // IDEN_UP_VU (VHF/UHF): iden 4, bw 4, tx offset sign 1 + 13, spacing 10, base 32.
        0x34 => {
            let spacing = t.arg(22, 10) as f64 * 125.0;
            let off = t.arg(9, 13) as f64 * spacing * if t.arg(8, 1) == 1 { 1.0 } else { -1.0 };
            let bw = match t.arg(4, 4) {
                0x4 => 6.25e3,
                0x5 => 12.5e3,
                _ => 0.0,
            };
            Message::IdenUpdate(BandPlan {
                iden: t.arg(0, 4) as u8,
                bandwidth_hz: bw,
                tx_offset_hz: off,
                spacing_hz: spacing,
                base_hz: t.arg(32, 32) as f64 * 5.0,
                slots: 1,
            })
        }
        // RFSS_STS_BCST: LRA 8, flags 4, sysid 12, rfss 8, site 8, channel 16, class 8.
        0x3A => Message::RfssStatus {
            system: t.arg(12, 12) as u16,
            rfss: t.arg(24, 8) as u8,
            site: t.arg(32, 8) as u8,
            channel: Channel::from(t.arg(40, 16)),
        },
        // NET_STS_BCST: LRA 8, WACN 20, sysid 12, channel 16, class 8.
        0x3B => Message::NetworkStatus { wacn: t.arg(8, 20) as u32, system: t.arg(28, 12) as u16, channel: Channel::from(t.arg(40, 16)) },
        // ADJ_STS_BCST: LRA 8, flags 4, sysid 12, rfss 8, site 8, channel 16, class 8.
        0x3C => Message::AdjacentSite {
            system: t.arg(12, 12) as u16,
            rfss: t.arg(24, 8) as u8,
            site: t.arg(32, 8) as u8,
            channel: Channel::from(t.arg(40, 16)),
        },
        _ => Message::Other { opcode: op, mfid },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode 12 bytes the way a transmitter would (with our tables), then
    /// check the decoder inverts it, with and without bit errors.
    fn encode(bytes: &[u8; 12], c: Coding) -> [u8; 98] {
        let mut inputs: Vec<usize> = (0..48).map(|k| (bytes[k / 4] >> (6 - 2 * (k % 4)) & 3) as usize).collect();
        inputs.push(0);
        let mut state = 0;
        let mut d = [0u8; 98];
        for (t, &i) in inputs.iter().enumerate() {
            let n = trellis(state, i, c.transpose);
            d[2 * t] = n >> 2;
            d[2 * t + 1] = n & 3;
            state = i;
        }
        let mut tx = [0u8; 98];
        for i in 0..98 {
            if c.scatter {
                tx[i] = d[INTERLEAVE[i]];
            } else {
                tx[INTERLEAVE[i]] = d[i];
            }
        }
        tx
    }

    #[test]
    fn roundtrip_with_errors() {
        let mut b = [0x3Bu8, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0, 0];
        let crc = crc_ccitt(&b[..10], CODING.crc_init, CODING.crc_xorout);
        b[10..].copy_from_slice(&crc.to_be_bytes());
        let mut tx = encode(&b, CODING);
        let t = decode_block(&tx, CODING);
        assert!(t.crc_ok && t.bytes == b && t.trellis_errors == 0);
        // Isolated flipped bits are corrected by the trellis.
        tx[5] ^= 1;
        tx[60] ^= 2;
        let t = decode_block(&tx, CODING);
        assert!(t.crc_ok && t.bytes == b, "{t:?}");
    }

    #[test]
    fn soft_decisions_decode_more_noisy_blocks_than_hard_ones() {
        let mut b = [0x3Bu8, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0, 0];
        let crc = crc_ccitt(&b[..10], CODING.crc_init, CODING.crc_xorout);
        b[10..].copy_from_slice(&crc.to_be_bytes());
        let tx = encode(&b, CODING);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut gauss = move || {
            let mut u = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 11) as f64 + 1.0) / (1u64 << 53) as f64
            };
            ((-2.0 * u().ln()).sqrt() * (std::f64::consts::TAU * u()).cos()) as f32
        };
        let (mut hard_ok, mut soft_ok) = (0, 0);
        for _ in 0..300 {
            // Noise of a third of the level spacing's width: about one symbol in eight read wrong.
            let soft: [f32; 98] = std::array::from_fn(|i| LEVEL[tx[i] as usize] + 0.27 * gauss());
            let hard: [u8; 98] = std::array::from_fn(|i| {
                let v = soft[i];
                if v >= 2.0 / 3.0 {
                    0b01
                } else if v >= 0.0 {
                    0b00
                } else if v >= -2.0 / 3.0 {
                    0b10
                } else {
                    0b11
                }
            });
            let h = decode_block(&hard, CODING);
            let t = decode_soft_block(&soft, CODING);
            assert!(!h.crc_ok || h.bytes == b);
            assert!(!t.crc_ok || t.bytes == b);
            hard_ok += usize::from(h.crc_ok);
            soft_ok += usize::from(t.crc_ok);
        }
        // As written: soft 295, hard 130.
        assert!(soft_ok > 2 * hard_ok && soft_ok >= 280, "soft {soft_ok}, hard {hard_ok} of 300");
    }

    #[test]
    fn crc_ccitt_check_value() {
        // CRC-16/XMODEM ("123456789") = 0x31C3 for init 0, no xorout.
        assert_eq!(crc_ccitt(b"123456789", 0, 0), 0x31C3);
    }
}

/// Everything learned about one P25 system from its control channel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemInfo {
    pub nac: Option<u16>,
    pub wacn: Option<u32>,
    pub system: Option<u16>,
    pub rfss: Option<u8>,
    pub site: Option<u8>,
    pub control_hz: Option<f64>,
    pub plans: std::collections::BTreeMap<u8, BandPlan>,
    /// (system, rfss, site) → control channel.
    pub adjacent: std::collections::BTreeMap<(u16, u8, u8), Channel>,
}

impl SystemInfo {
    pub fn freq(&self, ch: Channel) -> Option<f64> {
        self.plans.get(&ch.iden).map(|p| p.freq(ch))
    }

    pub fn slot(&self, ch: Channel) -> Option<u8> {
        self.plans.get(&ch.iden).filter(|p| p.slots > 1).map(|p| p.slot(ch))
    }

    /// Folds one message in. Returns grants (talkgroup, channel, source)
    /// for the caller to log.
    pub fn apply(&mut self, m: &Message) -> Vec<(u16, Channel, Option<u32>)> {
        match m {
            Message::IdenUpdate(bp) => {
                self.plans.insert(bp.iden, *bp);
            }
            Message::NetworkStatus { wacn, system, channel } => {
                self.wacn = Some(*wacn);
                self.system = Some(*system);
                self.control_hz = self.freq(*channel).or(self.control_hz);
            }
            Message::RfssStatus { system, rfss, site, channel } => {
                self.system = Some(*system);
                self.rfss = Some(*rfss);
                self.site = Some(*site);
                self.control_hz = self.freq(*channel).or(self.control_hz);
            }
            Message::AdjacentSite { system, rfss, site, channel } => {
                self.adjacent.insert((*system, *rfss, *site), *channel);
            }
            Message::GroupGrant { channel, group, source, .. } => return vec![(*group, *channel, Some(*source))],
            Message::GroupGrantUpdate { a, b } => {
                let mut v = vec![(a.1, a.0, None)];
                if let Some(b) = b {
                    v.push((b.1, b.0, None));
                }
                return v;
            }
            Message::Other { .. } => {}
        }
        Vec::new()
    }
}

/// All CRC-valid messages from the TSDU frames among `frames`.
pub fn messages(frames: &[Frame]) -> Vec<Message> {
    frames
        .iter()
        .filter(|f| f.protocol == "P25" && f.detail == "Phase 1")
        .filter(|f| crate::decode::p25_nid(f).is_some_and(|n| n.duid == 0x7))
        .flat_map(|f| tsbks(f, CODING))
        .filter(|t| t.crc_ok)
        .map(|t| parse(&t))
        .collect()
}
