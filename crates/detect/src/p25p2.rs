//! P25 Phase 2 (TDMA) signalling: who a Phase 2 voice channel carries.
//!
//! Outbound, the channel is a stream of superframe fragments of 720 dibits
//! (four 30 ms bursts). Each burst is 360 bits: a 40-bit inter-slot
//! signalling channel (ISCH) then 320 bits of timeslot; in every fragment
//! the third and fourth ISCH positions carry the 40-bit sync instead. The
//! first two ISCHs say which of the superframe's three fragments this is.
//!
//! A timeslot's DUID (8 bits at fixed positions) says what it carries:
//! voice, or FACCH/SACCH signalling, scrambled or not. Scrambling XORs the
//! timeslot with a sequence from a 44-bit LFSR seeded with the system's
//! WACN, system ID and NAC (known from the Phase 1 control channel).
//! FACCH and SACCH carry MAC PDUs protected by Reed-Solomon (63,35) over
//! GF(64), shortened and punctured: the punctured parity symbols decode as
//! erasures.
//!
//! Field positions follow the published P25 TDMA specification as
//! implemented by SDRTrunk; the code is SpecTrawl's own, and every
//! orientation is checked against real recordings (`examples/p25p2_probe`).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use crate::frames::Frame;
use crate::p25_lc::rs_decode_erasures;

/// Fragment length in dibits, and where the two syncs sit in it.
pub const FRAGMENT: usize = 720;
pub const SYNC_AT: [usize; 2] = [360, 540];
/// The sync, as dibits.
const SYNC: [u8; 20] = [1, 1, 1, 3, 1, 1, 3, 1, 1, 1, 1, 3, 3, 3, 1, 3, 3, 3, 3, 3];

/// The system identity that seeds the scrambling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrambleKey {
    pub wacn: u32,
    pub system: u16,
    pub nac: u16,
}

// ------------------------------------------------------ known systems ----

/// Systems (WACN, system ID) and NACs seen so far in this process: the
/// candidates for descrambling. The engine registers systems from Phase 1
/// control channels; NACs come from Phase 1 NIDs and Phase 2 end-of-PTT
/// messages (sent unscrambled). A wrong candidate can't produce a false
/// decode: Reed-Solomon fails on it.
#[derive(Default)]
struct Known {
    systems: BTreeSet<(u32, u16)>,
    nacs: BTreeSet<u16>,
    sequences: HashMap<ScrambleKey, std::sync::Arc<Vec<u8>>>,
}

fn known() -> &'static Mutex<Known> {
    static K: OnceLock<Mutex<Known>> = OnceLock::new();
    K.get_or_init(Default::default)
}

/// Registers a system learned from a Phase 1 control channel.
pub fn register_system(wacn: u32, system: u16) {
    known().lock().unwrap().systems.insert((wacn, system));
}

/// Registers a NAC seen on air.
pub fn register_nac(nac: u16) {
    if nac != 0 && nac < 0xF7E {
        known().lock().unwrap().nacs.insert(nac);
    }
}

/// Scrambling sequences for every known system × NAC (at most a few).
pub fn candidate_sequences() -> Vec<std::sync::Arc<Vec<u8>>> {
    let mut k = known().lock().unwrap();
    let keys: Vec<ScrambleKey> = k
        .systems
        .iter()
        .flat_map(|&(wacn, system)| k.nacs.iter().map(move |&nac| ScrambleKey { wacn, system, nac }))
        .take(64)
        .collect();
    keys.into_iter().map(|key| k.sequences.entry(key).or_insert_with(|| std::sync::Arc::new(scrambling(key))).clone()).collect()
}

/// The 4320-bit scrambling sequence for a system.
pub fn scrambling(k: ScrambleKey) -> Vec<u8> {
    let mask = (1u64 << 44) - 1;
    let mut reg = ((k.wacn as u64 & 0xF_FFFF) << 24 | (k.system as u64 & 0xFFF) << 12 | (k.nac as u64 & 0xFFF)) & mask;
    if reg == 0 {
        reg = mask;
    }
    let tap = |r: u64, b: u32| (r >> b & 1) as u8;
    (0..4320)
        .map(|_| {
            let out = tap(reg, 43);
            let fb = out ^ tap(reg, 33) ^ tap(reg, 19) ^ tap(reg, 14) ^ tap(reg, 8) ^ tap(reg, 3);
            reg = (reg << 1 & mask) | fb as u64;
            out
        })
        .collect()
}

