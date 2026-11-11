//! The record book: every source's detector and every device's emitters, the open records, a window of recent closed
//! ones, and the immutable view the snapshot carries (spec §4.4, §4.9).
//!
//! The book is the stages' [`RowSink`]: each row goes to its source's [`Detector`]; closed and open records become
//! [`SignalRecord`]s (wall times from the source's timeline), are grouped by the device's [`EmitterBook`], and get the
//! `Position` claims their place suggests. Closed records, emitter changes, tags and notes go to the [`RecordSink`]
//! (the store) after the book's lock is released.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use api::{Claim, EmitterId, EmitterView, Evidence, Note, RecordId, RecordsView, Role, Row, SignalRecord};
use parking_lot::Mutex;
use sdr_core::IfArchitecture;

use super::detector::{Detected, Detector, DetectorConfig};
use super::emitters::{EmitterBook, EmitterState};
use crate::budget::SourceId;
use crate::recorder::{Recorder, SliceEvent, SliceRequest};
use crate::source::Source;
use crate::stage::RowSink;

/// Closed records kept in memory, across sources (older ones are in the store).
pub const WINDOW: usize = 5000;

/// Where the book's lasting changes go: the store's queue. Called without the book's lock held, on the stage thread
/// or the publisher's; it must not block.
pub trait RecordSink: Send + Sync {
    fn record(&self, r: &SignalRecord);
    fn emitter(&self, e: &EmitterState);
    /// An emitter folded into another: its records now belong to `into`.
    fn emitter_folded(&self, gone: EmitterId, into: EmitterId);
    fn tag(&self, record: RecordId, tag: &str, on: bool);
    fn note(&self, record: RecordId, note: &Note);
    /// The next ids the book will hand out (written with every batch).
    fn next_ids(&self, record: u64, emitter: u64);
    /// A record no longer in memory lost its slice to the disk cap.
    fn slice_evicted(&self, record: RecordId);
}

/// What the book starts from (the store's load at start).
#[derive(Default)]
pub struct Loaded {
    /// Closed records, any order.
    pub records: Vec<SignalRecord>,
    pub emitters: Vec<EmitterState>,
    pub next_record_id: u64,
    pub next_emitter_id: u64,
}

struct PerSource {
    source: Arc<Source>,
    serial: Arc<str>,
    det: Option<Detector>,
    /// The detector is for stitched sweep rows (`DetectorConfig::sweep`).
    det_survey: bool,
    zero_if: bool,
    spur_grid_hz: Option<f64>,
    usable_fraction: f32,
    /// Open records by id (claims and all), as last built.
    open: HashMap<RecordId, Arc<SignalRecord>>,
}

/// Lasting changes made under the lock, handed to the sink after it.
#[derive(Default)]
struct Outbox {
    records: Vec<Arc<SignalRecord>>,
    emitters: Vec<EmitterState>,
    folded: Vec<(EmitterId, EmitterId)>,
    ids: Option<(u64, u64)>,
    slice_start: Vec<SliceRequest>,
    /// (record, end sample, final centre, final width).
    slice_end: Vec<(RecordId, u64, f64, f64)>,
    /// Open records' centre and width as they now stand: a slice that stops before its record closes (the source
    /// went, a gap, its longest) is re-centred on these.
    slice_shape: Vec<(RecordId, f64, f64)>,
    slice_cancel: Vec<RecordId>,
    evicted_elsewhere: Vec<RecordId>,
}

struct Inner {
    sources: HashMap<SourceId, PerSource>,
    emitters: HashMap<Arc<str>, EmitterBook>,
    /// Newest last.
    window: VecDeque<Arc<SignalRecord>>,
    next_record: u64,
    next_emitter: u64,
    version: u64,
    /// The view as last built, and emitter views by id (rebuilt only when their emitter changed).
    view: Arc<RecordsView>,
    emitter_views: HashMap<EmitterId, Arc<EmitterView>>,
    dirty_emitters: HashSet<EmitterId>,
    /// Claims from outside the row path (the detector bank, a decoder), by record and by what made them; each maker's
    /// set replaces its last. Kept for open records and the window.
    extra: HashMap<RecordId, BTreeMap<String, Vec<Claim>>>,
}

pub struct RecordBook {
    inner: Mutex<Inner>,
    sink: Option<Arc<dyn RecordSink>>,
    /// The slice tier, when it is on.
    recorder: OnceLock<Arc<Recorder>>,
}

