//! Groups records into emitters: persistent transmitters on a channel, per device (re-homed from
//! `engine-old::emitters`, grouping and confirmation only; spec §4.4).
//!
//! A record joins the emitter whose centre is within tolerance and whose bandwidth is comparable, or a confirmed
//! narrowband emitter it lies inside (one tone of a pager, a station's quiet carrier), which it then doesn't reshape;
//! otherwise it starts a new one. A record is placed from its first rows, so when one that started an emitter of its own
//! closes, it is matched again with its final shape. An emitter counts as `confirmed` once it can't be a noise blip; the
//! unconfirmed ones are dropped if they don't recur.
//!
//! Where a line sits relative to the receiver's own artefacts (the mirror image of another line about the tuner, a comb
//! of always-on lines, the spur grid, DC on a zero-IF radio) becomes a `Position` claim on the record: one claim among
//! others, never a sticky label. Label voting over records' claims comes with the detector bank (step 6).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use api::{Claim, EmitterId, EmitterView, Evidence, PositionKind, RecordId, SignalRecord};

/// Most emitters kept per device; the least recently seen inactive ones go first.
const MAX_EMITTERS: usize = 5000;
/// Confirmed once it has had this many records that weren't weak one-row blips (rows ≥ 2 or SNR ≥ `BLIP_SNR_DB`)...
const CONFIRM_RECORDS: u32 = 3;
const BLIP_SNR_DB: f32 = 12.0;
/// ...or one record lasting this long over at least `CONFIRM_ROWS` rows...
const CONFIRM_DURATION_S: f64 = 0.5;
const CONFIRM_ROWS: u32 = 3;
/// ...or one this far above the floor.
const CONFIRM_SNR_DB: f32 = 15.0;
/// Unconfirmed emitters not seen again within this are dropped.
const UNCONFIRMED_TTL: Duration = Duration::from_secs(120);
/// Confirmed emitters not seen within this leave memory (the store keeps them).
pub const EMITTER_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// Rows with bins this wide or wider (the survey's) match records by overlap.
const COARSE_BIN_HZ: f64 = 10e3;
/// Calls to `mark_active` between sweeps for stale emitters.
const PRUNE_EVERY: u32 = 64;
/// Widest emitter that takes in narrower records lying inside it: a narrowband channel.
const CONTAIN_MAX_HZ: f64 = 30e3;
/// Closed records whose sample extents an emitter remembers (for group windows, step 6).
const RECENT_RECORDS: usize = 8;

/// Narrower than this, a signal is a line: a carrier, a spur or a birdie.
const LINE_HZ: f64 = 5e3;
/// Lines this close in frequency are the same line (a bin or two).
const LINE_TOL_HZ: f64 = 1.5e3;
/// A comb needs this many lines at one spacing, this one included (with twenty-odd lines in a 10 MHz span, three at
/// some common spacing is coincidence about once per line; four is not)...
const COMB_LINES: usize = 4;
/// ...each with this many records (the radio's own 750 kHz comb lines had 12–40 in a minute, the false members at a
/// trunked site 1–5)...
const COMB_MEMBER_RECORDS: u32 = 6;
/// ...at spacings in this range (closer is one wide signal's structure, further apart is coincidence in a 10 MHz span)...
const COMB_SPACING_HZ: (f64, f64) = (20e3, 2.5e6);
/// ...and on the air at least this share of the time: a trunked site's narrow blips on its channel raster were 1–39% on
/// air and read as combs at every multiple of 12.5 kHz.
const COMB_MIN_DUTY: f64 = 0.5;
/// A line this close to a multiple of the radio's spur grid is on it.
const GRID_HZ: f64 = 2.5e3;
/// A line this close to the tuner centre is at DC.
const DC_HZ: f64 = 1.5e3;

/// A broadcast station's channel either side of its carrier: ±75 kHz of deviation and the programme's 15 kHz.
const STATION_HALF_HZ: f64 = 100e3;
/// HD Radio's digital sidebands either side of a station's carrier, from the widest (extended hybrid, 101 kHz in) to
/// their outer edge (198 kHz), with room for the carrier being a few kHz out. On air at 105.9 MHz they were records of 65–86 kHz at 105.700–105.786 and
/// 106.014–106.100, each its own emitter.
const STATION_HD_HZ: (f64, f64) = (95e3, 212e3);
/// A sideband's record: at least this wide, its outer edge at least this far out. A neighbouring station 200 kHz
/// away swings through the same place, but its fragments there are narrower (2–30 kHz on air) and its whole records
/// reach past the sideband's edge.
const STATION_HD_MIN_HZ: f64 = 50e3;
const STATION_HD_OUTER_HZ: f64 = 180e3;

/// How sure a position alone makes us: a suspicion until the line is measured (step 6).
const POSITION_CONFIDENCE: f32 = 0.3;
/// An image pair or a comb is stronger: another line in view says so.
const CONTEXT_CONFIDENCE: f32 = 0.5;

/// What the store keeps of an emitter.
#[derive(Debug, Clone, PartialEq)]
pub struct EmitterState {
    pub id: EmitterId,
    pub serial: Arc<str>,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    /// Bin width of the rows its records came from.
    pub bin_hz: f64,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub records: u32,
    /// Finished records that weren't weak one-row blips.
    pub solid_records: u32,
    /// Summed duration of finished records, seconds.
    pub on_time_s: f64,
    pub confirmed: bool,
    pub peak_snr_db: f32,
    /// Owns its broadcast channel (set from its headline, step 6): narrow records near its centre are it.
    pub station: bool,
    pub headline: Option<Claim>,
}

#[derive(Debug, Clone)]
struct Emitter {
    s: EmitterState,
    /// Start of the record still open on it (its time isn't in `on_time_s` yet).
    open_start: Option<SystemTime>,
    active: bool,
    /// Sample extents of the newest closed records, oldest first.
    recent: VecDeque<(u64, u64)>,
    /// What its records' claims say, for the headline vote.
    votes: Votes,
}

/// An emitter's records' claims, as votes (re-homed from `engine-old::emitters`).
#[derive(Debug, Clone, Default)]
struct Votes {
    /// The newest decoder claim, and its record.
    decoded: Option<(RecordId, Claim)>,
    /// Sync-bank protocols by label: the records that claimed it, whether any was strong, the most confident claim.
    synced: HashMap<String, SyncVote>,
    /// Measured analog modulations by label ([`api::records::is_analog_shape`]), when nothing decoded or synced.
    shaped: HashMap<String, SyncVote>,
    /// The headline the store had, kept until the votes name one.
    loaded: Option<Claim>,
    /// Carriers the looks at its records measured (a broadcast station's), newest last, one per record.
    carriers: VecDeque<(RecordId, f64)>,
}

/// Carriers a station's centre is the median of: a look cut on an HD Radio sideband measured 105.9 MHz 9–11 kHz high
/// on air, one cut at the core within 3 kHz.
const CARRIERS: usize = 16;

impl Votes {
    /// A record that left the emitter: its claims no longer vote here.
    fn forget(&mut self, record: RecordId) {
        if self.decoded.as_ref().is_some_and(|(r, _)| *r == record) {
            self.decoded = None;
        }
        for map in [&mut self.synced, &mut self.shaped] {
            for v in map.values_mut() {
                v.records.retain(|r| *r != record);
            }
            map.retain(|_, v| !v.records.is_empty());
        }
        self.carriers.retain(|(r, _)| *r != record);
    }

