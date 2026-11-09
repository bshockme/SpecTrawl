//! Records, the claims made about them, and the emitters they group into (spec §4.4).
//!
//! A [`SignalRecord`] is one emission as the detector saw it: a frequency extent, a stretch of samples inside one segment,
//! its power, and its own list of [`Claim`]s. Every claim says what produced it ([`Evidence`]); an interface styles each
//! kind of evidence differently and never collapses them to one string. An [`EmitterView`]'s headline is computed from
//! its records' claims; a record never borrows a label without saying so.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::SourceId;
use crate::timeline::SegmentId;

/// Stable across restarts: allocated from the store's counter and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RecordId(pub u64);

/// Stable across restarts, like [`RecordId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EmitterId(pub u64);

/// The receiver's job when the record was made (DESIGN §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Live,
    /// From a sweep (step 4a).
    Survey,
}

/// One statement about a record, with what produced it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Claim {
    pub label: String,
    /// 0..1.
    pub confidence: f32,
    pub by: Evidence,
}

/// Inferred from a neighbouring emitter: never more than this sure.
pub const NEIGHBOUR_MAX_CONFIDENCE: f32 = 0.5;

impl Claim {
    pub fn new(label: impl Into<String>, confidence: f32, by: Evidence) -> Claim {
        let cap = if matches!(by, Evidence::Neighbour { .. }) { NEIGHBOUR_MAX_CONFIDENCE } else { 1.0 };
        Claim { label: label.into(), confidence: confidence.clamp(0.0, cap), by }
    }

    /// Decoded from the signal itself (a decoder lock or a sync bank), as opposed to inferred from where it is, what is
    /// next to it, or its shape.
    pub fn is_decoded(&self) -> bool {
        matches!(self.by, Evidence::Decoder { .. } | Evidence::SyncBank { .. })
    }

    /// Inferred from where the signal is or what is next to it, not from the signal itself (spec §5: shown muted, with
    /// the word "inferred").
    pub fn is_inferred(&self) -> bool {
        matches!(self.by, Evidence::BandPlan { .. } | Evidence::Neighbour { .. } | Evidence::Position(_))
    }
}

impl Evidence {
    /// The kind of evidence in a word or two: "decoded", "sync bank", "shape", "band plan", "neighbour", "position".
    pub fn kind(&self) -> &'static str {
        match self {
            Evidence::SyncBank { .. } => "sync bank",
            Evidence::Features { .. } => "shape",
            Evidence::Decoder { .. } => "decoded",
            Evidence::BandPlan { .. } => "band plan",
            Evidence::Neighbour { .. } => "neighbour",
            Evidence::Position(_) => "position",
        }
    }
}

/// What produced a [`Claim`] (spec §4.4).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Evidence {
    /// The `detect` sync bank: syncs found, frames tried and decoded.
    SyncBank { protocol: String, syncs: u32, frames_tried: u32, frames_decoded: u32 },
    /// The record's own shape and power.
    Features { bandwidth_hz: f64, snr_db: f32, duty: f32, envelope_cv: Option<f32> },
    /// A decoder that locked, with its telemetry, and who the signal says it is when it says.
    Decoder {
        mode: String,
        locked: bool,
        summary: String,
        #[cfg_attr(feature = "serde", serde(default))]
        identity: Option<Identity>,
    },
    /// The band plan's entry for the frequency: never a rename.
    BandPlan { entry: String },
    /// Borrowed from a neighbouring emitter (confidence at most [`NEIGHBOUR_MAX_CONFIDENCE`]).
    Neighbour { emitter: EmitterId },
    /// Where it sits relative to the receiver's own artefacts.
    Position(PositionKind),
}

/// Who a decoded signal says it is. Every view of a signal's identity (its record, its emitter, a system row) reads
/// this one value, so they cannot disagree (spec §5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Identity {
    /// A P25 site: the NAC from the frames' NID, the rest from the control channel's broadcasts as they arrive.
    P25 { nac: u16, wacn: Option<u32>, system: Option<u16>, rfss: Option<u8>, site: Option<u8> },
    /// A channel of a band plan's raster, as the band names it: air band "128.775".
    Channel { plan: String, name: String },
    /// Any other decoder's identifier, as it names it (a DCS code, a colour code, a BSIC, an aircraft's registration).
    Other { id: String },
}

