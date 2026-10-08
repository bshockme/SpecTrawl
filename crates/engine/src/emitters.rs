//! Groups signal records into emitters: persistent transmitters on a channel.
//!
//! A record joins the emitter whose centre is within tolerance and whose
//! bandwidth is comparable; otherwise it starts a new one. Emitters keep
//! activity stats and a tally of what their records were classified as, and
//! can carry a classification of their own (made over a window spanning many
//! short bursts, which single records can't support).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use detect::Classification;

use crate::SignalRecord;

/// Most emitters kept per source; the least recently seen inactive ones go first.
const MAX_EMITTERS: usize = 5000;

/// Times a protocol must be identified on an emitter before it's believed.
const MIN_PROTOCOL_HITS: u32 = 2;

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
    pub peak_snr_db: f32,
    /// Classification labels seen on this emitter's records (and its own
    /// group classifications), with counts.
    pub tally: BTreeMap<String, u32>,
    /// Labels in `tally` that named a protocol.
    pub protocols: BTreeMap<String, u32>,
    /// Latest group-level classification.
    pub class: Option<Arc<Classification>>,
    /// What the emitter is best described as: the most frequent
    /// protocol-naming result if any, else the latest feature result.
    pub best: Option<Arc<Classification>>,
    pub(crate) class_at: Option<Instant>,
    /// Newest record's ring position and tuner centre (for group windows).
    pub(crate) last_end_sample: u64,
    pub(crate) tuner_center_hz: f64,
}

impl Emitter {
    /// Best single label: the most frequent protocol label if any was seen,
    /// else the group classification, else the most frequent feature label.
    pub fn label(&self) -> Option<String> {
        if let Some(l) = self.protocol_label() {
            return Some(l);
        }
        if let Some(c) = &self.class {
            if c.protocol.is_some() || !matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") {
                return Some(c.label.clone());
            }
        }
        self.tally
            .iter()
            .filter(|(l, _)| !matches!(l.as_str(), "too short" | "IQ expired" | "interrupted"))
            .max_by_key(|(_, n)| **n)
            .map(|(l, _)| l.clone())
    }

    /// The protocol label seen most, if it was seen at least
    /// `MIN_PROTOCOL_HITS` times (one stray result isn't an identification).
    pub fn protocol_label(&self) -> Option<String> {
        self.protocols.iter().filter(|(_, n)| **n >= MIN_PROTOCOL_HITS).max_by_key(|(_, n)| **n).map(|(l, _)| l.clone())
    }

    pub fn has_protocol(&self) -> bool {
        self.protocol_label().is_some()
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
}

impl EmitterBook {
    /// Frequency tolerance for joining: a quarter of the bandwidth, at least
    /// 2.5 kHz (record centres jitter with modulation).
    fn tolerance(bw: f64) -> f64 {
        (bw * 0.25).max(2.5e3)
    }

    /// Assigns a record (open or just closed) to an emitter, creating one if
    /// needed, and updates the emitter's stats. Returns the emitter id.
    pub(crate) fn observe(&mut self, r: &SignalRecord, closed: bool) -> u64 {
        let bw = r.bandwidth_hz().max(1.0);
        let c = r.center_hz();
        let id = match self.by_record.get(&r.id) {
            Some(&id) => id,
            None => {
                let found = self
                    .map
                    .values()
                    .filter(|e| {
                        let ratio = bw / e.bandwidth_hz.max(1.0);
                        (e.center_hz - c).abs() <= Self::tolerance(e.bandwidth_hz.max(bw)) && (0.33..=3.0).contains(&ratio)
                    })
                    .min_by(|a, b| (a.center_hz - c).abs().total_cmp(&(b.center_hz - c).abs()))
                    .map(|e| e.id);
                let id = found.unwrap_or_else(|| {
                    self.next_id += 1;
                    self.map.insert(self.next_id, Emitter {
                        id: self.next_id,
                        center_hz: c,
                        bandwidth_hz: bw,
                        first_seen: r.t_start,
                        last_seen: r.t_end,
                        records: 0,
                        on_time_s: 0.0,
                        active: false,
                        peak_snr_db: r.max_snr_db,
                        tally: BTreeMap::new(),
                        protocols: BTreeMap::new(),
                        class: None,
                        best: None,
                        class_at: None,
                        last_end_sample: r.last_row_end_sample,
                        tuner_center_hz: r.tuner_center_hz,
                    });
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
            }
        }
        id
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

    /// Stores a group-level classification for an emitter.
    pub(crate) fn set_group_class(&mut self, id: u64, c: Arc<Classification>) {
        if let Some(e) = self.map.get_mut(&id) {
            e.class = Some(c.clone());
            e.class_at = Some(Instant::now());
        }
        self.note(id, &c);
    }

    fn note(&mut self, id: u64, c: &Arc<Classification>) {
        let Some(e) = self.map.get_mut(&id) else { return };
        if matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") {
            return;
        }
        *e.tally.entry(c.label.clone()).or_default() += 1;
        if c.protocol.is_some() {
            *e.protocols.entry(c.label.clone()).or_default() += 1;
        }
        let count = |l: &str| e.protocols.get(l).copied().unwrap_or(0);
        let trusted = |cl: &Classification| cl.protocol.is_some() && count(&cl.label) >= MIN_PROTOCOL_HITS;
        let replace = match &e.best {
            None => c.protocol.is_none() || trusted(c),
            // A trusted protocol result is only displaced by a better-supported one.
            Some(b) if trusted(b) => trusted(c) && count(&c.label) >= count(&b.label),
            // Otherwise the newest result describes the emitter, unless it's an
            // untrusted protocol claim.
            Some(_) => c.protocol.is_none() || trusted(c),
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
}
