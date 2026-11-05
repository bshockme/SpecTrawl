//! The burst detector: waterfall rows in, records out (re-homed from `engine-old::bursts`, spec §4.4 and §8).
//!
//! Each row is reduced to the detector's resolution (the sum of `reduce` adjacent bins' power, 4 at the default: 610 Hz
//! at 10 MS/s, the resolution its thresholds were verified at on air), compared with a per-block percentile noise floor,
//! and cut into active segments; segments are matched to open tracks. A track's extent is its core (the bins hit in at
//! least a tenth of its busiest bin's rows), so stray smeared rows neither widen it nor fuse it with a neighbour; one
//! segment touching several tracks merges them only when they are an FSK tone pair, are bridged often, or touch for
//! their size, and is split between them otherwise. A track closes after `hangover_rows` without activity, and every
//! track closes when the rows move to another segment (a retune or a gap): a record never spans two segments.

use std::collections::HashMap;

use api::{RecordId, Row, Segment};

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Adjacent row bins summed into one detector bin.
    pub reduce: usize,
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
    /// Extent is trimmed to bins within this many dB of the segment peak (occupied bandwidth), so a strong signal's
    /// skirts don't inflate it.
    pub obw_db: f32,
    /// Active runs this close (in bins) merge into one segment.
    pub merge_gap_bins: usize,
    /// Frequency slack (bins) when matching a segment to a track.
    pub match_slack_bins: usize,
    /// Rows without activity before a track closes.
    pub hangover_rows: u64,
    /// A track this many rows old is established: matched by its typical extent.
    pub established_rows: u32,
    /// Tracks shorter than this are noise: never reported, never given an id.
    pub min_rows: u32,
    /// Fraction of the row (centred) to search; the rest is the tuner's roll-off. 1.0 = whole row.
    pub usable_fraction: f32,
}

