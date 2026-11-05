//! The waterfall's history and its ring texture's geometry: pure, no egui beyond its plain types.

use std::collections::VecDeque;

use api::Row;
use egui::{Color32, Rect, pos2};

/// History depth and texture height.
pub const ROWS: usize = 2048;
/// History columns.
pub const HIST_W: usize = 4096;
/// The narrowest zoom, in history columns.
pub const MIN_ZOOM_COLS: f64 = 24.0;
/// Rows or columns with no data.
pub const GAP: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);

/// One row reduced to `HIST_W` columns of dB (NaN = no data).
#[derive(Debug, Clone)]
pub struct HistRow {
    pub db: Vec<f32>,
    pub first_sample: u64,
    pub end_sample: u64,
}

/// What `History::push` did with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// Not newer than the newest row held.
    Skipped,
    Appended,
    /// Appended after an all-NaN row standing for the samples between.
    Hole,
    /// The tuning changed: the history was emptied and holds this row alone.
    Cleared,
}

/// The newest `ROWS` rows of one tuning, newest first.
#[derive(Debug, Clone)]
pub struct History {
    pub center_hz: f64,
    pub sample_rate: f64,
    rows: VecDeque<HistRow>,
}

impl Default for History {
    fn default() -> Self {
        History::new()
    }
}

impl History {
    pub fn new() -> History {
        History { center_hz: 0.0, sample_rate: 0.0, rows: VecDeque::with_capacity(ROWS) }
    }

    /// Pushes `row` if newer than the newest; a change of centre or rate clears everything first and returns `Cleared`;
    /// a hole before `row` (first_sample > newest.end_sample) pushes one all-NaN row first and returns `Hole`.
    pub fn push(&mut self, row: &Row) -> Pushed {
        let (center, rate) = (row.segment.center_hz, row.segment.sample_rate);
        let mut result = Pushed::Appended;
        if let Some(newest) = self.rows.front() {
            // Before the tuning test: a snapshot still carrying rows of the old tuning must not clear again.
            if row.first_sample <= newest.first_sample {
                return Pushed::Skipped;
            }
            if center != self.center_hz || rate != self.sample_rate {
                self.rows.clear();
                result = Pushed::Cleared;
            } else if row.first_sample > newest.end_sample {
                let gap = HistRow { db: vec![f32::NAN; HIST_W], first_sample: newest.end_sample, end_sample: row.first_sample };
                self.rows.push_front(gap);
                result = Pushed::Hole;
            }
        }
        self.center_hz = center;
        self.sample_rate = rate;
        let mut db = vec![0.0; HIST_W];
        reduce_db(&row.power, &mut db);
        self.rows.push_front(HistRow { db, first_sample: row.first_sample, end_sample: row.end_sample });
        self.rows.truncate(ROWS);
        result
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn newest(&self) -> Option<&HistRow> {
        self.rows.front()
    }

    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &HistRow> {
        self.rows.iter()
    }

    /// The history rows (0 = newest) holding samples `first .. end`, newest row first: `(top, bottom)`, clipped to the
    /// rows held; `None` when no held row overlaps them.
    pub fn rows_of(&self, first: u64, end: u64) -> Option<(usize, usize)> {
        let (newest, oldest) = (self.rows.front()?, self.rows.back()?);
        if end <= oldest.first_sample || first >= newest.end_sample || end <= first {
            return None;
        }
        // Rows are newest first, so `first_sample` falls along the deque: the row holding sample `s` is the first whose
        // start is at or before it.
        let row_of = |s: u64| self.rows.partition_point(|r| r.first_sample > s).min(self.rows.len() - 1);
        Some((row_of(end - 1), row_of(first)))
    }

    /// The frequencies the rows cover: centre ± rate/2. `None` before the first row.
    pub fn span(&self) -> Option<(f64, f64)> {
        (!self.rows.is_empty()).then(|| (self.center_hz - self.sample_rate / 2.0, self.center_hz + self.sample_rate / 2.0))
    }

    /// Seconds from one row's start to the next: the newest row's `first_sample` less the previous real row's
    /// (hole rows skipped), over the rate. Engine rows overlap by one hop, so `end − first` would overstate it;
    /// that is only the fallback for a single row. 0.05 when empty.
    pub fn secs_per_row(&self) -> f64 {
        let Some(newest) = self.rows.front().filter(|_| self.sample_rate > 0.0) else { return 0.05 };
        let is_hole = |r: &HistRow| r.db.iter().all(|v| v.is_nan());
        let samples = match self.rows.iter().skip(1).find(|r| !is_hole(r)) {
            Some(prev) => newest.first_sample - prev.first_sample,
            None => newest.end_sample - newest.first_sample,
        };
        samples as f64 / self.sample_rate
    }
}

/// The inputs `[a, b)` of output column `i` of `n` over `len > 0` inputs: never empty, so it also stretches.
pub fn column(i: usize, n: usize, len: usize) -> std::ops::Range<usize> {
    let a = (i * len / n).min(len - 1);
    let b = ((i + 1) * len / n).clamp(a + 1, len);
    a..b
}

/// Max per output column (NaN only where every input is NaN), shrinking or stretching `src` to `out.len()`.
pub fn resample_max(src: &[f32], out: &mut [f32]) {
    let (len, n) = (src.len(), out.len());
    if len == 0 {
        out.fill(f32::NAN);
        return;
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = src[column(i, n, len)].iter().fold(f32::NAN, |m, &v| m.max(v));
    }
}

/// Max per column in the linear domain, then 10·log10(max(p, 1e-30)).
pub fn reduce_db(power: &[f32], out: &mut [f32]) {
    resample_max(power, out);
    for v in out.iter_mut() {
        if !v.is_nan() {
            *v = 10.0 * v.max(1e-30).log10();
        }
    }
}

/// What the waterfall and trace show: the colour range and the zoom window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub db_min: f32,
    pub db_max: f32,
    /// `(lo, hi)` in Hz; `None` shows the whole span.
    pub zoom: Option<(f64, f64)>,
}

