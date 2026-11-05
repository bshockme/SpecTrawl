//! The slice tier (spec §4.8): every confirmed record's channelized IQ, written to disk as i16 SigMF by a bus consumer
//! at the lowest priority, so a record can be replayed or decoded after its samples have left the ring.
//!
//! A slice starts `pre_roll` before the record (back-filled from the ring, or from the oldest block it still holds) and
//! ends when the record closes, after `max`, at a gap in the stream, or when the pool sheds it. Finished slices are
//! handed back to the record book, which sets the record's `iq_clip`. The directory is kept under `cap_bytes`, oldest
//! slice first; an evicted slice's record says so.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use api::{IqClip, Priority, RecordId};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::channels::{BusError, ChannelBus, ChannelSpec, Consumer, Delivery, Flow, Start};
use crate::sigmf;

#[derive(Debug, Clone)]
pub struct SliceConfig {
    pub dir: PathBuf,
    /// The slice directory is kept under this, oldest slice first.
    pub cap_bytes: u64,
    /// A slice starts this long before its record.
    pub pre_roll: Duration,
    /// Longest slice.
    pub max: Duration,
}

impl SliceConfig {
    pub fn new(dir: impl Into<PathBuf>) -> SliceConfig {
        SliceConfig { dir: dir.into(), cap_bytes: 4 << 30, pre_roll: Duration::from_millis(50), max: Duration::from_secs(120) }
    }
}

/// `<Documents>/SpecTrawl/slices`.
pub fn default_dir() -> PathBuf {
    crate::store::default_path().with_file_name("slices")
}

/// Narrowest slice channel: a 12.5 kHz channel's.
const MIN_SLICE_HZ: f64 = 12.5e3;
/// The slice channel is this much wider than its record (its skirts and drift).
const SLICE_MARGIN: f64 = 1.25;

/// What a record asks for.
#[derive(Debug, Clone)]
pub struct SliceRequest {
    pub record: RecordId,
    pub source: SourceId,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub first_sample: u64,
    /// Where the files go (`<dir>/<day>/<record id>`); the day is the record's start.
    pub t_start: SystemTime,
    pub hw: String,
}

/// Back to the book.
#[derive(Debug, Clone, PartialEq)]
pub enum SliceEvent {
    Done { record: RecordId, clip: IqClip },
    Evicted(RecordId),
}

struct Live {
    channel: crate::channels::ChannelId,
    /// The record's end sample once it closed (`u64::MAX` while open).
    end: Arc<AtomicU64>,
}

pub struct Recorder {
    cfg: SliceConfig,
    bus: Mutex<Weak<ChannelBus>>,
    live: Mutex<HashMap<RecordId, Live>>,
    events: Mutex<Vec<SliceEvent>>,
    /// Finished slices on disk: record → (stem, bytes), oldest id first.
    index: Mutex<BTreeMap<RecordId, (PathBuf, u64)>>,
}