impl RecordBook {
    pub fn new(loaded: Loaded, sink: Option<Arc<dyn RecordSink>>) -> RecordBook {
        let mut emitters: HashMap<Arc<str>, Vec<EmitterState>> = HashMap::new();
        for e in loaded.emitters {
            emitters.entry(e.serial.clone()).or_default().push(e);
        }
        let emitters: HashMap<Arc<str>, EmitterBook> = emitters.into_iter().map(|(s, v)| (s.clone(), EmitterBook::load(s, v))).collect();
        let mut records = loaded.records;
        records.sort_by_key(|r| (r.t_end, r.id));
        let skip = records.len().saturating_sub(WINDOW);
        let window: VecDeque<Arc<SignalRecord>> = records.into_iter().skip(skip).map(Arc::new).collect();
        let max_rec = window.iter().map(|r| r.id.0 + 1).max().unwrap_or(1);
        let max_em = emitters.values().flat_map(|b| b.views().map(|v| v.id.0 + 1)).max().unwrap_or(1);
        let dirty_emitters = emitters.values().flat_map(|b| b.views().map(|v| v.id)).collect();
        RecordBook {
            inner: Mutex::new(Inner {
                sources: HashMap::new(),
                emitters,
                window,
                next_record: loaded.next_record_id.max(max_rec),
                next_emitter: loaded.next_emitter_id.max(max_em),
                version: 1,
                view: Arc::new(RecordsView::default()),
                emitter_views: HashMap::new(),
                dirty_emitters,
                extra: HashMap::new(),
            }),
            sink,
            recorder: OnceLock::new(),
        }
    }

    /// Turns the slice tier on (once).
    pub fn set_recorder(&self, r: Arc<Recorder>) {
        let _ = self.recorder.set(r);
    }

    /// Starts detecting on `source`'s rows (before the source is attached to the bus, so its first row is seen).
    pub fn add_source(&self, source: Arc<Source>) {
        let snap = source.snapshot();
        let serial: Arc<str> = snap.info.serial.as_str().into();
        let mut inner = self.inner.lock();
        inner.emitters.entry(serial.clone()).or_insert_with(|| EmitterBook::new(serial.clone()));
        inner.sources.insert(
            source.id(),
            PerSource {
                source,
                serial,
                det: None,
                det_survey: false,
                zero_if: snap.caps.if_arch == IfArchitecture::ZeroIf,
                spur_grid_hz: snap.caps.spur_grid_hz,
                usable_fraction: snap.caps.usable_bandwidth_fraction as f32,
                open: HashMap::new(),
            },
        );
    }

    /// Closes the source's open records (at the last row seen) and forgets the source. Call after its stage stopped.
    pub fn remove_source(&self, id: SourceId) {
        let out = {
            let mut inner = self.inner.lock();
            let Some(mut ps) = inner.sources.remove(&id) else { return };
            let mut out = Outbox::default();
            if let Some(det) = ps.det.as_mut() {
                let closed = {
                    let next = &mut inner.next_record;
                    det.close_all(&mut || take_id(next))
                };
                for d in closed {
                    inner.close(&mut ps, d, &mut out);
                }
            }
            if let Some(r) = self.recorder.get() {
                inner.slice_events(r.take_events(), &mut out);
            }
            inner.version += 1;
            out.ids = Some((inner.next_record, inner.next_emitter));
            out
        };
        self.send(out);
    }

