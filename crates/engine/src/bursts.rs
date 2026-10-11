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
        }
    }

    /// Feeds one row. Returns records that closed on this row.
    pub fn process(&mut self, row: &SpectrumRow) -> Vec<SignalRecord> {
        let mut closed = Vec::new();
        if self.tuning != Some(row.tuning) || self.bins != row.db.len() {
            // New frequency axis: everything open ends here.
            closed.extend(self.close_all());
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
            f_lo_hz: self.bin_hz(t.lo as f64),
            f_hi_hz: self.bin_hz(t.hi as f64 + 1.0),
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

    fn update_floor(&mut self, db: &[f32]) {
        let n = db.len();
        let b = self.cfg.block_bins.min(n).max(1);
        let nb = n.div_ceil(b);
        let fresh = self.block_floor.len() != nb;
        if fresh {
            self.block_floor = vec![0.0; nb];
        }
        // Unmeasured bins (a segmented sweep's skipped hops) don't count; a
        // block with none measured borrows its nearest measured neighbour's.
        let mut unmeasured: Vec<usize> = Vec::new();
        for k in 0..nb {
            let s = &db[k * b..((k + 1) * b).min(n)];
            self.scratch.clear();
            self.scratch.extend(s.iter().copied().filter(|v| !is_empty_db(*v)));
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
        let margin = ((1.0 - self.cfg.usable_fraction.clamp(0.0, 1.0)) / 2.0 * n as f32) as usize;
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
        for s in segs {
            let lo = s.lo.saturating_sub(slack);
            let hi = s.hi + slack;
            let hits: Vec<usize> = self.tracks.iter().enumerate().filter(|(_, t)| t.lo <= hi && t.hi >= lo).map(|(i, _)| i).collect();
            match hits.first() {
                None => self.tracks.push(Track {
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
                }),
                Some(_) => {
                    // Pull out every overlapping track (descending indices keep
                    // swap_remove safe) and merge them into the oldest.
                    let mut taken: Vec<Track> = hits.iter().rev().map(|&j| self.tracks.swap_remove(j)).collect();
                    taken.sort_by_key(|t| t.first_seq);
                    let mut t = taken.remove(0);
                    for o in taken {
                        t.lo = t.lo.min(o.lo);
                        t.hi = t.hi.max(o.hi);
                        t.peak_db = t.peak_db.max(o.peak_db);
                        t.snr_sum += o.snr_sum;
                        t.snr_max = t.snr_max.max(o.snr_max);
                        // Rows overlap in time; keep the larger count rather than double-count.
                        t.rows = t.rows.max(o.rows);
                        t.last_seq = t.last_seq.max(o.last_seq);
                        t.first_end = t.first_end.min(o.first_end);
                        t.last_end = t.last_end.max(o.last_end);
                    }
                    t.lo = t.lo.min(s.lo);
                    t.hi = t.hi.max(s.hi);
                    if t.last_seq != row.seq {
                        t.rows += 1;
                        t.snr_sum += s.snr;
                    }
                    t.last_seq = row.seq;
                    t.last_end = row.end_sample;
                    t.t_end = row.time;
                    t.peak_db = t.peak_db.max(s.peak_db);
                    t.snr_max = t.snr_max.max(s.snr);
                    self.tracks.push(t);
                }
            }
        }
    }
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
