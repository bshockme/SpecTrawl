//! First protocol fields, decoded from sliced frames with error correction.
//!
//! - P25 Phase 1: the NID after the frame sync (NAC + DUID), BCH(63,16).
//! - DMR: the slot type around a data/control sync (colour code + data
//!   type), Golay(20,8).
//! - NXDN: the LICH, which is part of the matched sync pattern itself.
//!
//! Both codes are decoded by nearest codeword (exhaustive over the info
//! space: 65,536 for P25, 256 for DMR), which is cheap at these rates.
//! Each code's details were confirmed on real captures: with the right
//! generator, clean frames match their parity exactly.

use crate::frames::Frame;

fn bits_from_dibits(d: &[u8]) -> Vec<u8> {
    d.iter().flat_map(|&x| [(x >> 1) & 1, x & 1]).collect()
}

fn to_u64(bits: &[u8]) -> u64 {
    bits.iter().fold(0u64, |a, &b| a << 1 | b as u64)
}

/// Systematic parity: (info · x^deg mod g), with `g` including its x^deg term.
fn parity(info: u64, g: u64, deg: u32, info_bits: u32) -> u64 {
    let mut r = info << deg;
    for bit in (deg..deg + info_bits).rev() {
        if r >> bit & 1 == 1 {
            r ^= g << (bit - deg);
        }
    }
    r & ((1u64 << deg) - 1)
}

// ---------------------------------------------------------------- P25 ----

/// P25 BCH(63,16) generator, octal 6331141367235453 (degree 47).
const P25_BCH_GEN: u64 = 0o6331141367235453;
/// Accept NIDs within this many bit errors of a codeword. BCH(63,16) corrects
/// 11; a random 63-bit word lands within 9 of some codeword with p ≈ 1e-4.
const P25_MAX_ERRORS: u32 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct P25Nid {
    pub nac: u16,
    pub duid: u8,
    /// Bits corrected.
    pub errors: u32,
}

pub fn p25_duid_name(d: u8) -> &'static str {
    match d {
        0x0 => "HDU",
        0x3 => "TDU",
        0x5 => "LDU1",
        0x7 => "TSDU",
        0xA => "LDU2",
        0xC => "PDU",
        0xF => "TDULC",
        _ => "?",
    }
}

/// The 63-bit BCH codeword of the NID for `nac` and `duid`, most significant bit first as sent (a transmitter follows
/// it with a parity bit, which the decoder doesn't read).
pub fn p25_nid_codeword(nac: u16, duid: u8) -> u64 {
    let info = u64::from(nac & 0xFFF) << 4 | u64::from(duid & 0xF);
    info << 47 | parity(info, P25_BCH_GEN, 47, 16)
}

/// Reads and corrects the NID of a frame whose `dibits` start at the frame
/// sync (`sync_start`), skipping the status symbol after every 35 dibits of
/// the frame (counted from the start of the sync).
pub fn p25_nid(f: &Frame) -> Option<P25Nid> {
    let base = f.sync_start;
    let mut nid_dibits = Vec::with_capacity(32);
    let mut idx = 24; // first dibit after the 24-dibit frame sync
    while nid_dibits.len() < 32 {
        if (idx + 1) % 36 != 0 {
            nid_dibits.push(*f.dibits.get(base + idx)?);
        }
        idx += 1;
    }
    let bits = bits_from_dibits(&nid_dibits);
    let rx = to_u64(&bits[..63]);
    // Fast path: received info bits already consistent.
    let info0 = rx >> 47;
    let e0 = ((rx & ((1 << 47) - 1)) ^ parity(info0, P25_BCH_GEN, 47, 16)).count_ones();
    let (info, errors) = if e0 == 0 {
        (info0, 0)
    } else {
        (0u64..1 << 16).map(|i| (i, (rx ^ (i << 47 | parity(i, P25_BCH_GEN, 47, 16))).count_ones())).min_by_key(|&(_, e)| e).unwrap()
    };
    (errors <= P25_MAX_ERRORS).then_some(P25Nid { nac: (info >> 4) as u16, duid: (info & 0xF) as u8, errors })
}

// ---------------------------------------------------------------- DMR ----

/// DMR slot type Golay(20,8): (x+1)·g23 where g23 = 0xC75 is the Golay(23,12)
/// generator, i.e. the extended Golay code shortened to 8 info bits. Found by
/// testing every degree-12 generator on real slot types; this one (and the
/// equivalent x·g23) are the only ones clean frames match.
const DMR_GOLAY_GEN: u64 = 0x149F;
const DMR_MAX_ERRORS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmrSlotType {
    pub colour_code: u8,
    pub data_type: u8,
    pub errors: u32,
}