impl View {
    /// The zoom window moved inside `span` (keeping its width), or the span itself.
    pub fn window(&self, span: (f64, f64)) -> (f64, f64) {
        match self.zoom {
            Some((lo, hi)) if hi - lo > 0.0 && hi - lo < span.1 - span.0 => {
                let w = hi - lo;
                let lo = lo.clamp(span.0, span.1 - w);
                (lo, lo + w)
            }
            _ => span,
        }
    }

    /// History columns `[c0, c1)` of the window, out of `n` across the span: c0 rounded down, c1 up, at least one.
    pub fn cols(&self, span: (f64, f64), n: usize) -> (usize, usize) {
        let (lo, hi) = self.window(span);
        let sw = span.1 - span.0;
        if sw.is_nan() || sw <= 0.0 || n == 0 {
            return (0, n.max(1));
        }
        let at = |f: f64| (f - span.0) / sw * n as f64;
        let c0 = (at(lo).floor().max(0.0) as usize).min(n - 1);
        let c1 = (at(hi).ceil().max(0.0) as usize).clamp(c0 + 1, n);
        (c0, c1)
    }

    /// Narrows the window by `factor` (widens below 1) centred on `hz`: never narrower than `MIN_ZOOM_COLS` history
    /// columns, never outside `span`; as wide as the span or wider is no zoom.
    pub fn zoom_about(&mut self, span: (f64, f64), hz: f64, factor: f64) {
        if factor.is_nan() || factor <= 0.0 {
            return;
        }
        let (lo, hi) = self.window(span);
        let sw = span.1 - span.0;
        let w = (hi - lo) / factor;
        if w >= sw {
            self.zoom = None;
            return;
        }
        let w = w.max(sw * MIN_ZOOM_COLS / HIST_W as f64);
        let lo = (hz - w / 2.0).clamp(span.0, span.1 - w);
        self.zoom = Some((lo, lo + w));
    }

    /// Moves a zoomed window by `dhz`, stopping at the span's edges.
    pub fn pan(&mut self, span: (f64, f64), dhz: f64) {
        if self.zoom.is_none() {
            return;
        }
        let (lo, hi) = self.window(span);
        let w = hi - lo;
        let lo = (lo + dhz).clamp(span.0, span.1 - w);
        self.zoom = Some((lo, lo + w));
    }

    pub fn hz_to_x(&self, span: (f64, f64), rect: Rect, hz: f64) -> f32 {
        let (lo, hi) = self.window(span);
        rect.left() + ((hz - lo) / (hi - lo)) as f32 * rect.width()
    }

    pub fn x_to_hz(&self, span: (f64, f64), rect: Rect, x: f32) -> f64 {
        let (lo, hi) = self.window(span);
        lo + ((x - rect.left()) / rect.width()) as f64 * (hi - lo)
    }
}

/// The waterfall's colour map.
pub struct Lut(pub [Color32; 256]);

