//! DMR data bursts: BPTC(196,96) payloads behind a data/control sync, and
//! the messages they carry (CSBK, full link control).
//!
//! A burst is 132 dibits: 49 info, 5 slot type, 24 sync, 5 slot type, 49
//! info. The 196 info bits are interleaved ((i·181) mod 196), then form a
//! 13×15 block product code: Hamming(15,11) rows and Hamming(13,9) columns
//! around 96 data bits. The data ends in a checksum chosen by the data type:
//! CSBK and data headers a CRC-CCITT (masked), link control headers and
//! terminators Reed-Solomon(12,9) parity (masked).
//!
//! As with P25 TSBKs, the details were confirmed on real captures: only the
//! right coding makes the checks pass (see `tests/real_captures.rs`).

use crate::frames::Frame;

/// Dibits of info before the slot type that precedes the sync.
const INFO_DIBITS: usize = 49;

/// Data type → what a burst carries (from the slot type).
pub const DT_VOICE_LC_HEADER: u8 = 1;
pub const DT_TERMINATOR: u8 = 2;
pub const DT_CSBK: u8 = 3;
pub const DT_DATA_HEADER: u8 = 6;
pub const DT_IDLE: u8 = 9;

/// The 196 coded info bits of a data burst whose sync starts at
/// `f.sync_start`; `f.dibits` must extend 54 dibits before and 78 after.
pub fn info_bits(f: &Frame) -> Option<[u8; 196]> {
    let s = f.sync_start;
    let a = f.dibits.get(s.checked_sub(5 + INFO_DIBITS)?..s - 5)?;
    let b = f.dibits.get(s + 29..s + 29 + INFO_DIBITS)?;
    let mut out = [0u8; 196];
    for (i, d) in a.iter().chain(b).enumerate() {
        out[2 * i] = (d >> 1) & 1;
        out[2 * i + 1] = d & 1;
    }
    Some(out)
}

/// Hamming(15,11,3) parity of 11 data bits, as MMDVM/DSD use for BPTC rows.
fn hamming15_parity(d: &[u8]) -> [u8; 4] {
    [
        d[0] ^ d[1] ^ d[2] ^ d[3] ^ d[5] ^ d[7] ^ d[8],
        d[1] ^ d[2] ^ d[3] ^ d[4] ^ d[6] ^ d[8] ^ d[9],
        d[2] ^ d[3] ^ d[4] ^ d[5] ^ d[7] ^ d[9] ^ d[10],
        d[0] ^ d[1] ^ d[2] ^ d[4] ^ d[6] ^ d[7] ^ d[10],
    ]
}

/// Hamming(13,9,3) parity of 9 data bits, for BPTC columns.
fn hamming13_parity(d: &[u8]) -> [u8; 4] {
    [
        d[0] ^ d[1] ^ d[3] ^ d[5] ^ d[6],
        d[0] ^ d[1] ^ d[2] ^ d[4] ^ d[6] ^ d[7],
        d[0] ^ d[1] ^ d[2] ^ d[3] ^ d[5] ^ d[7] ^ d[8],
        d[0] ^ d[2] ^ d[4] ^ d[5] ^ d[8],
    ]
}

/// Corrects one bit in a codeword of `n` bits (data then 4 parity) if a
/// single flip makes the parity check. Returns false if it couldn't.
fn correct(word: &mut [u8], parity: fn(&[u8]) -> [u8; 4]) -> bool {
    let k = word.len() - 4;
    let ok = |w: &[u8]| parity(&w[..k]) == w[k..];
    if ok(word) {
        return true;
    }
    for i in 0..word.len() {
        word[i] ^= 1;
        if ok(word) {
            return true;
        }
        word[i] ^= 1;
    }
    false
}

/// Fraction of rows and of columns whose Hamming parity checks as received
/// (no correction): a diagnostic for the parity equations.
pub fn bptc_parity_stats(raw: &[u8; 196]) -> (usize, usize) {
    let mut m = [0u8; 196];
    for (a, v) in m.iter_mut().enumerate() {
        *v = raw[(a * 181) % 196];
    }
    let rows = (0..9).filter(|r| {
        let w = &m[1 + r * 15..1 + r * 15 + 15];
        hamming15_parity(&w[..11]) == w[11..]
    });
    let cols = (0..15).filter(|c| {
        let w: Vec<u8> = (0..13).map(|r| m[1 + r * 15 + c]).collect();
        hamming13_parity(&w[..9]) == w[9..]
    });
    (rows.count(), cols.count())
}

