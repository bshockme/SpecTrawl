//! Energy/burst detection on spectrum rows → signal records (DESIGN.md §5.3, §6).
//!
//! Per row:
//! 1. Noise floor: a low percentile of each block of bins (wide blocks so a
//!    single strong or wideband-ish signal doesn't lift it), smoothed over
//!    time and interpolated between block centres.
//! 2. Bins above floor + threshold form runs; runs closer than a small gap
//!    merge into segments.
//! 3. Segments that overlap an open track (in frequency, within the
//!    hangover in time) extend it; otherwise they open a new track. A track
//!    that overlaps two others merges them.
//! 4. Tracks unseen for the hangover close and become finished records.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use detect::spectral::is_empty_db;

use crate::{SpectrumRow, Tuning};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Bins per noise-floor block.
    pub block_bins: usize,
    /// Percentile (0..1) of each block taken as its floor.
    pub floor_pct: f32,
    /// Per-row smoothing of the block floors (0..1, higher = faster).
    pub floor_alpha: f32,
    /// A bin is active above floor + this.
    pub thr_db: f32,
    /// A segment must peak at least this far above the floor.
    pub min_peak_db: f32,
    /// Extent is trimmed to bins within this many dB of the segment peak
    /// (occupied bandwidth), so a strong signal's skirts don't inflate it.
    pub obw_db: f32,
    /// Active runs this close (in bins) merge into one segment.
    pub merge_gap_bins: usize,
    /// Frequency slack (bins) when matching a segment to a track.
    pub match_slack_bins: usize,
    /// Rows without activity before a track closes.
    pub hangover_rows: u64,
    /// A track this many rows old is established: matched by its typical
    /// extent, and never merged with another established track by one row
    /// that bridges them (see `associate`).
    pub established_rows: u32,
    /// Closed tracks shorter than this are dropped as noise.
    pub min_rows: u32,
    /// Fraction of the row (centred) to search; the rest is the tuner's
    /// roll-off and is ignored. 1.0 = whole row.
    pub usable_fraction: f32,
}

impl DetectorConfig {
    /// For live rows (~50/s, 610 Hz bins at 10 MS/s / 16k FFT).
    pub fn live(rows_per_sec: f64) -> Self {
        Self {
            block_bins: 1024,
            floor_pct: 0.2,
            floor_alpha: 0.05,
            thr_db: 7.0,
            min_peak_db: 10.0,
            obw_db: 26.0,
            merge_gap_bins: 3,
            match_slack_bins: 2,
            hangover_rows: (0.3 * rows_per_sec).ceil() as u64,
            established_rows: (0.2 * rows_per_sec).ceil() as u32,
            min_rows: 2,
            usable_fraction: 1.0,
        }
    }

    /// For stitched sweep rows (one per pass, seconds apart, coarse bins).
    pub fn sweep() -> Self {
        Self {
            block_bins: 512,
            floor_pct: 0.2,
            floor_alpha: 0.3,
            thr_db: 6.0,
            min_peak_db: 8.0,
            obw_db: 20.0,
            merge_gap_bins: 1,
            match_slack_bins: 1,
            hangover_rows: 1,
            established_rows: 3,
            min_rows: 1,
            usable_fraction: 1.0,
        }
    }
}

/// A detected emission. Open records (`active`) are still being extended.
#[derive(Debug, Clone)]
pub struct SignalRecord {
    pub id: u64,
    pub source_serial: std::sync::Arc<str>,
    pub first_seq: u64,
    pub last_seq: u64,
    pub t_start: SystemTime,
    pub t_end: SystemTime,
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    /// Width of one spectrum bin: the resolution of `f_lo_hz`/`f_hi_hz`.
    pub bin_hz: f64,
    pub peak_dbfs: f32,
    /// Mean over rows of (row peak − floor).
    pub mean_snr_db: f32,
    pub max_snr_db: f32,
    pub rows: u32,
    pub active: bool,
    /// Ring sample index at the end of the first and last rows that saw the
    /// signal (the IQ for this record lies just before/within these).
    pub first_row_end_sample: u64,
    pub last_row_end_sample: u64,
    /// Tuner centre when the record was made (live rows).
    pub tuner_center_hz: f64,
    /// Filled in by the classifier when available.
    pub class: Option<std::sync::Arc<detect::Classification>>,
    /// The emitter this record belongs to, and that emitter's best class.
    pub emitter: Option<u64>,
    pub emitter_class: Option<std::sync::Arc<detect::Classification>>,
    /// When that emitter was first seen (the view outlines recent ones).
    pub emitter_first_seen: Option<std::time::SystemTime>,
}

