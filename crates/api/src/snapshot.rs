//! The immutable view of the engine that interfaces render.

use std::sync::Arc;
use std::time::SystemTime;

use sdr_core::{Capabilities, DeviceInfo, GainSetting};

use crate::audio::AudioView;
use crate::classify::{BandSpan, CatalogueEntry, ChannelsView, ClassifyView, DecoderView, SystemsView, WorkerMode, WorkerView};
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
    /// The audio output and the replays.
    pub audio: Arc<AudioView>,
    /// The detector bank's budget.
    pub classify: ClassifyView,
    /// The running decoders.
    pub decoders: Vec<DecoderView>,
    /// The systems the decoders' claims name (the same `Arc` while nothing changed).
    pub systems: Arc<SystemsView>,
    /// The band-plan channels the records were heard on (air band), built like `systems`.
    pub channels: Arc<ChannelsView>,
    /// The decoder workers running, and the modes the installed workers offer.
    pub workers: Vec<WorkerView>,
    pub worker_modes: Arc<Vec<WorkerMode>>,
    /// Every mode the engine can name or decode (the same `Arc` in every snapshot).
    pub catalogue: Arc<Vec<CatalogueEntry>>,
    /// The band plan's allocations (the same `Arc` in every snapshot).
    pub band_plan: Arc<Vec<BandSpan>>,
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
            audio: Arc::new(AudioView::default()),
            classify: ClassifyView::default(),
            decoders: Vec::new(),
            systems: Arc::new(SystemsView::default()),
            channels: Arc::new(ChannelsView::default()),
            workers: Vec::new(),
            worker_modes: Arc::new(Vec::new()),
            catalogue: Arc::new(Vec::new()),
            band_plan: Arc::new(Vec::new()),
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
    /// The newest finished rows, oldest first (48 at most: 1.1 s of 22.9 ms rows, so a stalled tick loses none). In
    /// Sweep mode each row is one stitched pass over the span.
    pub rows: Vec<Arc<Row>>,
    pub consumers: Vec<ConsumerView>,
    /// The sweep's progress, in Sweep mode.
    pub sweep: Option<SweepView>,
}

/// A sweep's progress (spec §4.6a).
#[derive(Debug, Clone, PartialEq)]
pub struct SweepView {
    pub start_hz: f64,
    /// The end of the hop grid (the rows' axis), measured or not.
    pub stop_hz: f64,
    pub hop_hz: f64,
    pub hops: usize,
    pub measured_hops: usize,
    pub current_hop: usize,
    pub passes: u64,
    pub last_pass_s: f64,
    pub hops_per_sec: f64,
    pub redone_hops: u64,
    pub settle_retries: u64,
    pub slow_hops: usize,
    pub lock_waits: u64,
    /// Retune sweep: retunes that never showed in the stream. Board sweep: times the board went quiet and was started
    /// again.
    pub unseen_retunes: u64,
    /// The device steps through the hops itself (an RFOne with sweep firmware).
    pub on_board: bool,
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
    pub kind: LogKind,
    pub text: String,
}

/// What a log line narrates (spec §5's Log slot), so the log can be filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogKind {
    /// A device opened, closed, lost, or enumerated; the budget changed.
    Device,
    /// A receiver retuned, changed mode or gain, or started a sweep.
    Tune,
    /// The bus shed a channel, or a consumer panicked.
    Bus,
    /// A claim changed an emitter's headline (or named it).
    Headline,
    /// A command failed.
    Error,
}

impl LogKind {
    pub const ALL: [LogKind; 5] = [LogKind::Device, LogKind::Tune, LogKind::Bus, LogKind::Headline, LogKind::Error];

    pub fn name(self) -> &'static str {
        match self {
            LogKind::Device => "Devices",
            LogKind::Tune => "Tuning",
            LogKind::Bus => "Bus",
            LogKind::Headline => "Names",
            LogKind::Error => "Errors",
        }
    }
}
