//! The detector bank (spec §4.5): each detector looks at a record's window and returns claims with their own
//! evidence. They run side by side; none decides for another, and none renames (position and band plan are the book's).

use api::{Claim, Evidence, Identity};
use detect::bank::{self as rungs, Window};
use detect::decode::Fields;
use detect::frames::Frame;

/// A modulation read off the signal's shape is a description, never more than this sure.
pub const MAX_FEATURE_CONFIDENCE: f32 = 0.6;
/// An identifier (a FLEX header, a GSM SCH, a squelch code, decoded frames with an id).
pub const IDENTIFIED_CONFIDENCE: f32 = 0.95;
/// A P25 site named by its NAC and a TSBK.
pub const P25_SITE_CONFIDENCE: f32 = 0.99;
/// NIDs that must agree on a NAC before it names anything.
pub const MIN_NIDS: usize = 2;

/// One member of the bank.
pub trait Detector: Send + Sync {
    fn name(&self) -> &'static str;
    fn look(&self, w: &Window<'_>) -> Vec<Claim>;
    /// Describes the signal's shape rather than decoding it (see [`look`]).
    fn describes(&self) -> bool {
        false
    }
}

/// The bank: the detectors that decode, then the one that describes.
pub fn bank() -> Vec<Box<dyn Detector>> {
    vec![Box::new(SyncBank), Box::new(Identifiers), Box::new(Modulation)]
}

/// Every claim the bank makes about a window. A detector that only [describes](Detector::describes) the signal's
/// shape speaks only when nothing decoded or synced: a decode already says what the modulation is, and the shape rules
/// read a P25 control channel as "Analog FM" and a FLEX pager as "Bursts every 33 ms" (they were tuned as the last
/// rungs of a ladder that stopped at the first decode).
pub fn look(bank: &[Box<dyn Detector>], w: &Window<'_>) -> Vec<Claim> {
    let mut out: Vec<Claim> = bank.iter().filter(|d| !d.describes()).flat_map(|d| d.look(w)).collect();
    if !out.iter().any(Claim::is_decoded) {
        out.extend(bank.iter().filter(|d| d.describes()).flat_map(|d| d.look(w)));
    }
    out
}

/// `detect`'s sync bank: a `SyncBank` claim with its sync count and frames, and a `Decoder` claim when frames decoded
/// and named the signal (a NAC, a colour code, a RAN; a P25 control channel's site from its TSBKs).
pub struct SyncBank;

impl Detector for SyncBank {
    fn name(&self) -> &'static str {
        "sync bank"
    }

    fn look(&self, w: &Window<'_>) -> Vec<Claim> {
        let Some(s) = rungs::sync_bank(w) else { return Vec::new() };
        let label = if s.sync.detail.is_empty() { s.sync.protocol.to_string() } else { format!("{} {}", s.sync.protocol, s.sync.detail) };
        let (tried, decoded) = s.fields.as_ref().map_or((0, 0), |f| (f.tried as u32, f.decoded as u32));
        let mut out = vec![Claim::new(
            label.clone(),
            s.sync.confidence,
            Evidence::SyncBank { protocol: label.clone(), syncs: s.sync.count as u32, frames_tried: tried, frames_decoded: decoded },
        )];
        if let Some(f) = s.fields.as_ref().filter(|f| f.decoded > 0) {
            let p25 = (s.sync.protocol == "P25" && s.sync.detail == "Phase 1").then(|| p25_site(&s.frames)).flatten();
            let (identity, confidence, mode, label) = match p25 {
                Some(site) if site.tsbks > 0 => (Some(site.identity), P25_SITE_CONFIDENCE, "P25 control", format!("{label} control")),
                Some(site) => (Some(site.identity), IDENTIFIED_CONFIDENCE, "P25", label),
                None => (f.id.clone().map(|id| Identity::Other { id }), IDENTIFIED_CONFIDENCE, s.sync.protocol, label),
            };
            if identity.is_some() {
                let summary = summary(f, identity.as_ref(), !is_pager(s.sync.protocol));
                out.push(Claim::new(label, confidence, Evidence::Decoder { mode: mode.into(), locked: true, summary, identity }));
            }
        }
        out
    }
}

/// Identifiers that need no sync bank: FLEX headers, a GSM SCH, ACARS blocks, 802.15.4g frames, sub-audible squelch codes (on windows
/// strong enough to describe). Each is a `Decoder` claim.
pub struct Identifiers;