impl SignalRecord {
    /// What to show for this record: its own class if it names a protocol,
    /// else its emitter's if that does, else whichever exists. (An emitter's
    /// best class only names a protocol once that was seen twice.)
    pub fn display_class(&self) -> Option<&detect::Classification> {
        let own = self.class.as_deref().filter(|c| !matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted"));
        let em = self.emitter_class.as_deref();
        match (own, em) {
            (Some(o), _) if o.protocol.is_some() => Some(o),
            (_, Some(e)) if e.protocol.is_some() => Some(e),
            (Some(o), _) => Some(o),
            (None, e) => e,
        }
    }
}

impl SignalRecord {
    pub fn center_hz(&self) -> f64 {
        (self.f_lo_hz + self.f_hi_hz) / 2.0
    }

    pub fn bandwidth_hz(&self) -> f64 {
        self.f_hi_hz - self.f_lo_hz
    }

    pub fn duration_s(&self) -> f64 {
        self.t_end.duration_since(self.t_start).map(|d| d.as_secs_f64()).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone)]
struct Track {
    id: u64,
    lo: usize,
    hi: usize,
    first_seq: u64,
    last_seq: u64,
    t_start: SystemTime,
    t_end: SystemTime,
    peak_db: f32,
    snr_sum: f32,
    snr_max: f32,
    rows: u32,
    first_end: u64,
    last_end: u64,
    /// Rows in which each bin from `lo` to `hi` belonged to the track. Its
    /// typical extent (`core`) is what most rows covered: a few rows of
    /// smeared spectrum (a second Airspy on the same USB controller puts
    /// 13–26 dB pedestals around strong signals in some rows) would
    /// otherwise widen the record, and fuse its neighbours, for good.
    hits: Vec<u32>,
}

impl Track {
    /// Extends `lo..=hi` to cover `lo2..=hi2` without counting a row.
    fn extend(&mut self, lo2: usize, hi2: usize) {
        if lo2 < self.lo {
            let mut v = vec![0; self.lo - lo2];
            v.extend_from_slice(&self.hits);
            self.hits = v;
            self.lo = lo2;
        }
        if hi2 > self.hi {
            self.hits.resize(self.hits.len() + (hi2 - self.hi), 0);
            self.hi = hi2;
        }
    }

    /// Counts one row's activity over `lo2..=hi2`.
    fn cover(&mut self, lo2: usize, hi2: usize) {
        self.extend(lo2, hi2);
        for c in &mut self.hits[lo2 - self.lo..=hi2 - self.lo] {
            *c += 1;
        }
    }

    /// Adds another track's per-bin counts (a merge).
    fn absorb(&mut self, o: &Track) {
        self.extend(o.lo, o.hi);
        for (k, c) in o.hits.iter().enumerate() {
            self.hits[o.lo - self.lo + k] += c;
        }
    }

    /// From the first to the last bin at least a tenth as busy as the
    /// busiest one: what the emission covers, all lobes of it (an FSK
    /// signal's two tones, an FM broadcast's ±75 kHz swing, which a
    /// majority would cut to a third), without the bins only stray smeared
    /// rows reached.
    fn core(&self) -> (usize, usize) {
        let max = self.hits.iter().copied().max().unwrap_or(0);
        let need = max.div_ceil(CORE_SHARE).max(1);
        let first = self.hits.iter().position(|&c| c >= need);
        let last = self.hits.iter().rposition(|&c| c >= need);
        match (first, last) {
            (Some(a), Some(b)) => (self.lo + a, self.lo + b),
            _ => (self.lo, self.hi),
        }
    }

    /// Where a new segment is matched against: the typical extent once the
    /// track is established, everything it covered until then.
    fn reach(&self, established: u32) -> (usize, usize) {
        if self.rows >= established { self.core() } else { (self.lo, self.hi) }
    }

    fn centre(&self) -> f64 {
        let (a, b) = self.core();
        (a + b) as f64 / 2.0
    }