impl DetectorConfig {
    /// For live rows at `rows_per_sec` (verified at ~50/s with 610 Hz bins).
    pub fn live(rows_per_sec: f64) -> Self {
        Self {
            reduce: 4,
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

    /// For stitched sweep rows (one per pass, seconds apart, coarse bins): step 4a.
    pub fn sweep() -> Self {
        Self {
            reduce: 1,
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

    /// Rows per second of a stage's rows: a row of `blocks` blocks spans `(blocks + 1)·L` samples (block `b` covers
    /// `(b − 1)·L .. (b + 1)·L`) and advances `blocks·L`.
    pub fn rows_per_sec(row: &Row) -> f64 {
        let span = row.end_sample.saturating_sub(row.first_sample) as f64;
        let hop = span / (row.blocks as f64 + 1.0);
        if hop > 0.0 { row.segment.sample_rate / (row.blocks as f64 * hop) } else { 50.0 }
    }
}

/// A record as the detector knows it: sample-indexed, in one segment. The book adds wall times, the emitter and the
/// claims.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub id: RecordId,
    pub segment: Segment,
    pub first_sample: u64,
    pub end_sample: u64,
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    pub bin_hz: f64,
    pub peak_dbfs: f32,
    pub mean_dbfs: f32,
    pub mean_snr_db: f32,
    pub max_snr_db: f32,
    pub rows: u32,
    pub open: bool,
}

impl Detected {
    pub fn center_hz(&self) -> f64 {
        (self.f_lo_hz + self.f_hi_hz) / 2.0
    }

    pub fn bandwidth_hz(&self) -> f64 {
        self.f_hi_hz - self.f_lo_hz
    }
}

/// What one row changed.
#[derive(Debug, Default)]
pub struct Output {
    /// Records that closed on this row.
    pub closed: Vec<Detected>,
    /// `(gone, into)`: a visible record merged into another (an emission first seen as pieces).
    pub merged: Vec<(RecordId, RecordId)>,
}

/// The bins one row covers, its place in the stream, and its axis.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    pub segment: &'a Segment,
    pub first_sample: u64,
    pub end_sample: u64,
    /// dBFS per detector bin, DC-centred (negative frequencies first); non-finite = not measured.
    pub db: &'a [f32],
}

#[derive(Debug, Clone)]
struct Track {
    /// The detector's own id (bridges); the record id comes once the track is long enough to report.
    key: u64,
    record: Option<RecordId>,
    lo: usize,
    hi: usize,
    first_row: u64,
    last_row: u64,
    first_sample: u64,
    end_sample: u64,
    peak_db: f32,
    snr_sum: f32,
    snr_max: f32,
    /// Sum over rows of the mean power (dB) across the bins the row touched.
    level_sum: f32,
    rows: u32,
    /// Rows in which each bin from `lo` to `hi` belonged to the track. Its typical extent ([`Track::core`]) is what
    /// most rows covered: a few rows of smeared spectrum (a second radio on the same USB controller puts 13–26 dB
    /// pedestals around strong signals in some rows) would otherwise widen the record, and fuse its neighbours, for good.
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

    /// From the first to the last bin at least a tenth as busy as the busiest one: what the emission covers, all lobes
    /// of it (an FSK signal's two tones, an FM broadcast's ±75 kHz swing, which a majority would cut to a third),
    /// without the bins only stray smeared rows reached.
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

    /// Where a new segment is matched against: the typical extent once the track is established, everything it
    /// covered until then.
    fn reach(&self, established: u32) -> (usize, usize) {
        if self.rows >= established { self.core() } else { (self.lo, self.hi) }
    }

    fn centre(&self) -> f64 {
        let (a, b) = self.core();
        (a + b) as f64 / 2.0
    }

    /// One row's activity over `lo..=hi`.
    fn touch(&mut self, lo: usize, hi: usize, s: Activity, row: u64, end_sample: u64) {
        self.cover(lo, hi);
        if self.last_row != row {
            self.rows += 1;
            self.snr_sum += s.snr;
            self.level_sum += s.level;
        }
        self.last_row = row;
        self.end_sample = end_sample;
        self.peak_db = self.peak_db.max(s.peak);
        self.snr_max = self.snr_max.max(s.snr);
    }
}

/// One segment's (or one piece of a split segment's) power in one row.
#[derive(Debug, Clone, Copy)]
struct Activity {
    peak: f32,
    snr: f32,
    /// Mean power across its bins, dB.
    level: f32,
}

struct Seg {
    lo: usize,
    hi: usize,
    act: Activity,
}

pub struct Detector {
    cfg: DetectorConfig,
    /// The segment the open tracks belong to.
    segment: Option<Segment>,
    bins: usize,
    block_floor: Vec<f32>,
    floor: Vec<f32>,
    tracks: Vec<Track>,
    scratch: Vec<f32>,
    /// The current row's dB, reduced.
    db: Vec<f32>,
    row: u64,
    next_key: u64,
    /// Rows in which one segment bridged two tracks, by their keys (lower first): frequent bridges are one emission (an
    /// FSK signal's two tones), occasional ones a smeared row.
    bridges: HashMap<(u64, u64), u32>,
}

impl Detector {
    pub fn new(cfg: DetectorConfig) -> Self {
        Self {
            cfg,
            segment: None,
            bins: 0,
            block_floor: Vec::new(),
            floor: Vec::new(),
            tracks: Vec::new(),
            scratch: Vec::new(),
            db: Vec::new(),
            row: 0,
            next_key: 1,
            bridges: HashMap::new(),
        }
    }

    pub fn config(&self) -> &DetectorConfig {
        &self.cfg
    }

    /// Feeds one stage row (linear power, 1.0 = 0 dBFS per bin). `ids` gives each track its record id when it first
    /// becomes reportable.
    pub fn process(&mut self, row: &Row, ids: &mut dyn FnMut() -> RecordId) -> Output {
        let k = self.cfg.reduce.max(1);
        self.db.clear();
        self.db.extend(row.power.chunks(k).map(|c| {
            let p: f32 = c.iter().sum();
            if p.is_finite() { 10.0 * p.max(1e-30).log10() } else { f32::NAN }
        }));
        let db = std::mem::take(&mut self.db);
        let out =
            self.process_db(Frame { segment: &row.segment, first_sample: row.first_sample, end_sample: row.end_sample, db: &db }, ids);
        self.db = db;
        out
    }

    /// Feeds one row already in detector bins.
    pub fn process_db(&mut self, f: Frame<'_>, ids: &mut dyn FnMut() -> RecordId) -> Output {
        let mut out = Output::default();
        self.row += 1;
        if self.segment.as_ref().map(|s| s.id) != Some(f.segment.id) || self.bins != f.db.len() {
            // A retune, a gap or a new axis: everything open ends here.
            let axis_changed =
                self.segment.as_ref().is_none_or(|s| s.center_hz != f.segment.center_hz || s.sample_rate != f.segment.sample_rate)
                    || self.bins != f.db.len();
            out.closed.extend(self.close_all(ids));
            self.bridges.clear();
            self.segment = Some(f.segment.clone());
            if axis_changed {
                self.bins = f.db.len();
                self.block_floor.clear();
            }
        }
        self.update_floor(f.db);
        let segs = self.segments(f.db);
        self.associate(segs, &f, &mut out, ids);

        let (hang, min_rows) = (self.cfg.hangover_rows, self.cfg.min_rows);
        let mut i = 0;
        while i < self.tracks.len() {
            if self.row.saturating_sub(self.tracks[i].last_row) > hang {
                let mut t = self.tracks.swap_remove(i);
                self.bridges.retain(|k, _| k.0 != t.key && k.1 != t.key);
                if t.rows >= min_rows {
                    out.closed.push(self.to_record(&mut t, false, ids));
                }
            } else {
                i += 1;
            }
        }
        out
    }

    /// The tracks still open that are long enough to report, giving the new ones their ids.
    pub fn open_records(&mut self, ids: &mut dyn FnMut() -> RecordId) -> Vec<Detected> {
        let min_rows = self.cfg.min_rows;
        let mut tracks = std::mem::take(&mut self.tracks);
        let out = tracks.iter_mut().filter(|t| t.rows >= min_rows).map(|t| self.to_record(t, true, ids)).collect();
        self.tracks = tracks;
        out
    }

    /// Closes every open track (the source went away, say).
    pub fn close_all(&mut self, ids: &mut dyn FnMut() -> RecordId) -> Vec<Detected> {
        let mut tracks = std::mem::take(&mut self.tracks);
        let min_rows = self.cfg.min_rows;
        tracks.iter_mut().filter(|t| t.rows >= min_rows).map(|t| self.to_record(t, false, ids)).collect()
    }

    /// Current noise-floor estimate per detector bin (dBFS).
    pub fn floor(&self) -> &[f32] {
        &self.floor
    }

    fn bin_hz(&self) -> f64 {
        self.segment.as_ref().map_or(0.0, |s| s.sample_rate / self.bins.max(1) as f64)
    }

    fn freq(&self, bin: f64) -> f64 {
        let s = self.segment.as_ref().expect("a segment");
        s.center_hz - s.sample_rate / 2.0 + bin / self.bins as f64 * s.sample_rate
    }

    fn to_record(&self, t: &mut Track, open: bool, ids: &mut dyn FnMut() -> RecordId) -> Detected {
        let id = *t.record.get_or_insert_with(&mut *ids);
        let (a, b) = t.core();
        let rows = t.rows.max(1) as f32;
        Detected {
            id,
            segment: self.segment.clone().expect("a segment"),
            first_sample: t.first_sample,
            end_sample: t.end_sample,
            f_lo_hz: self.freq(a as f64),
            f_hi_hz: self.freq(b as f64 + 1.0),
            bin_hz: self.bin_hz(),
            peak_dbfs: t.peak_db,
            mean_dbfs: t.level_sum / rows,
            mean_snr_db: t.snr_sum / rows,
            max_snr_db: t.snr_max,
            rows: t.rows,
            open,
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
        // Unmeasured bins (a segmented sweep's skipped hops) don't count, nor do the bins outside the usable fraction:
        // the tuner's IF filter falls off a cliff there (an Airspy at 10 MS/s: 14 dB down within 200 kHz past
        // ±4.45 MHz), which drags a block's percentile below the real floor and makes the last 100 kHz inside the edge
        // a "signal" at every tuning. A block with no measured bins borrows its nearest measured neighbour's.
        let margin = self.margin(n);
        let mut unmeasured: Vec<usize> = Vec::new();
        for k in 0..nb {
            let (lo, hi) = (k * b, ((k + 1) * b).min(n));
            let s = &db[lo..hi];
            self.scratch.clear();
            self.scratch
                .extend(s.iter().enumerate().filter(|(i, v)| (margin..n - margin).contains(&(lo + i)) && v.is_finite()).map(|(_, v)| *v));
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
                let near = (0..nb).filter(|j| !unmeasured.contains(j)).min_by_key(|j| j.abs_diff(k)).expect("a measured block");
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

    /// The activity over `lo..=hi` of the current row.
    fn activity(&self, db: &[f32], lo: usize, hi: usize) -> Activity {
        let (mut peak, mut snr, mut lin, mut n) = (f32::MIN, f32::MIN, 0.0f64, 0u32);
        for (&v, &fl) in db[lo..=hi].iter().zip(&self.floor[lo..=hi]) {
            if v.is_finite() {
                peak = peak.max(v);
                snr = snr.max(v - fl);
                lin += 10f64.powf(v as f64 / 10.0);
                n += 1;
            }
        }
        let level = if n > 0 { (10.0 * (lin / n as f64).log10()) as f32 } else { f32::MIN };
        Activity { peak, snr, level }
    }

    fn segments(&self, db: &[f32]) -> Vec<Seg> {
        let thr = self.cfg.thr_db;
        let active = |i: usize| db[i].is_finite() && db[i] - self.floor[i] > thr;
        let mut runs: Vec<(usize, usize, f32)> = Vec::new();
        let n = db.len();
        let margin = self.margin(n);
        let mut i = margin;
        let end = n - margin;
        while i < end {
            if !active(i) {
                i += 1;
                continue;
            }
            let lo = i;
            let mut snr = f32::MIN;
            while i < end && active(i) {
                snr = snr.max(db[i] - self.floor[i]);
                i += 1;
            }
            let hi = i - 1;
            match runs.last_mut() {
                Some(prev) if lo - prev.1 - 1 <= self.cfg.merge_gap_bins => {
                    prev.1 = hi;
                    prev.2 = prev.2.max(snr);
                }
                _ => runs.push((lo, hi, snr)),
            }
        }
        runs.retain(|r| r.2 >= self.cfg.min_peak_db);
        runs.into_iter()
            .map(|(mut lo, mut hi, _)| {
                let peak = (lo..=hi).map(|i| db[i]).fold(f32::MIN, f32::max);
                let cut = peak - self.cfg.obw_db;
                while lo < hi && db[lo] < cut {
                    lo += 1;
                }
                while hi > lo && db[hi] < cut {
                    hi -= 1;
                }
                Seg { lo, hi, act: self.activity(db, lo, hi) }
            })
            .collect()
    }

    fn associate(&mut self, segs: Vec<Seg>, f: &Frame<'_>, out: &mut Output, ids: &mut dyn FnMut() -> RecordId) {
        let slack = self.cfg.match_slack_bins;
        let est = self.cfg.established_rows;
        let bin_hz = self.bin_hz();
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
                let key = self.next_key;
                self.next_key += 1;
                self.tracks.push(Track {
                    key,
                    record: None,
                    lo: s.lo,
                    hi: s.hi,
                    first_row: self.row,
                    last_row: self.row,
                    first_sample: f.first_sample,
                    end_sample: f.end_sample,
                    peak_db: s.act.peak,
                    snr_sum: s.act.snr,
                    snr_max: s.act.snr,
                    level_sum: s.act.level,
                    rows: 1,
                    hits: vec![1; s.hi - s.lo + 1],
                });
                continue;
            }
            // Pull out every overlapping track (descending indices keep swap_remove safe).
            let mut sorted = hits.clone();
            sorted.sort_unstable();
            let mut taken: Vec<Track> = sorted.iter().rev().map(|&j| self.tracks.swap_remove(j)).collect();
            // One segment touching several tracks makes them one emission only when they are (an FSK signal's two
            // tones sit closer than any two channels: FLEX and POCSAG ±4.5–4.8 kHz, channels ≥ 12.5 kHz apart) or when
            // it keeps happening (a wide signal's fragments). A stray bridging row (adjacent channels of a trunked site
            // joined by a smeared row) is split between them, halfway between their typical extents.
            taken.sort_by(|a, b| a.centre().total_cmp(&b.centre()));
            let mut k = 0;
            while k + 1 < taken.len() {
                let (a, b) = (&taken[k], &taken[k + 1]);
                let key = (a.key.min(b.key), a.key.max(b.key));
                let n = {
                    let c = self.bridges.entry(key).or_insert(0);
                    *c += 1;
                    *c
                };
                let close = (b.centre() - a.centre()) * bin_hz < FSK_PAIR_HZ;
                let frequent = n >= 3 && n * 4 >= a.rows.min(b.rows);
                // Pieces of one wide signal (a broadcast FM station's swing breaks it up) touch for their size; two
                // channels of a raster are spaced well beyond their width.
                let ((a0, a1), (b0, b1)) = (a.core(), b.core());
                let gap = b0.saturating_sub(a1 + 1).max(a0.saturating_sub(b1 + 1));
                let touching = 2 * gap <= (a1 - a0 + 1).min(b1 - b0 + 1);
                if close || frequent || touching {
                    let o = taken.remove(k + 1);
                    self.bridges.retain(|q, _| q.0 != o.key && q.1 != o.key);
                    // The older track lives on (with its record id, when it has one).
                    let (mut keep, other) = if o.first_row < taken[k].first_row { (o, taken.remove(k)) } else { (taken.remove(k), o) };
                    if let Some(gone) = other.record {
                        let into = *keep.record.get_or_insert_with(&mut *ids);
                        out.merged.push((gone, into));
                    }
                    merge_into(&mut keep, &other);
                    taken.insert(k, keep);
                } else {
                    k += 1;
                }
            }
            if taken.len() == 1 {
                let mut t = taken.remove(0);
                t.touch(s.lo, s.hi, s.act, self.row, f.end_sample);
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
                let act = self.activity(f.db, a, b);
                t.touch(a, b, act, self.row, f.end_sample);
            }
            self.tracks.extend(taken);
        }
    }
}

/// A bin belongs to a track's extent once it was active in at least 1/`CORE_SHARE` of the rows its busiest bin was.
const CORE_SHARE: u32 = 10;

/// Established tracks whose typical extents are centred closer than this are one emission (an FSK signal's two tones),
/// never two channels.
const FSK_PAIR_HZ: f64 = 11e3;

/// Merges track `o` into `t` (spec §8: the merge used to inflate the mean SNR).
fn merge_into(t: &mut Track, o: &Track) {
    t.absorb(o);
    t.peak_db = t.peak_db.max(o.peak_db);
    // The tracks overlap in time, so `rows` keeps the larger count (not the sum) and the sums are rescaled to it: each
    // merged mean is the row-weighted mean of the two means, never above either track's maximum.
    let total = (t.rows + o.rows).max(1) as f32;
    let (snr, level) = ((t.snr_sum + o.snr_sum) / total, (t.level_sum + o.level_sum) / total);
    t.rows = t.rows.max(o.rows);
    t.snr_sum = snr * t.rows as f32;
    t.level_sum = level * t.rows as f32;
    t.snr_max = t.snr_max.max(o.snr_max);
    t.first_row = t.first_row.min(o.first_row);
    t.last_row = t.last_row.max(o.last_row);
    t.first_sample = t.first_sample.min(o.first_sample);
    t.end_sample = t.end_sample.max(o.end_sample);
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{Boundary, SegmentId};

    /// 4096 bins over 1 MHz at 100 MHz, one row every 20 000 samples.
    fn seg(id: u64, center_hz: f64) -> Segment {
        Segment { id: SegmentId(id), from_sample: 0, center_hz, sample_rate: 1e6, boundary: Boundary::Start }
    }

    struct Feed {
        d: Detector,
        seg: Segment,
        next: u64,
        closed: Vec<Detected>,
        merged: Vec<(RecordId, RecordId)>,
    }

    impl Feed {
        fn new(cfg: DetectorConfig) -> Feed {
            Feed { d: Detector::new(cfg), seg: seg(1, 100e6), next: 1, closed: vec![], merged: vec![] }
        }
        fn live() -> Feed {
            Feed::new(DetectorConfig { reduce: 1, ..DetectorConfig::live(50.0) })
        }
        fn row(&mut self, seq: u64, db: Vec<f32>) {
            let next = &mut self.next;
            let mut ids = || {
                *next += 1;
                RecordId(*next - 1)
            };
            let f = Frame { segment: &self.seg, first_sample: seq * 20_000, end_sample: seq * 20_000 + 30_000, db: &db };
            let out = self.d.process_db(f, &mut ids);
            self.closed.extend(out.closed);
            self.merged.extend(out.merged);
        }
        fn open(&mut self) -> Vec<Detected> {
            let next = &mut self.next;
            self.d.open_records(&mut || {
                *next += 1;
                RecordId(*next - 1)
            })
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

    const BIN: f64 = 1e6 / 4096.0;

    #[test]
    fn burst_becomes_one_record_with_right_extent() {
        let mut f = Feed::live();
        for seq in 1..=100u64 {
            let mut db = noise(4096, seq);
            if (30..60).contains(&seq) {
                for v in &mut db[1000..1020] {
                    *v = -70.0;
                }
            }
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let r = &f.closed[0];
        assert_eq!((r.first_sample, r.end_sample), (30 * 20_000, 59 * 20_000 + 30_000));
        assert!((r.f_lo_hz - (99.5e6 + 1000.0 * BIN)).abs() < BIN * 1.5);
        assert!((r.f_hi_hz - (99.5e6 + 1020.0 * BIN)).abs() < BIN * 1.5);
        assert!(r.mean_snr_db > 25.0);
        assert!((r.mean_dbfs - -70.0).abs() < 0.5, "mean {}", r.mean_dbfs);
        assert_eq!(r.peak_dbfs, -70.0);
        assert_eq!(r.id, RecordId(1), "the only reported track gets the first id");
    }

    #[test]
    fn unmeasured_stretch_makes_no_records_and_keeps_the_floor_honest() {
        // A segmented sweep row: bins 1000..3000 were never measured.
        let mut f = Feed::new(DetectorConfig::sweep());
        for seq in 1..=40u64 {
            let mut db = noise(4096, seq);
            for v in &mut db[1000..3000] {
                *v = f32::NAN;
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
            f.row(seq, db);
            // The floor over measured bins stays near the noise, even next to the gap.
            assert!(f.d.floor().iter().all(|x| (-103.0..-97.0).contains(x)), "floor off: {:?}", &f.d.floor()[980..1010]);
        }
        assert_eq!(f.closed.len(), 2, "{:?}", f.closed);
        for r in &f.closed {
            assert_eq!(r.first_sample, 10 * 20_000);
            assert!(r.mean_snr_db > 20.0 && r.mean_snr_db < 30.0, "snr {}", r.mean_snr_db);
        }
    }

    #[test]
    fn strong_signal_skirts_are_trimmed() {
        let mut f = Feed::live();
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
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1);
        let bins = f.closed[0].bandwidth_hz() / BIN;
        assert!(bins < 40.0, "bandwidth {bins} bins");
    }

    /// A 26 dB pedestal around strong signals (what a second radio on the same USB controller does on air).
    fn smeared(db: &mut [f32], lo: usize, hi: usize) {
        for v in &mut db[lo..hi] {
            *v = v.max(-80.0);
        }
    }

    #[test]
    fn occasional_smeared_rows_dont_widen_a_record() {
        let mut f = Feed::live();
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
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let bins = f.closed[0].bandwidth_hz() / BIN;
        assert!((18.0..24.0).contains(&bins), "bandwidth {bins} bins");
    }

    #[test]
    fn a_smeared_row_doesnt_fuse_established_neighbours() {
        let mut f = Feed::live();
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
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 2, "{:?}", f.closed);
        for r in &f.closed {
            let bins = r.bandwidth_hz() / BIN;
            assert!((16.0..22.0).contains(&bins), "bandwidth {bins} bins: {r:?}");
        }
    }

    #[test]
    fn fsk_tones_stay_one_record() {
        // An idle pager's two tones 9.6 kHz (39 bins) apart; the gap between them fills on some rows.
        let mut f = Feed::live();
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
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let bins = f.closed[0].bandwidth_hz() / BIN;
        assert!((44.0..52.0).contains(&bins), "bandwidth {bins} bins");
    }

    #[test]
    fn a_swinging_fm_signal_keeps_its_full_width() {
        // A broadcast FM signal's instantaneous spectrum is ~60 bins wide but swings over 200 bins with the programme.
        let mut f = Feed::live();
        for seq in 1..=230u64 {
            let mut db = noise(4096, seq);
            if (10..190).contains(&seq) {
                let off = ((seq as f64 * 0.7).sin() * 70.0) as i64;
                let c = (2100 + off) as usize;
                for v in &mut db[c - 30..c + 30] {
                    *v = -60.0;
                }
            }
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let bins = f.closed[0].bandwidth_hz() / BIN;
        assert!(bins > 170.0, "bandwidth {bins} bins");
    }

    #[test]
    fn pieces_of_a_wide_signal_become_one_record() {
        let mut f = Feed::live();
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
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let bins = f.closed[0].bandwidth_hz() / BIN;
        assert!(bins > 150.0, "bandwidth {bins} bins");
        for (gone, into) in &f.merged {
            assert_ne!(gone, into);
            assert!(!f.closed.iter().any(|r| r.id == *gone), "a merged-away record closed as well");
        }
    }

    #[test]
    fn noise_alone_makes_no_records() {
        let mut f = Feed::live();
        for seq in 1..=500u64 {
            f.row(seq, noise(4096, seq));
        }
        assert_eq!(f.closed.len() + f.open().len(), 0);
        assert_eq!(f.next, 1, "noise used up record ids");
    }

    #[test]
    fn slotted_bursts_within_hangover_stay_one_record() {
        let mut f = Feed::live();
        for seq in 1..=120u64 {
            let mut db = noise(4096, seq);
            if (20..80).contains(&seq) && seq % 2 == 0 {
                for v in &mut db[2000..2016] {
                    *v = -75.0;
                }
            }
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1);
    }

    #[test]
    fn a_new_segment_closes_every_open_record_at_the_boundary() {
        // Review focus 1: a retune (or a gap, which is a new segment too) mid-emission.
        let mut f = Feed::live();
        for seq in 1..=80u64 {
            if seq == 40 {
                f.seg = Segment { id: SegmentId(2), from_sample: 40 * 20_000, ..seg(2, 100e6) };
            }
            let mut db = noise(4096, seq);
            for v in &mut db[2000..2016] {
                *v = -60.0;
            }
            f.row(seq, db);
        }
        assert_eq!(f.closed.len(), 1, "{:?}", f.closed);
        let before = f.closed[0].clone();
        assert_eq!(before.segment.id, SegmentId(1));
        assert_eq!(before.end_sample, 39 * 20_000 + 30_000, "closed at the old segment's last row");
        let after = f.open();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].segment.id, SegmentId(2));
        assert_eq!(after[0].first_sample, 40 * 20_000);
        assert_ne!(after[0].id, before.id);
    }

    #[test]
    fn merging_never_inflates_the_mean_snr() {
        // Spec §8: two overlapping tracks, ~30 dB and ~10 dB above the floor for the same rows, merged by a row that
        // bridges them often: the mean is between the two, rows not summed.
        let (a, b) = (
            Track { snr_sum: 300.0, level_sum: -700.0, rows: 10, ..track() },
            Track { snr_sum: 100.0, level_sum: -900.0, rows: 10, ..track() },
        );
        let mut t = a.clone();
        merge_into(&mut t, &b);
        assert_eq!(t.rows, 10);
        assert!((t.snr_sum / t.rows as f32 - 20.0).abs() < 1e-4, "mean {}", t.snr_sum / t.rows as f32);
        assert!((t.level_sum / t.rows as f32 - -80.0).abs() < 1e-4);
    }

    fn track() -> Track {
        Track {
            key: 1,
            record: None,
            lo: 10,
            hi: 20,
            first_row: 1,
            last_row: 10,
            first_sample: 0,
            end_sample: 1,
            peak_db: -50.0,
            snr_sum: 0.0,
            snr_max: 30.0,
            level_sum: 0.0,
            rows: 1,
            hits: vec![1; 11],
        }
    }

    #[test]
    fn a_stage_row_is_reduced_four_to_one_in_power() {
        // A 65 536-bin row at 10 MS/s: a tone of −40 dBFS in one bin lands at −40 dBFS in its detector bin, and two
        // −43 dBFS tones in the same group of four sum to −40.
        let mut power = vec![1e-10f32; 65_536];
        power[40_001] = 1e-4;
        power[50_000] = 0.5e-4;
        power[50_003] = 0.5e-4;
        let segment = Segment { id: SegmentId(1), from_sample: 0, center_hz: 855.25e6, sample_rate: 10e6, boundary: Boundary::Start };
        let row = Row { first_sample: 0, end_sample: 8 * 32_768, blocks: 7, segment, power };
        let mut d = Detector::new(DetectorConfig::live(DetectorConfig::rows_per_sec(&row)));
        let _ = d.process(&row, &mut || RecordId(1));
        assert_eq!(d.db.len(), 16_384);
        assert!((d.db[10_000] - -40.0).abs() < 0.01, "{}", d.db[10_000]);
        assert!((d.db[12_500] - -40.0).abs() < 0.01, "{}", d.db[12_500]);
        assert!((d.db[0] - -94.0).abs() < 0.1, "noise bins sum four: {}", d.db[0]);
        assert!((DetectorConfig::rows_per_sec(&row) - 10e6 / (7.0 * 32_768.0)).abs() < 1e-6);
    }
}