    /// The median of the carriers measured.
    fn carrier(&self) -> Option<f64> {
        let mut c: Vec<f64> = self.carriers.iter().map(|(_, c)| *c).collect();
        c.sort_by(f64::total_cmp);
        c.get(c.len() / 2).copied()
    }
}

#[derive(Debug, Clone)]
struct SyncVote {
    records: Vec<RecordId>,
    strong: bool,
    best: Claim,
}

fn identity(c: &Claim) -> Option<&api::Identity> {
    match &c.by {
        Evidence::Decoder { identity, .. } => identity.as_ref(),
        _ => None,
    }
}

/// A protocol the sync bank names is believed for the emitter once this many of its records say so...
const PROTOCOL_RECORDS: usize = 2;
/// ...or once one record says so strongly: this many syncs, or half its frames decoded.
const STRONG_SYNCS: u32 = 4;
/// Records remembered per protocol vote.
const VOTE_RECORDS: usize = 16;

impl Votes {
    /// The headline the votes give: the decoder's claim, else a believed sync-bank protocol (most records, then most
    /// confident), else what the store had.
    fn headline(&self) -> Option<Claim> {
        if let Some((_, c)) = &self.decoded {
            return Some(c.clone());
        }
        let believed = self
            .synced
            .values()
            .filter(|v| v.records.len() >= PROTOCOL_RECORDS || v.strong)
            .max_by(|a, b| a.records.len().cmp(&b.records.len()).then(a.best.confidence.total_cmp(&b.best.confidence)));
        // An analog shape stands once two records measure it, or one broadcast pilot does.
        let shaped = || {
            self.shaped
                .values()
                .filter(|v| v.records.len() >= PROTOCOL_RECORDS || v.best.label.starts_with("FM broadcast"))
                .max_by(|a, b| a.records.len().cmp(&b.records.len()).then(a.best.confidence.total_cmp(&b.best.confidence)))
                .map(|v| v.best.clone())
        };
        believed.map(|v| v.best.clone()).or_else(shaped).or_else(|| self.loaded.clone())
    }
}

impl Emitter {
    /// Fraction of the time between first and last seen that it was on, counting a record still open (a steady line has
    /// one, all along).
    fn duty(&self) -> f64 {
        let s = &self.s;
        let span = s.last_seen.duration_since(s.first_seen).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let open = self.open_start.and_then(|t| s.last_seen.duration_since(t).ok()).map_or(0.0, |d| d.as_secs_f64());
        if span <= 0.0 { 1.0 } else { ((s.on_time_s + open) / span).min(1.0) }
    }

    fn view(&self) -> EmitterView {
        let s = &self.s;
        EmitterView {
            id: s.id,
            serial: s.serial.clone(),
            center_hz: s.center_hz,
            bandwidth_hz: s.bandwidth_hz,
            first_seen: s.first_seen,
            last_seen: s.last_seen,
            records: s.records,
            on_time_s: s.on_time_s,
            duty: self.duty() as f32,
            confirmed: s.confirmed,
            headline: s.headline.clone(),
        }
    }
}

/// What placing a record did.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub emitter: EmitterId,
    /// `(gone, into)`: an emitter its record had started on its own folded into another.
    pub folded: Option<(EmitterId, EmitterId)>,
    /// The emitter the record left: it joined while its first rows looked like that emitter, and closed as something
    /// else.
    pub left: Option<EmitterId>,
}

/// One device's emitters.
pub struct EmitterBook {
    serial: Arc<str>,
    map: HashMap<EmitterId, Emitter>,
    by_record: HashMap<RecordId, EmitterId>,
    /// Newest record time seen (the book's clock, for pruning).
    latest: Option<SystemTime>,
    calls: u32,
}

impl EmitterBook {
    pub fn new(serial: Arc<str>) -> Self {
        Self { serial, map: HashMap::new(), by_record: HashMap::new(), latest: None, calls: 0 }
    }

    /// A book holding emitters loaded from the store.
    pub fn load(serial: Arc<str>, states: Vec<EmitterState>) -> Self {
        let mut b = Self::new(serial);
        for s in states {
            b.latest = b.latest.max(Some(s.last_seen));
            let votes = Votes { loaded: s.headline.clone(), ..Default::default() };
            b.map.insert(s.id, Emitter { s, open_start: None, active: false, recent: VecDeque::new(), votes });
        }
        b
    }

    pub fn serial(&self) -> &Arc<str> {
        &self.serial
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn emitter_of(&self, record: RecordId) -> Option<EmitterId> {
        self.by_record.get(&record).copied()
    }

    pub fn state(&self, id: EmitterId) -> Option<&EmitterState> {
        self.map.get(&id).map(|e| &e.s)
    }

    pub fn view(&self, id: EmitterId) -> Option<EmitterView> {
        self.map.get(&id).map(Emitter::view)
    }

    /// Every emitter in memory.
    pub fn views(&self) -> impl Iterator<Item = EmitterView> + '_ {
        self.map.values().map(Emitter::view)
    }

    /// Frequency tolerance for joining: a quarter of the bandwidth, at least 2.5 kHz (record centres jitter with
    /// modulation) and at least 1.5 bins (record edges are whole bins; coarse survey bins move centres by one).
    fn tolerance(bw: f64, bin: f64) -> f64 {
        (bw * 0.25).max(2.5e3).max(bin * 1.5)
    }

    /// Does a record of this centre and bandwidth belong to `e`? Bandwidths must be comparable, allowing a bin either
    /// way. On coarse rows (the survey) a record only has to touch the emitter.
    fn fits(e: &EmitterState, c: f64, bw: f64, bin: f64) -> bool {
        let bin = bin.max(e.bin_hz);
        if bin >= COARSE_BIN_HZ {
            return (e.center_hz - c).abs() <= (e.bandwidth_hz + bw) / 2.0 + bin;
        }
        let ratio = (bw + bin) / (e.bandwidth_hz.max(1.0) + bin);
        (e.center_hz - c).abs() <= Self::tolerance(e.bandwidth_hz.max(bw), bin) && (0.33..=3.0).contains(&ratio)
    }

    /// Does a clearly narrower record lie inside `e`, a confirmed narrowband emitter (or have its centre inside a
    /// station's channel)? An FSK channel's records that caught one tone (an idle pager: ±4.8 kHz) or part of a burst
    /// are that channel, not a new "carrier" beside it. Fine rows only; wide emitters are left out so narrow signals
    /// inside them stay their own.
    fn contains(e: &EmitterState, c: f64, bw: f64, bin: f64) -> bool {
        let bin = bin.max(e.bin_hz);
        if bin >= COARSE_BIN_HZ {
            return false;
        }
        // A station: anything centred in its channel, however it swings, and reaching no further than its HD Radio
        // sidebands (on air at 97.7 MHz, records of 58 kHz 85 kHz either side: the swing's edge and a sideband's inner
        // part). A neighbour's fragments are centred 125 kHz away or more.
        if e.station {
            let off = (e.center_hz - c).abs();
            return off <= STATION_HALF_HZ + bin && off + bw / 2.0 <= STATION_HD_HZ.1 + bin;
        }
        if !e.confirmed || bw * 2.0 > e.bandwidth_hz + bin {
            return false;
        }
        e.bandwidth_hz <= CONTAIN_MAX_HZ && (e.center_hz - c).abs() + bw / 2.0 <= e.bandwidth_hz / 2.0 + bin
    }

