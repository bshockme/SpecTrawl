//! The detector bank's budget, the decoders' lamps, and the systems they found (spec §4.5, §4.6, §5).

use std::time::SystemTime;

use crate::records::{EmitterId, Identity, RecordId};
use crate::{ChannelId, SourceId};

/// Every record with IQ gets at least one look before its IQ leaves the ring; these say how that went (spec §4.5: "the
/// budget is a number on screen").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifyView {
    /// The pool's threads.
    pub workers: usize,
    /// Records looked at.
    pub looked: u64,
    /// Waiting for their window or for a thread, or being looked at.
    pub waiting: usize,
    /// Never looked at: their IQ had left the ring (or the source had gone) before the window could be cut.
    pub missed: u64,
    /// Dropped because the queue was full.
    pub dropped: u64,
    /// Survey records: a sweep keeps no IQ to look at.
    pub survey: u64,
    /// Seconds of IQ the looks took, summed, and the time they took, summed: their ratio is the pool's load.
    pub looked_secs: f64,
    pub work_secs: f64,
}

/// A decoder's state, as a lamp (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lamp {
    Searching,
    Syncing,
    Locked,
}

/// One running decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct DecoderView {
    pub source: SourceId,
    /// The emitter it is attached to.
    pub emitter: EmitterId,
    pub mode: String,
    pub center_hz: f64,
    pub lamp: Lamp,
    /// One sentence: "P25 control 859.7677 MHz, NAC 0x34D: 212 TSBKs, 14 grants".
    pub summary: String,
    /// Frames that decoded, and frames tried that didn't.
    pub blocks_ok: u64,
    pub blocks_bad: u64,
    /// The power of the channel it is fed, dBFS, smoothed over about a second; `None` before its first samples.
    pub level_dbfs: Option<f32>,
    /// The bus channel feeding it (its lag is that consumer's), while it has one.
    pub channel: Option<ChannelId>,
    /// Its newest calls or pages, one line each, newest last ("1234567 (A): text"; the part before ": " is who).
    pub recent: Vec<String>,
}

/// A trunked system, built from records' decoder claims (and nothing else), so it reads the same [`Identity`] as the
/// record and the emitter that carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemRow {
    pub identity: Identity,
    /// "P25 control".
    pub mode: String,
    pub source: SourceId,
    /// The control channel's emitter and its frequency.
    pub emitter: EmitterId,
    pub control_hz: f64,
    /// The records whose claims say this, newest last.
    pub records: Vec<RecordId>,
    /// The newest claim's summary.
    pub summary: String,
    /// When the newest of those records was last seen.
    pub last_seen: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemsView {
    pub systems: Vec<SystemRow>,
}

/// A channel of a band plan's raster that something was heard on (spec §5's CHANNELS: air band first), built from the
/// records' claims and nothing else, like [`SystemRow`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRow {
    /// "Air band", "Military air".
    pub plan: String,
    /// "128.775".
    pub name: String,
    pub hz: f64,
    /// The source of its newest record.
    pub source: SourceId,
    /// A record on it is open: someone is transmitting now.
    pub open: bool,
    /// Records on it: AM voice, decoded data (ACARS), and a carrier with nothing measured on it (the gaps in a call).
    pub voice: u32,
    pub data: u32,
    pub carrier: u32,
    /// What its decoders said, newest first, one per mode ("ACARS").
    pub modes: Vec<String>,
    /// The newest decoded summary ("N365NB NW2320 H1: …"), else "AM voice".
    pub summary: String,
    /// Seconds on the air over its records.
    pub airtime_s: f64,
    pub peak_dbfs: f32,
    pub first_heard: SystemTime,
    pub last_heard: SystemTime,
    /// Its records, oldest first.
    pub records: Vec<RecordId>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelsView {
    /// By frequency.
    pub channels: Vec<ChannelRow>,
}

/// A decoder worker's id, unique for the engine's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(pub u64);

/// A mode a worker program offers (the catalogue, spec §5).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerMode {
    /// The worker's own id for it ("flex", "airband").
    pub id: String,
    pub name: String,
    /// Which program serves it ("OnAir 3.6.0").
    pub by: String,
    /// The channel rate it is served at, S/s.
    pub rate: f64,
}

/// One message a worker decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerMessage {
    /// The worker's time for it.
    pub time: SystemTime,
    pub freq_hz: f64,
    /// The worker's kind ("pager.message").
    pub kind: String,
    /// One line.
    pub text: String,
}

/// One running decoder worker.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerView {
    pub id: WorkerId,
    pub source: SourceId,
    pub mode: String,
    pub by: String,
    pub center_hz: f64,
    pub rate: f64,
    pub lamp: Lamp,
    /// One sentence.
    pub summary: String,
    pub blocks_ok: u64,
    pub blocks_bad: u64,
    /// The newest messages, oldest first.
    pub messages: Vec<WorkerMessage>,
    /// Why it is not running, when it is not (it is restarted with a backoff).
    pub error: Option<String>,
    /// Times the process was restarted.
    pub restarts: u32,
}

/// Where a mode belongs in the catalogue (spec §5: modes grouped by domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Domain {
    Trunking,
    LandMobile,
    Aviation,
    Pagers,
    Broadcast,
    Cellular,
    Other,
}

impl Domain {
    pub fn name(self) -> &'static str {
        match self {
            Domain::Trunking => "Trunking",
            Domain::LandMobile => "Land mobile",
            Domain::Aviation => "Aviation",
            Domain::Pagers => "Pagers",
            Domain::Broadcast => "Broadcast",
            Domain::Cellular => "Cellular",
            Domain::Other => "Other",
        }
    }
}

/// How far SpecTrawl gets with a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    /// A decoder reads its frames: identities, messages, calls.
    Decodes,
    /// Its sync words name it; its frames aren't read.
    Names,
    /// Recognised from its shape alone (AM, analog FM, chirps).
    Shape,
}

impl Reach {
    pub fn name(self) -> &'static str {
        match self {
            Reach::Decodes => "decodes",
            Reach::Names => "names by sync",
            Reach::Shape => "by shape",
        }
    }
}

/// One mode the engine can name or decode (spec §5's catalogue, built from the decoder registry).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogueEntry {
    pub mode: &'static str,
    pub domain: Domain,
    pub reach: Reach,
    /// One line: what it is and what SpecTrawl reads from it.
    pub blurb: &'static str,
    /// Claim labels that are this mode: a claim whose label starts with one of them counts for it.
    pub labels: &'static [&'static str],
}

impl CatalogueEntry {
    /// Whether a claim label is this mode.
    pub fn matches(&self, label: &str) -> bool {
        self.labels.iter().any(|p| label.starts_with(p))
    }
}

/// One band-plan allocation, for the band strip on a receiver's frequency axis (spec §4.5's band plan; the guide's
/// O14). It says what a frequency is allocated to, never what a signal there is.
#[derive(Debug, Clone, PartialEq)]
pub struct BandSpan {
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// "Air band", "800 MHz SMR/public safety".
    pub name: &'static str,
    /// The kind of service: "Aviation voice", "Trunked radio".
    pub service: &'static str,
}