    /// One row's activity over `lo..=hi`.
    fn touch(&mut self, lo: usize, hi: usize, peak: f32, snr: f32, row: &SpectrumRow) {
        self.cover(lo, hi);
        if self.last_seq != row.seq {
            self.rows += 1;
            self.snr_sum += snr;
        }
        self.last_seq = row.seq;
        self.last_end = row.end_sample;
        self.t_end = row.time;
        self.peak_db = self.peak_db.max(peak);
        self.snr_max = self.snr_max.max(snr);
    }
}

pub struct BurstDetector {
    cfg: DetectorConfig,
    serial: std::sync::Arc<str>,
    tuning: Option<Tuning>,
    bins: usize,
    block_floor: Vec<f32>,
    floor: Vec<f32>,
    tracks: Vec<Track>,
    scratch: Vec<f32>,
    /// Rows in which one segment bridged two established tracks, by their
    /// ids (lower first): frequent bridges are one emission (an FSK
    /// signal's two tones), occasional ones a smeared row.
    bridges: HashMap<(u64, u64), u32>,
}

struct Segment {
    lo: usize,
    hi: usize,
    peak_db: f32,
    snr: f32,
}

impl BurstDetector {
    pub fn new(cfg: DetectorConfig, serial: String) -> Self {
        Self {
            cfg,
            serial: serial.into(),
            tuning: None,
            bins: 0,
            block_floor: Vec::new(),
            floor: Vec::new(),
            tracks: Vec::new(),
            scratch: Vec::new(),
            bridges: HashMap::new(),
        }
    }

    /// Feeds one row. Returns records that closed on this row.
    pub fn process(&mut self, row: &SpectrumRow) -> Vec<SignalRecord> {
        let mut closed = Vec::new();
        if self.tuning != Some(row.tuning) || self.bins != row.db.len() {
            // New frequency axis: everything open ends here.
            closed.extend(self.close_all());
            self.bridges.clear();
            self.tuning = Some(row.tuning);
            self.bins = row.db.len();
            self.block_floor.clear();
        }
        self.update_floor(&row.db);
        let segs = self.segments(&row.db);
        self.associate(segs, row);

        let (hang, min_rows) = (self.cfg.hangover_rows, self.cfg.min_rows);
        let mut i = 0;
        while i < self.tracks.len() {
            if row.seq.saturating_sub(self.tracks[i].last_seq) > hang {
                let t = self.tracks.swap_remove(i);
                self.bridges.retain(|k, _| k.0 != t.id && k.1 != t.id);
                if t.rows >= min_rows {
                    closed.push(self.to_record(&t, false));
                }
            } else {
                i += 1;
            }
        }
        closed
    }

    /// Snapshot of the tracks still open, as records.
    pub fn open_records(&self) -> Vec<SignalRecord> {
        self.tracks.iter().filter(|t| t.rows >= self.cfg.min_rows).map(|t| self.to_record(t, true)).collect()
    }

    /// Current noise-floor estimate per bin (dBFS).
    pub fn floor(&self) -> &[f32] {
        &self.floor
    }

    fn close_all(&mut self) -> Vec<SignalRecord> {
        let tracks = std::mem::take(&mut self.tracks);
        tracks.iter().filter(|t| t.rows >= self.cfg.min_rows).map(|t| self.to_record(t, false)).collect()
    }

    fn bin_hz(&self, bin: f64) -> f64 {
        let t = self.tuning.expect("tuning set");
        t.center_hz - t.span_hz / 2.0 + bin / self.bins as f64 * t.span_hz
    }

    fn to_record(&self, t: &Track, active: bool) -> SignalRecord {
        SignalRecord {
            id: t.id,
            source_serial: self.serial.clone(),
            first_seq: t.first_seq,
            last_seq: t.last_seq,
            t_start: t.t_start,
            t_end: t.t_end,
            f_lo_hz: self.bin_hz(t.core().0 as f64),
            f_hi_hz: self.bin_hz(t.core().1 as f64 + 1.0),
            bin_hz: self.tuning.map(|t| t.span_hz / self.bins as f64).unwrap_or(0.0),
            peak_dbfs: t.peak_db,
            mean_snr_db: t.snr_sum / t.rows.max(1) as f32,
            max_snr_db: t.snr_max,
            rows: t.rows,
            active,
            first_row_end_sample: t.first_end,
            last_row_end_sample: t.last_end,
            tuner_center_hz: self.tuning.map(|t| t.center_hz).unwrap_or(0.0),
            class: None,
            emitter: None,
            emitter_class: None,
            emitter_first_seen: None,
        }
    }