    /// The current view: the same `Arc` while nothing changed.
    pub fn view(&self) -> Arc<RecordsView> {
        let mut inner = self.inner.lock();
        if inner.view.version == inner.version {
            return inner.view.clone();
        }
        let inner = &mut *inner;
        for id in std::mem::take(&mut inner.dirty_emitters) {
            match inner.emitters.values().find_map(|b| b.view(id)) {
                Some(v) => inner.emitter_views.insert(id, Arc::new(v)),
                None => inner.emitter_views.remove(&id),
            };
        }
        let mut open: Vec<Arc<SignalRecord>> = inner.sources.values().flat_map(|s| s.open.values().cloned()).collect();
        open.sort_by(|a, b| b.first_sample.cmp(&a.first_sample).then(b.id.cmp(&a.id)));
        let records = open.into_iter().chain(inner.window.iter().rev().cloned()).collect();
        let mut emitters: Vec<Arc<EmitterView>> = inner.emitter_views.values().cloned().collect();
        emitters.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.id.cmp(&b.id)));
        inner.view = Arc::new(RecordsView { version: inner.version, records, emitters });
        inner.view.clone()
    }

    /// Adds or removes a tag on a record in memory; false if the book doesn't hold it (the store may).
    pub fn tag(&self, record: RecordId, tag: &str, on: bool) -> bool {
        let found = self.inner.lock().edit(record, |r| {
            let has = r.tags.iter().any(|t| t == tag);
            if on && !has {
                r.tags.push(tag.to_string());
            } else if !on {
                r.tags.retain(|t| t != tag);
            }
        });
        if let Some(s) = &self.sink {
            s.tag(record, tag, on);
        }
        found
    }

    /// Save IQ (see [`Recorder::save`]): the saved stem and whether the copy is still to come; the record is tagged
    /// `saved`. `Err` says why there is no IQ to keep.
    pub fn save_iq(&self, record: RecordId) -> Result<(PathBuf, bool), String> {
        let r = self.recorder.get().ok_or_else(|| format!("record {} has no IQ to save: slices are off", record.0))?;
        let saved = r.save(record).map_err(|e| {
            let why = match self.get(record) {
                _ if e.kind() != std::io::ErrorKind::NotFound => format!("copying its slice failed: {e}"),
                Some(rec) if rec.slice_evicted => "its slice was evicted to keep under the disk cap".into(),
                Some(_) => "it has no slice".into(),
                None => "its slice is gone".into(),
            };
            format!("record {} has no IQ to save: {why}", record.0)
        })?;
        self.tag(record, "saved", true);
        Ok(saved)
    }

    /// Adds a note to a record, timed by its source's timeline (else the newest time any open source gives, else the
    /// record's end); false if the book doesn't hold the record (the store may).
    pub fn note(&self, record: RecordId, text: &str) -> bool {
        let mut inner = self.inner.lock();
        let fallback = inner.now();
        let mut note = None;
        let found = inner.edit_with_time(record, |r, now| {
            let n = Note { text: text.to_string(), time: now.or(fallback).unwrap_or(r.t_end) };
            r.notes.push(n.clone());
            note = Some(n);
        });
        drop(inner);
        let n = note.unwrap_or_else(|| Note { text: text.to_string(), time: fallback.unwrap_or(SystemTime::UNIX_EPOCH) });
        if let Some(s) = &self.sink {
            s.note(record, &n);
        }
        found
    }

    /// The record with this id, if the book holds it.
    pub fn get(&self, record: RecordId) -> Option<Arc<SignalRecord>> {
        let inner = self.inner.lock();
        inner
            .sources
            .values()
            .find_map(|s| s.open.get(&record).cloned())
            .or_else(|| inner.window.iter().rev().find(|r| r.id == record).cloned())
    }

    /// The claims `from` last set on a record (empty if none, or if the book no longer holds it).
    pub fn claims_by(&self, record: RecordId, from: &str) -> Vec<Claim> {
        self.inner.lock().extra.get(&record).and_then(|m| m.get(from)).cloned().unwrap_or_default()
    }

    /// Sets the claims `from` (the bank, a decoder) makes about a record, replacing what `from` said before; the
    /// record's emitter re-votes, and a closed record goes back to the store. False if the book doesn't hold it.
    pub fn set_claims(&self, record: RecordId, from: &str, claims: Vec<Claim>) -> bool {
        let out = {
            let mut inner = self.inner.lock();
            let inner = &mut *inner;
            if inner.extra.get(&record).and_then(|m| m.get(from)) == Some(&claims) {
                return true;
            }
            let mut out = Outbox::default();
            let extra = inner.extra.entry(record).or_default();
            if claims.is_empty() {
                extra.remove(from);
            } else {
                extra.insert(from.to_string(), claims);
            }
            let all: Vec<Claim> = extra.values().flatten().cloned().collect();
            let mut changed = None;
            let held = inner.edit(record, |r| {
                r.claims.retain(derived);
                r.claims.extend(all);
                changed = Some((r.emitter, r.serial.clone(), r.claims.clone(), (!r.open).then(|| r.clone())));
            });
            if !held {
                inner.extra.remove(&record);
                return false;
            }
            if let Some((Some(em), serial, claims, closed)) = changed {
                let mut folds = Vec::new();
                let mut em = em;
                if let Some(b) = inner.emitters.get_mut(&serial) {
                    folds = b.note_claims(em, record, &claims);
                    for &(gone, into) in &folds {
                        if gone == em {
                            em = into;
                        }
                    }
                    if let Some(s) = b.state(em).filter(|_| closed.is_some() || !folds.is_empty()) {
                        out.emitters.push(s.clone());
                    }
                }
                inner.dirty_emitters.insert(em);
                for &(gone, into) in &folds {
                    inner.dirty_emitters.insert(gone);
                    out.folded.push((gone, into));
                    inner.repoint(gone, into);
                }
                // The record as it now stands: closed, it goes back to the store (its emitter may have just changed).
                let closed = closed.map(|mut r| {
                    if r.emitter.is_some_and(|e| folds.iter().any(|f| f.0 == e)) {
                        r.emitter = Some(em);
                    }
                    Arc::new(r)
                });
                out.records.extend(closed);
            }
            out
        };
        self.send(out);
        true
    }

    fn send(&self, out: Outbox) {
        if let Some(r) = self.recorder.get() {
            for id in &out.slice_cancel {
                r.cancel(*id);
            }
            for q in out.slice_start.iter().cloned() {
                r.start(q);
            }
            r.shapes(&out.slice_shape);
            for &(id, end, center_hz, bandwidth_hz) in &out.slice_end {
                r.end(id, end, center_hz, bandwidth_hz);
            }
        }
        let Some(s) = &self.sink else { return };
        for &id in &out.evicted_elsewhere {
            s.slice_evicted(id);
        }
        for r in &out.records {
            s.record(r);
        }
        for e in &out.emitters {
            s.emitter(e);
        }
        for &(gone, into) in &out.folded {
            s.emitter_folded(gone, into);
        }
        if let Some((r, e)) = out.ids {
            s.next_ids(r, e);
        }
    }
}