impl Recorder {
    /// Indexes the slices already in `cfg.dir`.
    pub fn new(cfg: SliceConfig) -> Arc<Recorder> {
        let mut index = BTreeMap::new();
        for day in std::fs::read_dir(&cfg.dir).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(day.path()).into_iter().flatten().flatten() {
                let p = f.path();
                let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".sigmf-data")) else { continue };
                let Ok(id) = stem.parse::<u64>() else { continue };
                let bytes = f.metadata().map_or(0, |m| m.len());
                index.insert(RecordId(id), (p.with_file_name(stem), bytes));
            }
        }
        Arc::new(Recorder {
            cfg,
            bus: Mutex::new(Weak::new()),
            live: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            index: Mutex::new(index),
        })
    }

    pub fn attach(&self, bus: &Arc<ChannelBus>) {
        *self.bus.lock() = Arc::downgrade(bus);
    }

    pub fn config(&self) -> &SliceConfig {
        &self.cfg
    }

    /// Bytes of finished slices on disk.
    pub fn bytes(&self) -> u64 {
        self.index.lock().values().map(|v| v.1).sum()
    }

    /// Starts slicing a record. Logged and ignored when the bus refuses.
    pub fn start(self: &Arc<Self>, req: SliceRequest) {
        let Some(bus) = self.bus.lock().upgrade() else { return };
        if self.live.lock().contains_key(&req.record) {
            return;
        }
        let Some(stage) = bus.stage(req.source) else { return };
        let rate = stage.source().segment_at(req.first_sample).sample_rate;
        let pre = (self.cfg.pre_roll.as_secs_f64() * rate) as u64;
        let bandwidth_hz = (req.bandwidth_hz * SLICE_MARGIN).max(MIN_SLICE_HZ);
        // The channelizer picks the rate (at least 1.6 × the bandwidth); this only has to be positive.
        let spec = ChannelSpec { source: req.source, center_hz: req.center_hz, bandwidth_hz, min_rate: bandwidth_hz, parent: None };
        let end = Arc::new(AtomicU64::new(u64::MAX));
        let writer = |start_sample: u64| SliceWriter {
            recorder: Arc::downgrade(self),
            req: req.clone(),
            end: end.clone(),
            max_input: (self.cfg.max.as_secs_f64() * rate) as u64,
            group_delay: stage.fft_size() as u64 / 4,
            start_sample,
            input_rate: rate,
            hop: stage.hop(),
            out: None,
        };
        let want = req.first_sample.saturating_sub(pre);
        let sub = match bus.subscribe(spec.clone(), Start::At(want), Priority::Recorder, Box::new(writer(want))) {
            // The ring no longer holds the record's start: from the oldest block it does hold.
            Err(BusError::NotHeld { oldest }) => {
                let from = oldest + stage.hop();
                bus.subscribe(spec, Start::At(from), Priority::Recorder, Box::new(writer(from)))
            }
            other => other,
        };
        match sub {
            Ok(info) => {
                self.live.lock().insert(req.record, Live { channel: info.id, end });
            }
            Err(e) => log::warn!("record {}: no slice: {e:?}", req.record.0),
        }
    }

    /// The record closed at `end_sample`: its slice ends there.
    pub fn end(&self, record: RecordId, end_sample: u64) {
        if let Some(l) = self.live.lock().get(&record) {
            l.end.store(end_sample, Ordering::Relaxed);
        }
    }

    /// The record is gone (merged into another): its slice stops and its files are removed.
    pub fn cancel(&self, record: RecordId) {
        if let (Some(l), Some(bus)) = (self.live.lock().remove(&record), self.bus.lock().upgrade()) {
            bus.unsubscribe(l.channel);
        }
        if let Some((stem, _)) = self.index.lock().remove(&record) {
            remove(&stem);
        }
    }

    /// Finished and evicted slices since the last call.
    pub fn take_events(&self) -> Vec<SliceEvent> {
        std::mem::take(&mut *self.events.lock())
    }

    /// A slice is finished: index it, keep the directory under the cap, tell the book.
    fn finished(&self, record: RecordId, clip: IqClip, bytes: u64) {
        self.live.lock().remove(&record);
        let mut events = vec![SliceEvent::Done { record, clip: clip.clone() }];
        {
            let mut index = self.index.lock();
            index.insert(record, (clip.path.clone(), bytes));
            let mut total: u64 = index.values().map(|v| v.1).sum();
            while total > self.cfg.cap_bytes {
                let Some((&old, _)) = index.iter().next() else { break };
                let (stem, b) = index.remove(&old).expect("the first entry");
                remove(&stem);
                total -= b;
                events.push(SliceEvent::Evicted(old));
            }
        }
        self.events.lock().extend(events);
    }

    fn stem(&self, req: &SliceRequest) -> PathBuf {
        let day = &sigmf::rfc3339(req.t_start)[..10];
        self.cfg.dir.join(day).join(req.record.0.to_string())
    }
}

fn remove(stem: &Path) {
    for ext in ["sigmf-data", "sigmf-meta"] {
        let _ = std::fs::remove_file(stem.with_extension(ext));
    }
}

/// An open slice file.
struct Out {
    data: BufWriter<File>,
    stem: PathBuf,
    /// The source sample of the slice's first sample (group delay taken off).
    from_sample: u64,
    samples: u64,
    rate: f64,
}

/// The bus consumer writing one slice.
struct SliceWriter {
    recorder: Weak<Recorder>,
    req: SliceRequest,
    end: Arc<AtomicU64>,
    max_input: u64,
    /// The channel filter's delay in input samples (`N/4`): a block's samples show the signal of that much earlier.
    group_delay: u64,
    start_sample: u64,
    input_rate: f64,
    hop: u64,
    out: Option<Out>,
}

