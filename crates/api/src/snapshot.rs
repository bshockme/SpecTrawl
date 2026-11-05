//! The immutable view of the engine that interfaces render.

use std::sync::Arc;
use std::time::SystemTime;

use sdr_core::{Capabilities, DeviceInfo, GainSetting};

use crate::command::Ack;
use crate::records::{QueryResult, RecordsView, StoreView};
use crate::timeline::{Row, SegmentId};
use crate::{ChannelId, ConsumerState, Mode, Priority, SourceId};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: u64,
    pub receivers: Vec<Receiver>,
    pub devices: Vec<DeviceEntry>,
    pub bus: BusView,
    pub acks: Vec<Ack>,
    pub log: Vec<Arc<LogEntry>>,
    /// Open and recent records and emitters (the same `Arc` while nothing changed).
    pub records: Arc<RecordsView>,
    /// `None` when the engine runs without a store.
    pub store: Option<StoreView>,
    /// The latest history search's answer.
    pub query: Option<Arc<QueryResult>>,
}

impl Snapshot {
    /// Version 0, everything empty.
    pub fn empty() -> Snapshot {
        Snapshot {
            version: 0,
            receivers: Vec::new(),
            devices: Vec::new(),
            bus: BusView { workers: 0, saturated: false, shed_total: 0, panicked_total: 0 },
            acks: Vec::new(),
            log: Vec::new(),
            records: Arc::new(RecordsView::default()),
            store: None,
            query: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceEntry {
    pub info: DeviceInfo,
    pub open_as: Option<SourceId>,
}

#[derive(Debug, Clone)]
pub struct Receiver {
    pub id: SourceId,
    pub letter: char,
    pub device: DeviceInfo,
    pub caps: Arc<Capabilities>,
    pub mode: Mode,
    pub center_hz: f64,
    pub sample_rate: f64,
    pub segment: SegmentId,
    pub gain: Option<GainSetting>,
    pub status: Status,
    pub ring: RingView,
    pub stage: StageView,
    pub head: u64,
    pub head_time: Option<SystemTime>,
    /// The newest finished rows, oldest first (48 at most: 1.1 s of 22.9 ms rows, so a stalled tick loses none).
    pub rows: Vec<Arc<Row>>,
    pub consumers: Vec<ConsumerView>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Health {
    Ok,
    Warning,
    Lost,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub health: Health,
    pub text: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RingView {
    pub held_secs: f64,
    pub guaranteed_secs: f64,
    pub ceiling_secs: f64,
    pub device_gaps: u64,
    pub starved_samples: u64,
    pub overloads: u64,
    pub device_lost: bool,
}

#[derive(Debug, Clone)]
pub struct StageView {
    pub blocks: u64,
    pub skipped_blocks: u64,
    pub block_micros: f64,
    pub behind_secs: f64,
}

#[derive(Debug, Clone)]
pub struct ConsumerView {
    pub id: ChannelId,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub priority: Priority,
    pub lag_secs: f64,
    pub state: ConsumerState,
}

#[derive(Debug, Clone)]
pub struct BusView {
    pub workers: usize,
    pub saturated: bool,
    pub shed_total: u64,
    pub panicked_total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub n: u64,
    pub receiver: Option<char>,
    pub text: String,
}