impl Identity {
    /// The fields as `(name, value)`, in the order a reader looks for them: "NAC 0x34D", "WACN BEE00", "SYS 348",
    /// "RFSS 1", "SITE 13".
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            Identity::P25 { nac, wacn, system, rfss, site } => {
                let mut v = vec![("NAC", format!("0x{nac:03X}"))];
                v.extend(wacn.map(|w| ("WACN", format!("{w:05X}"))));
                v.extend(system.map(|s| ("SYS", format!("{s:03X}"))));
                v.extend(rfss.map(|r| ("RFSS", r.to_string())));
                v.extend(site.map(|s| ("SITE", s.to_string())));
                v
            }
            Identity::Channel { name, .. } => vec![("CH", name.clone())],
            Identity::Other { id } => vec![("ID", id.clone())],
        }
    }

    /// One line: "NAC 0x34D WACN BEE00 SYS 348 RFSS 1 SITE 13".
    pub fn line(&self) -> String {
        match self {
            Identity::Other { id } => id.clone(),
            Identity::Channel { plan, name } => format!("{plan} {name}"),
            _ => self.fields().iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" "),
        }
    }

    /// Whether `other` can be the same system: equal where both are known (a site seen before its network broadcast
    /// is the same site).
    pub fn same(&self, other: &Identity) -> bool {
        let agree = |a: Option<u32>, b: Option<u32>| a.is_none() || b.is_none() || a == b;
        match (self, other) {
            (
                Identity::P25 { nac: n1, wacn: w1, system: s1, rfss: r1, site: t1 },
                Identity::P25 { nac: n2, wacn: w2, system: s2, rfss: r2, site: t2 },
            ) => {
                n1 == n2
                    && agree(*w1, *w2)
                    && agree(s1.map(u32::from), s2.map(u32::from))
                    && agree(r1.map(u32::from), r2.map(u32::from))
                    && agree(t1.map(u32::from), t2.map(u32::from))
            }
            (Identity::Other { id: a }, Identity::Other { id: b }) => a == b,
            (Identity::Channel { plan: p1, name: n1 }, Identity::Channel { plan: p2, name: n2 }) => p1 == p2 && n1 == n2,
            _ => false,
        }
    }

    /// How many fields are known (a fuller identity replaces a thinner one it is the [`same`](Identity::same) as).
    pub fn known(&self) -> usize {
        self.fields().len()
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PositionKind {
    /// At the tuner's centre.
    Dc,
    /// On the radio's clock-spur grid.
    SpurGrid { grid_hz: f64 },
    /// The mirror image of another emitter about the tuner centre.
    Image { of: EmitterId },
    /// One line of an evenly spaced comb.
    Comb { spacing_hz: f64, lines: u32 },
}

/// A record's channelized IQ on disk (the slice tier, spec §4.8).
#[derive(Debug, Clone, PartialEq)]
pub struct IqClip {
    /// The SigMF pair's stem (`<stem>.sigmf-meta`, `<stem>.sigmf-data`).
    pub path: PathBuf,
    /// The source sample the slice's first sample corresponds to; later than the record's start (minus pre-roll) when
    /// the ring no longer held that.
    pub from_sample: u64,
    pub samples: u64,
    pub rate: f64,
    pub center_hz: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub text: String,
    /// When it was applied: the record's source timeline at its head then (neither the engine nor an interface reads
    /// the wall clock), or the record's end time when its source has gone.
    pub time: SystemTime,
}

/// One emission, as the detector saw it.
#[derive(Clone, PartialEq)]
pub struct SignalRecord {
    pub id: RecordId,
    pub source: SourceId,
    pub serial: Arc<str>,
    pub role: Role,
    pub segment: SegmentId,
    pub tuner_hz: f64,
    /// The first sample of the first row that saw it, and the end of the last.
    pub first_sample: u64,
    pub end_sample: u64,
    /// From the timeline (the nearest time anchor).
    pub t_start: SystemTime,
    pub t_end: SystemTime,
    /// Occupied extent.
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    /// The detector's bin width: the resolution of the extent.
    pub bin_hz: f64,
    pub peak_dbfs: f32,
    /// Mean over rows of the mean power across the occupied bins.
    pub mean_dbfs: f32,
    pub mean_snr_db: f32,
    pub max_snr_db: f32,
    pub rows: u32,
    /// Still being extended.
    pub open: bool,
    pub claims: Vec<Claim>,
    pub emitter: Option<EmitterId>,
    pub tags: Vec<String>,
    pub notes: Vec<Note>,
    pub iq_clip: Option<IqClip>,
    /// It had a slice, removed to keep the slice tier under its disk cap.
    pub slice_evicted: bool,
}

impl SignalRecord {
    pub fn center_hz(&self) -> f64 {
        (self.f_lo_hz + self.f_hi_hz) / 2.0
    }

    pub fn bandwidth_hz(&self) -> f64 {
        self.f_hi_hz - self.f_lo_hz
    }

    pub fn duration(&self) -> Duration {
        self.t_end.duration_since(self.t_start).unwrap_or_default()
    }

    /// What to headline (spec §4.5): the most confident decoder claim, else the most confident sync-bank claim, else a
    /// measured analog modulation ([`is_analog_shape`]), else nothing, which an interface shows as "unknown". A digital
    /// mode is never named from its shape alone: it needs a sync word or a decode.
    pub fn headline(&self) -> Option<&Claim> {
        headline(&self.claims)
    }
}

/// The headline of a set of claims; see [`SignalRecord::headline`].
pub fn headline(claims: &[Claim]) -> Option<&Claim> {
    let best = |keep: &dyn Fn(&Claim) -> bool| claims.iter().filter(|c| keep(c)).max_by(|a, b| a.confidence.total_cmp(&b.confidence));
    best(&|c| matches!(c.by, Evidence::Decoder { .. }))
        .or_else(|| best(&|c| matches!(c.by, Evidence::SyncBank { .. })))
        .or_else(|| best(&is_analog_shape))
}

/// Analog modulations a shape measurement may name outright (owner, 2026-11-06): AM and analog FM carry no code
/// words, and the shape rules measure them positively. `detect`'s rules give 0.6 where they measured the content (a
/// carrier with voice-band sidebands, one steady tone, a broadcast's 19 kHz pilot) and 0.5 for FM read from its
/// deviation alone, which an unnamed digital mode can show too; the `Features` cap is 0.6.
pub fn is_analog_shape(c: &Claim) -> bool {
    matches!(c.by, Evidence::Features { .. })
        && c.confidence >= ANALOG_SHAPE_CONFIDENCE
        && (c.label == "AM" || c.label == "Analog FM" || c.label.starts_with("FM broadcast"))
}

/// Least confidence of an analog shape that headlines (see [`is_analog_shape`]).
pub const ANALOG_SHAPE_CONFIDENCE: f32 = 0.599;

/// Short: claims, tags and notes are counted, not printed.
impl std::fmt::Debug for SignalRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalRecord")
            .field("id", &self.id.0)
            .field("serial", &self.serial)
            .field("samples", &(self.first_sample..self.end_sample))
            .field("mhz", &format_args!("{:.4}", self.center_hz() / 1e6))
            .field("khz", &format_args!("{:.1}", self.bandwidth_hz() / 1e3))
            .field("max_snr_db", &self.max_snr_db)
            .field("rows", &self.rows)
            .field("open", &self.open)
            .field("headline", &self.headline().map(|c| c.label.as_str()))
            .field("claims", &self.claims.len())
            .field("emitter", &self.emitter.map(|e| e.0))
            .field("tags", &self.tags.len())
            .field("notes", &self.notes.len())
            .field("iq_clip", &self.iq_clip.is_some())
            .finish()
    }
}