impl Detector for Identifiers {
    fn name(&self) -> &'static str {
        "identifiers"
    }

    fn look(&self, w: &Window<'_>) -> Vec<Claim> {
        let squelch = (!w.weak()).then(|| rungs::squelch(w)).flatten();
        [rungs::flex(w), rungs::gsm(w), rungs::acars(w), rungs::sun_fsk(w), squelch]
            .into_iter()
            .flatten()
            .map(|i| {
                let identity = i.fields.as_ref().and_then(|f| f.id.clone()).map(|id| Identity::Other { id });
                let summary =
                    i.fields.as_ref().map(|f| summary(f, identity.as_ref(), !i.protocol.is_some_and(is_pager))).unwrap_or_default();
                let mode = i.protocol.unwrap_or("FM");
                let label = match &identity {
                    Some(Identity::Other { id }) if i.protocol.is_none() => format!("{} ({id})", i.label),
                    _ => i.label,
                };
                Claim::new(
                    label,
                    i.confidence.max(IDENTIFIED_CONFIDENCE),
                    Evidence::Decoder { mode: mode.into(), locked: true, summary, identity },
                )
            })
            .collect()
    }
}

/// The modulation from the signal's shape (and LoRa's chirps): a `Features` claim, capped at
/// [`MAX_FEATURE_CONFIDENCE`]. Nothing for a weak window (noise has a shape too) or an "Unknown" one: no claim is the
/// honest answer there.
pub struct Modulation;

impl Detector for Modulation {
    fn name(&self) -> &'static str {
        "modulation"
    }

    fn describes(&self) -> bool {
        true
    }

    fn look(&self, w: &Window<'_>) -> Vec<Claim> {
        if w.weak() {
            return Vec::new();
        }
        let d = rungs::lora(w).unwrap_or_else(|| rungs::modulation(w));
        if d.label == "Unknown" {
            return Vec::new();
        }
        let f = &w.features;
        let by = Evidence::Features {
            bandwidth_hz: w.bandwidth_hz,
            snr_db: f.snr_db,
            duty: w.on_fraction(),
            envelope_cv: Some(f.envelope_cv).filter(|v| v.is_finite()),
            symbol_rate: f.symbol_rate,
            fsk_levels: Some(f.fsk_levels).filter(|&n| n >= 2),
            deviation_hz: Some(f.deviation_hz).filter(|v| v.is_finite() && *v > 0.0),
            // Set by the pool, which knows where the look was cut.
            carrier_hz: None,
        };
        vec![Claim::new(d.label, d.confidence.min(MAX_FEATURE_CONFIDENCE), by)]
    }
}

/// A P25 Phase 1 site as frames name it: the NAC by majority over the frames' NIDs, the rest from the control
/// channel's TSBKs (network and RFSS status).
#[derive(Debug, Clone, PartialEq)]
pub struct P25Site {
    pub identity: Identity,
    pub tsbks: usize,
    pub grants: usize,
}

pub fn p25_site(frames: &[Frame]) -> Option<P25Site> {
    let mut nacs: std::collections::BTreeMap<u16, usize> = Default::default();
    for n in frames.iter().filter_map(detect::decode::p25_nid) {
        *nacs.entry(n.nac).or_default() += 1;
    }
    // Two NIDs must agree (DESIGN §7.1's two-confirmation): the NID's BCH code "corrects" a badly hit word to another
    // valid one, and on air single-NID records named NACs no site here uses (0x018, and 0x344 a bit from 0x340).
    let (nac, votes) = nacs.into_iter().max_by_key(|&(nac, n)| (n, std::cmp::Reverse(nac)))?;
    if votes < MIN_NIDS {
        return None;
    }
    let msgs = detect::p25::messages(frames);
    let mut info = detect::p25::SystemInfo::default();
    let grants: usize = msgs.iter().map(|m| info.apply(m).len()).sum();
    Some(P25Site {
        identity: Identity::P25 { nac, wacn: info.wacn, system: info.system, rfss: info.rfss, site: info.site },
        tsbks: msgs.len(),
        grants,
    })
}

/// "NAC 0x34D: TSDU×12, 12 of 14 frames": the identity, then what decoded.
/// Pager protocols: their messages are people's pages.
fn is_pager(protocol: &str) -> bool {
    matches!(protocol, "POCSAG" | "FLEX")
}

/// `message`: end with the newest message decoded. Pages never are: pager traffic is personal and medical, and a claim's
/// summary is stored and shown everywhere the claim is.
fn summary(f: &Fields, identity: Option<&Identity>, message: bool) -> String {
    let kinds: Vec<String> = f.kinds.iter().take(3).map(|(k, n)| format!("{k}×{n}")).collect();
    let mut s = identity.map(|i| i.line()).unwrap_or_default();
    if !kinds.is_empty() {
        if !s.is_empty() {
            s.push_str(": ");
        }
        s.push_str(&kinds.join(", "));
    }
    if f.tried > 0 {
        s.push_str(&format!("{}{} of {} frames", if s.is_empty() { "" } else { ", " }, f.decoded, f.tried));
    }
    if let Some(t) = &f.talk {
        s.push_str(&format!(", {t}"));
    }
    // The newest message decoded (an ACARS block), as the decoder words it.
    if let Some(m) = f.messages.last().filter(|_| message) {
        s.push_str(&format!(" | {m}"));
    }
    s
}