    /// Is a record of this centre and bandwidth one of station `e`'s HD Radio sidebands?
    fn sideband(e: &EmitterState, c: f64, bw: f64, bin: f64) -> bool {
        let bin = bin.max(e.bin_hz);
        let off = (c - e.center_hz).abs();
        e.station
            && bin < COARSE_BIN_HZ
            && bw >= STATION_HD_MIN_HZ
            && off - bw / 2.0 >= STATION_HD_HZ.0 - bin
            && (STATION_HD_OUTER_HZ - bin..=STATION_HD_HZ.1 + bin).contains(&(off + bw / 2.0))
    }

    /// Is `r` clearly wider than `e` and spanning all of it?
    fn covers(r: &SignalRecord, e: &EmitterState) -> bool {
        let bin = r.bin_hz.max(e.bin_hz);
        r.bandwidth_hz() >= 1.5 * e.bandwidth_hz
            && r.f_lo_hz <= e.center_hz - e.bandwidth_hz / 2.0 + bin
            && r.f_hi_hz >= e.center_hz + e.bandwidth_hz / 2.0 - bin
    }

    /// The emitter a record belongs to, other than `except`: a confirmed narrowband emitter or a station containing it
    /// (the busiest), else a station whose HD Radio sideband it is, else the closest it fits.
    fn find(&self, c: f64, bw: f64, bin: f64, except: Option<EmitterId>) -> Option<EmitterId> {
        let others = || self.map.values().map(|e| &e.s).filter(move |e| Some(e.id) != except);
        others()
            .filter(|e| Self::contains(e, c, bw, bin))
            .max_by_key(|e| e.records)
            .or_else(|| others().filter(|e| Self::sideband(e, c, bw, bin)).max_by_key(|e| e.records))
            .or_else(|| {
                others().filter(|e| Self::fits(e, c, bw, bin)).min_by(|a, b| (a.center_hz - c).abs().total_cmp(&(b.center_hz - c).abs()))
            })
            .map(|e| e.id)
    }

    /// Assigns a record (open or just closed) to an emitter, creating one (with an id from `ids`) if needed, and
    /// updates the emitter's stats.
    pub fn observe(&mut self, r: &SignalRecord, ids: &mut dyn FnMut() -> EmitterId) -> Observed {
        let closed = !r.open;
        let bw = r.bandwidth_hz().max(1.0);
        let c = r.center_hz();
        let mut folded = None;
        let mut left = None;
        let id = match self.by_record.get(&r.id) {
            // A record that closes wider than its emitter and covering it, on an emitter with only a few records, built
            // that emitter from its own early fragments (two tracks of a broadcast station that later merged): the
            // emitter takes its final shape.
            Some(&id)
                if closed
                    && self
                        .map
                        .get(&id)
                        .is_some_and(|e| !e.s.station && (e.s.records == 1 || (e.s.records <= 3 && Self::covers(r, &e.s)))) =>
            {
                let (to, f) = self.refit(r, id, c, bw);
                folded = f;
                to
            }
            // A record placed from its first rows that closes as something its emitter isn't (two pager channels keyed
            // together look like one wide signal at first; the record then closes on one of them): it leaves for the
            // emitter its final shape fits, or a new one, taking its votes with it. Left alone, such records drag an
            // emitter's centre between channels, and a decoder tuned there hears neither.
            Some(&id)
                if closed
                    && self.map.get(&id).is_some_and(|e| {
                        e.s.records > 1
                            && !Self::fits(&e.s, c, bw, r.bin_hz)
                            && !Self::contains(&e.s, c, bw, r.bin_hz)
                            && !Self::sideband(&e.s, c, bw, r.bin_hz)
                    }) =>
            {
                if let Some(e) = self.map.get_mut(&id) {
                    e.s.records = e.s.records.saturating_sub(1);
                    e.votes.forget(r.id);
                }
                left = Some(id);
                let to = self.find(c, bw, r.bin_hz, Some(id)).unwrap_or_else(|| {
                    let n = ids();
                    self.map.insert(n, Self::new_emitter(n, self.serial.clone(), r, c, bw));
                    n
                });
                if let Some(e) = self.map.get_mut(&to) {
                    if e.s.records > 0 && !e.s.station && !Self::contains(&e.s, c, bw, r.bin_hz) {
                        e.s.center_hz += (c - e.s.center_hz) * 0.1;
                        e.s.bandwidth_hz += (bw - e.s.bandwidth_hz) * 0.1;
                    }
                    e.s.records += 1;
                }
                self.by_record.insert(r.id, to);
                to
            }
            Some(&id) => id,
            None => {
                let id = self.find(c, bw, r.bin_hz, None).unwrap_or_else(|| {
                    let id = ids();
                    self.map.insert(id, Self::new_emitter(id, self.serial.clone(), r, c, bw));
                    id
                });
                // A record taken in because it lies inside the emitter is a partial view: it doesn't move the
                // emitter's centre or width. Nor does any record move a station, which sits on its carrier.
                let partial = self.map.get(&id).is_some_and(|e| {
                    e.s.records > 0 && (e.s.station || Self::contains(&e.s, c, bw, r.bin_hz) || Self::sideband(&e.s, c, bw, r.bin_hz))
                });
                if let Some(e) = self.map.get_mut(&id) {
                    e.s.records += 1;
                    if !partial {
                        // Slow average so one odd record doesn't drag the centre.
                        e.s.center_hz += (c - e.s.center_hz) * 0.1;
                        e.s.bandwidth_hz += (bw - e.s.bandwidth_hz) * 0.1;
                    }
                }
                self.by_record.insert(r.id, id);
                id
            }
        };
        if self.latest.is_none_or(|t| r.t_end > t) {
            self.latest = Some(r.t_end);
        }
        if let Some(e) = self.map.get_mut(&id) {
            let s = &mut e.s;
            s.last_seen = s.last_seen.max(r.t_end);
            s.first_seen = s.first_seen.min(r.t_start);
            s.peak_snr_db = s.peak_snr_db.max(r.max_snr_db);
            e.open_start = if closed { None } else { Some(e.open_start.map_or(r.t_start, |t| t.min(r.t_start))) };
            if closed {
                s.on_time_s += r.duration().as_secs_f64();
                if r.rows >= 2 || r.max_snr_db >= BLIP_SNR_DB {
                    s.solid_records += 1;
                }
                if e.recent.len() >= RECENT_RECORDS {
                    e.recent.pop_front();
                }
                e.recent.push_back((r.first_sample, r.end_sample));
            }
            s.confirmed |= s.solid_records >= CONFIRM_RECORDS
                || (r.duration().as_secs_f64() >= CONFIRM_DURATION_S && r.rows >= CONFIRM_ROWS)
                || r.max_snr_db >= CONFIRM_SNR_DB;
        }
        Observed { emitter: id, folded, left }
    }

    /// A record that was visible merged into another before it closed: it no longer counts.
    pub fn forget_record(&mut self, record: RecordId) {
        if let Some(id) = self.by_record.remove(&record)
            && let Some(e) = self.map.get_mut(&id)
        {
            e.s.records = e.s.records.saturating_sub(1);
        }
    }