/// Result of a BPTC(196,96) decode.
#[derive(Debug, Clone, Copy)]
pub struct Bptc {
    pub data: [u8; 12],
    /// Rows/columns that failed their Hamming check even after correction.
    pub bad_lines: u32,
}

/// De-interleaves, corrects (a few passes of columns then rows) and extracts
/// the 96 data bits as 12 bytes.
pub fn bptc_decode(raw: &[u8; 196]) -> Bptc {
    let mut m = [0u8; 196];
    for (a, v) in m.iter_mut().enumerate() {
        *v = raw[(a * 181) % 196];
    }
    // m[0] is a reserved bit; the matrix is 13 rows × 15 columns from m[1].
    let mut bad = 0;
    for _ in 0..3 {
        bad = 0;
        for c in 0..15 {
            let mut col: Vec<u8> = (0..13).map(|r| m[1 + r * 15 + c]).collect();
            if !correct(&mut col, hamming13_parity) {
                bad += 1;
            }
            for r in 0..13 {
                m[1 + r * 15 + c] = col[r];
            }
        }
        for r in 0..9 {
            let row = &mut m[1 + r * 15..1 + r * 15 + 15];
            if !correct(row, hamming15_parity) {
                bad += 1;
            }
        }
        if bad == 0 {
            break;
        }
    }
    let mut bits = Vec::with_capacity(96);
    bits.extend_from_slice(&m[4..12]);
    for r in 1..9 {
        let s = 1 + r * 15;
        bits.extend_from_slice(&m[s..s + 11]);
    }
    let mut data = [0u8; 12];
    for (i, b) in bits.iter().enumerate() {
        data[i / 8] |= b << (7 - i % 8);
    }
    Bptc { data, bad_lines: bad }
}

/// CRC-CCITT (poly 0x1021, init 0), inverted, MSB first.
pub fn crc_ccitt(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { crc << 1 ^ 0x1021 } else { crc << 1 };
        }
    }
    !crc
}

/// CRC masks per data type.
pub const CSBK_CRC_MASK: u16 = 0xA5A5;
pub const DATA_HEADER_CRC_MASK: u16 = 0xCCCC;

pub fn crc_ok(data: &[u8; 12], mask: u16) -> bool {
    crc_ccitt(&data[..10]) ^ mask == u16::from_be_bytes([data[10], data[11]])
}

// ------------------------------------------------- Reed-Solomon (12,9) ----

fn gf_mul(a: u8, b: u8) -> u8 {
    // GF(256), primitive polynomial x^8+x^4+x^3+x^2+1 (0x11D).
    let (mut a, mut b, mut p) = (a as u16, b, 0u16);
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a <<= 1;
        if a & 0x100 != 0 {
            a ^= 0x11D;
        }
        b >>= 1;
    }
    p as u8
}

/// RS(12,9) parity of 9 LC bytes: generator (x+α)(x+α²)(x+α³) =
/// x³ + 14x² + 56x + 64, as in ETSI TS 102 361-1 B.3.6.
pub fn rs129_parity(msg: &[u8]) -> [u8; 3] {
    const G: [u8; 3] = [64, 56, 14];
    let mut p = [0u8; 3];
    for &m in msg {
        let fb = m ^ p[2];
        p[2] = p[1] ^ gf_mul(G[2], fb);
        p[1] = p[0] ^ gf_mul(G[1], fb);
        p[0] = gf_mul(G[0], fb);
    }
    // Highest-order parity byte is sent first.
    [p[2], p[1], p[0]]
}

pub const LC_HEADER_MASK: [u8; 3] = [0x96, 0x96, 0x96];
pub const LC_TERMINATOR_MASK: [u8; 3] = [0x99, 0x99, 0x99];

pub fn lc_ok(data: &[u8; 12], mask: [u8; 3]) -> bool {
    let p = rs129_parity(&data[..9]);
    (0..3).all(|i| p[i] ^ mask[i] == data[9 + i])
}

// ------------------------------------------------------------ messages ----

/// Full link control: who is talking to whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkControl {
    /// Full link control opcode: 0 group voice, 3 unit-to-unit voice.
    pub flco: u8,
    /// Feature set ID: 0 standard, 0x10 Motorola, 0x68 Hytera, ...
    pub fid: u8,
    pub options: u8,
    pub target: u32,
    pub source: u32,
    /// Capacity Plus: the voice channel (group voice) or rest channel (wide
    /// area voice) as (repeater 1-16, timeslot 1-2).
    pub channel: Option<(u8, u8)>,
}

/// Motorola's feature set ID (Capacity Plus, Connect Plus).
pub const FID_MOTOROLA: u8 = 0x10;