/// An emitter: records grouped by frequency on one device.
#[derive(Debug, Clone, PartialEq)]
pub struct EmitterView {
    pub id: EmitterId,
    pub serial: Arc<str>,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub records: u32,
    pub on_time_s: f64,
    /// Share of the time since first seen that it was on the air.
    pub duty: f32,
    pub confirmed: bool,
    /// From its records' claims (see [`headline`]).
    pub headline: Option<Claim>,
}

/// The records an interface can show without asking the store: the open ones, then the newest closed ones; and the
/// recently seen emitters. Rebuilt by the engine only when something changed, so consecutive snapshots often share it.
#[derive(Debug, Clone, Default)]
pub struct RecordsView {
    /// Rises with every change.
    pub version: u64,
    /// Open records first, then closed ones, newest first.
    pub records: Vec<Arc<SignalRecord>>,
    pub emitters: Vec<Arc<EmitterView>>,
}

/// The store's state, as shown to the user.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreView {
    pub path: PathBuf,
    /// Operations written since the engine started.
    pub written: u64,
    pub queued: usize,
    /// Operations dropped because the queue was full.
    pub dropped: u64,
    pub error: Option<String>,
}

/// A history search, answered by the store into a later snapshot. Every field narrows; `None` doesn't.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordQuery {
    pub serial: Option<String>,
    pub from: Option<SystemTime>,
    pub to: Option<SystemTime>,
    pub f_lo_hz: Option<f64>,
    pub f_hi_hz: Option<f64>,
    /// Words that must appear in a claim label, a tag or a note.
    pub text: Option<String>,
    pub tag: Option<String>,
    /// At most this many, newest first (capped at 5000).
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    /// The `QueryRecords` command's sequence number.
    pub seq: u64,
    pub records: Vec<Arc<SignalRecord>>,
    /// More matched than `limit`.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digital_modes_need_a_sync_or_a_decode_and_measured_analog_headlines() {
        let shape = |label: &str, confidence: f32| {
            Claim::new(label, confidence, Evidence::Features { bandwidth_hz: 12.5e3, snr_db: 20.0, duty: 1.0, envelope_cv: None })
        };
        // A digital shape names nothing on its own, however confident.
        assert_eq!(headline(&[shape("4FSK", 0.6), shape("2FSK 2400 Bd", 0.6), shape("QPSK", 0.6)]), None);
        // Analog FM read from its deviation alone is not a measurement of analog content.
        assert_eq!(headline(&[shape("Analog FM", 0.5)]), None);
        for label in ["AM", "Analog FM", "FM broadcast (stereo, RDS)"] {
            assert_eq!(headline(&[shape(label, 0.6)]).map(|c| c.label.as_str()), Some(label));
        }
        // A sync word outranks any shape.
        let sync = Claim::new(
            "DMR BS data",
            0.9,
            Evidence::SyncBank { protocol: "DMR BS data".into(), syncs: 3, frames_tried: 0, frames_decoded: 0 },
        );
        assert_eq!(headline(&[shape("Analog FM", 0.6), sync.clone()]), Some(&sync));
    }

    #[test]
    fn an_identity_reads_as_one_line_and_matches_a_thinner_one_of_the_same_site() {
        let full = Identity::P25 { nac: 0x34D, wacn: Some(0xBEE00), system: Some(0x348), rfss: Some(1), site: Some(13) };
        assert_eq!(full.line(), "NAC 0x34D WACN BEE00 SYS 348 RFSS 1 SITE 13");
        let thin = Identity::P25 { nac: 0x34D, wacn: None, system: None, rfss: None, site: None };
        assert!(full.same(&thin) && thin.same(&full));
        assert!(full.known() > thin.known());
        let other_site = Identity::P25 { nac: 0x34D, wacn: Some(0xBEE00), system: Some(0x348), rfss: Some(1), site: Some(14) };
        assert!(!full.same(&other_site));
        assert!(!thin.same(&Identity::P25 { nac: 0x340, wacn: None, system: None, rfss: None, site: None }));
    }

    fn record(claims: Vec<Claim>) -> SignalRecord {
        SignalRecord {
            id: RecordId(7),
            source: SourceId(1),
            serial: "SIM-0001".into(),
            role: Role::Live,
            segment: SegmentId(1),
            tuner_hz: 855.25e6,
            first_sample: 0,
            end_sample: 1000,
            t_start: SystemTime::UNIX_EPOCH,
            t_end: SystemTime::UNIX_EPOCH + Duration::from_millis(100),
            f_lo_hz: 851.0e6,
            f_hi_hz: 851.0125e6,
            bin_hz: 610.0,
            peak_dbfs: -40.0,
            mean_dbfs: -45.0,
            mean_snr_db: 30.0,
            max_snr_db: 35.0,
            rows: 5,
            open: false,
            claims,
            emitter: None,
            tags: vec![],
            notes: vec![],
            iq_clip: None,
            slice_evicted: false,
        }
    }

    fn sync(label: &str, c: f32) -> Claim {
        Claim::new(label, c, Evidence::SyncBank { protocol: label.into(), syncs: 3, frames_tried: 2, frames_decoded: 2 })
    }

    #[test]
    fn the_headline_prefers_a_decoder_then_a_sync_bank_and_is_otherwise_unknown() {
        let shape = Claim::new("narrowband", 0.9, Evidence::Features { bandwidth_hz: 12.5e3, snr_db: 30.0, duty: 1.0, envelope_cv: None });
        assert_eq!(record(vec![shape.clone()]).headline(), None);
        let r = record(vec![shape.clone(), sync("DMR", 0.6), sync("P25", 0.8)]);
        assert_eq!(r.headline().unwrap().label, "P25");
        let dec = Claim::new(
            "P25 Phase 1",
            0.4,
            Evidence::Decoder { mode: "P25".into(), locked: true, summary: "NAC 0x340".into(), identity: None },
        );
        let r = record(vec![shape, sync("P25", 0.9), dec]);
        assert_eq!(r.headline().unwrap().label, "P25 Phase 1", "a decoder lock beats a more confident sync");
        assert!(r.headline().unwrap().is_decoded());
    }

    #[test]
    fn a_neighbours_claim_is_never_more_than_half_sure() {
        let c = Claim::new("P25", 0.95, Evidence::Neighbour { emitter: EmitterId(3) });
        assert_eq!(c.confidence, NEIGHBOUR_MAX_CONFIDENCE);
        assert!(!c.is_decoded());
        assert_eq!(Claim::new("x", 1.7, Evidence::BandPlan { entry: "x".into() }).confidence, 1.0);
    }

    #[test]
    fn a_records_debug_is_short() {
        let r = record((0..50).map(|k| sync(&format!("P{k}"), 0.5)).collect());
        let text = format!("{r:?}");
        assert!(text.contains("claims: 50"), "{text}");
        assert!(!text.contains("frames_tried"), "{text}");
        assert!(text.len() < 400, "{} chars: {text}", text.len());
    }
}