    /// A record that started emitter `id` on its own has closed: if its final shape fits another emitter, fold `id`
    /// into that one; otherwise give `id` the final shape. Returns the record's emitter and the fold, if any.
    fn refit(&mut self, r: &SignalRecord, id: EmitterId, c: f64, bw: f64) -> (EmitterId, Option<(EmitterId, EmitterId)>) {
        let Some(to) = self.find(c, bw, r.bin_hz, Some(id)) else {
            if let Some(e) = self.map.get_mut(&id) {
                e.s.center_hz = c;
                e.s.bandwidth_hz = bw;
            }
            return (id, None);
        };
        if !self.fold(id, to) {
            return (id, None);
        }
        self.by_record.insert(r.id, to);
        if let Some(e) = self.map.get_mut(&to).filter(|e| !e.s.station) {
            e.s.center_hz += (c - e.s.center_hz) * 0.1;
            e.s.bandwidth_hz += (bw - e.s.bandwidth_hz) * 0.1;
        }
        (to, Some((id, to)))
    }

    /// Folds emitter `id` into `to`: its records (all of them), counts and times. False if either is gone.
    pub fn fold(&mut self, id: EmitterId, to: EmitterId) -> bool {
        if id == to || !self.map.contains_key(&to) {
            return false;
        }
        let Some(from) = self.map.remove(&id) else { return false };
        for v in self.by_record.values_mut() {
            if *v == id {
                *v = to;
            }
        }
        let Some(e) = self.map.get_mut(&to) else { return false };
        let (s, f) = (&mut e.s, &from.s);
        s.records += f.records;
        s.solid_records += f.solid_records;
        s.on_time_s += f.on_time_s;
        s.first_seen = s.first_seen.min(f.first_seen);
        s.last_seen = s.last_seen.max(f.last_seen);
        s.confirmed |= f.confirmed;
        s.peak_snr_db = s.peak_snr_db.max(f.peak_snr_db);
        s.station |= f.station;
        if s.headline.is_none() {
            s.headline = f.headline.clone();
        }
        e.active |= from.active;
        e.open_start = match (e.open_start, from.open_start) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut recent: Vec<(u64, u64)> = e.recent.drain(..).chain(from.recent).collect();
        recent.sort();
        recent.dedup();
        e.recent.extend(recent.into_iter().rev().take(RECENT_RECORDS).rev());
        true
    }

    fn new_emitter(id: EmitterId, serial: Arc<str>, r: &SignalRecord, c: f64, bw: f64) -> Emitter {
        Emitter {
            s: EmitterState {
                id,
                serial,
                center_hz: c,
                bandwidth_hz: bw,
                bin_hz: r.bin_hz,
                first_seen: r.t_start,
                last_seen: r.t_end,
                records: 0,
                solid_records: 0,
                on_time_s: 0.0,
                confirmed: false,
                peak_snr_db: r.max_snr_db,
                station: false,
                headline: None,
            },
            open_start: None,
            active: false,
            recent: VecDeque::new(),
            votes: Votes::default(),
        }
    }

    /// Recomputes `active` from the currently open records; now and then drops stale unconfirmed emitters, confirmed
    /// ones unseen for [`EMITTER_WINDOW`], and the least recently seen past [`MAX_EMITTERS`]. Returns the ids dropped.
    pub fn mark_active(&mut self, open: &[RecordId]) -> Vec<EmitterId> {
        for e in self.map.values_mut() {
            e.active = false;
        }
        for r in open {
            if let Some(e) = self.by_record.get(r).and_then(|id| self.map.get_mut(id)) {
                e.active = true;
            }
        }
        self.calls = self.calls.wrapping_add(1);
        let mut gone = Vec::new();
        if let Some(now) = self.latest.filter(|_| self.calls.is_multiple_of(PRUNE_EVERY)) {
            let stale = |e: &Emitter| {
                let idle = now.duration_since(e.s.last_seen).unwrap_or_default();
                !e.active && ((!e.s.confirmed && idle > UNCONFIRMED_TTL) || idle > EMITTER_WINDOW)
            };
            gone.extend(self.map.values().filter(|e| stale(e)).map(|e| e.s.id));
        }
        if self.map.len() > MAX_EMITTERS {
            let mut order: Vec<(bool, SystemTime, EmitterId)> = self.map.values().map(|e| (e.active, e.s.last_seen, e.s.id)).collect();
            order.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            gone.extend(order[MAX_EMITTERS..].iter().map(|o| o.2));
        }
        if !gone.is_empty() {
            gone.sort();
            gone.dedup();
            self.map.retain(|id, _| gone.binary_search(id).is_err());
            let map = &self.map;
            self.by_record.retain(|_, e| map.contains_key(e));
        }
        gone
    }

    /// Takes record `record`'s claims (all of them, as they now stand) into its emitter's vote, and the vote into the
    /// headline (spec §4.4: an emitter's label is computed from its records' claims). `station` follows a headline that
    /// names a broadcast station; a station sits on its carrier, where a look measured it, and is its channel wide.
    /// An emitter just named a station takes in the emitters its fragments made before (see [`Self::gather`]): the
    /// folds, `(gone, into)`.
    pub fn note_claims(&mut self, id: EmitterId, record: RecordId, claims: &[Claim]) -> Vec<(EmitterId, EmitterId)> {
        let Some(e) = self.map.get_mut(&id) else { return Vec::new() };
        let v = &mut e.votes;
        // The newest decoder claim stands for the emitter, unless it names the same identity as the one standing with
        // fewer of its fields (a site heard again before its network broadcast came round).
        if let Some(c) =
            claims.iter().filter(|c| matches!(c.by, Evidence::Decoder { .. })).max_by(|a, b| a.confidence.total_cmp(&b.confidence))
        {
            let thinner = match (&v.decoded, identity(c)) {
                (Some((r, old)), Some(new)) if *r != record => identity(old).is_some_and(|o| o.same(new) && o.known() > new.known()),
                _ => false,
            };
            if !thinner {
                v.decoded = Some((record, c.clone()));
            }
        }
        for c in claims {
            let Evidence::SyncBank { syncs, frames_tried, frames_decoded, .. } = &c.by else { continue };
            let strong = *syncs >= STRONG_SYNCS || (*frames_tried > 0 && 2 * frames_decoded >= *frames_tried);
            let vote = v.synced.entry(c.label.clone()).or_insert_with(|| SyncVote { records: Vec::new(), strong: false, best: c.clone() });
            if !vote.records.contains(&record) {
                vote.records.push(record);
                if vote.records.len() > VOTE_RECORDS {
                    vote.records.remove(0);
                }
            }
            vote.strong |= strong;
            if c.confidence > vote.best.confidence {
                vote.best = c.clone();
            }
        }
        for c in claims.iter().filter(|c| api::records::is_analog_shape(c)) {
            let vote = v.shaped.entry(c.label.clone()).or_insert_with(|| SyncVote { records: Vec::new(), strong: false, best: c.clone() });
            if !vote.records.contains(&record) {
                vote.records.push(record);
                if vote.records.len() > VOTE_RECORDS {
                    vote.records.remove(0);
                }
            }
            if c.confidence > vote.best.confidence {
                vote.best = c.clone();
            }
        }
        if let Some(c) = claims.iter().find_map(|c| match c.by {
            Evidence::Features { carrier_hz: Some(c), .. } if c.is_finite() => Some(c),
            _ => None,
        }) {
            v.carriers.retain(|(r, _)| *r != record);
            v.carriers.push_back((record, c));
            while v.carriers.len() > CARRIERS {
                v.carriers.pop_front();
            }
        }
        e.s.headline = v.headline();
        let was = e.s.station;
        e.s.station = e.s.headline.as_ref().is_some_and(|h| h.label.starts_with("FM broadcast"));
        if !e.s.station {
            return Vec::new();
        }
        if let Some(c) = e.votes.carrier() {
            e.s.center_hz = c;
        }
        e.s.bandwidth_hz = 2.0 * STATION_HALF_HZ;
        if was { Vec::new() } else { self.gather(id) }
    }

