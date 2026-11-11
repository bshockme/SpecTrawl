//! The slice tier (spec §4.8): every confirmed record's channelized IQ, written to disk as cf32 SigMF by a bus consumer
//! at the lowest priority, so a record can be replayed or decoded after its samples have left the ring.
//!
//! A slice starts `pre_roll` before the record (back-filled from the ring, or from the oldest block it still holds) and
//! ends `post_roll` after the record closes, after `max`, at a gap in the stream, or when the pool sheds it. Finished
//! slices are handed back to the record book, which sets the record's `iq_clip`. The directory is kept under
//! `cap_bytes`, oldest slice first; an evicted slice's record says so. A slice the owner saves (Save IQ) is copied to
//! `saved_dir`, outside the cap, and kept until the owner deletes it.
//!
//! A slice is cut where the record's first rows put it, which can be kHz from where the record ends up: those rows
//! see only part of the signal. So it is collected [`COLLECT_SLACK_HZ`] wider each side, and once the record closes
//! the recorder's finisher thread re-centres it on the record's final centre and narrows it to the record's final
//! width, at the same rate and sample for sample.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use api::{IqClip, Priority, RecordId};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::channels::{BusError, ChannelBus, ChannelId, ChannelSpec, Consumer, Delivery, Flow, Start};
use crate::sigmf;

#[derive(Debug, Clone)]
pub struct SliceConfig {
    pub dir: PathBuf,
    /// The slice directory is kept under this, oldest slice first.
    pub cap_bytes: u64,
    /// A slice starts this long before its record.
    pub pre_roll: Duration,
    /// A slice ends this long after its record.
    pub post_roll: Duration,
    /// Longest slice.
    pub max: Duration,
    /// Saved slices (`record-<id>.sigmf-*`), outside the cap.
    pub saved_dir: PathBuf,
}