impl LinkControl {
    pub fn group(&self) -> bool {
        self.flco == 0 || (self.fid == FID_MOTOROLA && self.flco == 4)
    }
}

/// A control signalling block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csbk {
    pub last_block: bool,
    pub opcode: u8,
    pub fid: u8,
    /// The 8 argument bytes.
    pub args: [u8; 8],
}

/// What a decoded data burst said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Burst {
    VoiceHeader(LinkControl),
    Terminator(LinkControl),
    Csbk(Csbk),
    DataHeader { dpf: u8, target: u32, source: u32, group: bool },
}

fn lc_from(d: &[u8; 12]) -> LinkControl {
    let u24 = |i: usize| (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
    let (flco, fid) = (d[0] & 0x3F, d[1]);
    if fid == FID_MOTOROLA && (flco == 0 || flco == 4) {
        // Capacity Plus group voice (0) and wide-area group voice (4): an
        // 8-bit talkgroup, then a repeater (4 bits) and timeslot, then a
        // 16-bit radio ID (field positions as documented by SDRTrunk).
        return LinkControl {
            flco,
            fid,
            options: d[2],
            target: d[5] as u32,
            source: (d[7] as u32) << 8 | d[8] as u32,
            channel: Some(((d[6] >> 1 & 0xF) + 1, (d[6] & 1) + 1)),
        };
    }
    LinkControl { flco, fid, options: d[2], target: u24(3), source: u24(6), channel: None }
}

/// Decodes the payload of a data burst of the given data type (from its
/// slot type). None if the type isn't one we read or its check fails.
pub fn decode_burst(f: &Frame, data_type: u8) -> Option<Burst> {
    let raw = info_bits(f)?;
    let b = bptc_decode(&raw);
    let d = &b.data;
    match data_type {
        DT_VOICE_LC_HEADER if lc_ok(d, LC_HEADER_MASK) => Some(Burst::VoiceHeader(lc_from(d))),
        DT_TERMINATOR if lc_ok(d, LC_TERMINATOR_MASK) => Some(Burst::Terminator(lc_from(d))),
        DT_CSBK if crc_ok(d, CSBK_CRC_MASK) => {
            Some(Burst::Csbk(Csbk { last_block: d[0] & 0x80 != 0, opcode: d[0] & 0x3F, fid: d[1], args: d[2..10].try_into().unwrap() }))
        }
        DT_DATA_HEADER if crc_ok(d, DATA_HEADER_CRC_MASK) => Some(Burst::DataHeader {
            group: d[0] & 0x80 != 0,
            dpf: d[0] & 0x0F,
            target: (d[2] as u32) << 16 | (d[3] as u32) << 8 | d[4] as u32,
            source: (d[5] as u32) << 16 | (d[6] as u32) << 8 | d[7] as u32,
        }),
        _ => None,
    }
}

/// Capacity Plus system status (Motorola CSBK 0x3E). Field positions as
/// documented by SDRTrunk. Unconfirmed here: on the local idle site the
/// "rest repeater" climbs by one every several minutes (3, 4, ... 7 over a
/// day), identical on every repeater, always slot 2, so it may be a
/// timed rest rotation or not the rest channel at all. The activity bytes
/// aren't decoded (all zero on an idle site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapPlusStatus {
    /// Fragment flags (first/last of a multi-CSBK status): 3 = complete.
    pub fragment: u8,
    /// The rest channel: repeater 1-16 and timeslot 1-2.
    pub rest_repeater: u8,
    pub rest_slot: u8,
    /// Repeater activity, still raw.
    pub activity: [u8; 7],
}

impl Csbk {
    pub fn capplus_status(&self) -> Option<CapPlusStatus> {
        (self.fid == FID_MOTOROLA && self.opcode == 0x3E).then(|| {
            let b = self.args[0];
            CapPlusStatus { fragment: b >> 6, rest_repeater: (b >> 1 & 0xF) + 1, rest_slot: (b & 1) + 1, activity: self.args[1..8].try_into().unwrap() }
        })
    }
}

