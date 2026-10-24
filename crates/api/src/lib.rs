//! The contract between the engine and its user interfaces: plain data, a command channel, and formatting helpers.
//!
//! The engine publishes a [`Snapshot`] through a [`Handle`]; interfaces read it and send [`Command`]s back.
//! This crate creates no threads and reads no clocks.

pub mod command;
pub mod fmt;
pub mod handle;
pub mod snapshot;
pub mod timeline;

pub use command::{Ack, Command, CommandReceiver, CommandSender, Outcome, command_channel};
pub use handle::Handle;
pub use sdr_core::{Capabilities, DeviceInfo, GainSetting};
pub use snapshot::{BusView, ConsumerView, DeviceEntry, Health, LogEntry, Receiver, RingView, Snapshot, StageView, Status};
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