/// The 320 scrambling bits for timeslot `index` (0..12) of a superframe:
/// the sequence starts half way through the first ISCH, and each timeslot
/// takes 320 of every 360 bits.
fn slot_sequence(seq: &[u8], index: usize) -> &[u8] {
    let s = 20 + 360 * index;
    &seq[s..s + 320]
}

// --------------------------------------------------------------- ISCH ----

/// ISCH generator (9 × 40) and the offset XORed onto every codeword.
const ISCH_G: [u64; 9] = [
    0b1000100000010110110011100011011011010111,
    0b0010000000011101111111010100111101100100,
    0b0001000000001111010010110001011101011000,
    0b0000110000000000110111101101000110001110,
    0b0000001000001000000001111111011111111111,
    0b0000100100000100100011011001101101110010,
    0b0000000010011101101000111010000101110001,
    0b0000000001011000110010111010101001001110,
    0b0000000000110100001111011000010110010111,
];
const ISCH_OFFSET: u64 = 0x18_4229_D461;

fn isch_codebook() -> &'static Vec<(u64, u16)> {
    static T: OnceLock<Vec<(u64, u16)>> = OnceLock::new();
    T.get_or_init(|| {
        (0u16..512)
            .map(|m| {
                // Row 0 is the message's most significant bit.
                let cw = (0..9).filter(|i| m >> (8 - i) & 1 == 1).fold(0u64, |a, i| a ^ ISCH_G[i]);
                (cw ^ ISCH_OFFSET, m)
            })
            .collect()
    })
}

/// A decoded ISCH: channel (0/1), fragment of the superframe (0..2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isch {
    pub channel: u8,
    pub fragment: u8,
    pub errors: u32,
}

/// Nearest-codeword decoding of a 40-bit ISCH.
pub fn isch(bits: u64) -> Isch {
    let (cw, m) = isch_codebook().iter().min_by_key(|(cw, _)| (cw ^ bits).count_ones()).copied().unwrap();
    Isch { channel: (m >> 5 & 3) as u8, fragment: (m >> 3 & 3) as u8, errors: (cw ^ bits).count_ones() }
}

// ---------------------------------------------------------- timeslots ----

/// DUID bit positions in a timeslot.
const DUID_BITS: [usize; 8] = [0, 1, 74, 75, 244, 245, 318, 319];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Burst {
    Voice4,
    Voice2,
    Sacch { scrambled: bool },
    Facch { scrambled: bool },
    Unknown,
}

fn duid(ts: &[u8]) -> Burst {
    let v = DUID_BITS.iter().fold(0u8, |a, &i| a << 1 | ts[i]);
    // The high nibble is the DUID; take the nearest of the six valid
    // (value, parity) bytes.
    const VALID: [(u8, Burst); 6] = [
        (0x00, Burst::Voice4),
        (0x39, Burst::Sacch { scrambled: true }),
        (0x65, Burst::Voice2),
        (0x9A, Burst::Facch { scrambled: true }),
        (0xC6, Burst::Sacch { scrambled: false }),
        (0xFF, Burst::Facch { scrambled: false }),
    ];
    let (code, b) = VALID.iter().min_by_key(|(c, _)| (c ^ v).count_ones()).unwrap();
    if (code ^ v).count_ones() <= 1 { *b } else { Burst::Unknown }
}

/// Hexbit positions (first bit of each 6) in a FACCH timeslot: 26 info then
/// 19 parity, skipping the DUID bits and the 40 bits at 138..180 where the
/// burst carries no FACCH data.
fn facch_hexbits() -> &'static Vec<[usize; 6]> {
    static T: OnceLock<Vec<[usize; 6]>> = OnceLock::new();
    T.get_or_init(|| hexbit_layout(&[(0, 2), (74, 76), (138, 180), (244, 246), (318, 320)], 45))
}

/// SACCH: 30 info then 22 parity, skipping the DUID bits.
fn sacch_hexbits() -> &'static Vec<[usize; 6]> {
    static T: OnceLock<Vec<[usize; 6]>> = OnceLock::new();
    T.get_or_init(|| hexbit_layout(&[(0, 2), (74, 76), (244, 246), (318, 320)], 52))
}

fn hexbit_layout(skip: &[(usize, usize)], count: usize) -> Vec<[usize; 6]> {
    let bits: Vec<usize> = (0..320).filter(|i| !skip.iter().any(|&(a, b)| (a..b).contains(i))).collect();
    bits.as_chunks::<6>().0.iter().take(count).copied().collect()
}