    /// Bins at each end of a row of `n` outside the usable fraction.
    fn margin(&self, n: usize) -> usize {
        ((1.0 - self.cfg.usable_fraction.clamp(0.0, 1.0)) / 2.0 * n as f32) as usize
    }

    fn update_floor(&mut self, db: &[f32]) {
        let n = db.len();
        let b = self.cfg.block_bins.min(n).max(1);
        let nb = n.div_ceil(b);
        let fresh = self.block_floor.len() != nb;
        if fresh {
            self.block_floor = vec![0.0; nb];
        }
        // Unmeasured bins (a segmented sweep's skipped hops) don't count,
        // nor do the bins outside the usable fraction: the tuner's IF
        // filter falls off a cliff there (an Airspy at 10 MS/s: 14 dB
        // down within 200 kHz past ±4.45 MHz), which drags a block's
        // percentile below the real floor and makes the last 100 kHz
        // inside the edge a "signal" at every tuning. A block with no
        // measured bins borrows its nearest measured neighbour's.
        let margin = self.margin(n);
        let mut unmeasured: Vec<usize> = Vec::new();
        for k in 0..nb {
            let (lo, hi) = (k * b, ((k + 1) * b).min(n));
            let s = &db[lo..hi];
            self.scratch.clear();
            self.scratch.extend(
                s.iter().enumerate().filter(|(i, v)| (margin..n - margin).contains(&(lo + i)) && !is_empty_db(**v)).map(|(_, v)| *v),
            );
            if self.scratch.is_empty() {
                unmeasured.push(k);
                continue;
            }
            let idx = ((self.scratch.len() as f32 * self.cfg.floor_pct) as usize).min(self.scratch.len() - 1);
            let (_, q, _) = self.scratch.select_nth_unstable_by(idx, f32::total_cmp);
            let q = *q;
            let f = &mut self.block_floor[k];
            *f = if fresh { q } else { *f + self.cfg.floor_alpha * (q - *f) };
        }
        if unmeasured.len() < nb {
            for &k in &unmeasured {
                let near = (0..nb).filter(|j| !unmeasured.contains(j)).min_by_key(|j| j.abs_diff(k)).unwrap();
                self.block_floor[k] = self.block_floor[near];
            }
        }
        // Interpolate between block centres.
        self.floor.resize(n, 0.0);
        let centre = |k: usize| (k * b) as f32 + (((k + 1) * b).min(n) - k * b) as f32 / 2.0;
        let mut k = 0;
        for (i, out) in self.floor.iter_mut().enumerate() {
            let x = i as f32 + 0.5;
            while k + 1 < nb && centre(k + 1) < x {
                k += 1;
            }
            *out = if nb == 1 || x <= centre(0) {
                self.block_floor[0]
            } else if k + 1 >= nb {
                self.block_floor[nb - 1]
            } else {
                let (x0, x1) = (centre(k), centre(k + 1));
                let t = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
                self.block_floor[k] + t * (self.block_floor[k + 1] - self.block_floor[k])
            };
        }
    }

    fn segments(&self, db: &[f32]) -> Vec<Segment> {
        let thr = self.cfg.thr_db;
        let mut out: Vec<Segment> = Vec::new();
        let n = db.len();
        let margin = self.margin(n);
        let mut i = margin;
        let n = n - margin;
        while i < n {
            if is_empty_db(db[i]) || db[i] - self.floor[i] <= thr {
                i += 1;
                continue;
            }
            let lo = i;
            let (mut peak, mut snr) = (f32::MIN, f32::MIN);
            while i < n && !is_empty_db(db[i]) && db[i] - self.floor[i] > thr {
                peak = peak.max(db[i]);
                snr = snr.max(db[i] - self.floor[i]);
                i += 1;
            }
            let hi = i - 1;
            match out.last_mut() {
                Some(prev) if lo - prev.hi - 1 <= self.cfg.merge_gap_bins => {
                    prev.hi = hi;
                    prev.peak_db = prev.peak_db.max(peak);
                    prev.snr = prev.snr.max(snr);
                }
                _ => out.push(Segment { lo, hi, peak_db: peak, snr }),
            }
        }
        out.retain(|s| s.snr >= self.cfg.min_peak_db);
        for s in &mut out {
            let cut = s.peak_db - self.cfg.obw_db;
            while s.lo < s.hi && db[s.lo] < cut {
                s.lo += 1;
            }
            while s.hi > s.lo && db[s.hi] < cut {
                s.hi -= 1;
            }
        }
        out
    }

