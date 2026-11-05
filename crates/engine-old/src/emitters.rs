//! Groups signal records into emitters: persistent transmitters on a channel.
//!
//! A record joins the emitter whose centre is within tolerance and whose
//! bandwidth is comparable; otherwise it starts a new one. A record is placed
//! from its first rows, so when one that started an emitter of its own closes,
//! it is matched again with its final shape. Emitters keep activity stats and
//! a tally of what their records were classified as, and can carry a
//! classification of their own (made over a window spanning many short
//! bursts, which single records can't support).
//!
//! An emitter counts as `confirmed` once it can't be a noise blip; the
//! unconfirmed ones are dropped if they don't recur.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use detect::Classification;

use crate::SignalRecord;

/// Most emitters kept per source; the least recently seen inactive ones go first.
const MAX_EMITTERS: usize = 5000;

/// An emitter is confirmed once it has had this many records that weren't
/// weak one-row blips (rows >= 2 or SNR >= `BLIP_SNR_DB`): a survey pass
/// every 0.3 s puts noise blips on any one patch of band several times a
/// minute...
const CONFIRM_RECORDS: u32 = 3;
const BLIP_SNR_DB: f32 = 12.0;
/// ...or one record lasting this long over at least `CONFIRM_ROWS` rows (two
/// survey passes in a row can still be noise)...
const CONFIRM_DURATION_S: f64 = 0.5;
const CONFIRM_ROWS: u32 = 3;
/// ...or one this far above the floor.
const CONFIRM_SNR_DB: f32 = 15.0;
/// Unconfirmed emitters not seen again within this are dropped.
const UNCONFIRMED_TTL: Duration = Duration::from_secs(120);
/// Rows with bins this wide or wider (the survey's) match records by overlap.
const COARSE_BIN_HZ: f64 = 10e3;
/// Calls to `mark_active` between sweeps for stale unconfirmed emitters.
const PRUNE_EVERY: u32 = 64;

/// See `Emitter::label`: labels fixed by where a signal sits rather than
/// by its modulation. Matched by prefix ("Internal spur (image)", "Spur
/// comb 750 kHz").
const STICKY_LABELS: [&str; 3] = ["Internal spur", "Spur comb", "DC offset"];

/// A label fixed by position (see `STICKY_LABELS`).
/// Share of an emitter's analysed time a modulation-describing label needs
/// to beat the undescriptive ones (see `Emitter::majority_feature_label`).
const DESCRIBED_SHARE: f32 = 0.25;

/// Feature labels that say nothing about the modulation: what a look at a
/// keyed-down, idle or too-short stretch of any transmitter gives.
fn describes_modulation(label: &str) -> bool {
    !matches!(label, "Carrier" | "Unknown" | "Wideband") && !label.ends_with(" Carrier") && !label.ends_with(" Unknown")
}

pub fn is_sticky(label: &str) -> bool {
    STICKY_LABELS.iter().any(|s| label.starts_with(s))
}

/// Sticky labels that only make sense on a line (see `spur_context`).
fn is_line_label(label: &str) -> bool {
    label.starts_with("Internal spur (image)") || label.starts_with("Spur comb")
}

/// Narrower than this, a signal is a line: a carrier, a spur or a birdie.
const LINE_HZ: f64 = 5e3;
/// Lines this close in frequency are the same line (a bin or two).
const LINE_TOL_HZ: f64 = 1.5e3;
/// A comb needs this many lines at one spacing, this one included (with
/// twenty-odd lines in a 10 MHz span, three at some common spacing is
/// coincidence about once per line; four is not)...
const COMB_LINES: usize = 4;
/// Records a line needs before it can be a comb member (persistent, not
/// a blip: the radio's own 750 kHz comb lines had 12–40 in a minute, the
/// false members at a trunked site 1–5).
const COMB_MEMBER_RECORDS: u32 = 6;
/// ...with spacings in this range (closer is one wide signal's structure,
/// further apart is coincidence in a 10 MHz span).
const COMB_SPACING_HZ: (f64, f64) = (20e3, 2.5e6);
/// Comb members are on the air at least this share of the time (see
/// `Emitter::duty`): a trunked site's narrow blips on its channel raster
/// were 1–39% on air and read as combs at every multiple of 12.5 kHz.
const COMB_MIN_DUTY: f64 = 0.5;
/// Closed records whose ring extents an emitter remembers.
const RECENT_RECORDS: usize = 8;

/// Times a protocol must be identified on an emitter before it's believed...
const MIN_PROTOCOL_HITS: u32 = 2;
/// ...unless one identification alone contained this many syncs.
const STRONG_SYNCS: usize = 4;
/// Least analysed time a classification weighs (a look shorter than this
/// still counts for something).
const MIN_WEIGHT_S: f32 = 0.05;