fn slice_request(ps: &PerSource, d: &Detected) -> SliceRequest {
    SliceRequest {
        record: d.id,
        source: ps.source.id(),
        center_hz: d.center_hz(),
        bandwidth_hz: d.bandwidth_hz(),
        first_sample: d.first_sample,
        t_start: ps.source.time_of(d.first_sample).unwrap_or(SystemTime::UNIX_EPOCH),
        hw: ps.serial.to_string(),
    }
}

/// Claims the book derives itself each time it places a record (where it sits, the band plan); everything else comes
/// from [`RecordBook::set_claims`].
fn derived(c: &Claim) -> bool {
    matches!(c.by, Evidence::Position(_) | Evidence::BandPlan { .. })
}

/// How sure the band plan alone makes us of anything: it says what the band is for, not what is in it.
const BAND_PLAN_CONFIDENCE: f32 = 0.2;

/// The band plan's entry for `hz` (spec §4.5: a claim, never a rename).
fn band_plan(hz: f64) -> Option<Claim> {
    let b = detect::bandplan::lookup(hz)?;
    let entry = if b.name == b.service.name() { b.name.to_string() } else { format!("{} ({})", b.name, b.service.name()) };
    Some(Claim::new(b.name, BAND_PLAN_CONFIDENCE, Evidence::BandPlan { entry }))
}

fn take_id(next: &mut u64) -> RecordId {
    *next += 1;
    RecordId(*next - 1)
}

impl RowSink for RecordBook {
    fn on_row(&self, source: SourceId, row: &Arc<Row>) {
        let out = {
            let mut inner = self.inner.lock();
            let inner = &mut *inner;
            let Some(mut ps) = inner.sources.remove(&source) else { return };
            let mut out = inner.row(&mut ps, row, self.recorder.get().is_some());
            inner.sources.insert(source, ps);
            if let Some(r) = self.recorder.get() {
                inner.slice_events(r.take_events(), &mut out);
            }
            out
        };
        self.send(out);
    }
}