    /// Station `id` takes in what is part of it: a second emitter on the same carrier (two began before either was
    /// named; the busier stays), then every other emitter inside its channel or on its HD Radio sidebands. A weak
    /// station's fragments each made an emitter of their own before any look named one of them: on air at 99.1 MHz,
    /// a dozen per receiver.
    fn gather(&mut self, id: EmitterId) -> Vec<(EmitterId, EmitterId)> {
        let mut out = Vec::new();
        let Some(s) = self.state(id).cloned() else { return out };
        let mut keep = id;
        let twin = self
            .map
            .values()
            .map(|e| &e.s)
            .filter(|o| o.id != id && o.station && (o.center_hz - s.center_hz).abs() <= STATION_HALF_HZ / 2.0)
            .max_by_key(|o| o.records)
            .map(|o| (o.id, o.records));
        if let Some((t, n)) = twin {
            let (from, to) = if n >= s.records { (id, t) } else { (t, id) };
            if self.fold(from, to) {
                out.push((from, to));
                keep = to;
            }
        }
        let Some(k) = self.state(keep).cloned() else { return out };
        let pieces: Vec<EmitterId> = self
            .map
            .values()
            .map(|e| &e.s)
            .filter(|o| {
                o.id != keep
                    && !o.station
                    && (Self::contains(&k, o.center_hz, o.bandwidth_hz, o.bin_hz)
                        || Self::sideband(&k, o.center_hz, o.bandwidth_hz, o.bin_hz))
            })
            .map(|o| o.id)
            .collect();
        for p in pieces {
            if self.fold(p, keep) {
                out.push((p, keep));
            }
        }
        out
    }