#[derive(Debug, Clone)]
pub struct Emitter {
    pub id: u64,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub records: u32,
    /// Summed duration of finished records, seconds.
    pub on_time_s: f64,
    /// Start of the record still open on it (its time isn't in
    /// `on_time_s` yet).
    pub(crate) open_start: Option<SystemTime>,
    pub active: bool,
    /// Seen often, long or strongly enough not to be a noise blip.
    pub confirmed: bool,
    pub peak_snr_db: f32,
    /// Classification labels seen on this emitter's records (and its own
    /// group classifications), with counts.
    pub tally: BTreeMap<String, u32>,
    /// Seconds of signal analysed behind each label in `tally`: a label
    /// from a 1.5 s window weighs ten times one from a 0.15 s look.
    pub weight: BTreeMap<String, f32>,
    /// Labels in `tally` that named a protocol.
    pub protocols: BTreeMap<String, u32>,
    /// Decoded system identities ("NAC 0x2A5", "CC 9"), weighted by frames.
    pub ids: BTreeMap<String, usize>,
    /// What its records were used for (voice / control / data), with counts.
    pub activity: BTreeMap<detect::decode::Activity, u32>,
    /// Calls heard ("TG 1234 ← 5678"), with counts, and the latest.
    pub talk: BTreeMap<String, u32>,
    pub last_talk: Option<String>,
    /// Capacity Plus repeater number this transmitter was seen to be.
    pub capplus_repeater: Option<u8>,
    /// Protocol labels seen at least once with `STRONG_SYNCS` or more syncs,
    /// or with frames that passed their code.
    pub strong: std::collections::BTreeSet<String>,
    /// Latest group-level classification.
    pub class: Option<Arc<Classification>>,
    /// What the emitter is best described as: the most frequent
    /// protocol-naming result if any, else the latest feature result.
    pub best: Option<Arc<Classification>>,
    pub(crate) class_at: Option<Instant>,
    /// Group classifications in a row that identified nothing.
    pub(crate) group_misses: u32,
    /// Newest record's ring position and tuner centre (for group windows).
    pub(crate) last_end_sample: u64,
    /// Ring extents (first row end, last row end) of the newest closed
    /// records, oldest first, so a group window can take in several short
    /// bursts rather than a fixed stretch behind the last one.
    pub(crate) recent: std::collections::VecDeque<(u64, u64)>,
    pub(crate) tuner_center_hz: f64,
    /// Bin width of the rows its records came from.
    pub(crate) bin_hz: f64,
    /// Finished records that weren't weak one-row blips.
    pub(crate) solid_records: u32,
}

impl Emitter {
    /// Best single label: the most frequent protocol label once it has been
    /// seen at least twice, else the most frequent feature label.
    pub fn label(&self) -> Option<String> {
        // Labels fixed by where the signal sits (the tuner's DC, the radio's
        // own spur grid) beat any feature vote: early looks at a record can
        // see it wider than it is and vote otherwise.
        // A label given to a line (an image, a comb member) lapses once
        // the emitter has shown itself wider than a line: a data channel's
        // first narrow record is not what it is.
        if let Some(l) = self.tally.keys().find(|l| is_sticky(l) && (!is_line_label(l) || self.bandwidth_hz < LINE_HZ * 1.5)) {
            return Some(l.clone());
        }
        self.protocol_label().or_else(|| self.majority_feature_label())
    }

    /// Most-seen decoded identity (NAC, colour code), if any.
    pub fn id(&self) -> Option<&str> {
        self.ids.iter().max_by_key(|(_, n)| **n).map(|(k, _)| k.as_str())
    }

    /// `id`, plus the Capacity Plus repeater number when known:
    /// "CC 9 · Cap+ rep 3".
    pub fn id_label(&self) -> String {
        match (self.id(), self.capplus_repeater) {
            (Some(i), Some(r)) => format!("{i} · Cap+ rep {r}"),
            (None, Some(r)) => format!("Cap+ rep {r}"),
            (i, None) => i.unwrap_or("").to_string(),
        }
    }

    /// The feature label with the most analysed time behind it, except
    /// that a label describing a modulation beats one that doesn't
    /// ("Carrier", "Unknown", "Wideband") once it holds `DESCRIBED_SHARE`
    /// of the time: a voice channel is keyed in short looks that read as
    /// a bare carrier far more often than it is caught talking, and it
    /// is the talking that says what it is.
    fn majority_feature_label(&self) -> Option<String> {
        let feature = |l: &&String| !self.protocols.contains_key(*l);
        let w = |l: &String| self.weight.get(l).copied().unwrap_or(0.0);
        let total: f32 = self.tally.keys().filter(feature).map(w).sum();
        let best = |describing: bool| {
            self.tally
                .keys()
                .filter(feature)
                .filter(|l| describes_modulation(l) == describing)
                .max_by(|a, b| w(a).total_cmp(&w(b)).then_with(|| self.tally[*b].cmp(&self.tally[*a])))
        };
        match best(true) {
            Some(l) if w(l) >= DESCRIBED_SHARE * total => Some(l.clone()),
            _ => self.tally.keys().filter(feature).max_by(|a, b| w(a).total_cmp(&w(b))).cloned(),
        }
    }

    /// The protocol label seen most, if it was seen at least
    /// `MIN_PROTOCOL_HITS` times (one stray result isn't an identification).
    pub fn protocol_label(&self) -> Option<String> {
        self.protocols
            .iter()
            .filter(|(l, n)| **n >= MIN_PROTOCOL_HITS || self.strong.contains(*l))
            .max_by_key(|(_, n)| **n)
            .map(|(l, _)| l.clone())
    }

    /// What the emitter is mostly used for.
    pub fn activity(&self) -> Option<detect::decode::Activity> {
        self.activity.iter().max_by_key(|(_, n)| **n).map(|(a, _)| *a)
    }

    pub fn has_protocol(&self) -> bool {
        self.protocol_label().is_some()
    }

    /// Wait before the next group attempt: `base` doubled per miss, capped.
    pub(crate) fn group_backoff(&self, base: std::time::Duration, max: std::time::Duration) -> std::time::Duration {
        base.saturating_mul(1u32 << self.group_misses.min(8)).min(max)
    }

    /// Bin width of the rows its records came from.
    pub fn bin_hz(&self) -> f64 {
        self.bin_hz
    }