    fn associate(&mut self, segs: Vec<Segment>, row: &SpectrumRow) {
        let slack = self.cfg.match_slack_bins;
        let est = self.cfg.established_rows;
        for s in segs {
            let lo = s.lo.saturating_sub(slack);
            let hi = s.hi + slack;
            let hits: Vec<usize> = self
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    let (a, b) = t.reach(est);
                    a <= hi && b >= lo
                })
                .map(|(i, _)| i)
                .collect();
            if hits.is_empty() {
                self.tracks.push(Track {
                    id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                    lo: s.lo,
                    hi: s.hi,
                    first_seq: row.seq,
                    last_seq: row.seq,
                    t_start: row.time,
                    t_end: row.time,
                    peak_db: s.peak_db,
                    snr_sum: s.snr,
                    snr_max: s.snr,
                    rows: 1,
                    first_end: row.end_sample,
                    last_end: row.end_sample,
                    hits: vec![1; s.hi - s.lo + 1],
                });
                continue;
            }
            // Pull out every overlapping track (descending indices keep
            // swap_remove safe).
            let mut sorted = hits.clone();
            sorted.sort_unstable();
            let mut taken: Vec<Track> = sorted.iter().rev().map(|&j| self.tracks.swap_remove(j)).collect();
            // One segment touching several tracks makes them one emission
            // only when they are (an FSK signal's two tones sit closer than
            // any two channels: FLEX and POCSAG ±4.5–4.8 kHz, channels ≥ 12.5
            // kHz apart) or when it keeps happening (a wide signal's
            // fragments). A stray bridging row (adjacent channels of a
            // trunked site joined by a smeared row) is split between them,
            // halfway between their typical extents.
            taken.sort_by(|a, b| a.centre().total_cmp(&b.centre()));
            let bin_hz = self.tuning.map_or(0.0, |t| t.span_hz / self.bins.max(1) as f64);
            let mut k = 0;
            while k + 1 < taken.len() {
                let (a, b) = (&taken[k], &taken[k + 1]);
                let key = (a.id.min(b.id), a.id.max(b.id));
                let n = {
                    let c = self.bridges.entry(key).or_insert(0);
                    *c += 1;
                    *c
                };
                let close = (b.centre() - a.centre()) * bin_hz < FSK_PAIR_HZ;
                let frequent = n >= 3 && n * 4 >= a.rows.min(b.rows);
                // Pieces of one wide signal (a broadcast FM station's
                // swing breaks it up) touch for their size; two channels
                // of a raster are spaced well beyond their width.
                let ((a0, a1), (b0, b1)) = (a.core(), b.core());
                let gap = b0.saturating_sub(a1 + 1).max(a0.saturating_sub(b1 + 1));
                let touching = 2 * gap <= (a1 - a0 + 1).min(b1 - b0 + 1);
                if close || frequent || touching {
                    let o = taken.remove(k + 1);
                    self.bridges.retain(|q, _| q.0 != o.id && q.1 != o.id);
                    let (keep, other) = if o.first_seq < taken[k].first_seq { (o, taken.remove(k)) } else { (taken.remove(k), o) };
                    let mut keep = keep;
                    merge_into(&mut keep, &other);
                    taken.insert(k, keep);
                } else {
                    k += 1;
                }
            }
            if taken.len() == 1 {
                let mut t = taken.remove(0);
                t.touch(s.lo, s.hi, s.peak_db, s.snr, row);
                self.tracks.push(t);
                continue;
            }
            let centres: Vec<f64> = taken.iter().map(|t| t.centre()).collect();
            for (k, t) in taken.iter_mut().enumerate() {
                let a = if k == 0 { s.lo } else { s.lo.max(((centres[k - 1] + centres[k]) / 2.0).floor() as usize + 1) };
                let b = if k + 1 == centres.len() { s.hi } else { s.hi.min(((centres[k] + centres[k + 1]) / 2.0).floor() as usize) };
                if a > b {
                    continue;
                }
                let (peak, snr) = (a..=b).fold((f32::MIN, f32::MIN), |(p, q), i| (p.max(row.db[i]), q.max(row.db[i] - self.floor[i])));
                t.touch(a, b, peak, snr, row);
            }
            self.tracks.extend(taken);
        }
    }
}