pub fn dmr_data_type_name(t: u8) -> &'static str {
    match t {
        0 => "PI header",
        1 => "voice LC header",
        2 => "terminator",
        3 => "CSBK",
        4 => "MBC header",
        5 => "MBC cont.",
        6 => "data header",
        7 => "rate ½ data",
        8 => "rate ¾ data",
        9 => "idle",
        10 => "rate 1 data",
        _ => "?",
    }
}

/// The slot type straddles a data/control sync: 5 dibits before it and 5
/// after. Only data syncs carry one (voice bursts carry EMB instead).
pub fn dmr_slot_type(f: &Frame) -> Option<DmrSlotType> {
    if f.protocol != "DMR" || !f.detail.contains("data") {
        return None;
    }
    let s = f.sync_start;
    let before = f.dibits.get(s.checked_sub(5)?..s)?;
    let after = f.dibits.get(s + 24..s + 29)?;
    let rx = to_u64(&bits_from_dibits(&[before, after].concat()));
    let (info, errors) =
        (0u64..256).map(|i| (i, (rx ^ (i << 12 | parity(i, DMR_GOLAY_GEN, 12, 8))).count_ones())).min_by_key(|&(_, e)| e).unwrap();
    (errors <= DMR_MAX_ERRORS).then_some(DmrSlotType { colour_code: (info >> 4) as u8, data_type: (info & 0xF) as u8, errors })
}

// --------------------------------------------------------------- NXDN ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NxdnLich {
    pub raw: u8,
    pub rf_channel: u8,
    pub functional: u8,
    pub option: u8,
    /// The direction bit as received; which value means outbound is not yet
    /// confirmed against a known transmitter.
    pub direction_bit: bool,
}

pub fn nxdn_rf_channel_name(t: u8) -> &'static str {
    match t {
        0 => "control (RCCH)",
        1 => "traffic (RTCH)",
        2 => "data (RDCH)",
        _ => "composite (RTCH_C)",
    }
}

/// The last 8 symbols of the matched FSW+LICH pattern are the LICH bits:
/// RF channel type (2), functional channel type (2), option (2), direction
/// (1), parity (1; even parity over the top 4 bits — true for all 18 values
/// in the LICH table).
pub fn nxdn_lich(f: &Frame) -> Option<NxdnLich> {
    if f.protocol != "NXDN" {
        return None;
    }
    let raw = (f.pattern & 0xFF) as u8;
    Some(NxdnLich { raw, rf_channel: raw >> 6, functional: (raw >> 4) & 3, option: (raw >> 2) & 3, direction_bit: raw >> 1 & 1 == 1 })
}

// ------------------------------------------------------------ summary ----

/// What a digital channel is being used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Activity {
    /// Someone talking: voice frames, call headers and terminators.
    Voice,
    /// Trunking control: TSBKs, CSBKs, control-channel messages, idle fill.
    Control,
    /// Packet data.
    Data,
}

impl Activity {
    pub fn name(self) -> &'static str {
        match self {
            Activity::Voice => "voice",
            Activity::Control => "control",
            Activity::Data => "data",
        }
    }
}

/// Decoded fields aggregated over a channel's frames, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields {
    /// System identity: "NAC 0x2A5", "CC 9", "RAN …" — the most common value.
    pub id: Option<String>,
    /// Frame/message types seen, most common first, e.g. "TSDU×64".
    pub kinds: Vec<(String, usize)>,
    /// Frames that decoded (passed their code) / frames tried.
    pub decoded: usize,
    pub tried: usize,
    /// What the channel carried most: voice wins if any real voice was seen.
    pub activity: Option<Activity>,
    /// Who was talking, from a call header: "TG 1234 ← 5678" (group) or
    /// "5678 → 1234" (private); `call.summary()`.
    pub talk: Option<String>,
    /// The same call, as fields.
    pub call: Option<Call>,
    /// Text messages decoded (pages), oldest first.
    pub messages: Vec<String>,
    /// Capacity Plus: this channel's own repeater number, from group voice
    /// link control (a voice channel names itself).
    pub capplus_repeater: Option<u8>,
    /// Capacity Plus: the rest channel (repeater, timeslot) the latest
    /// system status here announces. Unconfirmed: on an idle site the value
    /// climbs by one every several minutes, the same on every repeater.
    pub capplus_rest: Option<(u8, u8)>,
}

/// Frames of voice that make a channel a voice channel (one stray voice
/// sync can be a false match on a busy data channel).
const MIN_VOICE_FRAMES: usize = 2;

/// A call: who is talking to whom, from any protocol's call signalling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Call {
    /// Group call (to a talkgroup) or unit-to-unit.
    pub group: bool,
    /// Talkgroup, or the called unit.
    pub target: u32,
    /// The talking radio, when the signalling names it.
    pub source: Option<u32>,
    pub encrypted: bool,
    pub emergency: bool,
}

