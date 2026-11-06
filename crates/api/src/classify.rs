//! The detector bank's budget, the decoders' lamps, and the systems they found (spec §4.5, §4.6, §5).

use std::time::SystemTime;

use crate::SourceId;
use crate::records::{EmitterId, Identity, RecordId};

/// Every record with IQ gets at least one look before its IQ leaves the ring; these say how that went (spec §4.5: "the
/// budget is a number on screen").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifyView {
    /// The pool's threads.
    pub workers: usize,
    /// Records looked at.
    pub looked: u64,
    /// Waiting for their window or for a thread.
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