impl Inner {
    fn row(&mut self, ps: &mut PerSource, row: &Row, slicing: bool) -> Outbox {
        let mut out = Outbox::default();
        // A sweep's stitched rows (spec §4.6a) go to a detector configured for them; the switch closes what was open
        // (the segment changes with it anyway).
        let survey = row.segment.id.0 & crate::sweep::STITCHED != 0;
        if ps.det.is_some() && ps.det_survey != survey {
            let closed = {
                let next = &mut self.next_record;
                ps.det.as_mut().map(|d| d.close_all(&mut || take_id(next))).unwrap_or_default()
            };
            for d in closed {
                if slicing {
                    out.slice_end.push((d.id, d.end_sample, d.center_hz(), d.bandwidth_hz()));
                }
                self.close(ps, d, &mut out);
            }
            ps.det = None;
        }
        ps.det_survey = survey;
        let slicing = slicing && !survey;
        let det = ps.det.get_or_insert_with(|| {
            if survey {
                Detector::new(DetectorConfig::sweep())
            } else {
                Detector::new(DetectorConfig {
                    usable_fraction: ps.usable_fraction,
                    ..DetectorConfig::live(DetectorConfig::rows_per_sec(row))
                })
            }
        });
        let (result, open) = {
            let next = &mut self.next_record;
            let mut ids = || take_id(next);
            let r = det.process(row, &mut ids);
            let open = det.open_records(&mut ids);
            (r, open)
        };
        let before = self.next_record;
        for (gone, into) in result.merged {
            ps.open.remove(&gone);
            self.extra.remove(&gone);
            out.slice_cancel.push(gone);
            if let Some(b) = self.emitters.get_mut(&ps.serial) {
                b.forget_record(gone);
            }
            log::debug!("record {} merged into {}", gone.0, into.0);
        }
        for d in result.closed {
            if slicing && !ps.open.contains_key(&d.id) {
                // Reportable and closed on the same row: its whole slice is back-filled.
                out.slice_start.push(slice_request(ps, &d));
            }
            if slicing {
                out.slice_end.push((d.id, d.end_sample, d.center_hz(), d.bandwidth_hz()));
            }
            self.close(ps, d, &mut out);
        }
        let mut still = HashSet::new();
        for d in open {
            still.insert(d.id);
            if slicing {
                if !ps.open.contains_key(&d.id) {
                    out.slice_start.push(slice_request(ps, &d));
                }
                out.slice_shape.push((d.id, d.center_hz(), d.bandwidth_hz()));
            }
            let r = self.place(ps, d, &mut out);
            ps.open.insert(r.id, r);
        }
        ps.open.retain(|id, _| still.contains(id));
        if let Some(b) = self.emitters.get_mut(&ps.serial) {
            let ids: Vec<RecordId> = ps.open.keys().copied().collect();
            for gone in b.mark_active(&ids) {
                self.dirty_emitters.insert(gone);
            }
        }
        self.version += 1;
        if self.next_record != before || !out.records.is_empty() {
            out.ids = Some((self.next_record, self.next_emitter));
        }
        out
    }

    /// A record as the API shows it, grouped and with its position claims.
    fn place(&mut self, ps: &PerSource, d: Detected, out: &mut Outbox) -> Arc<SignalRecord> {
        let t = |s: u64| ps.source.time_of(s).unwrap_or(SystemTime::UNIX_EPOCH);
        let prev = ps.open.get(&d.id);
        let survey = ps.det_survey;
        let mut r = SignalRecord {
            id: d.id,
            source: ps.source.id(),
            serial: ps.serial.clone(),
            role: if survey { Role::Survey } else { Role::Live },
            segment: d.segment.id,
            tuner_hz: d.segment.center_hz,
            first_sample: d.first_sample,
            end_sample: d.end_sample,
            t_start: t(d.first_sample),
            t_end: t(d.end_sample),
            f_lo_hz: d.f_lo_hz,
            f_hi_hz: d.f_hi_hz,
            bin_hz: d.bin_hz,
            peak_dbfs: d.peak_dbfs,
            mean_dbfs: d.mean_dbfs,
            mean_snr_db: d.mean_snr_db,
            max_snr_db: d.max_snr_db,
            rows: d.rows,
            open: d.open,
            claims: vec![],
            emitter: None,
            // What a user added to the open record stays with it.
            tags: prev.map(|p| p.tags.clone()).unwrap_or_default(),
            notes: prev.map(|p| p.notes.clone()).unwrap_or_default(),
            iq_clip: prev.and_then(|p| p.iq_clip.clone()),
            slice_evicted: false,
        };
        if let Some(mut b) = self.emitters.remove(&ps.serial) {
            let next = &mut self.next_emitter;
            let o = b.observe(&r, &mut || {
                *next += 1;
                EmitterId(*next - 1)
            });
            r.emitter = Some(o.emitter);
            // A stitched row's "tuner" is the middle of the span: no position claims there (the sweep blanks the
            // radio's spur grid itself).
            if !survey {
                r.claims = b.position_claims(&r, ps.zero_if, ps.spur_grid_hz);
            }
            r.claims.extend(band_plan(r.center_hz()));
            r.claims.extend(self.extra.get(&r.id).into_iter().flat_map(|m| m.values().flatten().cloned()));
            let mut folds: Vec<(EmitterId, EmitterId)> = o.folded.into_iter().collect();
            folds.extend(b.note_claims(o.emitter, r.id, &r.claims));
            // A station named just now may have gone into an older one on its carrier.
            let mut em = o.emitter;
            for &(gone, into) in &folds {
                if gone == em {
                    em = into;
                }
            }
            r.emitter = Some(em);
            self.dirty_emitters.insert(em);
            if !r.open
                && let Some(s) = b.state(em)
            {
                out.emitters.push(s.clone());
            }
            if let Some(from) = o.left {
                self.dirty_emitters.insert(from);
            }
            for (gone, into) in folds {
                self.dirty_emitters.insert(gone);
                self.dirty_emitters.insert(into);
                out.folded.push((gone, into));
                self.repoint(gone, into);
            }
            self.emitters.insert(ps.serial.clone(), b);
        }
        Arc::new(r)
    }

