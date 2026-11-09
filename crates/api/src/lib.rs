//! The contract between the engine and its user interfaces: plain data, a command channel, and formatting helpers.
//!
//! A [`Snapshot`] is an immutable picture of everything an interface may show: the devices, the budget, each receiver's
//! state, status line and newest finished waterfall rows, the consumers' lag, and the log. The engine's snapshot thread
//! builds one every 50 ms and publishes it through a [`Handle`]; a published snapshot is never changed.
//!
//! A [`Command`] goes the other way over a channel and is acknowledged, not answered: sending one returns a sequence number,
//! and the outcome appears as an [`Ack`] in a later snapshot, so an interface learns what happened by reading the
//! next picture, never by waiting.
//!
//! An interface takes one `Arc<Snapshot>` per frame and draws from it, sends commands, and holds no engine type, lock,
//! thread or clock. This crate itself creates no threads and reads no clocks.

pub mod audio;
pub mod classify;
pub mod command;
pub mod fmt;
pub mod handle;
pub mod records;
pub mod snapshot;
pub mod timeline;

pub use audio::{AudioMode, AudioView, PlayFrom, PlayId, PlayState, PlayTarget, PlayTo, PlayView};
pub use classify::{
    BandSpan, CatalogueEntry, ChannelRow, ChannelsView, ClassifyView, DecoderView, Domain, Lamp, Reach, SystemRow, SystemsView, WorkerId,
    WorkerMessage, WorkerMode, WorkerView,
};
pub use command::{Ack, Command, CommandReceiver, CommandSender, Outcome, command_channel};
pub use handle::Handle;
pub use records::{
    Claim, EmitterId, EmitterView, Evidence, Identity, IqClip, Note, PositionKind, QueryResult, RecordId, RecordQuery, RecordsView, Role,
    SignalRecord, Standing, StoreView, headline, standing,
};
pub use sdr_core::{Capabilities, DeviceInfo, GainSetting};
pub use snapshot::{
    BusView, ConsumerView, DeviceEntry, Health, LogEntry, LogKind, Receiver, RingView, Snapshot, StageView, Status, SweepView,
};
pub use timeline::{Boundary, Row, Segment, SegmentId};

/// Identifies a source within the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceId(pub u32);

/// How a source is being used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Sweep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChannelId(pub u64);

/// Lowest first (the spec's §4.3 list reversed): a worker always takes the
/// highest-priority subscription that has a block ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Recorder,
    PagerMonitor,
    Classify,
    EmitterDecoder,
    TrunkControl,
    Listen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerState {
    Running,
    OutOfSpan,
    Shed,
    Ended,
}