impl Call {
    /// "TG 1234 ← 5678 (enc)" or "5678 → 1234".
    pub fn summary(&self) -> String {
        let flags = format!("{}{}", if self.emergency { " EMERGENCY" } else { "" }, if self.encrypted { " (enc)" } else { "" });
        let src = self.source.map(|s| s.to_string());
        if self.group {
            format!("TG {}{}{flags}", self.target, src.map(|s| format!(" ← {s}")).unwrap_or_default())
        } else {
            format!("{} → {}{flags}", src.unwrap_or_else(|| "?".into()), self.target)
        }
    }
}

/// The call heard most: grouped by talkgroup or target, then the variant
/// that names the source, if any, most common first.
fn best_call(calls: &std::collections::BTreeMap<Call, usize>) -> Option<Call> {
    let mut groups: std::collections::BTreeMap<(bool, u32), usize> = Default::default();
    for (c, n) in calls {
        *groups.entry((c.group, c.target)).or_default() += n;
    }
    let best = groups.into_iter().max_by_key(|(_, n)| *n)?.0;
    calls.iter().filter(|(c, _)| (c.group, c.target) == best).max_by_key(|(c, n)| (c.source.is_some(), **n)).map(|(c, _)| *c)
}

pub fn summarise(frames: &[Frame]) -> Fields {
    use std::collections::BTreeMap;
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut talks: BTreeMap<Call, usize> = BTreeMap::new();
    let mut act: BTreeMap<Activity, usize> = BTreeMap::new();
    let mut f = Fields::default();
    // Phase 2: both syncs of a fragment make frames; decode each fragment
    // once (keyed by where it starts in the input).
    let mut p2_seen = std::collections::HashSet::new();
    let p2_seqs = if frames.iter().any(|fr| fr.protocol == "P25" && fr.detail == "Phase 2") {
        crate::p25p2::candidate_sequences()
    } else {
        Vec::new()
    };
    let p2_refs: Vec<&[u8]> = p2_seqs.iter().map(|s| s.as_slice()).collect();
    for fr in frames {
        match fr.protocol {
            "P25" if fr.detail == "Phase 2" => {
                let Some(info) = crate::p25p2::fragment(fr, &p2_refs) else { continue };
                let at = (fr.at_symbol + info.start).saturating_sub(fr.sync_start) / 8;
                if !p2_seen.insert(at) {
                    continue;
                }
                f.tried += 1;
                if !info.signals.is_empty() {
                    f.decoded += 1;
                }
                for b in &info.bursts {
                    use crate::p25p2::Burst;
                    let (name, a) = match b {
                        Burst::Voice4 | Burst::Voice2 => ("voice", Some(Activity::Voice)),
                        Burst::Facch { .. } => ("FACCH", Some(Activity::Control)),
                        Burst::Sacch { .. } => ("SACCH", Some(Activity::Control)),
                        Burst::Unknown => continue,
                    };
                    *kinds.entry(name.into()).or_default() += 1;
                    if let Some(a) = a {
                        *act.entry(a).or_default() += 1;
                    }
                }
                for sig in &info.signals {
                    if let Some(c) = sig.mac.call() {
                        *talks.entry(c).or_default() += 1;
                    }
                    if let crate::p25p2::Mac::Ptt { nac, .. } = sig.mac {
                        *ids.entry(format!("NAC 0x{nac:03X}")).or_default() += 1;
                    }
                }
            }
            "P25" if fr.detail == "Phase 1" => {
                f.tried += 1;
                if let Some(n) = p25_nid(fr) {
                    f.decoded += 1;
                    crate::p25p2::register_nac(n.nac);
                    *ids.entry(format!("NAC 0x{:03X}", n.nac)).or_default() += 1;
                    *kinds.entry(p25_duid_name(n.duid).to_string()).or_default() += 1;
                    let lc = match n.duid {
                        0xF => crate::p25_lc::tdulc(fr),
                        0x5 => crate::p25_lc::ldu1(fr),
                        _ => None,
                    };
                    {
                        if let Some(lc) = lc {
                            let name = if lc.mfid > 1 { "vendor" } else { crate::p25_lc::lco_name(lc.lco) };
                            *kinds.entry(format!("LC {name}")).or_default() += 1;
                            if let Some(c) = lc.call() {
                                *talks.entry(c).or_default() += 1;
                            }
                        }
                    }
                    let a = match n.duid {
                        0x7 => Some(Activity::Control),
                        0xC => Some(Activity::Data),
                        0x0 | 0x3 | 0x5 | 0xA | 0xF => Some(Activity::Voice),
                        _ => None,
                    };
                    if let Some(a) = a {
                        *act.entry(a).or_default() += 1;
                    }
                }
            }
            "DMR" if fr.detail.contains("voice") => {
                *kinds.entry("voice".into()).or_default() += 1;
                *act.entry(Activity::Voice).or_default() += 1;
            }
            "DMR" if fr.detail.contains("data") => {
                f.tried += 1;
                let Some(s) = dmr_slot_type(fr) else { continue };
                f.decoded += 1;
                *ids.entry(format!("CC {}", s.colour_code)).or_default() += 1;
                *kinds.entry(dmr_data_type_name(s.data_type).to_string()).or_default() += 1;
                let a = match s.data_type {
                    0..=2 => Activity::Voice,
                    3..=5 | 9 => Activity::Control,
                    _ => Activity::Data,
                };
                *act.entry(a).or_default() += 1;
                match crate::dmr::decode_burst(fr, s.data_type) {
                    Some(crate::dmr::Burst::VoiceHeader(l) | crate::dmr::Burst::Terminator(l)) => {
                        let c = Call {
                            group: l.group(),
                            target: l.target,
                            source: (l.source != 0).then_some(l.source),
                            encrypted: false,
                            emergency: false,
                        };
                        *talks.entry(c).or_default() += 1;
                        // Cap+ group voice names the voice channel it's on.
                        if let (Some((rep, _)), 0) = (l.channel, l.flco) {
                            f.capplus_repeater = Some(rep);
                        }
                    }
                    Some(crate::dmr::Burst::Csbk(c)) => {
                        *kinds.entry(format!("CSBK {}", crate::dmr::csbk_name(c.fid, c.opcode))).or_default() += 1;
                        if let Some(st) = c.capplus_status() {
                            f.capplus_rest = Some((st.rest_repeater, st.rest_slot));
                        }
                    }
                    _ => {}
                }
            }
            "NXDN" => {
                if let Some(l) = nxdn_lich(fr) {
                    f.tried += 1;
                    f.decoded += 1;
                    *kinds.entry(nxdn_rf_channel_name(l.rf_channel).to_string()).or_default() += 1;
                    let a = match l.rf_channel {
                        0 => Activity::Control,
                        2 => Activity::Data,
                        _ => Activity::Voice,
                    };
                    *act.entry(a).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    f.id = ids.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
    f.call = best_call(&talks);
    f.talk = f.call.map(|c| c.summary());
    let voice = act.get(&Activity::Voice).copied().unwrap_or(0);
    f.activity = if voice >= MIN_VOICE_FRAMES || f.talk.is_some() {
        Some(Activity::Voice)
    } else {
        act.into_iter().filter(|(a, _)| *a != Activity::Voice).max_by_key(|(_, n)| *n).map(|(a, _)| a)
    };
    let mut k: Vec<(String, usize)> = kinds.into_iter().collect();
    k.sort_by_key(|e| std::cmp::Reverse(e.1));
    f.kinds = k;
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn divisible(cw: u64, g: u64, deg: u32, total: u32) -> bool {
        let mut r = cw;
        for bit in (deg..total).rev() {
            if r >> bit & 1 == 1 {
                r ^= g << (bit - deg);
            }
        }
        r == 0
    }

    #[test]
    fn codewords_are_divisible_by_their_generators() {
        for info in [0x0001u64, 0x2937, 0xFFFF, 0x8000] {
            assert!(divisible(info << 47 | parity(info, P25_BCH_GEN, 47, 16), P25_BCH_GEN, 47, 63));
        }
        for info in 0u64..256 {
            assert!(divisible(info << 12 | parity(info, DMR_GOLAY_GEN, 12, 8), DMR_GOLAY_GEN, 12, 20));
        }
    }

    #[test]
    fn dmr_golay_has_distance_8() {
        // Extended Golay shortened: minimum distance 8, so up to 3 errors correct.
        let cws: Vec<u64> = (0u64..256).map(|i| i << 12 | parity(i, DMR_GOLAY_GEN, 12, 8)).collect();
        let dmin = cws.iter().enumerate().flat_map(|(a, x)| cws[a + 1..].iter().map(move |y| (x ^ y).count_ones())).min().unwrap();
        assert_eq!(dmin, 8);
    }

    #[test]
    fn nxdn_lich_parity_matches_the_lich_table() {
        for p in [0xC8u8, 0xE1, 0xE5, 0xC4, 0x8B, 0x87, 0x89, 0x85, 0xA4, 0x4B, 0x47, 0x27, 0x49, 0x45, 0x79, 0x75, 0x64, 0x25] {
            assert_eq!((p >> 4).count_ones() % 2, (p & 1) as u32, "{p:#x}");
        }
    }
}
