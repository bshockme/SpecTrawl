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

/// See `Emitter::label`.
const STICKY_LABELS: [&str; 2] = ["Internal spur", "DC offset"];

/// Times a protocol must be identified on an emitter before it's believed...
const MIN_PROTOCOL_HITS: u32 = 2;
/// ...unless one identification alone contained this many syncs.
const STRONG_SYNCS: usize = 4;

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
    pub active: bool,
    /// Seen often, long or strongly enough not to be a noise blip.
    pub confirmed: bool,
    pub peak_snr_db: f32,
    /// Classification labels seen on this emitter's records (and its own
    /// group classifications), with counts.
    pub tally: BTreeMap<String, u32>,
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
    /// Protocol labels seen at least once with `STRONG_SYNCS` or more syncs.
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
        if let Some(l) = STICKY_LABELS.iter().find(|l| self.tally.contains_key(**l)) {
            return Some(l.to_string());
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

    fn majority_feature_label(&self) -> Option<String> {
        self.tally.iter().filter(|(l, _)| !self.protocols.contains_key(*l)).max_by_key(|(_, n)| **n).map(|(l, _)| l.clone())
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

    /// Fraction of the time between first and last seen that it was on.
    pub fn duty(&self) -> f64 {
        let span = self.last_seen.duration_since(self.first_seen).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if span <= 0.0 { 1.0 } else { (self.on_time_s / span).min(1.0) }
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

    /// The closest emitter a record fits, other than `except`.
    fn find(&self, c: f64, bw: f64, bin: f64, except: Option<u64>) -> Option<u64> {
        self.map
            .values()
            .filter(|e| Some(e.id) != except && Self::fits(e, c, bw, bin))
            .min_by(|a, b| (a.center_hz - c).abs().total_cmp(&(b.center_hz - c).abs()))
            .map(|e| e.id)
    }

    /// Assigns a record (open or just closed) to an emitter, creating one if
    /// needed, and updates the emitter's stats. Returns the emitter id.
    pub(crate) fn observe(&mut self, r: &SignalRecord, closed: bool) -> u64 {
        let bw = r.bandwidth_hz().max(1.0);
        let c = r.center_hz();
        let id = match self.by_record.get(&r.id) {
            Some(&id) if closed && self.map.get(&id).is_some_and(|e| e.records == 1) => self.refit(r, id, c, bw),
            Some(&id) => id,
            None => {
                let id = self.find(c, bw, r.bin_hz, None).unwrap_or_else(|| {
                    self.next_id += 1;
                    self.map.insert(self.next_id, Self::new_emitter(self.next_id, r, c, bw));
                    self.next_id
                });
                if let Some(e) = self.map.get_mut(&id) {
                    e.records += 1;
                    // Slow average so one odd record doesn't drag the centre.
                    e.center_hz += (c - e.center_hz) * 0.1;
                    e.bandwidth_hz += (bw - e.bandwidth_hz) * 0.1;
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
            if closed {
                e.on_time_s += r.duration_s();
                if r.rows >= 2 || r.max_snr_db >= BLIP_SNR_DB {
                    e.solid_records += 1;
                }
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
        let Some(from) = self.map.remove(&id) else { return id };
        self.by_record.insert(r.id, to);
        let Some(e) = self.map.get_mut(&to) else { return to };
        e.records += from.records;
        e.solid_records += from.solid_records;
        e.center_hz += (c - e.center_hz) * 0.1;
        e.bandwidth_hz += (bw - e.bandwidth_hz) * 0.1;
        e.on_time_s += from.on_time_s;
        e.first_seen = e.first_seen.min(from.first_seen);
        e.last_seen = e.last_seen.max(from.last_seen);
        e.active |= from.active;
        e.confirmed |= from.confirmed;
        e.peak_snr_db = e.peak_snr_db.max(from.peak_snr_db);
        e.last_end_sample = e.last_end_sample.max(from.last_end_sample);
        for (k, n) in from.tally {
            *e.tally.entry(k).or_default() += n;
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
        to
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
            active: false,
            confirmed: false,
            peak_snr_db: r.max_snr_db,
            tally: BTreeMap::new(),
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
        if matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") {
            return;
        }
        *e.tally.entry(c.label.clone()).or_default() += 1;
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
            if c.sync.as_ref().is_some_and(|s| s.count >= STRONG_SYNCS) {
                e.strong.insert(c.label.clone());
            }
        }
        let pcount = |l: &str| e.protocols.get(l).copied().unwrap_or(0);
        let tcount = |l: &str| e.tally.get(l).copied().unwrap_or(0);
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
    }

    pub(crate) fn get(&self, id: u64) -> Option<&Emitter> {
        self.map.get(&id)
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
        // First rows of a key-up: narrow, so it starts an emitter of its own...
        let mut r = rec(10, 856.3380, 1.8, 10);
        b.observe(&r, false);
        assert_eq!(b.map.len(), 2);
        // ...and joins the channel's emitter once it closes at full width.
        r.f_lo_hz = 856.3375e6 - 4.5e3;
        r.f_hi_hz = 856.3375e6 + 4.5e3;
        let id = b.observe(&r, true);
        assert_eq!(b.map.len(), 1);
        assert_eq!(b.by_record[&10], id);
        assert_eq!(b.get(id).unwrap().records, 4);
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