    /// Fraction of the time between first and last seen that it was on,
    /// counting a record still open (a steady line has one, all along).
    pub fn duty(&self) -> f64 {
        let span = self.last_seen.duration_since(self.first_seen).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let open = self.open_start.and_then(|t| self.last_seen.duration_since(t).ok()).map_or(0.0, |d| d.as_secs_f64());
        if span <= 0.0 { 1.0 } else { ((self.on_time_s + open) / span).min(1.0) }
    }
}

#[derive(Default)]
pub(crate) struct EmitterBook {
    /// Keyed by emitter id: records look their emitter up on every row.
    pub(crate) map: HashMap<u64, Emitter>,
    pub(crate) by_record: HashMap<u64, u64>,
    next_id: u64,
    /// Newest record time seen (the book's clock, for pruning).
    latest: Option<SystemTime>,
    calls: u32,
}

/// Widest emitter that takes in narrower records lying inside it (see
/// `EmitterBook::contains`): a narrowband channel.
const CONTAIN_MAX_HZ: f64 = 30e3;
/// Half a broadcast FM channel (200 kHz in the Americas; stations 100 kHz
/// apart elsewhere stay apart because pieces that are stations themselves
/// are never folded).
const STATION_HALF_HZ: f64 = 100e3;

impl EmitterBook {
    /// Frequency tolerance for joining: a quarter of the bandwidth, at least
    /// 2.5 kHz (record centres jitter with modulation) and at least 1.5 bins
    /// (record edges are whole bins; coarse survey bins move centres by one).
    fn tolerance(bw: f64, bin: f64) -> f64 {
        (bw * 0.25).max(2.5e3).max(bin * 1.5)
    }

    /// Does a record of this centre and bandwidth belong to `e`? Bandwidths
    /// must be comparable, allowing a bin either way. On coarse rows (the
    /// survey) a record only has to touch the emitter: there a signal's
    /// extent swings from pass to pass and weak activity flickers across a
    /// hump of raised floor, so shape is no guide.
    fn fits(e: &Emitter, c: f64, bw: f64, bin: f64) -> bool {
        let bin = bin.max(e.bin_hz);
        if bin >= COARSE_BIN_HZ {
            return (e.center_hz - c).abs() <= (e.bandwidth_hz + bw) / 2.0 + bin;
        }
        let ratio = (bw + bin) / (e.bandwidth_hz.max(1.0) + bin);
        (e.center_hz - c).abs() <= Self::tolerance(e.bandwidth_hz.max(bw), bin) && (0.33..=3.0).contains(&ratio)
    }

    /// Does a clearly narrower record lie inside `e`, a confirmed narrowband
    /// emitter (or have its centre inside a broadcast FM station's)? An FSK channel's records that caught one tone (an idle
    /// pager: ±4.8 kHz) or part of a burst are that channel, not a new
    /// "carrier" beside it. Fine rows only; wide emitters (LTE, TV, a
    /// "Wideband" blob) are left out so narrow signals inside them stay
    /// their own.
    fn contains(e: &Emitter, c: f64, bw: f64, bin: f64) -> bool {
        let bin = bin.max(e.bin_hz);
        if bin >= COARSE_BIN_HZ || !e.confirmed || bw * 2.0 > e.bandwidth_hz + bin {
            return false;
        }
        // A broadcast FM station owns its channel: a narrow record at its
        // carrier (quiet programme) or a piece of its swing is the station.
        if e.best.as_ref().is_some_and(|b| b.label.starts_with("FM broadcast") || b.label == "Wideband FM") {
            return (e.center_hz - c).abs() <= e.bandwidth_hz / 2.0 + bin;
        }
        e.bandwidth_hz <= CONTAIN_MAX_HZ && (e.center_hz - c).abs() + bw / 2.0 <= e.bandwidth_hz / 2.0 + bin
    }

    /// Is `r` clearly wider than `e` and spanning all of it?
    fn covers(r: &SignalRecord, e: &Emitter) -> bool {
        let bin = r.bin_hz.max(e.bin_hz);
        r.bandwidth_hz() >= 1.5 * e.bandwidth_hz
            && r.f_lo_hz <= e.center_hz - e.bandwidth_hz / 2.0 + bin
            && r.f_hi_hz >= e.center_hz + e.bandwidth_hz / 2.0 - bin
    }

    /// The emitter a record belongs to, other than `except`: a confirmed
    /// narrowband emitter containing it (the busiest), else the closest it
    /// fits.
    fn find(&self, c: f64, bw: f64, bin: f64, except: Option<u64>) -> Option<u64> {
        let others = || self.map.values().filter(move |e| Some(e.id) != except);
        others()
            .filter(|e| Self::contains(e, c, bw, bin))
            .max_by_key(|e| e.records)
            .or_else(|| others().filter(|e| Self::fits(e, c, bw, bin)).min_by(|a, b| (a.center_hz - c).abs().total_cmp(&(b.center_hz - c).abs())))
            .map(|e| e.id)
    }