/// A bin belongs to a track's extent once it was active in at least
/// 1/`CORE_SHARE` of the rows its busiest bin was.
const CORE_SHARE: u32 = 10;

/// Established tracks whose typical extents are centred closer than this
/// are one emission (an FSK signal's two tones), never two channels.
const FSK_PAIR_HZ: f64 = 11e3;

/// Merges track `o` into `t`.
fn merge_into(t: &mut Track, o: &Track) {
    t.absorb(o);
    t.peak_db = t.peak_db.max(o.peak_db);
    // The tracks overlap in time, so `rows` keeps the larger count (not the
    // sum) and `snr_sum` is rescaled to it: the merged mean is the
    // row-weighted mean of the two means, never above either track's
    // maximum.
    let mean = (t.snr_sum + o.snr_sum) / (t.rows + o.rows) as f32;
    t.rows = t.rows.max(o.rows);
    t.snr_sum = mean * t.rows as f32;
    t.snr_max = t.snr_max.max(o.snr_max);
    t.first_seq = t.first_seq.min(o.first_seq);
    t.t_start = t.t_start.min(o.t_start);
    t.last_seq = t.last_seq.max(o.last_seq);
    t.first_end = t.first_end.min(o.first_end);
    t.last_end = t.last_end.max(o.last_end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u64, db: Vec<f32>) -> SpectrumRow {
        SpectrumRow {
            seq,
            tuning: Tuning { center_hz: 100e6, span_hz: 1e6 },
            end_sample: 0,
            time: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(seq * 20),
            db,
        }
    }

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                -100.0 + ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 3.0
            })
            .collect()
    }

    #[test]
    fn burst_becomes_one_record_with_right_extent() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let n = 4096;
        let mut closed = Vec::new();
        for seq in 1..=100u64 {
            let mut db = noise(n, seq);
            if (30..60).contains(&seq) {
                for v in &mut db[1000..1020] {
                    *v = -70.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1, "{closed:?}");
        let r = &closed[0];
        assert_eq!((r.first_seq, r.last_seq), (30, 59));
        let bin = 1e6 / n as f64;
        assert!((r.f_lo_hz - (99.5e6 + 1000.0 * bin)).abs() < bin * 1.5);
        assert!((r.f_hi_hz - (99.5e6 + 1020.0 * bin)).abs() < bin * 1.5);
        assert!(r.mean_snr_db > 25.0);
    }

    #[test]
    fn unmeasured_stretch_makes_no_records_and_keeps_the_floor_honest() {
        // A segmented sweep row: bins 1000..3000 were never measured.
        let mut d = BurstDetector::new(DetectorConfig::sweep(), "T".into());
        let n = 4096;
        let mut closed = Vec::new();
        for seq in 1..=40u64 {
            let mut db = noise(n, seq);
            for v in &mut db[1000..3000] {
                *v = detect::spectral::EMPTY_DB;
            }
            if (10..30).contains(&seq) {
                // A real signal right at the gap's edge, and one far from it.
                for v in &mut db[990..1000] {
                    *v = -75.0;
                }
                for v in &mut db[3500..3510] {
                    *v = -75.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
            // The floor over measured bins stays near the noise, even next
            // to the gap, so nothing in the quiet rows is "above floor".
            assert!(d.floor().iter().all(|f| (-103.0..-97.0).contains(f)), "floor off: {:?}", &d.floor()[980..1010]);
        }
        assert_eq!(closed.len(), 2, "{closed:?}");
        for r in &closed {
            assert_eq!((r.first_seq, r.last_seq), (10, 29));
            assert!(r.mean_snr_db > 20.0 && r.mean_snr_db < 30.0, "snr {}", r.mean_snr_db);
        }
    }

    #[test]
    fn strong_signal_skirts_are_trimmed() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=60u64 {
            let mut db = noise(4096, seq);
            if (10..40).contains(&seq) {
                // 10-bin core at -40 dBFS with skirts falling 2 dB/bin to the floor.
                for k in 0..80usize {
                    let lvl = if k < 10 { -40.0 } else { -40.0 - 2.0 * (k - 9) as f32 };
                    db[2000 + k] = db[2000 + k].max(lvl);
                    db[1999 - k.min(1999)] = db[1999 - k.min(1999)].max(if k < 1 { -40.0 } else { -40.0 - 2.0 * k as f32 });
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1);
        let bins = closed[0].bandwidth_hz() / (1e6 / 4096.0);
        assert!(bins < 40.0, "bandwidth {bins} bins");
    }

    /// Every `every`-th row a 26 dB pedestal smears the spectrum around
    /// strong signals (what a second Airspy on the same USB controller does
    /// on air).
    fn smeared(db: &mut [f32], lo: usize, hi: usize) {
        for v in &mut db[lo..hi] {
            *v = v.max(-80.0);
        }
    }

    #[test]
    fn occasional_smeared_rows_dont_widen_a_record() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                if seq % 25 == 0 {
                    smeared(&mut db, 1950, 2070);
                }
                for v in &mut db[2000..2020] {
                    *v = -60.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1, "{closed:?}");
        let bins = closed[0].bandwidth_hz() / (1e6 / 4096.0);
        assert!((18.0..24.0).contains(&bins), "bandwidth {bins} bins");
    }

    #[test]
    fn a_smeared_row_doesnt_fuse_established_neighbours() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                if seq % 11 == 0 {
                    smeared(&mut db, 1990, 2140);
                }
                // Two channels 25 kHz (102 bins) apart.
                for v in &mut db[2000..2018] {
                    *v = -55.0;
                }
                for v in &mut db[2102..2120] {
                    *v = -62.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 2, "{closed:?}");
        for r in &closed {
            let bins = r.bandwidth_hz() / (1e6 / 4096.0);
            assert!((16.0..22.0).contains(&bins), "bandwidth {bins} bins: {r:?}");
        }
    }

    #[test]
    fn fsk_tones_stay_one_record() {
        // An idle pager's two tones 9.6 kHz (39 bins) apart; the gap
        // between them fills on some rows (data transitions).
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                for k in (2000..2008).chain(2039..2047) {
                    db[k] = -60.0;
                }
                if seq % 6 == 0 {
                    for v in &mut db[2008..2039] {
                        *v = -75.0;
                    }
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1, "{closed:?}");
        let bins = closed[0].bandwidth_hz() / (1e6 / 4096.0);
        assert!((44.0..52.0).contains(&bins), "bandwidth {bins} bins");
    }

    #[test]
    fn a_swinging_fm_signal_keeps_its_full_width() {
        // A broadcast FM signal's instantaneous spectrum is ~60 bins wide
        // but swings over 200 bins with the programme.
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                let off = ((seq as f64 * 0.7).sin() * 70.0) as i64;
                let c = (2100 + off) as usize;
                for v in &mut db[c - 30..c + 30] {
                    *v = -60.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1, "{closed:?}");
        let bins = closed[0].bandwidth_hz() / (1e6 / 4096.0);
        assert!(bins > 170.0, "bandwidth {bins} bins");
    }

    #[test]
    fn pieces_of_a_wide_signal_become_one_record() {
        // A wide signal that some rows show as two pieces with a dip
        // between them, and other rows whole.
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                for k in (2000..2060).chain(2080..2160) {
                    db[k] = -60.0;
                }
                if seq % 9 == 0 {
                    for v in &mut db[2060..2080] {
                        *v = -66.0;
                    }
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1, "{closed:?}");
        let bins = closed[0].bandwidth_hz() / (1e6 / 4096.0);
        assert!(bins > 150.0, "bandwidth {bins} bins");
    }

    #[test]
    fn noise_alone_makes_no_records() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut count = 0;
        for seq in 1..=500u64 {
            count += d.process(&row(seq, noise(4096, seq))).len();
        }
        assert_eq!(count + d.open_records().len(), 0);
    }

    #[test]
    fn slotted_bursts_within_hangover_stay_one_record() {
        let mut d = BurstDetector::new(DetectorConfig::live(50.0), "T".into());
        let mut closed = Vec::new();
        for seq in 1..=120u64 {
            let mut db = noise(4096, seq);
            if (20..80).contains(&seq) && seq % 2 == 0 {
                for v in &mut db[2000..2016] {
                    *v = -75.0;
                }
            }
            closed.extend(d.process(&row(seq, db)));
        }
        assert_eq!(closed.len(), 1);
    }
}