/// RS(63,35) decode of a signalling timeslot into its MAC PDU bits.
/// `info`/`parity` hexbits are sent; the code is shortened by `63 - 28 -
/// info` leading zeros and the last `28 - parity` parity symbols punctured.
fn mac_bits(ts: &[u8], layout: &[[usize; 6]], info: usize) -> Option<(Vec<u8>, usize)> {
    let parity = layout.len() - info;
    let short = 35 - info;
    let mut cw = [0u8; 63];
    for (k, pos) in layout.iter().enumerate() {
        cw[short + k] = pos.iter().fold(0, |a, &i| a << 1 | ts[i]);
    }
    let erasures: Vec<usize> = (short + info + parity..63).collect();
    let fixed = rs_decode_erasures(&mut cw, 28, &erasures)?;
    // Corrections inside the shortened zeros mean a wrong decode.
    if cw[..short].iter().any(|&c| c != 0) {
        return None;
    }
    let bits = cw[short..short + info].iter().flat_map(|&h| (0..6).rev().map(move |i| h >> i & 1)).collect();
    Some((bits, fixed.saturating_sub(erasures.len())))
}

// ----------------------------------------------------------------- MAC ----

/// What a MAC PDU said about the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mac {
    /// MAC_PTT / MAC_END_PTT: the call starting or ending, with the
    /// channel's colour code (its NAC).
    Ptt { end: bool, source: u32, group: u16, nac: u16 },
    GroupVoice { group: u16, source: u32, encrypted: bool, emergency: bool },
    UnitVoice { target: u32, source: u32, encrypted: bool },
    /// Any other PDU: its type and first opcode.
    Other { pdu: u8, opcode: u8 },
}

fn field(b: &[u8], start: usize, len: usize) -> u32 {
    b[start..start + len].iter().fold(0, |a, &x| a << 1 | x as u32)
}

pub fn parse_mac(b: &[u8]) -> Mac {
    let pdu = field(b, 0, 3) as u8;
    match pdu {
        1 | 2 => Mac::Ptt { end: pdu == 2, source: field(b, 104, 24), group: field(b, 128, 16) as u16, nac: field(b, 12, 12) as u16 },
        3 | 4 | 6 => {
            // IDLE / ACTIVE / HANGTIME: the first MAC structure follows the
            // 8-bit header.
            let op = field(b, 8, 8) as u8;
            let svc = field(b, 16, 8);
            let (encrypted, emergency) = (svc & 0x40 != 0, svc & 0x80 != 0);
            match op {
                0x01 | 0x21 => Mac::GroupVoice { group: field(b, 24, 16) as u16, source: field(b, 40, 24), encrypted, emergency },
                0x02 | 0x22 => Mac::UnitVoice { target: field(b, 24, 24), source: field(b, 48, 24), encrypted },
                _ => Mac::Other { pdu, opcode: op },
            }
        }
        _ => Mac::Other { pdu, opcode: 0 },
    }
}

impl Mac {
    /// A talk string as for Phase 1 and DMR, when the PDU names a call.
    pub fn talk(&self) -> Option<String> {
        let src = |s: u32| if s == 0 { String::new() } else { format!(" ← {s}") };
        match *self {
            Mac::Ptt { group, source, end: false, .. } if group != 0 => Some(format!("TG {group}{}", src(source))),
            Mac::GroupVoice { group, source, encrypted, emergency } => Some(format!(
                "TG {group}{}{}{}",
                src(source),
                if emergency { " EMERGENCY" } else { "" },
                if encrypted { " (enc)" } else { "" }
            )),
            Mac::UnitVoice { target, source, encrypted } => {
                Some(format!("{source} → {target}{}", if encrypted { " (enc)" } else { "" }))
            }
            _ => None,
        }
    }
}

// ------------------------------------------------------------- frames ----

/// One decoded signalling burst.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    /// Logical channel (0/1, from the ISCH) and burst kind.
    pub channel: u8,
    pub burst: Burst,
    pub mac: Mac,
    /// Hexbits Reed-Solomon corrected.
    pub corrected: usize,
}

/// What one fragment held: its ISCHs and each burst.
#[derive(Debug, Clone)]
pub struct FragmentInfo {
    pub fragment: Option<u8>,
    /// Index into the frame's dibits where the fragment starts.
    pub start: usize,
    pub bursts: Vec<Burst>,
    pub signals: Vec<Signal>,
}

fn bits_of(d: &[u8]) -> Vec<u8> {
    d.iter().flat_map(|&v| [v >> 1, v & 1]).collect()
}