impl SliceWriter {
    fn open(&mut self, first_sample: u64, rate: f64) -> std::io::Result<()> {
        let Some(rec) = self.recorder.upgrade() else { return Ok(()) };
        let stem = rec.stem(&self.req);
        if let Some(dir) = stem.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = BufWriter::new(File::create(stem.with_extension("sigmf-data"))?);
        self.out = Some(Out { data, stem, from_sample: first_sample.saturating_sub(self.group_delay), samples: 0, rate });
        Ok(())
    }

    fn write(&mut self, x: &[Complex32]) -> std::io::Result<()> {
        let Some(o) = self.out.as_mut() else { return Ok(()) };
        let mut buf = Vec::with_capacity(x.len() * 4);
        for c in x {
            for v in [c.re, c.im] {
                buf.extend_from_slice(&((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
            }
        }
        o.data.write_all(&buf)?;
        o.samples += x.len() as u64;
        Ok(())
    }

    /// Closes the files and tells the recorder. The record's end is known only once it closed, a hangover after its
    /// last active row, so the slice is cut back to it.
    fn finish(&mut self) {
        let Some(mut o) = self.out.take() else { return };
        let Some(rec) = self.recorder.upgrade() else { return };
        let end = self.end.load(Ordering::Relaxed);
        let decim = self.input_rate / o.rate;
        if end != u64::MAX && decim > 0.0 {
            let keep = ((end.saturating_sub(o.from_sample) as f64 / decim).ceil() as u64).min(o.samples);
            o.samples = keep;
        }
        let cut = o.data.flush().and_then(|()| o.data.get_ref().set_len(o.samples * 4));
        if let Err(e) = cut {
            log::warn!("record {}: slice write failed: {e}", self.req.record.0);
            remove(&o.stem);
            return;
        }
        let meta = sigmf::Meta {
            datatype: "ci16_le",
            sample_rate: o.rate,
            frequency: self.req.center_hz,
            datetime: Some(sigmf::rfc3339(self.req.t_start)),
            hw: self.req.hw.clone(),
            description: format!("record {} slice from source sample {}", self.req.record.0, o.from_sample),
            sample_count: o.samples,
        };
        if let Err(e) = sigmf::write_meta(&o.stem.with_extension("sigmf-meta"), &meta) {
            log::warn!("record {}: slice metadata failed: {e}", self.req.record.0);
        }
        let bytes = o.samples * 4;
        let clip =
            IqClip { path: o.stem.clone(), from_sample: o.from_sample, samples: o.samples, rate: o.rate, center_hz: self.req.center_hz };
        rec.finished(self.req.record, clip, bytes);
    }
}

impl Consumer for SliceWriter {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                if first_sample.saturating_sub(self.group_delay) >= self.end.load(Ordering::Relaxed)
                    || first_sample.saturating_sub(self.start_sample) >= self.max_input
                {
                    self.finish();
                    return Flow::Stop;
                }
                if self.out.is_none() {
                    // The channel's output rate: the input rate × output samples per block ÷ the hop.
                    let rate = self.input_rate * samples.len() as f64 / self.hop as f64;
                    if let Err(e) = self.open(first_sample, rate) {
                        log::warn!("record {}: no slice: {e}", self.req.record.0);
                        return Flow::Stop;
                    }
                }
                if let Err(e) = self.write(samples) {
                    log::warn!("record {}: slice write failed: {e}", self.req.record.0);
                    self.finish();
                    return Flow::Stop;
                }
                Flow::Continue
            }
            // A hole in the stream (the record closed at its segment boundary anyway).
            Delivery::Gap { .. } => {
                self.finish();
                Flow::Stop
            }
            Delivery::Shed => {
                log::info!("record {}: slice shed by a saturated pool; kept as far as it got", self.req.record.0);
                self.finish();
                Flow::Stop
            }
            Delivery::Ended { .. } => {
                self.finish();
                Flow::Stop
            }
        }
    }
}

impl Drop for SliceWriter {
    /// Unsubscribed mid-slice (the record merged away): its files go.
    fn drop(&mut self) {
        if let Some(o) = self.out.take() {
            drop(o.data);
            remove(&o.stem);
        }
    }
}