    /// What the receiver's own artefacts say about a record at `r`: at DC (on a zero-IF radio), on the spur grid, the
    /// mirror image of another line about the tuner centre, or one line of a comb of always-on lines. Lines only
    /// (narrower than 5 kHz); nothing for anything wider.
    pub fn position_claims(&self, r: &SignalRecord, zero_if: bool, spur_grid_hz: Option<f64>) -> Vec<Claim> {
        let (c, bw, tuner) = (r.center_hz(), r.bandwidth_hz(), r.tuner_hz);
        let mut out = Vec::new();
        if bw >= LINE_HZ {
            return out;
        }
        if zero_if && (c - tuner).abs() < DC_HZ {
            out.push(Claim::new("At the tuner centre (DC)", POSITION_CONFIDENCE, Evidence::Position(PositionKind::Dc)));
        }
        if let Some(g) = spur_grid_hz.filter(|g| *g > 0.0)
            && (c - (c / g).round() * g).abs() < GRID_HZ
        {
            out.push(Claim::new(
                "On the radio's spur grid",
                POSITION_CONFIDENCE,
                Evidence::Position(PositionKind::SpurGrid { grid_hz: g }),
            ));
        }
        let own = self.by_record.get(&r.id).copied();
        let lines = |min_records: u32, min_duty: f64| -> Vec<&EmitterState> {
            self.map
                .values()
                .filter(|e| {
                    Some(e.s.id) != own
                        && e.s.bandwidth_hz < LINE_HZ
                        && e.s.records >= min_records
                        && e.duty() >= min_duty
                        && (e.s.center_hz - c).abs() > LINE_TOL_HZ
                })
                .map(|e| &e.s)
                .collect()
        };
        // Image pairs sit symmetrically about the tuner centre and move with it (the radio's own, from its IF or
        // sampling).
        let offset = c - tuner;
        if offset.abs() > LINE_TOL_HZ
            && let Some(m) = lines(2, 0.0).into_iter().find(|e| (e.center_hz - (tuner - offset)).abs() <= LINE_TOL_HZ)
        {
            out.push(Claim::new("Internal spur (image)", CONTEXT_CONFIDENCE, Evidence::Position(PositionKind::Image { of: m.id })));
        }
        // A comb is made of lines that are always there (a clock's harmonics); a busy site's brief blips fall on a
        // common spacing by chance and must not count. Each other line proposes a spacing; the spacing most lines fit
        // wins.
        let members: Vec<f64> = lines(COMB_MEMBER_RECORDS, COMB_MIN_DUTY).into_iter().map(|e| e.center_hz).collect();
        let mut best: Option<(usize, f64)> = None;
        for l in &members {
            let d = (l - c).abs();
            if !(COMB_SPACING_HZ.0..=COMB_SPACING_HZ.1).contains(&d) {
                continue;
            }
            let n = 1 + members
                .iter()
                .filter(|m| {
                    let k = ((*m - c) / d).round();
                    k != 0.0 && (*m - (c + k * d)).abs() <= LINE_TOL_HZ
                })
                .count();
            if n >= COMB_LINES && best.is_none_or(|(bn, bd)| n > bn || (n == bn && d < bd)) {
                best = Some((n, d));
            }
        }
        if let Some((lines, spacing_hz)) = best {
            let label = if spacing_hz >= 1e6 {
                format!("Spur comb {:.2} MHz ({lines} lines)", spacing_hz / 1e6)
            } else {
                format!("Spur comb {:.0} kHz ({lines} lines)", spacing_hz / 1e3)
            };
            out.push(Claim::new(label, CONTEXT_CONFIDENCE, Evidence::Position(PositionKind::Comb { spacing_hz, lines: lines as u32 })));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{Role, SegmentId, SourceId};

    fn rec(id: u64, mhz: f64, bw_khz: f64, t: u64) -> SignalRecord {
        let (c, bw) = (mhz * 1e6, bw_khz * 1e3);
        SignalRecord {
            id: RecordId(id),
            source: SourceId(1),
            serial: "T".into(),
            role: Role::Live,
            segment: SegmentId(1),
            tuner_hz: 0.0,
            first_sample: t * 1000,
            end_sample: t * 1000 + 30,
            t_start: SystemTime::UNIX_EPOCH + Duration::from_secs(t),
            t_end: SystemTime::UNIX_EPOCH + Duration::from_secs(t) + Duration::from_millis(30),
            f_lo_hz: c - bw / 2.0,
            f_hi_hz: c + bw / 2.0,
            bin_hz: 610.0,
            peak_dbfs: -60.0,
            mean_dbfs: -62.0,
            mean_snr_db: 20.0,
            max_snr_db: 22.0,
            rows: 2,
            open: false,
            claims: vec![],
            emitter: None,
            tags: vec![],
            notes: vec![],
            iq_clip: None,
            slice_evicted: false,
        }
    }

    fn open(mut r: SignalRecord) -> SignalRecord {
        r.open = true;
        r
    }

    struct Book {
        b: EmitterBook,
        next: u64,
    }

    impl Book {
        fn new() -> Book {
            Book { b: EmitterBook::new("T".into()), next: 0 }
        }
        fn observe(&mut self, r: &SignalRecord) -> EmitterId {
            self.observed(r).emitter
        }
        fn observed(&mut self, r: &SignalRecord) -> Observed {
            let next = &mut self.next;
            self.b.observe(r, &mut || {
                *next += 1;
                EmitterId(*next)
            })
        }
        fn of(&self, record: u64) -> EmitterId {
            self.b.emitter_of(RecordId(record)).unwrap()
        }
        fn get(&self, id: EmitterId) -> Option<&EmitterState> {
            self.b.state(id)
        }
    }

    fn fm_station() -> Claim {
        Claim::new(
            "FM broadcast (stereo, RDS)",
            0.9,
            Evidence::Decoder { mode: "WFM".into(), locked: true, summary: "RDS".into(), identity: None },
        )
    }

    #[test]
    fn bursts_on_one_channel_form_one_emitter_neighbours_stay_apart() {
        let mut b = Book::new();
        for i in 0..40 {
            // 30 ms TDMA bursts with a little centre jitter...
            b.observe(&rec(i, 856.3375 + (i % 3) as f64 * 0.0008, 9.0, i));
            // ...and the adjacent 12.5 kHz channel.
            b.observe(&rec(1000 + i, 856.3500, 9.0, i));
        }
        // A wideband signal at the same centre is something else.
        b.observe(&rec(5000, 856.3375, 200.0, 50));
        assert_eq!(b.b.len(), 3);
        let e = b.get(b.of(5)).unwrap();
        assert_eq!(e.records, 40);
        assert!((e.on_time_s - 1.2).abs() < 1e-6);
    }

    #[test]
    fn coarse_bins_dont_split_a_signal() {
        // Survey rows: 54 kHz bins. One FM station comes out 1, 2 or 4 bins wide with its centre moving.
        let bin = 54.1e3;
        let mut b = Book::new();
        for (i, (off, bins)) in [(0.0, 1.0), (-0.5, 2.0), (0.0, 4.0), (1.0, 1.0), (0.5, 2.0)].iter().enumerate() {
            let mut r = rec(i as u64, 96.3 + off * bin / 1e6, bins * bin / 1e3, i as u64 * 10);
            r.bin_hz = bin;
            b.observe(&r);
        }
        assert_eq!(b.b.len(), 1);
    }

    #[test]
    fn record_placed_from_narrow_onset_joins_on_close() {
        let mut b = Book::new();
        for i in 0..3 {
            b.observe(&rec(i, 856.3375, 9.0, i));
        }
        // First rows of a key-up: narrow, but inside the channel's confirmed emitter, so it joins it straight away...
        let mut r = open(rec(10, 856.3380, 1.8, 10));
        b.observe(&r);
        assert_eq!(b.b.len(), 1);
        // ...and stays there once it closes at full width.
        r.f_lo_hz = 856.3375e6 - 4.5e3;
        r.f_hi_hz = 856.3375e6 + 4.5e3;
        r.open = false;
        let id = b.observe(&r);
        assert_eq!(b.b.len(), 1);
        assert_eq!(b.of(10), id);
        assert_eq!(b.get(id).unwrap().records, 4);
    }

    #[test]
    fn pieces_of_a_named_station_join_it() {
        let mut b = Book::new();
        let st = b.observe(&rec(1, 93.7, 130.0, 1));
        for i in 2..4 {
            b.observe(&rec(i, 93.7, 130.0, i));
        }
        b.b.note_claims(st, RecordId(0), &[fm_station()]);
        assert!(b.get(st).unwrap().confirmed && b.get(st).unwrap().station);
        // Its carrier alone (quiet programme), and a piece of its swing.
        assert_eq!(b.observe(&rec(10, 93.7004, 10.0, 10)), st);
        assert_eq!(b.observe(&rec(11, 93.6400, 40.0, 11)), st);
        // A station 200 kHz away is not part of it.
        assert_ne!(b.observe(&rec(12, 93.9, 130.0, 12)), st);
    }

    /// What a look at a piece of a station says: its pilot, and its carrier (`carrier_mhz`).
    fn pilot(carrier_mhz: f64) -> Claim {
        Claim::new(
            "FM broadcast (stereo)",
            0.6,
            Evidence::Features {
                bandwidth_hz: 280e3,
                snr_db: 12.0,
                duty: 1.0,
                envelope_cv: Some(0.05),
                symbol_rate: None,
                fsk_levels: None,
                deviation_hz: Some(60e3),
                carrier_hz: Some(carrier_mhz * 1e6),
            },
        )
    }

    #[test]
    fn a_weak_stations_fragments_fold_into_it_once_one_is_named() {
        // A weak station at 99.1 MHz is seen as fragments of its swing, up to 70 kHz either side, each too unlike the
        // others to join them (as on air: a dozen emitters per receiver).
        let mut b = Book::new();
        let pieces: Vec<EmitterId> = [(99.030, 9.0), (99.047, 30.0), (99.068, 6.0), (99.105, 40.0), (99.148, 8.0), (99.163, 15.0)]
            .iter()
            .enumerate()
            .map(|(i, (mhz, khz))| b.observe(&rec(i as u64 + 1, *mhz, *khz, i as u64)))
            .collect();
        assert_eq!(b.b.len(), 6);
        // A neighbour 200 kHz up swings through 99.24 too.
        let neighbour = b.observe(&rec(20, 99.240, 20.0, 20));
        // One look, cut at station width around a fragment, finds the pilot and measures the carrier.
        let named = pieces[1];
        let folds = b.b.note_claims(named, RecordId(2), &[pilot(99.1003)]);
        let e = b.get(named).unwrap();
        assert!(e.station && (e.center_hz - 99.1003e6).abs() < 1.0 && e.bandwidth_hz == 200e3, "{e:?}");
        assert_eq!(folds.len(), 5, "{folds:?}");
        assert!(folds.iter().all(|&(gone, into)| into == named && gone != neighbour));
        assert_eq!(b.b.len(), 2);
        assert_eq!(b.of(1), named);
        // Later fragments join it straight away; the neighbour's don't.
        assert_eq!(b.observe(&rec(30, 99.035, 5.0, 30)), named);
        assert_eq!(b.observe(&rec(31, 99.245, 12.0, 31)), neighbour);
    }

    #[test]
    fn a_stations_hd_radio_sidebands_are_part_of_it() {
        let mut b = Book::new();
        let st = b.observe(&rec(1, 105.900, 166.0, 1));
        b.b.note_claims(st, RecordId(1), &[pilot(105.9)]);
        // Extended hybrid sidebands, as on air: 105.700–105.786 and 106.014–106.100.
        assert_eq!(b.observe(&rec(2, 105.743, 86.0, 2)), st);
        assert_eq!(b.observe(&rec(3, 106.057, 86.0, 3)), st);
        // The core and a sideband in one record.
        assert_eq!(b.observe(&rec(4, 105.842, 283.0, 4)), st);
        // The swing's edge and a sideband's inner part (as on air at 97.7 MHz: 58 kHz, 85 kHz out).
        assert_eq!(b.observe(&rec(6, 105.815, 58.0, 6)), st);
        assert_eq!(b.observe(&rec(7, 105.986, 59.0, 7)), st);
        assert_eq!(b.get(st).unwrap().center_hz, 105.9e6, "a station's records don't drag it off its carrier");
        // A narrow fragment where a sideband would be is a neighbour's, not this station's.
        assert_ne!(b.observe(&rec(5, 106.070, 15.0, 5)), st);
    }

    #[test]
    fn a_station_sits_on_the_median_of_its_looks_carriers() {
        let mut b = Book::new();
        let st = b.observe(&rec(1, 105.900, 166.0, 1));
        // The first look was cut on a sideband and measured the carrier 9 kHz high; its sideband still joins.
        b.b.note_claims(st, RecordId(1), &[pilot(105.909)]);
        assert_eq!(b.observe(&rec(2, 105.743, 86.0, 2)), st);
        for (r, mhz) in [(3, 105.9008), (4, 105.8994), (5, 105.9011)] {
            b.observe(&rec(r, 105.9, 160.0, r));
            b.b.note_claims(st, RecordId(r), &[pilot(mhz)]);
        }
        let c = b.get(st).unwrap().center_hz;
        assert!((c - 105.9e6).abs() < 1.5e3, "centre {c}");
    }

    #[test]
    fn two_emitters_named_for_one_station_become_one() {
        let mut b = Book::new();
        let a = b.observe(&rec(1, 105.890, 181.0, 1));
        b.observe(&rec(2, 105.890, 181.0, 2));
        let c = b.observe(&rec(3, 105.700, 600.0, 3));
        assert_ne!(a, c);
        b.b.note_claims(a, RecordId(1), &[pilot(105.9)]);
        let folds = b.b.note_claims(c, RecordId(3), &[pilot(105.901)]);
        assert_eq!(folds, vec![(c, a)], "the busier stays");
        assert_eq!(b.of(3), a);
    }

    #[test]
    fn a_station_built_from_its_fragments_takes_its_final_shape() {
        let mut b = Book::new();
        // Two pieces of one station seen while still open...
        let a = b.observe(&open(rec(1, 97.64, 47.0, 1)));
        assert_eq!(b.observe(&open(rec(2, 97.645, 45.0, 1))), a);
        assert_eq!(b.get(a).unwrap().records, 2);
        // ...then their tracks merged (record 2 went into 1) and the record closes at full width.
        b.b.forget_record(RecordId(2));
        assert_eq!(b.get(a).unwrap().records, 1);
        let mut whole = rec(1, 97.7034, 207.0, 1);
        whole.t_end = whole.t_start + Duration::from_secs(60);
        assert_eq!(b.observe(&whole), a);
        let e = b.get(a).unwrap();
        assert!((e.center_hz - 97.7034e6).abs() < 2e3 && e.bandwidth_hz > 200e3, "{} {}", e.center_hz, e.bandwidth_hz);
    }

    #[test]
    fn a_record_whose_emitter_fits_another_on_closing_folds_it_in() {
        let mut b = Book::new();
        let main = b.observe(&rec(1, 856.3375, 9.0, 1));
        // A key-up seen first 5 kHz off to one side, outside the channel: its own emitter...
        let side = b.observe(&open(rec(2, 856.3425, 3.0, 2)));
        assert_ne!(side, main);
        // ...that closes at the channel's full width.
        let o = b.observed(&rec(2, 856.3375, 9.0, 2));
        assert_eq!(o, Observed { emitter: main, folded: Some((side, main)), left: None });
        assert!(b.get(side).is_none());
        assert_eq!(b.get(main).unwrap().records, 2);
    }

    #[test]
    fn a_record_that_joined_while_wide_and_closed_on_one_channel_leaves_for_it() {
        let mut b = Book::new();
        // Two pager channels 50 kHz apart keyed together: three records catch both as one wide signal.
        let both = b.observe(&rec(1, 929.6375, 54.0, 1));
        for i in 2..4 {
            assert_eq!(b.observe(&rec(i, 929.6375, 54.0, i)), both);
        }
        // A key-up seen first as the same wide signal joins that emitter...
        let mut r = open(rec(10, 929.6375, 54.0, 10));
        assert_eq!(b.observe(&r), both);
        // ...and closes on 929.6125 alone: it leaves for an emitter of its own channel.
        (r.f_lo_hz, r.f_hi_hz, r.open) = (929.6125e6 - 7.5e3, 929.6125e6 + 7.5e3, false);
        let o = b.observed(&r);
        assert_eq!(o.left, Some(both));
        assert_ne!(o.emitter, both);
        assert_eq!(b.get(both).unwrap().records, 3);
        let mine = b.get(o.emitter).unwrap();
        assert!((mine.center_hz - 929.6125e6).abs() < 1e3, "{}", mine.center_hz);
        // The next record on that channel joins it, not the wide one.
        assert_eq!(b.observe(&rec(11, 929.6125, 15.0, 11)), o.emitter);
    }

    #[test]
    fn one_tone_of_a_pager_joins_its_channel() {
        let mut b = Book::new();
        // A FLEX channel at full width (both tones)...
        for i in 0..3 {
            b.observe(&rec(i, 929.6125, 14.0, i));
        }
        // ...then records that caught one tone each, 4.7 kHz either side.
        let a = b.observe(&rec(10, 929.6078, 3.0, 10));
        let c = b.observe(&rec(11, 929.6172, 3.0, 11));
        assert_eq!((a, c), (b.of(0), b.of(0)));
        assert_eq!(b.b.len(), 1);
        // ...without shrinking it to a tone's width.
        assert!(b.get(a).unwrap().bandwidth_hz > 13e3, "{}", b.get(a).unwrap().bandwidth_hz);
        // A narrow line 20 kHz away is not part of it.
        b.observe(&rec(12, 929.6325, 3.0, 12));
        assert_eq!(b.b.len(), 2);
    }

    fn position(b: &Book, mhz: f64, khz: f64, tuner_mhz: f64) -> Vec<String> {
        let mut r = rec(999_999, mhz, khz, 0);
        r.tuner_hz = tuner_mhz * 1e6;
        b.b.position_claims(&r, false, None).into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn image_pairs_and_combs_are_position_claims() {
        let mut b = Book::new();
        // Lines at the tuner (240 MHz) ± 96, ± 296 and ± 392 kHz, as seen on air...
        let mut n = 0;
        for off in [-392.0, -296.0, -96.0, 96.0, 296.0, 392.0] {
            for _ in 0..3 {
                b.observe(&rec(n, 240.0 + off / 1e3, 1.8, n));
                n += 1;
            }
        }
        // ...and a comb at 750 kHz, each line steady and seen often enough to be a member.
        for mhz in [94.4992, 95.2492, 95.9992, 97.4992] {
            for i in 0..COMB_MEMBER_RECORDS as u64 {
                let mut r = rec(n, mhz, 1.8, 100 + i);
                r.t_end = r.t_start + Duration::from_millis(900);
                b.observe(&r);
                n += 1;
            }
        }
        // A lone line, and a voice channel.
        b.observe(&rec(n, 241.0, 1.8, n));
        b.observe(&rec(n + 1, 241.0, 1.8, n + 1));
        b.observe(&rec(n + 2, 240.2, 12.5, n + 2));
        assert_eq!(position(&b, 240.096, 1.8, 240.0), ["Internal spur (image)"]);
        assert_eq!(position(&b, 239.608, 1.8, 240.0), ["Internal spur (image)"]);
        assert_eq!(position(&b, 95.2492, 1.8, 98.0), ["Spur comb 750 kHz (4 lines)"]);
        assert!(position(&b, 241.0, 1.8, 240.0).is_empty());
        assert!(position(&b, 240.2, 12.5, 240.0).is_empty(), "not a line");
        // Four brief blips on a 25 kHz grid (a trunked site's leftovers) are not a comb: members must be persistent.
        for mhz in [852.325, 852.35, 852.375, 852.4] {
            for _ in 0..3 {
                b.observe(&rec(n, mhz, 3.0, n));
                n += 1;
            }
        }
        assert!(position(&b, 852.425, 3.0, 855.5).is_empty(), "blips on a grid");
        // Nor are blips seen often enough to pass the record count: a few tenths of the time on air.
        for (k, mhz) in [853.2, 853.2625, 853.325, 853.3875].into_iter().enumerate() {
            for i in 0..(COMB_MEMBER_RECORDS as u64 + 4) {
                let mut r = rec(n, mhz, 3.0, 200 + k as u64 + i * 10);
                r.t_end = r.t_start + Duration::from_millis(400);
                b.observe(&r);
                n += 1;
            }
        }
        assert!(position(&b, 853.45, 3.0, 855.5).is_empty(), "frequent blips on a raster");
        // An image claim names the line it mirrors.
        let mut r = rec(999_998, 240.096, 1.8, 0);
        r.tuner_hz = 240e6;
        let claims = b.b.position_claims(&r, false, None);
        let Evidence::Position(PositionKind::Image { of }) = claims[0].by else { panic!("{claims:?}") };
        assert!((b.get(of).unwrap().center_hz - 239.904e6).abs() < 1e3);
        assert!(!claims[0].is_decoded() && claims[0].confidence <= 0.5);
    }

    #[test]
    fn grid_and_dc_positions_are_suspicions_only() {
        let b = Book::new();
        let mut r = rec(1, 920.0004, 2.4, 0);
        r.tuner_hz = 924e6;
        let claims = b.b.position_claims(&r, false, Some(5e6));
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].by, Evidence::Position(PositionKind::SpurGrid { grid_hz: 5e6 }));
        assert!(claims[0].confidence <= POSITION_CONFIDENCE);
        assert!(b.b.position_claims(&r, false, None).is_empty());
        let mut d = rec(2, 924.0003, 1.0, 0);
        d.tuner_hz = 924e6;
        assert!(b.b.position_claims(&d, false, None).is_empty(), "DC is an artefact only on a zero-IF radio");
        assert_eq!(b.b.position_claims(&d, true, None)[0].by, Evidence::Position(PositionKind::Dc));
        let mut wide = rec(3, 920.0, 12.5, 0);
        wide.tuner_hz = 924e6;
        assert!(b.b.position_claims(&wide, true, Some(5e6)).is_empty(), "a channel on a grid frequency is not a spur");
    }

    #[test]
    fn noise_blips_stay_unconfirmed_and_expire() {
        let mut b = Book::new();
        let mut blip = rec(1, 300.0, 54.1, 0);
        blip.max_snr_db = 9.0;
        blip.rows = 1;
        let id = b.observe(&blip);
        // Weak one-row blips landing on the same patch again don't confirm it.
        for i in 0..5 {
            let mut again = blip.clone();
            again.id = RecordId(100 + i);
            assert_eq!(b.observe(&again), id);
        }
        assert!(!b.get(id).unwrap().confirmed);
        // A strong one is confirmed at once.
        let sid = b.observe(&rec(2, 400.0, 54.1, 0));
        assert!(b.get(sid).unwrap().confirmed);
        // Time passes (records elsewhere); the blip never recurs and is dropped.
        let mut later = rec(3, 400.0, 54.1, 200);
        later.max_snr_db = 9.0;
        b.observe(&later);
        let mut gone = vec![];
        for _ in 0..PRUNE_EVERY {
            gone.extend(b.b.mark_active(&[]));
        }
        assert_eq!(gone, [id]);
        assert!(b.get(id).is_none());
        assert!(b.get(sid).is_some());
    }

    #[test]
    fn a_loaded_book_keeps_its_emitters_and_their_ids() {
        let mut b = Book::new();
        for i in 0..3 {
            b.observe(&rec(i, 856.3375, 9.0, i));
        }
        let states: Vec<EmitterState> = b.b.map.values().map(|e| e.s.clone()).collect();
        let mut again = Book { b: EmitterBook::load("T".into(), states), next: 100 };
        let id = again.observe(&rec(50, 856.3376, 9.0, 50));
        assert_eq!(id, b.of(0), "a new record joins the loaded emitter");
        assert_eq!(again.get(id).unwrap().records, 4);
    }

    #[test]
    fn a_decoded_headline_beats_a_more_confident_guess() {
        let mut b = Book::new();
        let id = b.observe(&rec(1, 851.0125, 9.0, 1));
        let sync = Claim::new("P25", 0.95, Evidence::SyncBank { protocol: "P25".into(), syncs: 5, frames_tried: 0, frames_decoded: 0 });
        b.b.note_claims(id, RecordId(1), std::slice::from_ref(&sync));
        assert_eq!(b.get(id).unwrap().headline.as_ref(), Some(&sync), "five syncs are strong");
        let dec = Claim::new(
            "P25 Phase 1",
            0.7,
            Evidence::Decoder { mode: "P25".into(), locked: true, summary: "NAC 0x340".into(), identity: None },
        );
        b.b.note_claims(id, RecordId(2), std::slice::from_ref(&dec));
        assert_eq!(b.get(id).unwrap().headline.as_ref(), Some(&dec));
        b.b.note_claims(id, RecordId(3), &[sync]);
        assert_eq!(b.get(id).unwrap().headline.as_ref(), Some(&dec), "a sync claim does not replace a decoder lock");
    }

    #[test]
    fn a_protocol_is_believed_from_two_records_or_one_strong_one() {
        let mut b = Book::new();
        let id = b.observe(&rec(1, 857.2375, 9.0, 1));
        let weak = |syncs| Claim::new("DMR", 0.8, Evidence::SyncBank { protocol: "DMR".into(), syncs, frames_tried: 4, frames_decoded: 0 });
        b.b.note_claims(id, RecordId(1), &[weak(2)]);
        assert_eq!(b.get(id).unwrap().headline, None, "one record with two syncs is not enough");
        b.b.note_claims(id, RecordId(1), &[weak(2)]);
        assert_eq!(b.get(id).unwrap().headline, None, "the same record again is still one record");
        b.b.note_claims(id, RecordId(2), &[weak(3)]);
        assert_eq!(b.get(id).unwrap().headline.as_ref().map(|c| c.label.as_str()), Some("DMR"));
        let id2 = b.observe(&rec(3, 858.8125, 9.0, 3));
        let decoded = Claim::new("NXDN", 0.8, Evidence::SyncBank { protocol: "NXDN".into(), syncs: 2, frames_tried: 2, frames_decoded: 1 });
        b.b.note_claims(id2, RecordId(3), &[decoded]);
        assert_eq!(b.get(id2).unwrap().headline.as_ref().map(|c| c.label.as_str()), Some("NXDN"), "half its frames decoded");
    }

    #[test]
    fn a_thinner_identity_of_the_same_site_does_not_replace_a_fuller_one() {
        let mut b = Book::new();
        let id = b.observe(&rec(1, 859.7677, 9.0, 1));
        let p25 = |wacn: Option<u32>, site: Option<u8>| {
            Claim::new(
                "P25 control",
                0.99,
                Evidence::Decoder {
                    mode: "P25".into(),
                    locked: true,
                    summary: String::new(),
                    identity: Some(api::Identity::P25 { nac: 0x34D, wacn, system: wacn.map(|_| 0x348), rfss: site.map(|_| 1), site }),
                },
            )
        };
        b.b.note_claims(id, RecordId(1), &[p25(Some(0xBEE00), Some(13))]);
        b.b.note_claims(id, RecordId(2), &[p25(None, None)]);
        assert_eq!(b.get(id).unwrap().headline, Some(p25(Some(0xBEE00), Some(13))));
        b.b.note_claims(id, RecordId(2), &[p25(Some(0xBEE00), Some(14))]);
        assert_eq!(b.get(id).unwrap().headline, Some(p25(Some(0xBEE00), Some(14))), "another site replaces it");
    }
}