    /// Assigns a record (open or just closed) to an emitter, creating one if
    /// needed, and updates the emitter's stats. Returns the emitter id.
    pub(crate) fn observe(&mut self, r: &SignalRecord, closed: bool) -> u64 {
        let bw = r.bandwidth_hz().max(1.0);
        let c = r.center_hz();
        let id = match self.by_record.get(&r.id) {
            // A record that closes wider than its emitter and covering it,
            // on an emitter with only a few records, built that emitter
            // from its own early fragments (two tracks of a broadcast
            // station that later merged): the emitter takes its final shape.
            Some(&id) if closed && self.map.get(&id).is_some_and(|e| e.records == 1 || (e.records <= 3 && Self::covers(r, e))) => self.refit(r, id, c, bw),
            Some(&id) => id,
            None => {
                let id = self.find(c, bw, r.bin_hz, None).unwrap_or_else(|| {
                    self.next_id += 1;
                    self.map.insert(self.next_id, Self::new_emitter(self.next_id, r, c, bw));
                    self.next_id
                });
                // A record taken in because it lies inside the emitter (one
                // tone of a pager, a station's quiet carrier) is a partial
                // view: it doesn't move the emitter's centre or width.
                let partial = self.map.get(&id).is_some_and(|e| e.records > 0 && Self::contains(e, c, bw, r.bin_hz));
                if let Some(e) = self.map.get_mut(&id) {
                    e.records += 1;
                    if !partial {
                        // Slow average so one odd record doesn't drag the centre.
                        e.center_hz += (c - e.center_hz) * 0.1;
                        e.bandwidth_hz += (bw - e.bandwidth_hz) * 0.1;
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
            if r.t_end > e.last_seen {
                e.last_seen = r.t_end;
            }
            if r.t_start < e.first_seen {
                e.first_seen = r.t_start;
            }
            e.peak_snr_db = e.peak_snr_db.max(r.max_snr_db);
            e.last_end_sample = e.last_end_sample.max(r.last_row_end_sample);
            e.tuner_center_hz = r.tuner_center_hz;
            e.open_start = if closed { None } else { Some(e.open_start.map_or(r.t_start, |t| t.min(r.t_start))) };
            if closed {
                e.on_time_s += r.duration_s();
                if r.rows >= 2 || r.max_snr_db >= BLIP_SNR_DB {
                    e.solid_records += 1;
                }
                if e.recent.len() >= RECENT_RECORDS {
                    e.recent.pop_front();
                }
                e.recent.push_back((r.first_row_end_sample, r.last_row_end_sample));
            }
            e.confirmed |= e.solid_records >= CONFIRM_RECORDS
                || (r.duration_s() >= CONFIRM_DURATION_S && r.rows >= CONFIRM_ROWS)
                || r.max_snr_db >= CONFIRM_SNR_DB;
        }
        id
    }

    /// A record that started emitter `id` on its own has closed: if its final
    /// shape fits another emitter, fold `id` into that one; otherwise give
    /// `id` the final shape. Returns the record's emitter.
    fn refit(&mut self, r: &SignalRecord, id: u64, c: f64, bw: f64) -> u64 {
        let Some(to) = self.find(c, bw, r.bin_hz, Some(id)) else {
            if let Some(e) = self.map.get_mut(&id) {
                e.center_hz = c;
                e.bandwidth_hz = bw;
            }
            return id;
        };
        if !self.fold(id, to) {
            return id;
        }
        self.by_record.insert(r.id, to);
        if let Some(e) = self.map.get_mut(&to) {
            e.center_hz += (c - e.center_hz) * 0.1;
            e.bandwidth_hz += (bw - e.bandwidth_hz) * 0.1;
        }
        to
    }

    /// Folds emitter `id` into `to`: its records (all of them, not only
    /// the one being placed), counts and votes. False if either is gone.
    fn fold(&mut self, id: u64, to: u64) -> bool {
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
        e.records += from.records;
        e.solid_records += from.solid_records;
        e.on_time_s += from.on_time_s;
        e.first_seen = e.first_seen.min(from.first_seen);
        e.last_seen = e.last_seen.max(from.last_seen);
        e.active |= from.active;
        e.confirmed |= from.confirmed;
        e.peak_snr_db = e.peak_snr_db.max(from.peak_snr_db);
        e.last_end_sample = e.last_end_sample.max(from.last_end_sample);
        e.recent.extend(from.recent);
        let mut recent: Vec<(u64, u64)> = e.recent.drain(..).collect();
        recent.sort();
        recent.dedup();
        e.recent.extend(recent.into_iter().rev().take(RECENT_RECORDS).rev());
        for (k, n) in from.tally {
            *e.tally.entry(k).or_default() += n;
        }
        for (k, n) in from.weight {
            *e.weight.entry(k).or_default() += n;
        }
        for (k, n) in from.protocols {
            *e.protocols.entry(k).or_default() += n;
        }
        for (k, n) in from.ids {
            *e.ids.entry(k).or_default() += n;
        }
        e.capplus_repeater = e.capplus_repeater.or(from.capplus_repeater);
        e.strong.extend(from.strong);
        for (k, n) in from.activity {
            *e.activity.entry(k).or_default() += n;
        }
        for (k, n) in from.talk {
            *e.talk.entry(k).or_default() += n;
        }
        if e.last_talk.is_none() {
            e.last_talk = from.last_talk;
        }
        if e.best.is_none() {
            e.best = from.best;
        }
        true
    }

    /// A broadcast FM station owns its channel: emitters within
    /// `STATION_HALF_HZ` of it that aren't stations themselves (pieces
    /// split off by quiet programme or the swing, seen before it was
    /// named) fold into it.
    fn fold_station(&mut self, id: u64) {
        let Some(c) = self.map.get(&id).map(|e| e.center_hz) else { return };
        let is_station = |e: &Emitter| e.best.as_ref().is_some_and(|b| b.label.starts_with("FM broadcast"));
        let pieces: Vec<u64> = self.map.values().filter(|e| e.id != id && (e.center_hz - c).abs() <= STATION_HALF_HZ && !is_station(e)).map(|e| e.id).collect();
        for p in pieces {
            self.fold(p, id);
        }
    }

    fn new_emitter(id: u64, r: &SignalRecord, c: f64, bw: f64) -> Emitter {
        Emitter {
            id,
            center_hz: c,
            bandwidth_hz: bw,
            first_seen: r.t_start,
            last_seen: r.t_end,
            records: 0,
            solid_records: 0,
            on_time_s: 0.0,
            open_start: None,
            active: false,
            confirmed: false,
            peak_snr_db: r.max_snr_db,
            tally: BTreeMap::new(),
            weight: BTreeMap::new(),
            protocols: BTreeMap::new(),
            ids: BTreeMap::new(),
            activity: BTreeMap::new(),
            talk: BTreeMap::new(),
            last_talk: None,
            capplus_repeater: None,
            strong: Default::default(),
            class: None,
            best: None,
            class_at: None,
            group_misses: 0,
            last_end_sample: r.last_row_end_sample,
            recent: Default::default(),
            tuner_center_hz: r.tuner_center_hz,
            bin_hz: r.bin_hz,
        }
    }

    /// Recomputes `active` from the currently open records.
    pub(crate) fn mark_active(&mut self, open: &[SignalRecord]) {
        for e in self.map.values_mut() {
            e.active = false;
        }
        for r in open {
            if let Some(&id) = self.by_record.get(&r.id) {
                if let Some(e) = self.map.get_mut(&id) {
                    e.active = true;
                }
            }
        }
        self.calls = self.calls.wrapping_add(1);
        if let Some(now) = self.latest.filter(|_| self.calls.is_multiple_of(PRUNE_EVERY)) {
            let stale = |e: &Emitter| !e.confirmed && !e.active && now.duration_since(e.last_seen).is_ok_and(|d| d > UNCONFIRMED_TTL);
            if self.map.values().any(stale) {
                self.map.retain(|_, e| !stale(e));
                let map = &self.map;
                self.by_record.retain(|_, e| map.contains_key(e));
            }
        }
        if self.map.len() > MAX_EMITTERS {
            let mut order: Vec<(bool, SystemTime, u64)> = self.map.values().map(|e| (e.active, e.last_seen, e.id)).collect();
            order.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            let gone: std::collections::HashSet<u64> = order[MAX_EMITTERS..].iter().map(|o| o.2).collect();
            self.map.retain(|id, _| !gone.contains(id));
            self.by_record.retain(|_, e| !gone.contains(e));
        }
    }

    /// Notes a record's classification against its emitter.
    pub(crate) fn tally(&mut self, record_id: u64, c: &Arc<Classification>) {
        if let Some(&id) = self.by_record.get(&record_id) {
            self.note(id, c);
        }
    }

    /// Takes a verdict made elsewhere (an explore visit on another receiver)
    /// as this emitter's label outright: counted as enough hits to be
    /// believed, and made `best`.
    pub(crate) fn adopt(&mut self, id: u64, c: Arc<Classification>) {
        let Some(e) = self.map.get_mut(&id) else { return };
        *e.tally.entry(c.label.clone()).or_default() += MIN_PROTOCOL_HITS;
        *e.weight.entry(c.label.clone()).or_default() += (c.analysed_s as f32).max(MIN_WEIGHT_S) * MIN_PROTOCOL_HITS as f32;
        if c.protocol.is_some() {
            *e.protocols.entry(c.label.clone()).or_default() += MIN_PROTOCOL_HITS;
            e.strong.insert(c.label.clone());
        }
        if let Some(i) = c.fields.as_ref().and_then(|f| f.id.clone()) {
            *e.ids.entry(i).or_default() += 1;
        }
        e.class = Some(c.clone());
        e.best = Some(c);
    }

    /// Stores a group-level classification for an emitter.
    pub(crate) fn set_group_class(&mut self, id: u64, c: Arc<Classification>) {
        if let Some(e) = self.map.get_mut(&id) {
            e.class = Some(c.clone());
            e.class_at = Some(Instant::now());
            e.group_misses = if c.protocol.is_some() { 0 } else { e.group_misses + 1 };
        }
        self.note(id, &c);
    }

    fn note(&mut self, id: u64, c: &Arc<Classification>) {
        let Some(e) = self.map.get_mut(&id) else { return };
        if matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") || c.label == detect::WEAK {
            return;
        }
        *e.tally.entry(c.label.clone()).or_default() += 1;
        *e.weight.entry(c.label.clone()).or_default() += (c.analysed_s as f32).max(MIN_WEIGHT_S);
        if let Some(id) = c.fields.as_ref().and_then(|f| f.id.clone().map(|id| (id, f.decoded))) {
            *e.ids.entry(id.0).or_default() += id.1;
        }
        if let Some(f) = &c.fields {
            if let Some(a) = f.activity {
                *e.activity.entry(a).or_default() += 1;
            }
            if let Some(t) = &f.talk {
                *e.talk.entry(t.clone()).or_default() += 1;
                e.last_talk = Some(t.clone());
            }
            if f.capplus_repeater.is_some() {
                e.capplus_repeater = f.capplus_repeater;
            }
        }
        if c.protocol.is_some() {
            *e.protocols.entry(c.label.clone()).or_default() += 1;
            // Strong: enough syncs, or frames that passed their code (a FLEX
            // frame header's BCH, a P25 NID, a DMR slot type), most of those
            // tried. A weak pager decodes a header now and then, with no
            // sync-bank syncs at all, and was left "Unknown".
            let checked = c.fields.as_ref().is_some_and(|f| f.decoded >= 1 && f.decoded * 2 >= f.tried);
            if checked || c.sync.as_ref().is_some_and(|s| s.count >= STRONG_SYNCS) {
                e.strong.insert(c.label.clone());
            }
        }
        let pcount = |l: &str| e.protocols.get(l).copied().unwrap_or(0);
        let tcount = |l: &str| e.weight.get(l).copied().unwrap_or(0.0);
        let trusted =
            |cl: &Classification| cl.protocol.is_some() && (pcount(&cl.label) >= MIN_PROTOCOL_HITS || e.strong.contains(&cl.label));
        // `best` follows `label()`: a trusted protocol if any, else the most
        // frequent feature result. Untrusted protocol claims never become best.
        let replace = match &e.best {
            _ if c.protocol.is_some() && !trusted(c) => false,
            None => true,
            Some(b) if trusted(b) => trusted(c) && pcount(&c.label) >= pcount(&b.label),
            Some(_) if trusted(c) => true,
            Some(b) => tcount(&c.label) >= tcount(&b.label),
        };
        if replace {
            e.best = Some(c.clone());
        }
        if replace && c.label.starts_with("FM broadcast") {
            self.fold_station(id);
        }
    }

    pub(crate) fn get(&self, id: u64) -> Option<&Emitter> {
        self.map.get(&id)
    }

    /// What the other lines in view say about a line at `center_hz` (a
    /// signal narrower than `LINE_HZ`) on a receiver tuned to `tuner_hz`:
    /// its mirror image about the tuner centre is also a line (the radio's
    /// own, from its IF or sampling: image pairs sit symmetrically about
    /// the centre and move with it), or it is one of a comb of lines at a
    /// common spacing (a clock harmonic family, the host's or the radio's).
    /// None when it stands alone.
    pub(crate) fn spur_context(&self, center_hz: f64, bandwidth_hz: f64, tuner_hz: f64) -> Option<SpurContext> {
        if bandwidth_hz >= LINE_HZ {
            return None;
        }
        let others = |min_records: u32| -> Vec<f64> {
            self.map
                .values()
                .filter(|e| e.bandwidth_hz < LINE_HZ && e.records >= min_records && (e.center_hz - center_hz).abs() > LINE_TOL_HZ)
                .map(|e| e.center_hz)
                .collect()
        };
        let lines = others(2);
        let near = |f: f64| lines.iter().any(|l| (l - f).abs() <= LINE_TOL_HZ);
        let offset = center_hz - tuner_hz;
        if offset.abs() > LINE_TOL_HZ && near(tuner_hz - offset) {
            return Some(SpurContext::ImagePair);
        }
        // A comb is made of lines that are always there (a clock's
        // harmonics). The brief narrow blips of a busy trunked site, or a
        // hopping meter's sidelines, fall on a common spacing by chance
        // (a 25 kHz channel grid, the hop grid) and must not count.
        // A clock's harmonic is on the air nearly all the time; a site's
        // blips (seen often enough over a few minutes to pass the record
        // count) are on a few tenths of it at most.
        let lines: Vec<f64> = self
            .map
            .values()
            .filter(|e| {
                e.bandwidth_hz < LINE_HZ
                    && e.records >= COMB_MEMBER_RECORDS
                    && e.duty() >= COMB_MIN_DUTY
                    && (e.center_hz - center_hz).abs() > LINE_TOL_HZ
            })
            .map(|e| e.center_hz)
            .collect();
        // Each other line proposes a spacing; the spacing most lines fit wins.
        let mut best: Option<(usize, f64)> = None;
        for l in &lines {
            let d = (l - center_hz).abs();
            if !(COMB_SPACING_HZ.0..=COMB_SPACING_HZ.1).contains(&d) {
                continue;
            }
            let n = 1 + lines
                .iter()
                .filter(|m| {
                    let k = ((*m - center_hz) / d).round();
                    k != 0.0 && (*m - (center_hz + k * d)).abs() <= LINE_TOL_HZ
                })
                .count();
            if n >= COMB_LINES && best.is_none_or(|(bn, bd)| n > bn || (n == bn && d < bd)) {
                best = Some((n, d));
            }
        }
        best.map(|(lines, spacing_hz)| SpurContext::Comb { spacing_hz, lines })
    }
}

/// See `EmitterBook::spur_context`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpurContext {
    ImagePair,
    Comb { spacing_hz: f64, lines: usize },
}

impl SpurContext {
    pub fn label(&self) -> String {
        match self {
            SpurContext::ImagePair => "Internal spur (image)".to_string(),
            SpurContext::Comb { spacing_hz, lines } => {
                if *spacing_hz >= 1e6 {
                    format!("Spur comb {:.2} MHz ({lines} lines)", spacing_hz / 1e6)
                } else {
                    format!("Spur comb {:.0} kHz ({lines} lines)", spacing_hz / 1e3)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rec(id: u64, mhz: f64, bw_khz: f64, t: u64) -> SignalRecord {
        let c = mhz * 1e6;
        let bw = bw_khz * 1e3;
        SignalRecord {
            id,
            source_serial: "T".into(),
            first_seq: t,
            last_seq: t + 1,
            t_start: SystemTime::UNIX_EPOCH + Duration::from_secs(t),
            t_end: SystemTime::UNIX_EPOCH + Duration::from_secs(t) + Duration::from_millis(30),
            f_lo_hz: c - bw / 2.0,
            f_hi_hz: c + bw / 2.0,
            bin_hz: 610.0,
            peak_dbfs: -60.0,
            mean_snr_db: 20.0,
            max_snr_db: 22.0,
            rows: 2,
            active: false,
            first_row_end_sample: 0,
            last_row_end_sample: 0,
            tuner_center_hz: 0.0,
            class: None,
            emitter: None,
            emitter_class: None,
            emitter_first_seen: None,
        }
    }

    #[test]
    fn bursts_on_one_channel_form_one_emitter_neighbours_stay_apart() {
        let mut b = EmitterBook::default();
        for i in 0..40 {
            // 30 ms TDMA bursts with a little centre jitter...
            b.observe(&rec(i, 856.3375 + (i % 3) as f64 * 0.0008, 9.0, i), true);
            // ...and the adjacent 12.5 kHz channel.
            b.observe(&rec(1000 + i, 856.3500, 9.0, i), true);
        }
        // A wideband signal at the same centre is something else.
        b.observe(&rec(5000, 856.3375, 200.0, 50), true);
        assert_eq!(b.map.len(), 3);
        let e = b.get(b.by_record[&5]).unwrap();
        assert_eq!(e.records, 40);
        assert!((e.on_time_s - 1.2).abs() < 1e-6);
    }

    #[test]
    fn coarse_bins_dont_split_a_signal() {
        // Survey rows: 54 kHz bins. One FM station comes out 1, 2 or 4 bins
        // wide with its centre moving by half a bin or a bin.
        let bin = 54.1e3;
        let mut b = EmitterBook::default();
        for (i, (off, bins)) in [(0.0, 1.0), (-0.5, 2.0), (0.0, 4.0), (1.0, 1.0), (0.5, 2.0)].iter().enumerate() {
            let mut r = rec(i as u64, 96.3 + off * bin / 1e6, bins * bin / 1e3, i as u64 * 10);
            r.bin_hz = bin;
            b.observe(&r, true);
        }
        assert_eq!(b.map.len(), 1);
    }

    #[test]
    fn record_placed_from_narrow_onset_joins_on_close() {
        let mut b = EmitterBook::default();
        for i in 0..3 {
            b.observe(&rec(i, 856.3375, 9.0, i), true);
        }
        // First rows of a key-up: narrow, but inside the channel's confirmed
        // emitter, so it joins it straight away...
        let mut r = rec(10, 856.3380, 1.8, 10);
        b.observe(&r, false);
        assert_eq!(b.map.len(), 1);
        // ...and stays there once it closes at full width.
        r.f_lo_hz = 856.3375e6 - 4.5e3;
        r.f_hi_hz = 856.3375e6 + 4.5e3;
        let id = b.observe(&r, true);
        assert_eq!(b.map.len(), 1);
        assert_eq!(b.by_record[&10], id);
        assert_eq!(b.get(id).unwrap().records, 4);
    }

    #[test]
    fn pieces_of_a_broadcast_station_join_it() {
        let mut b = EmitterBook::default();
        let st = b.observe(&rec(1, 93.7, 130.0, 1), true);
        for i in 2..4 {
            b.observe(&rec(i, 93.7, 130.0, i), true);
        }
        let fm = Arc::new(Classification {
            label: "FM broadcast (stereo, RDS)".into(),
            protocol: None,
            confidence: 0.9,
            features: Default::default(),
            sync: None,
            fields: None,
            analysed_s: 1.5,
        });
        b.set_group_class(st, fm);
        assert!(b.get(st).unwrap().confirmed);
        // Its carrier alone (quiet programme), and a piece of its swing.
        assert_eq!(b.observe(&rec(10, 93.7004, 10.0, 10), true), st);
        assert_eq!(b.observe(&rec(11, 93.6400, 40.0, 11), true), st);
        // A station 200 kHz away is not part of it.
        assert_ne!(b.observe(&rec(12, 93.9, 130.0, 12), true), st);
    }

    #[test]
    fn a_station_built_from_its_fragments_takes_its_final_shape() {
        let mut b = EmitterBook::default();
        // Two pieces of one station seen while still open...
        let a = b.observe(&rec(1, 97.64, 47.0, 1), false);
        assert_eq!(b.observe(&rec(2, 97.645, 45.0, 1), false), a);
        assert_eq!(b.get(a).unwrap().records, 2);
        // ...then their tracks merged and the record closes at full width.
        let mut whole = rec(1, 97.7034, 207.0, 1);
        whole.t_end = whole.t_start + Duration::from_secs(60);
        assert_eq!(b.observe(&whole, true), a);
        let e = b.get(a).unwrap();
        assert!((e.center_hz - 97.7034e6).abs() < 2e3 && e.bandwidth_hz > 200e3, "{} {}", e.center_hz, e.bandwidth_hz);
    }

    #[test]
    fn a_named_station_takes_in_its_pieces() {
        let mut b = EmitterBook::default();
        let st = b.observe(&rec(1, 93.6826, 72.0, 1), true);
        let p1 = b.observe(&rec(2, 93.6195, 11.0, 2), true);
        let p2 = b.observe(&rec(3, 93.7657, 28.0, 3), true);
        let other = b.observe(&rec(4, 93.9, 150.0, 4), true);
        assert_eq!(b.map.len(), 4);
        let fm = Arc::new(Classification {
            label: "FM broadcast (stereo, RDS)".into(),
            protocol: None,
            confidence: 0.9,
            features: Default::default(),
            sync: None,
            fields: None,
            analysed_s: 1.5,
        });
        b.set_group_class(st, fm);
        assert!(b.get(p1).is_none() && b.get(p2).is_none());
        assert_eq!((b.by_record[&2], b.by_record[&3]), (st, st));
        assert_eq!(b.get(st).unwrap().records, 3);
        assert!(b.get(other).is_some(), "the next channel up is another station");
    }

    #[test]
    fn one_checked_frame_names_the_protocol() {
        let flex = |decoded, tried| {
            Arc::new(Classification {
                label: "FLEX 1600/4".into(),
                protocol: Some("FLEX".into()),
                confidence: 0.95,
                features: Default::default(),
                sync: None,
                fields: Some(detect::decode::Fields { decoded, tried, ..Default::default() }),
                analysed_s: 1.5,
            })
        };
        let mut b = EmitterBook::default();
        let a = b.observe(&rec(1, 931.3375, 12.0, 1), true);
        b.set_group_class(a, flex(1, 1));
        assert_eq!(b.get(a).unwrap().protocol_label().as_deref(), Some("FLEX 1600/4"));
        // One lucky pass among many failures is not enough.
        let c = b.observe(&rec(2, 929.1, 12.0, 2), true);
        b.set_group_class(c, flex(1, 6));
        assert_eq!(b.get(c).unwrap().protocol_label(), None);
    }

    #[test]
    fn one_tone_of_a_pager_joins_its_channel() {
        let mut b = EmitterBook::default();
        // A FLEX channel at full width (both tones)...
        for i in 0..3 {
            b.observe(&rec(i, 929.6125, 14.0, i), true);
        }
        // ...then records that caught one tone each, 4.7 kHz either side.
        let a = b.observe(&rec(10, 929.6078, 3.0, 10), true);
        let c = b.observe(&rec(11, 929.6172, 3.0, 11), true);
        assert_eq!((a, c), (b.by_record[&0], b.by_record[&0]));
        assert_eq!(b.map.len(), 1);
        // ...without shrinking it to a tone's width.
        assert!(b.get(a).unwrap().bandwidth_hz > 13e3, "{}", b.get(a).unwrap().bandwidth_hz);
        // A narrow line 20 kHz away is not part of it.
        b.observe(&rec(12, 929.6325, 3.0, 12), true);
        assert_eq!(b.map.len(), 2);
    }

    #[test]
    fn image_pairs_and_combs_are_the_radios_own() {
        let mut b = EmitterBook::default();
        // Lines at the tuner (240 MHz) ± 96, ± 296 and ± 392 kHz, as seen on air...
        let mut n = 0;
        for off in [-392.0, -296.0, -96.0, 96.0, 296.0, 392.0] {
            for _ in 0..3 {
                b.observe(&rec(n, 240.0 + off / 1e3, 1.8, n), true);
                n += 1;
            }
        }
        // ...and a comb at 750 kHz: 94.4992, 95.2492, 95.9992, 97.4992 MHz,
        // each line seen often enough to be a comb member.
        for (k, mhz) in [94.4992, 95.2492, 95.9992, 97.4992].into_iter().enumerate() {
            for i in 0..COMB_MEMBER_RECORDS as u64 {
                // Steady: each second on the air for most of it.
                let mut r = rec(n, mhz, 1.8, 100 + i);
                r.t_end = r.t_start + Duration::from_millis(900);
                b.observe(&r, true);
                n += 1;
            }
            let _ = k;
        }
        // A lone line, and a voice channel.
        b.observe(&rec(n, 241.0, 1.8, n), true);
        b.observe(&rec(n + 1, 241.0, 1.8, n + 1), true);
        b.observe(&rec(n + 2, 240.2, 12.5, n + 2), true);
        assert_eq!(b.spur_context(240.096e6, 1.8e3, 240e6), Some(SpurContext::ImagePair));
        assert_eq!(b.spur_context(239.608e6, 1.8e3, 240e6), Some(SpurContext::ImagePair));
        match b.spur_context(95.2492e6, 1.8e3, 98e6) {
            Some(SpurContext::Comb { spacing_hz, lines }) => {
                assert!((spacing_hz - 750e3).abs() < 2e3, "{spacing_hz}");
                assert_eq!(lines, 4);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(b.spur_context(241.0e6, 1.8e3, 240e6), None);
        assert_eq!(b.spur_context(240.2e6, 12.5e3, 240e6), None, "not a line");
        // Four brief blips on a 25 kHz grid (a trunked site's leftovers)
        // are not a comb: members must be persistent.
        for mhz in [852.325, 852.35, 852.375, 852.4] {
            for _ in 0..3 {
                b.observe(&rec(n, mhz, 3.0, n), true);
                n += 1;
            }
        }
        assert_eq!(b.spur_context(852.425e6, 3.0e3, 855.5e6), None, "blips on a grid");
        // Nor are blips seen often enough to pass the record count: on
        // air a few tenths of the time, as on eddie's P25 site.
        for (k, mhz) in [853.2, 853.2625, 853.325, 853.3875].into_iter().enumerate() {
            for i in 0..(COMB_MEMBER_RECORDS as u64 + 4) {
                let mut r = rec(n, mhz, 3.0, 200 + k as u64 + i * 10);
                r.t_end = r.t_start + Duration::from_millis(400);
                b.observe(&r, true);
                n += 1;
            }
        }
        assert_eq!(b.spur_context(853.45e6, 3.0e3, 855.5e6), None, "frequent blips on a raster");
    }

    #[test]
    fn noise_blips_stay_unconfirmed_and_expire() {
        let mut b = EmitterBook::default();
        let mut blip = rec(1, 300.0, 54.1, 0);
        blip.max_snr_db = 9.0;
        blip.rows = 1;
        let id = b.observe(&blip, true);
        // Weak one-row blips landing on the same patch again don't confirm it.
        for i in 0..5 {
            let mut again = blip.clone();
            again.id = 100 + i;
            assert_eq!(b.observe(&again, true), id);
        }
        assert!(!b.get(id).unwrap().confirmed);
        // A strong one is confirmed at once.
        let strong = rec(2, 400.0, 54.1, 0);
        let sid = b.observe(&strong, true);
        assert!(b.get(sid).unwrap().confirmed);
        // Time passes (records elsewhere); the blip never recurs and is dropped.
        let mut later = rec(3, 400.0, 54.1, 200);
        later.max_snr_db = 9.0;
        b.observe(&later, true);
        for _ in 0..PRUNE_EVERY {
            b.mark_active(&[]);
        }
        assert!(b.get(id).is_none());
        assert!(b.get(sid).is_some());
    }
}