/// Short name of a CSBK, by feature set and opcode. Standard (FID 0) names
/// from ETSI TS 102 361-2/-4; Motorola (FID 0x10) Capacity Plus / Connect
/// Plus names as commonly documented by open decoders.
pub fn csbk_name(fid: u8, op: u8) -> &'static str {
    match (fid, op) {
        (0x00, 0x04) => "UU_V_Req",
        (0x00, 0x05) => "UU_Ans_Rsp",
        (0x00, 0x07) => "CT_CSBK",
        (0x00, 0x19) => "Aloha",
        (0x00, 0x1A) => "Ahoy",
        (0x00, 0x1C) => "Ackvitation",
        (0x00, 0x1E) => "Random access",
        (0x00, 0x1F) => "Call alert",
        (0x00, 0x20) => "Ack response",
        (0x00, 0x21) => "Ack outbound",
        (0x00, 0x26) => "Nack response",
        (0x00, 0x28) => "C_Bcast",
        (0x00, 0x2E) => "Clear",
        (0x00, 0x2F) => "Protect",
        (0x00, 0x30) => "Private voice grant",
        (0x00, 0x31) => "Talkgroup voice grant",
        (0x00, 0x32) => "Broadcast voice grant",
        (0x00, 0x33) => "Private data grant",
        (0x00, 0x34) => "Talkgroup data grant",
        (0x00, 0x38) => "BS outbound activation",
        (0x00, 0x3D) => "Preamble",
        (0x10, 0x01) => "Connect+ neighbours",
        (0x10, 0x03) => "Connect+ voice grant",
        (0x10, 0x06) => "Connect+ data grant",
        (0x10, 0x0C) => "Connect+ terminate",
        (0x10, 0x3E) => "Cap+ system status",
        (0x10, 0x3B) => "Cap+ neighbours",
        (0x10, 0x3D) => "Preamble",
        (0x10, 0x11) => "Cap+ data window",
        (_, 0x3D) => "Preamble",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the 196 interleaved bits for 12 data bytes, the way a
    /// transmitter does, so the decoder can be checked to invert it.
    fn bptc_encode(data: &[u8; 12]) -> [u8; 196] {
        let bits: Vec<u8> = (0..96).map(|i| data[i / 8] >> (7 - i % 8) & 1).collect();
        let mut m = [0u8; 196];
        m[4..12].copy_from_slice(&bits[..8]);
        let mut k = 8;
        for r in 1..9 {
            for c in 0..11 {
                m[1 + r * 15 + c] = bits[k];
                k += 1;
            }
        }
        for r in 0..9 {
            let s = 1 + r * 15;
            let p = hamming15_parity(&m[s..s + 11]);
            m[s + 11..s + 15].copy_from_slice(&p);
        }
        for c in 0..15 {
            let col: Vec<u8> = (0..9).map(|r| m[1 + r * 15 + c]).collect();
            let p = hamming13_parity(&col);
            for (i, v) in p.iter().enumerate() {
                m[1 + (9 + i) * 15 + c] = *v;
            }
        }
        let mut raw = [0u8; 196];
        for a in 0..196 {
            raw[(a * 181) % 196] = m[a];
        }
        raw
    }

    #[test]
    fn bptc_roundtrip_corrects_errors() {
        let mut d = [0x31u8, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0, 0, 0];
        let crc = crc_ccitt(&d[..10]) ^ CSBK_CRC_MASK;
        d[10..].copy_from_slice(&crc.to_be_bytes());
        let mut raw = bptc_encode(&d);
        let b = bptc_decode(&raw);
        assert_eq!(b.data, d);
        assert!(crc_ok(&b.data, CSBK_CRC_MASK));
        for i in [3, 50, 101, 150, 190] {
            raw[i] ^= 1;
        }
        let b = bptc_decode(&raw);
        assert_eq!(b.data, d, "bad lines {}", b.bad_lines);
    }

    #[test]
    fn capplus_status_rest_channel() {
        // From the 855.588 MHz capture: byte 0xE5 = fragment 3, rest
        // repeater 3, timeslot 2.
        let c = Csbk { last_block: true, opcode: 0x3E, fid: 0x10, args: [0xE5, 0, 0, 0, 0, 0, 0, 0] };
        let s = c.capplus_status().unwrap();
        assert_eq!((s.fragment, s.rest_repeater, s.rest_slot), (3, 3, 2));
    }

    #[test]
    fn rs129_parity_is_a_codeword() {
        // A codeword evaluates to zero at the generator's roots α, α², α³.
        let msg = [0x00u8, 0x00, 0x20, 0x00, 0x0C, 0x30, 0x2F, 0x9B, 0xE5];
        let p = rs129_parity(&msg);
        let cw: Vec<u8> = msg.iter().chain(&p).copied().collect();
        let mut alpha = 2u8;
        for _ in 0..3 {
            let mut acc = 0u8;
            for &c in &cw {
                acc = gf_mul(acc, alpha) ^ c;
            }
            assert_eq!(acc, 0);
            alpha = gf_mul(alpha, 2);
        }
    }
}