/// Decodes the fragment around a Phase 2 sync frame. `f` must be sliced
/// with at least 540 dibits before the sync and 360 after. `seqs` are the
/// candidate scrambling sequences; scrambled bursts try each until one
/// decodes.
pub fn fragment(f: &Frame, seqs: &[&[u8]]) -> Option<FragmentInfo> {
    let sync_err = |at: usize| -> Option<usize> {
        let d = f.dibits.get(at..at + 20)?;
        Some(d.iter().zip(&SYNC).map(|(a, b)| (a ^ b).count_ones() as usize).sum())
    };
    // Is this the fragment's first or second sync? The other must be 180
    // dibits away.
    let s = f.sync_start;
    let start = SYNC_AT
        .iter()
        .filter_map(|&off| {
            let start = s.checked_sub(off)?;
            let other = start + if off == SYNC_AT[0] { SYNC_AT[1] } else { SYNC_AT[0] };
            sync_err(other).filter(|&e| e <= 6).map(|e| (e, start))
        })
        .min()?
        .1;
    let d = f.dibits.get(start..start + FRAGMENT)?;
    let bits = bits_of(d);
    let word = |b: &[u8]| b.iter().fold(0u64, |a, &x| a << 1 | x as u64);
    let i0 = isch(word(&bits[0..40]));
    let i1 = isch(word(&bits[360..400]));
    let fragment = [i0, i1].iter().filter(|i| i.errors <= 6).map(|i| i.fragment).find(|&f| f < 3);
    // The last fragment of a superframe swaps channels in its second half.
    let last = fragment == Some(2);
    let mut out = FragmentInfo { fragment, start, bursts: Vec::new(), signals: Vec::new() };
    for (k, ts_start) in [40usize, 400, 760, 1120].into_iter().enumerate() {
        let ts = bits[ts_start..ts_start + 320].to_vec();
        let burst = duid(&ts);
        out.bursts.push(burst);
        let channel = match (k, last) {
            (0, _) => 0,
            (1, _) => 1,
            (2, false) | (3, true) => 0,
            _ => 1,
        };
        let decode = |ts: &[u8]| match burst {
            Burst::Facch { .. } => mac_bits(ts, facch_hexbits(), 26),
            Burst::Sacch { .. } => mac_bits(ts, sacch_hexbits(), 30),
            _ => None,
        };
        let scrambled = matches!(burst, Burst::Facch { scrambled: true } | Burst::Sacch { scrambled: true });
        let decoded = if scrambled {
            let Some(fr) = fragment else { continue };
            seqs.iter().find_map(|seq| {
                let t: Vec<u8> = ts.iter().zip(slot_sequence(seq, fr as usize * 4 + k)).map(|(b, s)| b ^ s).collect();
                decode(&t)
            })
        } else {
            decode(&ts)
        };
        if let Some((mac, corrected)) = decoded {
            let mac = parse_mac(&mac);
            if let Mac::Ptt { nac, .. } = mac {
                register_nac(nac);
            }
            out.signals.push(Signal { channel, burst, mac, corrected });
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrambling_starts_from_the_seed() {
        // The first 44 output bits are the seed itself, MSB first.
        let k = ScrambleKey { wacn: 0xBEE00, system: 0x2A5, nac: 0x2A5 };
        let seq = scrambling(k);
        let seed = (0xBEE00u64 << 24) | (0x2A5 << 12) | 0x2A5;
        let first: u64 = seq[..44].iter().fold(0, |a, &b| a << 1 | b as u64);
        assert_eq!(first, seed);
        assert_eq!(seq.len(), 4320);
    }

    #[test]
    fn isch_round_trip() {
        for (m, (cw, mm)) in isch_codebook().iter().enumerate() {
            assert_eq!(*mm as usize, m);
            let i = isch(*cw ^ 1 << 7 ^ 1 << 30);
            assert_eq!((i.channel, i.fragment), ((m >> 5 & 3) as u8, (m >> 3 & 3) as u8));
        }
    }

    #[test]
    fn duid_tolerates_one_bit() {
        let mut ts = vec![0u8; 320];
        for (k, &i) in DUID_BITS.iter().enumerate() {
            ts[i] = 0x9A >> (7 - k) & 1;
        }
        assert_eq!(duid(&ts), Burst::Facch { scrambled: true });
        ts[74] ^= 1;
        assert_eq!(duid(&ts), Burst::Facch { scrambled: true });
    }

    #[test]
    fn layouts_fit() {
        assert_eq!(facch_hexbits().len(), 45);
        assert_eq!(sacch_hexbits().len(), 52);
    }
}