impl SliceConfig {
    pub fn new(dir: impl Into<PathBuf>) -> SliceConfig {
        // The looks' lead (a record opens once its rows clear the threshold, after a transmission's start: a short FLEX
        // frame's only header is its first 80 ms), at both ends.
        let dir: PathBuf = dir.into();
        SliceConfig {
            saved_dir: dir.with_file_name("saved"),
            dir,
            cap_bytes: 4 << 30,
            pre_roll: crate::classify::LEAD,
            post_roll: crate::classify::LEAD,
            max: Duration::from_secs(120),
        }
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
/// A slice is collected this much wider each side than its record's first rows ask for, so that re-centred on the
/// record's final centre it still holds the record's final width. On air (931 MHz FLEX pagers on the R2, 2026-11-07)
/// the first rows sat 4–5 kHz off a 15 kHz signal's centre and saw 5 kHz of it; the 12.5 kHz slices cut there held
/// one side of the signal, and 1 of 39 decoded. ±10 kHz around a centre 5 kHz away needs this much past ±6.25 kHz.
const COLLECT_SLACK_HZ: f64 = 12e3;

/// A slice's passband for a record `bw` wide.
fn slice_width(bw: f64) -> f64 {
    (bw * SLICE_MARGIN).max(MIN_SLICE_HZ)
}

/// Slices are stored as floats: i16 at full scale left a weak channel, whose level after channelizing is far below the
/// ADC's, with a few bits (a 931 MHz pager peaked at 10–12 counts). Twice the disk of i16 (owner, 2026-11-07: quality
/// first).
pub const FORMAT: sigmf::Format = sigmf::Format::Cf32;

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

/// A slice in progress: from its start until its finisher has handed it to the book. A slice whose entry is gone by
/// then was cancelled.
struct Live {
    /// `None` until the bus has taken the subscription.
    channel: Option<ChannelId>,
    ending: Arc<Ending>,
}

/// How the record ended, set when it closed.
struct Ending {
    /// Where the slice ends: the record's end plus the post-roll (`u64::MAX` while the record is open).
    end: AtomicU64,
    /// The post-roll in source samples.
    post: u64,
    /// The record's centre and width: as it now stands while it is open, its final ones once it closed.
    shape: Mutex<Option<(f64, f64)>>,
}

/// A written slice, for the finisher thread.
struct Finish {
    record: RecordId,
    stem: PathBuf,
    from_sample: u64,
    samples: u64,
    rate: f64,
    /// The slice channel's centre and passband as collected.
    center_hz: f64,
    collected_hz: f64,
    /// The record's final centre and width, or the latest while it was open (`None` if no row gave one).
    shape: Option<(f64, f64)>,
    t_start: SystemTime,
    hw: String,
}

pub struct Recorder {
    cfg: SliceConfig,
    bus: Mutex<Weak<ChannelBus>>,
    live: Mutex<HashMap<RecordId, Live>>,
    events: Mutex<Vec<SliceEvent>>,
    /// Finished slices on disk: record → (stem, bytes), oldest id first.
    index: Mutex<BTreeMap<RecordId, (PathBuf, u64)>>,
    /// Records saved while their slice was still being written: copied out as it finishes.
    to_save: Mutex<HashSet<RecordId>>,
    /// Written slices to the finisher thread, which re-centres them off the bus's workers.
    jobs: Mutex<Option<mpsc::Sender<Finish>>>,
    finisher: Mutex<Option<JoinHandle<()>>>,
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
        let (tx, rx) = mpsc::channel::<Finish>();
        Arc::new_cyclic(|weak: &Weak<Recorder>| {
            let recorder = weak.clone();
            let finisher = std::thread::Builder::new()
                .name("slice-finisher".into())
                .spawn(move || {
                    for job in rx {
                        let done = job.write();
                        // Gone (the engine is closing): the files are whole, and indexed when the next engine starts.
                        if let Some(rec) = recorder.upgrade() {
                            rec.completed(&job, done);
                        }
                    }
                })
                .expect("spawn the slice finisher");
            Recorder {
                cfg,
                bus: Mutex::new(Weak::new()),
                live: Mutex::new(HashMap::new()),
                events: Mutex::new(Vec::new()),
                index: Mutex::new(index),
                to_save: Mutex::new(HashSet::new()),
                jobs: Mutex::new(Some(tx)),
                finisher: Mutex::new(Some(finisher)),
            }
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
        let Some(stage) = bus.stage(req.source) else { return };
        let rate = stage.source().segment_at(req.first_sample).sample_rate;
        let ending = Arc::new(Ending {
            end: AtomicU64::new(u64::MAX),
            post: (self.cfg.post_roll.as_secs_f64() * rate) as u64,
            shape: Mutex::new(None),
        });
        {
            let mut live = self.live.lock();
            if live.contains_key(&req.record) {
                return;
            }
            live.insert(req.record, Live { channel: None, ending: ending.clone() });
        }
        let pre = (self.cfg.pre_roll.as_secs_f64() * rate) as u64;
        let writer = |start_sample: u64, collected_hz: f64| SliceWriter {
            recorder: Arc::downgrade(self),
            req: req.clone(),
            ending: ending.clone(),
            max_input: (self.cfg.max.as_secs_f64() * rate) as u64,
            group_delay: stage.fft_size() as u64 / 4,
            start_sample,
            input_rate: rate,
            hop: stage.hop(),
            collected_hz,
            out: None,
        };
        let want = req.first_sample.saturating_sub(pre);
        let width = slice_width(req.bandwidth_hz);
        let mut sub = Err(BusError::NoSuchSource);
        // With slack for the centre to move; at a span's edge, where the bus may refuse the wider channel, as the
        // record's first rows stand.
        for w in [width + 2.0 * COLLECT_SLACK_HZ, width] {
            // The channelizer picks the rate (at least 1.6 × the bandwidth); this only has to be positive.
            let spec = ChannelSpec { source: req.source, center_hz: req.center_hz, bandwidth_hz: w, min_rate: w, parent: None };
            sub = match bus.subscribe(spec.clone(), Start::At(want), Priority::Recorder, Box::new(writer(want, w))) {
                // The ring no longer holds the record's start: from the oldest block it does hold.
                Err(BusError::NotHeld { oldest }) => {
                    let from = oldest + stage.hop();
                    bus.subscribe(spec, Start::At(from), Priority::Recorder, Box::new(writer(from, w)))
                }
                other => other,
            };
            if !matches!(sub, Err(BusError::Design(_))) {
                break;
            }
        }
        match sub {
            Ok(info) => {
                if let Some(l) = self.live.lock().get_mut(&req.record) {
                    l.channel = Some(info.id);
                }
            }
            Err(e) => {
                self.live.lock().remove(&req.record);
                log::warn!("record {}: no slice: {e:?}", req.record.0);
            }
        }
    }

    /// Open records' (id, centre, width) as they now stand.
    pub fn shapes(&self, shapes: &[(RecordId, f64, f64)]) {
        if shapes.is_empty() {
            return;
        }
        let live = self.live.lock();
        for &(record, center_hz, bandwidth_hz) in shapes {
            if let Some(l) = live.get(&record) {
                *l.ending.shape.lock() = Some((center_hz, bandwidth_hz));
            }
        }
    }

    /// The record closed at `end_sample`, centred at `center_hz` and `bandwidth_hz` wide: its slice ends a post-roll
    /// later and is re-centred on it.
    pub fn end(&self, record: RecordId, end_sample: u64, center_hz: f64, bandwidth_hz: f64) {
        if let Some(l) = self.live.lock().get(&record) {
            *l.ending.shape.lock() = Some((center_hz, bandwidth_hz));
            l.ending.end.store(end_sample.saturating_add(l.ending.post), Ordering::Release);
        }
    }

    /// The record is gone (merged into another): its slice stops and its files are removed (by the finisher, if the
    /// slice is already with it).
    pub fn cancel(&self, record: RecordId) {
        let channel = self.live.lock().remove(&record).and_then(|l| l.channel);
        if let (Some(ch), Some(bus)) = (channel, self.bus.lock().upgrade()) {
            bus.unsubscribe(ch);
        }
        if let Some((stem, _)) = self.index.lock().remove(&record) {
            remove(&stem);
        }
    }

    /// Where a record's saved slice goes (a SigMF stem).
    pub fn saved_stem(&self, record: RecordId) -> PathBuf {
        self.cfg.saved_dir.join(format!("record-{}", record.0))
    }

    /// Save IQ: copies the record's slice out of the capped directory, now if it is written, else as soon as it is.
    /// The saved stem, and whether the copy is still to come; `Err` if the record has no slice, here or coming.
    pub fn save(&self, record: RecordId) -> Result<(PathBuf, bool), std::io::Error> {
        let stem = self.saved_stem(record);
        // The index's lock is held while a slice moves from `live` to the index (see `completed`).
        let index = self.index.lock();
        if let Some((from, _)) = index.get(&record) {
            copy_slice(from, &stem)?;
            return Ok((stem, false));
        }
        if self.live.lock().contains_key(&record) {
            self.to_save.lock().insert(record);
            return Ok((stem, true));
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no slice"))
    }

    /// Hands a written slice to the finisher thread (or finishes it here once the thread has stopped).
    fn submit(&self, job: Finish) {
        let job = match self.jobs.lock().as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let done = job.write();
        self.completed(&job, done);
    }

    /// The finisher is done with a slice: to the book, unless its record was cancelled meanwhile.
    fn completed(&self, job: &Finish, done: std::io::Result<(IqClip, u64)>) {
        // Held until the slice is indexed, so a save in between finds it in one or the other.
        let mut index = self.index.lock();
        if self.live.lock().remove(&job.record).is_none() {
            drop(index);
            remove(&job.stem);
            return;
        }
        match done {
            Ok((clip, bytes)) => self.finished(&mut index, job.record, clip, bytes),
            Err(e) => {
                log::warn!("record {}: slice not finished: {e}", job.record.0);
                remove(&job.stem);
            }
        }
    }

    /// A slice that never got going: forgotten.
    fn abandon(&self, record: RecordId) {
        self.live.lock().remove(&record);
    }

    /// Finished and evicted slices since the last call.
    pub fn take_events(&self) -> Vec<SliceEvent> {
        std::mem::take(&mut *self.events.lock())
    }

    /// A slice is finished: copy it out if it was saved, index it, keep the directory under the cap, tell the book.
    fn finished(&self, index: &mut BTreeMap<RecordId, (PathBuf, u64)>, record: RecordId, clip: IqClip, bytes: u64) {
        if self.to_save.lock().remove(&record)
            && let Err(e) = copy_slice(&clip.path, &self.saved_stem(record))
        {
            log::warn!("record {}: saving its slice failed: {e}", record.0);
        }
        let mut events = vec![SliceEvent::Done { record, clip: clip.clone() }];
        index.insert(record, (clip.path.clone(), bytes));
        let mut total: u64 = index.values().map(|v| v.1).sum();
        while total > self.cfg.cap_bytes {
            let Some((&old, _)) = index.iter().next() else { break };
            let (stem, b) = index.remove(&old).expect("the first entry");
            remove(&stem);
            total -= b;
            events.push(SliceEvent::Evicted(old));
        }
        self.events.lock().extend(events);
    }

    fn stem(&self, req: &SliceRequest) -> PathBuf {
        let day = &sigmf::rfc3339(req.t_start)[..10];
        self.cfg.dir.join(day).join(req.record.0.to_string())
    }
}

impl Drop for Recorder {
    /// Lets the finisher finish what it has. It may be the one dropping the last reference, and then it is not waited for
    /// (it ends once the queue is closed).
    fn drop(&mut self) {
        drop(self.jobs.lock().take());
        if let Some(h) = self.finisher.lock().take()
            && h.thread().id() != std::thread::current().id()
        {
            let _ = h.join();
        }
    }
}

impl Finish {
    /// Re-centres the data file on the record's final centre and narrows it to its final width, then writes the
    /// metadata: the clip and its bytes.
    fn write(&self) -> std::io::Result<(IqClip, u64)> {
        let mut center_hz = self.center_hz;
        let mut note = String::new();
        if let Some((c, bw)) = self.shape {
            let width = slice_width(bw);
            let offset = c - self.center_hz;
            if offset.abs() >= 1.0 || self.collected_hz > width + 1.0 {
                recentre(&self.stem.with_extension("sigmf-data"), self.samples, self.rate, offset, width)?;
                center_hz = c;
                note = format!(
                    "; collected {:.1} kHz wide at {:.4} MHz, re-centred by {offset:+.0} Hz and narrowed to {:.1} kHz",
                    self.collected_hz / 1e3,
                    self.center_hz / 1e6,
                    width / 1e3
                );
            }
        }
        let meta = sigmf::Meta {
            datatype: FORMAT.datatype(),
            sample_rate: self.rate,
            frequency: center_hz,
            datetime: Some(sigmf::rfc3339(self.t_start)),
            hw: self.hw.clone(),
            description: format!("record {} slice from source sample {}{note}", self.record.0, self.from_sample),
            sample_count: self.samples,
        };
        if let Err(e) = sigmf::write_meta(&self.stem.with_extension("sigmf-meta"), &meta) {
            log::warn!("record {}: slice metadata failed: {e}", self.record.0);
        }
        let clip = IqClip { path: self.stem.clone(), from_sample: self.from_sample, samples: self.samples, rate: self.rate, center_hz };
        Ok((clip, self.samples * FORMAT.bytes() as u64))
    }
}

/// Shifts the data file at `path` by `offset_hz` and low-passes it to `width_hz`, keeping its rate and its length: the
/// filter's delay is taken up by zeros either side, so every sample stays at its source time.
fn recentre(path: &Path, samples: u64, rate: f64, offset_hz: f64, width_hz: f64) -> std::io::Result<()> {
    const CHUNK: usize = 1 << 16;
    let mut ddc = dsp::Ddc::new(rate, offset_hz, width_hz / 2.0, rate);
    let zeros = vec![Complex32::default(); ddc.delay()];
    let part = path.with_extension("sigmf-data.part");
    let mut input = BufReader::new(File::open(path)?);
    let mut out = BufWriter::new(File::create(&part)?);
    let size = FORMAT.bytes();
    let (mut raw, mut x, mut y, mut bytes) = (vec![0u8; CHUNK * size], Vec::with_capacity(CHUNK), Vec::new(), Vec::new());
    let mut written = 0u64;
    let mut run = |x: &[Complex32], out: &mut BufWriter<File>| {
        y.clear();
        ddc.process(x, &mut y);
        bytes.clear();
        FORMAT.encode(&y, &mut bytes);
        written += y.len() as u64;
        out.write_all(&bytes)
    };
    run(&zeros, &mut out)?;
    let mut left = samples;
    while left > 0 {
        let n = (left as usize).min(CHUNK);
        input.read_exact(&mut raw[..n * size])?;
        x.clear();
        FORMAT.decode(&raw[..n * size], &mut x);
        run(&x, &mut out)?;
        left -= n as u64;
    }
    run(&zeros, &mut out)?;
    out.flush()?;
    drop(out);
    debug_assert_eq!(written, samples);
    std::fs::rename(&part, path)
}

/// Copies a slice's files to `to` (a stem): a hard link where the file system allows (the cap's eviction then removes
/// only the slice directory's name), else a copy. A copy saved before is replaced.
fn copy_slice(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    for ext in ["sigmf-data", "sigmf-meta"] {
        let (src, dst) = (from.with_extension(ext), to.with_extension(ext));
        let _ = std::fs::remove_file(&dst);
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn remove(stem: &Path) {
    for ext in ["sigmf-data", "sigmf-meta", "sigmf-data.part"] {
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
    ending: Arc<Ending>,
    max_input: u64,
    /// The channel filter's delay in input samples (`N/4`): a block's samples show the signal of that much earlier.
    group_delay: u64,
    start_sample: u64,
    input_rate: f64,
    hop: u64,
    /// The slice channel's passband.
    collected_hz: f64,
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
        let mut buf = Vec::with_capacity(x.len() * FORMAT.bytes());
        FORMAT.encode(x, &mut buf);
        o.data.write_all(&buf)?;
        o.samples += x.len() as u64;
        Ok(())
    }

    /// Closes the files and tells the recorder. The record's end is known only once it closed, a hangover after its
    /// last active row, so the slice is cut back to it.
    fn finish(&mut self) {
        let Some(rec) = self.recorder.upgrade() else { return };
        let Some(mut o) = self.out.take() else {
            rec.abandon(self.req.record);
            return;
        };
        let end = self.ending.end.load(Ordering::Acquire);
        let decim = self.input_rate / o.rate;
        if end != u64::MAX && decim > 0.0 {
            let keep = ((end.saturating_sub(o.from_sample) as f64 / decim).ceil() as u64).min(o.samples);
            o.samples = keep;
        }
        let cut = o.data.flush().and_then(|()| o.data.get_ref().set_len(o.samples * FORMAT.bytes() as u64));
        drop(o.data);
        if let Err(e) = cut {
            log::warn!("record {}: slice write failed: {e}", self.req.record.0);
            remove(&o.stem);
            rec.abandon(self.req.record);
            return;
        }
        rec.submit(Finish {
            record: self.req.record,
            stem: o.stem,
            from_sample: o.from_sample,
            samples: o.samples,
            rate: o.rate,
            center_hz: self.req.center_hz,
            collected_hz: self.collected_hz,
            shape: *self.ending.shape.lock(),
            t_start: self.req.t_start,
            hw: self.req.hw.clone(),
        });
    }
}

impl Consumer for SliceWriter {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                if first_sample.saturating_sub(self.group_delay) >= self.ending.end.load(Ordering::Acquire)
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
                        if let Some(rec) = self.recorder.upgrade() {
                            rec.abandon(self.req.record);
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: f64, k: usize) -> Complex32 {
        let w = std::f64::consts::TAU * hz / rate * k as f64;
        Complex32::new(w.cos() as f32, w.sin() as f32)
    }

    /// Power at `hz` (a single-bin DFT over `x`).
    fn power_at(x: &[Complex32], hz: f64, rate: f64) -> f64 {
        let s: Complex32 = x.iter().enumerate().map(|(k, v)| v * tone(-hz, rate, k)).sum();
        (s.norm() as f64 / x.len() as f64).powi(2)
    }

    #[test]
    fn recentring_moves_the_signal_keeps_its_timing_and_drops_what_is_outside() {
        let dir = std::env::temp_dir().join(format!("spectrawl-recentre-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1.sigmf-data");
        let rate = 78_125.0;
        // A tone 5 kHz above the slice's centre that starts at sample 4000, and a neighbour 20 kHz below it.
        let n = 20_000;
        let x: Vec<Complex32> =
            (0..n).map(|k| if k >= 4000 { tone(5e3, rate, k) } else { Complex32::default() } + tone(-20e3, rate, k) * 0.5).collect();
        let mut raw = Vec::new();
        FORMAT.encode(&x, &mut raw);
        std::fs::write(&path, raw).unwrap();

        recentre(&path, n as u64, rate, 5e3, 12.5e3).unwrap();
        let mut y = Vec::new();
        FORMAT.decode(&std::fs::read(&path).unwrap(), &mut y);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(y.len(), n, "sample for sample");
        let on = &y[6000..];
        assert!(power_at(on, 0.0, rate) > 0.9, "the tone is at the centre now: {}", power_at(on, 0.0, rate));
        assert!(power_at(on, 5e3, rate) < 1e-4);
        // The neighbour, now 25 kHz off, is outside the 12.5 kHz passband.
        assert!(power_at(on, -25e3, rate) < 1e-4, "{}", power_at(on, -25e3, rate));
        // The burst still starts at sample 4000: half-way up there, within a couple of samples.
        let rise = y.iter().position(|v| v.norm() > 0.5).unwrap();
        assert!(rise.abs_diff(4000) <= 2, "rises at {rise}");
    }
}