    /// A record closed: placed, kept in the window, sent to the store.
    fn close(&mut self, ps: &mut PerSource, d: Detected, out: &mut Outbox) {
        let r = self.place(ps, d, out);
        ps.open.remove(&r.id);
        self.window.push_back(r.clone());
        while self.window.len() > WINDOW {
            if let Some(gone) = self.window.pop_front() {
                self.extra.remove(&gone.id);
            }
        }
        out.records.push(r);
    }

    /// Records of emitter `gone` now belong to `into`.
    fn repoint(&mut self, gone: EmitterId, into: EmitterId) {
        let fix = |r: &mut Arc<SignalRecord>| {
            if r.emitter == Some(gone) {
                Arc::make_mut(r).emitter = Some(into);
            }
        };
        self.window.iter_mut().for_each(fix);
        for s in self.sources.values_mut() {
            s.open.values_mut().for_each(fix);
        }
    }

    /// Finished slices set their record's clip; evicted ones clear it. Closed records go back to the store.
    fn slice_events(&mut self, events: Vec<SliceEvent>, out: &mut Outbox) {
        for ev in events {
            let (id, clip, evicted) = match ev {
                SliceEvent::Done { record, clip } => (record, Some(clip), false),
                SliceEvent::Evicted(record) => (record, None, true),
            };
            let mut updated = None;
            let held = self.edit(id, |r| {
                r.iq_clip = clip;
                r.slice_evicted |= evicted;
                if !r.open {
                    updated = Some(r.clone());
                }
            });
            if let Some(r) = updated {
                out.records.push(Arc::new(r));
            } else if !held && evicted {
                out.evicted_elsewhere.push(id);
            }
        }
    }

    /// The newest time any open source's timeline gives for its head.
    fn now(&self) -> Option<SystemTime> {
        self.sources.values().filter_map(|s| s.source.time_of(s.source.head())).max()
    }

    fn edit(&mut self, record: RecordId, f: impl FnOnce(&mut SignalRecord)) -> bool {
        self.edit_with_time(record, |r, _| f(r))
    }

    /// Edits the record wherever the book holds it, with its source's current time if the source is still open.
    fn edit_with_time(&mut self, record: RecordId, f: impl FnOnce(&mut SignalRecord, Option<SystemTime>)) -> bool {
        let now = |s: &PerSource| s.source.time_of(s.source.head());
        for s in self.sources.values_mut() {
            let t = now(s);
            if let Some(r) = s.open.get_mut(&record) {
                f(Arc::make_mut(r), t);
                self.version += 1;
                return true;
            }
        }
        if let Some(r) = self.window.iter_mut().rev().find(|r| r.id == record) {
            let t = self.sources.get(&r.source).and_then(|s| s.source.time_of(s.source.head()));
            f(Arc::make_mut(r), t);
            self.version += 1;
            return true;
        }
        false
    }
}