/// One colour per dB value: NaN → `GAP`; clamped to the view's range.
pub fn colorize(db: &[f32], view: &View, lut: &Lut, out: &mut Vec<Color32>) {
    let k = 255.0 / (view.db_max - view.db_min).max(1e-3);
    out.clear();
    out.extend(db.iter().map(|&v| if v.is_nan() { GAP } else { lut.0[((v - view.db_min) * k).clamp(0.0, 255.0) as usize] }));
}

/// The texture row the next row goes to: one up the ring.
pub fn next_write_row(write_row: usize) -> usize {
    (write_row + ROWS - 1) % ROWS
}

/// The two textured quads that draw a ring texture whose newest row is `write_row`, newest at the top:
/// (uv rect, fraction of the screen rect top..bottom) each; the second is absent when write_row == 0.
pub fn quads(write_row: usize) -> [Option<(Rect, (f32, f32))>; 2] {
    let row = write_row % ROWS;
    let w = row as f32 / ROWS as f32;
    let top = (Rect::from_min_max(pos2(0.0, w), pos2(1.0, 1.0)), (0.0, 1.0 - w));
    let rest = (row != 0).then(|| (Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, w)), (1.0 - w, 1.0)));
    [Some(top), rest]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use api::{Boundary, Segment, SegmentId};
    use egui::{pos2, vec2};

    fn row(first: u64, end: u64, center: f64, hot_bin: usize) -> Row {
        let mut power = vec![1e-10f32; 65536];
        power[hot_bin] = 1.0;
        let segment = Segment { id: SegmentId(0), from_sample: 0, center_hz: center, sample_rate: 10e6, boundary: Boundary::Start };
        Row { first_sample: first, end_sample: end, blocks: 1, segment, power }
    }

    #[test]
    fn a_single_hot_bin_survives_reduction() {
        let mut out = vec![0.0; HIST_W];
        let mut p = vec![1e-10f32; 65536];
        p[12_345] = 1.0;
        reduce_db(&p, &mut out);
        let hot = 12_345 * HIST_W / 65536;
        assert!((out[hot] - 0.0).abs() < 1e-3, "{}", out[hot]);
        assert!(out.iter().enumerate().filter(|(i, _)| *i != hot).all(|(_, &v)| (v + 100.0).abs() < 1e-3));
    }

    #[test]
    fn history_appends_contiguous_rows_marks_holes_and_clears_on_retune() {
        let mut h = History::new();
        assert_eq!(h.push(&row(0, 32_768, 855.25e6, 0)), Pushed::Appended);
        assert_eq!(h.push(&row(32_768, 65_536, 855.25e6, 0)), Pushed::Appended);
        assert_eq!(h.push(&row(32_768, 65_536, 855.25e6, 0)), Pushed::Skipped); // seen already
        assert_eq!(h.push(&row(98_304, 131_072, 855.25e6, 0)), Pushed::Hole);
        assert_eq!(h.len(), 4);
        assert!(h.iter().nth(1).unwrap().db.iter().all(|v| v.is_nan()));
        assert_eq!(h.push(&row(131_072, 163_840, 851.25e6, 0)), Pushed::Cleared);
        assert_eq!((h.len(), h.center_hz), (1, 851.25e6));
        for k in 0..(ROWS as u64 + 10) {
            h.push(&row(200_000 + k * 32_768, 200_000 + (k + 1) * 32_768, 851.25e6, 0));
        }
        assert_eq!(h.len(), ROWS);
    }

    #[test]
    fn feeding_every_third_snapshot_loses_no_rows() {
        // Many rows per snapshot, 1 new row per snapshot: skipping two of every three snapshots still gives a contiguous history.
        let rows: Vec<Row> = (0..60u64).map(|k| row(k * 32_768, (k + 1) * 32_768, 855.25e6, 0)).collect();
        let mut h = History::new();
        for v in (0..60usize).step_by(3) {
            let newest = &rows[v.saturating_sub(15)..=v];
            for r in newest {
                h.push(r);
            }
        }
        assert_eq!(h.len(), 58); // rows 0..=57 (the last snapshot taken is v = 57)
        assert!(h.iter().all(|r| !r.db[0].is_nan()));
    }

    #[test]
    fn a_row_lasts_from_one_row_start_to_the_next() {
        let mut h = History::new();
        assert_eq!(h.secs_per_row(), 0.05);
        h.push(&row(0, 32_768, 855.25e6, 0));
        assert!((h.secs_per_row() - 0.0032768).abs() < 1e-12);
        h.push(&row(32_768, 65_536, 855.25e6, 0));
        assert!((h.secs_per_row() - 0.0032768).abs() < 1e-12);
        // Engine rows overlap by one hop: the period is start to start, not first to end.
        let mut h = History::new();
        h.push(&row(0, 65_536, 855.25e6, 0));
        h.push(&row(32_768, 98_304, 855.25e6, 0));
        assert!((h.secs_per_row() - 0.0032768).abs() < 1e-12, "{}", h.secs_per_row());
        // A hole row is skipped when picking the previous row.
        h.push(&row(200_000, 232_768, 855.25e6, 0));
        assert!(h.iter().nth(1).unwrap().db[0].is_nan());
        assert!((h.secs_per_row() - (200_000.0 - 32_768.0) / 10e6).abs() < 1e-12);
    }

    #[test]
    fn zoom_stays_inside_the_span_and_above_the_minimum() {
        let span = (850.25e6, 860.25e6);
        let mut v = View { db_min: -110.0, db_max: -45.0, zoom: None };
        v.zoom_about(span, 855.0e6, 2.0);
        assert_eq!(v.window(span), (852.5e6, 857.5e6));
        for _ in 0..40 {
            v.zoom_about(span, 855.0e6, 2.0);
        }
        let (lo, hi) = v.window(span);
        assert!((hi - lo - 10e6 * MIN_ZOOM_COLS / HIST_W as f64).abs() < 1.0);
        v.pan(span, -100e6);
        assert_eq!(v.window(span).0, span.0);
        v.zoom_about(span, 855.0e6, 0.001);
        assert_eq!(v.zoom, None);
    }

    #[test]
    fn hz_and_x_round_trip_and_columns_cover_the_window() {
        let span = (850.25e6, 860.25e6);
        let v = View { db_min: -110.0, db_max: -45.0, zoom: Some((852.25e6, 853.25e6)) };
        let rect = Rect::from_min_size(pos2(10.0, 0.0), vec2(1000.0, 100.0));
        assert!((v.x_to_hz(span, rect, v.hz_to_x(span, rect, 852.75e6)) - 852.75e6).abs() < 1.0);
        assert_eq!(v.cols(span, HIST_W), (819, 1229));
        let full = View { zoom: None, ..v };
        assert_eq!(full.cols(span, HIST_W), (0, HIST_W));
    }

    #[test]
    fn colorize_clamps_and_marks_gaps() {
        let lut = Lut(theme::waterfall_lut());
        let v = View { db_min: -100.0, db_max: -50.0, zoom: None };
        let mut out = Vec::new();
        colorize(&[-200.0, -100.0, -75.0, -50.0, 0.0, f32::NAN], &v, &lut, &mut out);
        assert_eq!((out[0], out[1], out[3], out[4], out[5]), (lut.0[0], lut.0[0], lut.0[255], lut.0[255], GAP));
        assert_eq!(out[2], lut.0[127]);
    }

    #[test]
    fn a_records_samples_map_to_the_rows_holding_them() {
        let mut h = History::new();
        // Rows of 10 samples each, starting at 0, 10, ..., 90 (overlapping by nothing here).
        for k in 0..10u64 {
            h.push(&row(k * 10, k * 10 + 10, 100e6, 5));
        }
        assert_eq!(h.rows_of(95, 100), Some((0, 0)), "the newest row");
        assert_eq!(h.rows_of(0, 10), Some((9, 9)), "the oldest row");
        assert_eq!(h.rows_of(25, 61), Some((3, 7)));
        assert_eq!(h.rows_of(0, 1000), Some((0, 9)), "clipped to what is held");
        assert_eq!(h.rows_of(100, 120), None, "newer than every row");
        assert_eq!(History::new().rows_of(0, 10), None);
    }

    #[test]
    fn the_ring_quads_cover_the_texture_once() {
        assert_eq!(next_write_row(0), ROWS - 1);
        assert_eq!(next_write_row(5), 4);
        let [a, b] = quads(0);
        assert!(b.is_none());
        assert_eq!(a.unwrap().1, (0.0, 1.0));
        let [a, b] = quads(ROWS / 4);
        let (ua, fa) = a.unwrap();
        let (ub, fb) = b.unwrap();
        assert_eq!((fa, fb), ((0.0, 0.75), (0.75, 1.0))); // rows ROWS/4..ROWS on top, then rows 0..ROWS/4
        assert_eq!((ua.min.y, ua.max.y, ub.min.y, ub.max.y), (0.25, 1.0, 0.0, 0.25));
    }
}
