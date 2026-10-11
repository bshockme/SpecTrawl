//! Spectrum trace + waterfall for one source.
//!
//! The waterfall is a GPU texture used as a ring: each new row is colourised on
//! the CPU and uploaded with a one-row partial texture update, and drawing is
//! two textured quads split at the ring's write position. Rows are kept in dB
//! so the texture can be recoloured when the range changes or the view resizes.

use std::collections::VecDeque;

use egui::{Color32, Mesh, Pos2, Rect, Shape, Stroke, TextureHandle, TextureOptions, pos2, vec2};
use engine::{SignalRecord, Source, SweepStatus, Tuning};

use crate::theme;

/// Waterfall depth in rows (at 50 rows/s ≈ 20 s on screen).
const ROWS: usize = 1024;
/// Colour of stretches the survey doesn't measure (outside its segments).
const GAP: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);
/// Resolution rows are kept at for recolouring; display width never exceeds it.
const HIST_W: usize = 4096;
/// Survey rows are kept near their native bin count (a full-band sweep is
/// 32768 bins) so zooming in is a view operation: 1024 rows × 16384 × 4 B.
const HIST_W_NATIVE: usize = 16384;
/// Narrowest zoom, in history columns.
const MIN_ZOOM_COLS: f64 = 24.0;

pub struct SpectrumView {
    pub db_min: f32,
    pub db_max: f32,
    pub smoothing: f32,
    /// Waterfall row height in points; `None` fits the whole history.
    pub row_px: Option<f32>,
    /// Draw unselected record boxes faintly and without labels (dense views).
    pub subtle_boxes: bool,
    /// While paused, the mouse wheel scrolls back through history.
    pub paused: bool,
    /// Channels being continuously decoded: (centre Hz, short label).
    pub markers: Vec<(f64, String)>,
    /// Known wideband occupants (FM, TV, LTE): (lo Hz, hi Hz, label).
    pub known: Vec<(f64, f64, String)>,
    /// Spectrum squelch: dim known occupants and hide their records.
    pub squelch: bool,
    /// Records of emitters first seen within this long are outlined as new.
    pub new_within: std::time::Duration,
    /// Keep history at near-native resolution (survey views).
    hist_w: usize,
    /// Clock-driven rows: a new history row every period, filled from the
    /// sweep's row in progress (hop by hop), instead of one row per pass.
    pub row_period: Option<std::time::Duration>,
    last_timed: Option<std::time::Instant>,
    /// The frequency window shown, when zoomed in (Hz); None = the whole span.
    zoom: Option<(f64, f64)>,
    /// The zoom the texture was built for.
    tex_zoom: Option<(f64, f64)>,
    /// Rows scrolled back from the newest (0 = live edge).
    scroll: usize,
    /// Wall time of each history row, newest first (parallel to `history`).
    row_times: VecDeque<std::time::SystemTime>,
    trace: Vec<f32>,
    hold: Vec<f32>,
    tuning: Option<Tuning>,
    last_seq: Option<u64>,
    history: VecDeque<Vec<f32>>,
    tex: Option<TextureHandle>,
    tex_w: usize,
    write_row: usize,
    tex_range: (f32, f32),
    lut: [Color32; 256],
    scratch: Vec<f32>,
}

impl SpectrumView {
    pub fn new() -> Self {
        Self {
            db_min: -110.0,
            db_max: -45.0,
            smoothing: 0.6,
            row_px: None,
            subtle_boxes: false,
            paused: false,
            markers: Vec::new(),
            known: Vec::new(),
            squelch: false,
            new_within: std::time::Duration::from_secs(120),
            hist_w: HIST_W,
            row_period: None,
            last_timed: None,
            zoom: None,
            tex_zoom: None,
            scroll: 0,
            row_times: VecDeque::with_capacity(ROWS),
            trace: Vec::new(),
            hold: Vec::new(),
            tuning: None,
            last_seq: None,
            history: VecDeque::with_capacity(ROWS),
            tex: None,
            tex_w: 0,
            write_row: 0,
            tex_range: (0.0, 0.0),
            lut: theme::waterfall_lut(),
            scratch: Vec::new(),
        }
    }

    /// Keeps history at near-native resolution, for views whose rows are
    /// wider than any screen (the survey).
    pub fn with_native_history(mut self) -> Self {
        self.hist_w = HIST_W_NATIVE;
        self
    }

    /// The tuned extent, ignoring any zoom.
    fn full_span(&self) -> Option<(f64, f64)> {
        self.tuning.map(|t| (t.center_hz - t.span_hz / 2.0, t.center_hz + t.span_hz / 2.0))
    }

    /// Zoom factor relative to the full span (1 = not zoomed).
    pub fn zoom_factor(&self) -> f64 {
        match (self.zoom, self.full_span()) {
            (Some((a, b)), Some((f0, f1))) if b > a => (f1 - f0) / (b - a),
            _ => 1.0,
        }
    }

    pub fn reset_zoom(&mut self) {
        self.zoom = None;
    }

    /// Zooms by `factor` (> 1 in) keeping `at_hz` under the cursor.
    fn zoom_by(&mut self, factor: f64, at_hz: f64) {
        let Some((f0, f1)) = self.full_span() else { return };
        let (a, b) = self.zoom.unwrap_or((f0, f1));
        let cols = self.history.front().map(|r| r.len()).unwrap_or(self.trace.len()).max(1) as f64;
        let min_w = (f1 - f0) * (MIN_ZOOM_COLS / cols).min(1.0);
        let w = ((b - a) / factor).clamp(min_w, f1 - f0);
        let t = ((at_hz - a) / (b - a)).clamp(0.0, 1.0);
        let mut na = at_hz - t * w;
        na = na.clamp(f0, f1 - w);
        self.zoom = if w >= (f1 - f0) * 0.999 { None } else { Some((na, na + w)) };
    }

    /// Pans the zoom window by `dhz`.
    fn pan_by(&mut self, dhz: f64) {
        let (Some((f0, f1)), Some((a, b))) = (self.full_span(), self.zoom) else { return };
        let w = b - a;
        let na = (a + dhz).clamp(f0, f1 - w);
        self.zoom = Some((na, na + w));
    }

    /// History/trace index range covering the zoom window of a row `len` wide.
    fn zoom_cols(&self, len: usize) -> (usize, usize) {
        match (self.zoom, self.full_span()) {
            (Some((a, b)), Some((f0, f1))) if f1 > f0 && len > 0 => {
                let c0 = (((a - f0) / (f1 - f0)) * len as f64).floor().clamp(0.0, (len - 1) as f64) as usize;
                let c1 = (((b - f0) / (f1 - f0)) * len as f64).ceil().clamp((c0 + 1) as f64, len as f64) as usize;
                (c0, c1)
            }
            _ => (0, len),
        }
    }

    /// Puts the noise floor near the bottom of the colour map.
    pub fn auto_range(&mut self) {
        if self.trace.is_empty() {
            return;
        }
        // Unmeasured stretches (survey segments) aren't spectrum.
        let mut v: Vec<f32> = self.trace.iter().copied().filter(|v| !engine::is_empty_db(*v)).collect();
        if v.is_empty() {
            return;
        }
        v.sort_by(f32::total_cmp);
        let floor = v[v.len() / 5];
        self.db_min = (floor - 6.0).round();
        self.db_max = self.db_min + 65.0;
    }

    /// Starts (or restarts) the traces on a row with a new tuning or size.
    fn retune_to(&mut self, row: &engine::SpectrumRow) {
        let first = self.tuning.is_none();
        self.tuning = Some(row.tuning);
        if let (Some((a, b)), Some((f0, f1))) = (self.zoom, self.full_span()) {
            if a < f0 || b > f1 {
                self.zoom = None;
            }
        }
        self.trace = row.db.clone();
        self.hold = row.db.clone();
        if first {
            self.auto_range();
        }
    }

    /// Pulls new spectrum rows from the source.
    pub fn ingest(&mut self, src: &Source) {
        if let Some(period) = self.row_period {
            self.ingest_timed(src, period);
            return;
        }
        let rows = src.rows_after(self.last_seq);
        let skip = rows.len().saturating_sub(ROWS);
        for row in rows.iter().skip(skip) {
            self.last_seq = Some(row.seq);
            if self.tuning != Some(row.tuning) || self.trace.len() != row.db.len() {
                // Retune or FFT size change: restart the traces.
                let first = self.tuning.is_none();
                self.tuning = Some(row.tuning);
                // A zoom window that fell off the new span goes.
                if let (Some((a, b)), Some((f0, f1))) = (self.zoom, self.full_span()) {
                    if a < f0 || b > f1 {
                        self.zoom = None;
                    }
                }
                self.trace = row.db.clone();
                self.hold = row.db.clone();
                if first {
                    self.auto_range();
                }
            }
            let a = self.smoothing;
            for ((t, h), &v) in self.trace.iter_mut().zip(self.hold.iter_mut()).zip(&row.db) {
                *t = a * *t + (1.0 - a) * v;
                *h = (*h - 0.05).max(v);
            }
            let mut dec = vec![0.0; row.db.len().min(self.hist_w)];
            dsp::decimate_max(&row.db, &mut dec);
            if self.history.len() == ROWS {
                self.history.pop_back();
                self.row_times.pop_back();
            }
            self.history.push_front(dec);
            self.row_times.push_front(row.time);
            self.push_tex_row();
        }
    }

    /// Clock-driven rows: the newest row is the sweep's row in progress and
    /// is redrawn as hops arrive; every `period` it is frozen and a new one
    /// starts. A slow pass repeats cells rather than stretching a row.
    fn ingest_timed(&mut self, src: &Source, period: std::time::Duration) {
        let rows = src.rows_after(self.last_seq);
        if let Some(r) = rows.last() {
            self.last_seq = Some(r.seq);
        }
        let Some(cur) = src.sweep_partial().or_else(|| rows.last().cloned()) else { return };
        if self.tuning != Some(cur.tuning) || self.trace.len() != cur.db.len() {
            self.retune_to(&cur);
            self.history.clear();
            self.row_times.clear();
            self.last_timed = None;
        }
        let a = self.smoothing;
        for ((t, h), &v) in self.trace.iter_mut().zip(self.hold.iter_mut()).zip(&cur.db) {
            *t = a * *t + (1.0 - a) * v;
            *h = (*h - 0.05).max(v);
        }
        let mut dec = vec![0.0; cur.db.len().min(self.hist_w)];
        dsp::decimate_max(&cur.db, &mut dec);
        let now = std::time::Instant::now();
        let due = self.last_timed.is_none_or(|t| now.duration_since(t) >= period) || self.history.is_empty();
        if due {
            if self.history.len() == ROWS {
                self.history.pop_back();
                self.row_times.pop_back();
            }
            self.history.push_front(dec);
            self.row_times.push_front(cur.time);
            self.last_timed = Some(now);
            self.push_tex_row();
        } else {
            if let Some(front) = self.history.front_mut() {
                *front = dec;
            }
            if let Some(t) = self.row_times.front_mut() {
                *t = cur.time;
            }
            self.refresh_tex_row();
        }
    }

    /// Re-uploads the newest history row into its texture row (the row in
    /// progress changed) without advancing the ring.
    fn refresh_tex_row(&mut self) {
        let Some(len) = self.history.front().map(|r| r.len()) else { return };
        let (c0, c1) = self.zoom_cols(len);
        let (Some(tex), Some(row)) = (self.tex.as_mut(), self.history.front()) else { return };
        if self.tex_w == 0 {
            return;
        }
        let mut cols = vec![0.0; self.tex_w];
        dsp::decimate_max(&row[c0..c1], &mut cols);
        let (lo, k, lut) = (self.db_min, 255.0 / (self.db_max - self.db_min).max(1.0), &self.lut);
        let px: Vec<Color32> = cols.iter().map(|&v| if engine::is_empty_db(v) { GAP } else { lut[((v - lo) * k).clamp(0.0, 255.0) as usize] }).collect();
        let img = egui::ColorImage::new([self.tex_w, 1], px);
        tex.set_partial([0, self.write_row], img, TextureOptions::LINEAR);
    }

    fn colorize(&self, db: &[f32], out: &mut Vec<Color32>) {
        let lo = self.db_min;
        let k = 255.0 / (self.db_max - self.db_min).max(1.0);
        out.clear();
        out.extend(db.iter().map(|&v| if engine::is_empty_db(v) { GAP } else { self.lut[((v - lo) * k).clamp(0.0, 255.0) as usize] }));
    }

    fn push_tex_row(&mut self) {
        let Some(len) = self.history.front().map(|r| r.len()) else { return };
        let (c0, c1) = self.zoom_cols(len);
        let (Some(tex), Some(row)) = (self.tex.as_mut(), self.history.front()) else { return };
        if self.tex_w == 0 {
            return;
        }
        let mut cols = vec![0.0; self.tex_w];
        dsp::decimate_max(&row[c0..c1], &mut cols);
        let mut px = Vec::with_capacity(self.tex_w);
        // Borrow juggling: colourise with a copy of the parameters.
        let (lo, k, lut) = (self.db_min, 255.0 / (self.db_max - self.db_min).max(1.0), &self.lut);
        px.extend(cols.iter().map(|&v| if engine::is_empty_db(v) { GAP } else { lut[((v - lo) * k).clamp(0.0, 255.0) as usize] }));
        self.write_row = (self.write_row + ROWS - 1) % ROWS;
        let img = egui::ColorImage::new([self.tex_w, 1], px);
        tex.set_partial([0, self.write_row], img, TextureOptions::LINEAR);
    }

    /// Rebuilds the whole texture from history (on resize or range change).
    fn rebuild_tex(&mut self, ctx: &egui::Context, width: usize) {
        self.tex_w = width;
        self.tex_range = (self.db_min, self.db_max);
        self.tex_zoom = self.zoom;
        self.write_row = 0;
        let mut px = vec![theme::PLOT_BG; width * ROWS];
        let mut cols = vec![0.0; width];
        let mut line = Vec::with_capacity(width);
        for (r, row) in self.history.iter().enumerate() {
            let (c0, c1) = self.zoom_cols(row.len());
            dsp::decimate_max(&row[c0..c1], &mut cols);
            self.colorize(&cols, &mut line);
            px[r * width..(r + 1) * width].copy_from_slice(&line);
        }
        let img = egui::ColorImage::new([width, ROWS], px);
        match &mut self.tex {
            Some(t) if t.size() == [width, ROWS] => t.set(img, TextureOptions::LINEAR),
            _ => self.tex = Some(ctx.load_texture("waterfall", img, TextureOptions::LINEAR)),
        }
    }

    /// Frequency extent currently displayed, in Hz (the zoom window when
    /// zoomed in).
    pub fn span(&self) -> Option<(f64, f64)> {
        self.zoom.or_else(|| self.full_span())
    }

    /// Draws a labelled bracket over `[lo, hi]` Hz (e.g. a focus pane's coverage).
    pub fn bracket(&self, ui: &egui::Ui, rect: Rect, lo: f64, hi: f64, label: &str) {
        let Some((f0, f1)) = self.span() else { return };
        let body = Rect::from_min_max(pos2(rect.left(), rect.top() + HEADER_H), rect.max);
        let x = |f: f64| body.left() + ((f - f0) / (f1 - f0)) as f32 * body.width();
        let (x0, x1) = (x(lo) - 2.0, x(hi) + 2.0);
        if x1 < body.left() || x0 > body.right() {
            return;
        }
        let r = Rect::from_x_y_ranges(x0.max(body.left())..=x1.min(body.right()), body.y_range());
        let p = ui.painter_at(rect);
        p.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(230, 237, 243, 14));
        let edge = Stroke::new(1.0, Color32::from_rgba_unmultiplied(230, 237, 243, 170));
        p.vline(r.left(), r.y_range(), edge);
        p.vline(r.right(), r.y_range(), edge);
        let g = p.layout_no_wrap(label.to_string(), mono(11.0), theme::BG);
        let at = pos2(r.left() + 4.0, r.top() + 4.0);
        p.rect_filled(Rect::from_min_size(at - vec2(4.0, 1.0), g.size() + vec2(8.0, 2.0)), 3.0, theme::CHIP_FOCUS_BG);
        p.galley(at, g, theme::BG);
    }

    /// Newest row ingested so far.
    pub fn newest_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Oldest row sequence number that can be on screen.
    pub fn oldest_seq(&self) -> u64 {
        self.last_seq.unwrap_or(0).saturating_sub(ROWS as u64)
    }

    /// Draws header, spectrum, waterfall and record boxes into `rect`.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        title: &str,
        sweep: Option<&SweepStatus>,
        records: &[SignalRecord],
        selected: Option<u64>,
        selected_emitter: Option<u64>,
    ) -> ShowResult {
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_H));
        {
            let p = ui.painter_at(rect);
            p.rect_filled(header, 0.0, theme::PANEL);
            p.text(header.left_center() + vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, title, egui::FontId::proportional(12.5), theme::TEXT);
            p.hline(rect.x_range(), header.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
        }
        let mut full = Rect::from_min_max(pos2(rect.left(), header.bottom()), rect.max);
        if let Some(s) = sweep {
            // Hop freshness strip: bright = just measured, fading with age.
            let strip = Rect::from_min_size(full.min, vec2(full.width(), 5.0));
            let p = ui.painter_at(strip);
            p.rect_filled(strip, 0.0, theme::PLOT_BG);
            let now = std::time::Instant::now();
            let pass = s.last_pass_s.max(0.5);
            for (h, t) in s.hop_times.iter().enumerate() {
                let Some(t) = t else { continue };
                if !s.measured.get(h).copied().unwrap_or(true) {
                    continue;
                }
                let age = now.duration_since(*t).as_secs_f64() / pass;
                let a = if h == s.current_hop { 255 } else { (200.0 * (1.0 - age.min(1.0) * 0.8)) as u8 };
                let (z0, z1) = self.span().unwrap_or((s.start_hz, s.stop_hz()));
                let (h0, h1) = (s.start_hz + s.hop_hz * h as f64, s.start_hz + s.hop_hz * (h + 1) as f64);
                if h1 < z0 || h0 > z1 {
                    continue;
                }
                let x0 = strip.left() + strip.width() * ((h0 - z0) / (z1 - z0)).clamp(0.0, 1.0) as f32;
                let x1 = strip.left() + strip.width() * ((h1 - z0) / (z1 - z0)).clamp(0.0, 1.0) as f32;
                p.rect_filled(
                    Rect::from_x_y_ranges(x0..=(x1 - 1.0).max(x0 + 0.5), strip.y_range()),
                    0.0,
                    Color32::from_rgba_unmultiplied(0x38, 0xd3, 0xcf, a),
                );
            }
            full.min.y += 6.0;
        }
        let resp = ui.allocate_rect(full, egui::Sense::click_and_drag());
        // Zoom and pan: Ctrl + wheel zooms about the cursor, a right or
        // middle drag pans, a right double-click resets.
        if resp.hovered() {
            let (ctrl, dy, pos) = ui.input(|i| (i.modifiers.command, i.smooth_scroll_delta.y, i.pointer.hover_pos()));
            if ctrl && dy != 0.0 {
                if let (Some(p), Some((f0, f1))) = (pos, self.span()) {
                    let at = f0 + ((p.x - full.left()) / full.width()).clamp(0.0, 1.0) as f64 * (f1 - f0);
                    self.zoom_by((1.0 + dy.abs() as f64 / 120.0).powf(if dy > 0.0 { 1.0 } else { -1.0 }), at);
                }
            }
        }
        if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
            if let Some((f0, f1)) = self.span() {
                let d = resp.drag_delta().x as f64 / full.width() as f64 * (f1 - f0);
                self.pan_by(-d);
            }
        }
        if resp.double_clicked_by(egui::PointerButton::Secondary) {
            self.reset_zoom();
        }
        let ctrl_held = ui.input(|i| i.modifiers.command);
        if !self.paused {
            self.scroll = 0;
        } else if resp.hovered() && !ctrl_held {
            // Wheel up = further back in time, ~10 rows per notch.
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                let step = (dy.abs() / 5.0).ceil() as usize;
                let max = self.history.len().saturating_sub(1);
                self.scroll = if dy > 0.0 { (self.scroll + step).min(max) } else { self.scroll.saturating_sub(step) };
            }
        }
        let painter = ui.painter_at(full);
        let spec_h = (full.height() * 0.32).round();
        let spec = Rect::from_min_size(full.min, vec2(full.width(), spec_h));
        let wf = Rect::from_min_max(pos2(full.left(), spec.bottom() + 1.0), full.max);

        let Some((f0, f1)) = self.span() else {
            painter.rect_filled(full, 0.0, theme::PLOT_BG);
            let msg = if sweep.is_some() { "first sweep pass in progress…" } else { "waiting for samples…" };
            painter.text(full.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(14.0), theme::MUTED);
            return ShowResult::default();
        };
        let visible = match self.row_px {
            Some(px) => ((wf.height() / px).floor() as usize).clamp(1, ROWS),
            None => ROWS,
        };

        // --- waterfall ---
        let ppp = ui.ctx().pixels_per_point();
        let want_w = ((wf.width() * ppp).round() as usize).clamp(64, HIST_W);
        if self.tex.is_none() || want_w != self.tex_w || self.tex_range != (self.db_min, self.db_max) || self.tex_zoom != self.zoom {
            self.rebuild_tex(ui.ctx(), want_w);
        }
        if let Some(tex) = &self.tex {
            // Newest row at the top. Show `visible` rows starting at the ring's
            // write position, wrapping once past the end of the texture.
            let start = (self.write_row + self.scroll) % ROWS;
            let first = visible.min(ROWS - start);
            let v = |r: usize| r as f32 / ROWS as f32;
            let mut mesh = Mesh::with_texture(tex.id());
            let r1 = Rect::from_min_size(wf.min, vec2(wf.width(), wf.height() * first as f32 / visible as f32));
            mesh.add_rect_with_uv(r1, Rect::from_min_max(pos2(0.0, v(start)), pos2(1.0, v(start + first))), Color32::WHITE);
            if first < visible {
                let r2 = Rect::from_min_max(pos2(wf.left(), r1.bottom()), wf.max);
                mesh.add_rect_with_uv(r2, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, v(visible - first))), Color32::WHITE);
            }
            painter.add(Shape::mesh(mesh));
        }
        let x_known = |f: f64| full.left() + ((f - f0) / (f1 - f0)) as f32 * full.width();
        if self.squelch {
            for (lo, hi, _) in &self.known {
                if *hi < f0 || *lo > f1 {
                    continue;
                }
                let r = Rect::from_x_y_ranges(x_known(*lo).max(wf.left())..=x_known(*hi).min(wf.right()), wf.y_range());
                painter.rect_filled(r, 0.0, Color32::from_black_alpha(175));
            }
        }

        // --- spectrum ---
        painter.rect_filled(spec, 0.0, theme::PLOT_BG);
        let pad_b = 16.0;
        let plot = Rect::from_min_max(spec.min + vec2(0.0, 4.0), spec.max - vec2(0.0, pad_b));
        let y_of = |db: f32| {
            let t = ((db - self.db_min) / (self.db_max - self.db_min)).clamp(0.0, 1.0);
            plot.bottom() - t * plot.height()
        };
        for i in 0..=4 {
            let y = plot.top() + plot.height() * i as f32 / 4.0;
            painter.hline(plot.x_range(), y, Stroke::new(1.0, theme::GRID));
            let db = self.db_max - (self.db_max - self.db_min) * i as f32 / 4.0;
            painter.text(pos2(plot.left() + 4.0, y + 1.0), egui::Align2::LEFT_TOP, format!("{db:.0}"), mono(10.0), theme::MUTED);
        }
        let span = f1 - f0;
        let step = nice_step(span, (spec.width() / 110.0) as f64);
        let dec = decimals(step / 1e6);
        let mut f = (f0 / step).ceil() * step;
        while f <= f1 {
            let x = spec.left() + ((f - f0) / span) as f32 * spec.width();
            painter.vline(x, plot.y_range(), Stroke::new(1.0, theme::GRID));
            if x > spec.left() + 30.0 && x < spec.right() - 30.0 {
                painter.text(
                    pos2(x, spec.bottom() - 3.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!("{:.*}", dec, f / 1e6),
                    mono(10.0),
                    theme::MUTED,
                );
            }
            f += step;
        }

        let cols = (spec.width() as usize).max(2);
        self.scratch.resize(cols, 0.0);
        let x_of = |i: usize| spec.left() + i as f32 * spec.width() / (cols - 1) as f32;
        // Peak hold and trace, over the zoom window's bins.
        let (t0, t1) = self.zoom_cols(self.trace.len());
        dsp::decimate_max(&self.hold[t0..t1], &mut self.scratch);
        let hold_pts: Vec<Pos2> = self.scratch.iter().enumerate().map(|(i, &v)| pos2(x_of(i), y_of(v))).collect();
        // Trace with fill.
        dsp::decimate_max(&self.trace[t0..t1], &mut self.scratch);
        let pts: Vec<Pos2> = self.scratch.iter().enumerate().map(|(i, &v)| pos2(x_of(i), y_of(v))).collect();
        let mut fill = Mesh::default();
        let fill_c = Color32::from_rgba_unmultiplied(0x38, 0xd3, 0xcf, 16);
        for (i, p) in pts.iter().enumerate() {
            fill.colored_vertex(*p, fill_c);
            fill.colored_vertex(pos2(p.x, plot.bottom()), fill_c);
            if i > 0 {
                let b = (i as u32) * 2;
                fill.add_triangle(b - 2, b - 1, b);
                fill.add_triangle(b - 1, b + 1, b);
            }
        }
        painter.add(Shape::mesh(fill));
        // Lines break over unmeasured stretches, which get a hatch-free
        // grey band instead of a trace.
        let runs = |pts: &[Pos2], vals: &[f32]| -> Vec<Vec<Pos2>> {
            let mut out: Vec<Vec<Pos2>> = Vec::new();
            let mut cur: Vec<Pos2> = Vec::new();
            for (p, v) in pts.iter().zip(vals) {
                if engine::is_empty_db(*v) {
                    if cur.len() > 1 {
                        out.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                } else {
                    cur.push(*p);
                }
            }
            if cur.len() > 1 {
                out.push(cur);
            }
            out
        };
        let mut gap_from: Option<f32> = None;
        for (i, v) in self.scratch.iter().enumerate() {
            let empty = engine::is_empty_db(*v);
            match (gap_from, empty) {
                (None, true) => gap_from = Some(x_of(i)),
                (Some(x0), false) => {
                    painter.rect_filled(Rect::from_x_y_ranges(x0..=x_of(i), plot.y_range()), 0.0, GAP);
                    gap_from = None;
                }
                _ => {}
            }
        }
        if let Some(x0) = gap_from {
            painter.rect_filled(Rect::from_x_y_ranges(x0..=plot.right(), plot.y_range()), 0.0, GAP);
        }
        let hold_vals: Vec<f32> = {
            let mut h = vec![0.0; cols];
            dsp::decimate_max(&self.hold[t0..t1], &mut h);
            h
        };
        for run in runs(&hold_pts, &hold_vals) {
            painter.add(Shape::line(run, Stroke::new(1.0, Color32::from_rgba_unmultiplied(242, 163, 58, 110))));
        }
        for run in runs(&pts, &self.scratch) {
            painter.add(Shape::line(run, Stroke::new(1.3, theme::ACCENT)));
        }
        // Known occupants: a bar along the bottom of the plot, labelled
        // where there's room.
        let mut known_chips: Vec<Rect> = Vec::new();
        for (lo, hi, label) in &self.known {
            if *hi < f0 || *lo > f1 {
                continue;
            }
            let (x0, x1) = (x_known(*lo).max(plot.left()), x_known(*hi).min(plot.right()));
            if self.squelch {
                painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, plot.y_range()), 0.0, Color32::from_black_alpha(110));
            }
            painter.rect_filled(Rect::from_x_y_ranges(x0..=x1.max(x0 + 1.0), plot.bottom() - 3.0..=plot.bottom()), 0.0, theme::KNOWN);
            let g = painter.layout_no_wrap(label.clone(), mono(9.5), theme::BG);
            if g.size().x + 6.0 > (x1 - x0).max(0.0) + 60.0 {
                continue;
            }
            let at = pos2(
                ((x0 + x1) / 2.0 - g.size().x / 2.0).clamp(plot.left() + 2.0, plot.right() - g.size().x - 2.0),
                plot.bottom() - 4.0 - g.size().y,
            );
            let chip = Rect::from_min_size(at - vec2(3.0, 0.0), g.size() + vec2(6.0, 0.0));
            if known_chips.iter().any(|c| c.intersects(chip)) {
                continue;
            }
            painter.rect_filled(chip, 2.0, theme::KNOWN);
            painter.galley(at, g, theme::BG);
            known_chips.push(chip);
        }
        painter.hline(full.x_range(), spec.bottom() + 0.5, Stroke::new(1.0, theme::BORDER));

        // --- record boxes ---
        let newest = self.last_seq.unwrap_or(0).saturating_sub(self.scroll as u64);
        if self.scroll > 0 {
            // Show how far back the top row is.
            if let Some(t) = self.row_times.get(self.scroll) {
                let ago = std::time::SystemTime::now().duration_since(*t).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let g = painter.layout_no_wrap(format!("◀ {ago:.1} s ago · wheel to scroll"), mono(11.0), theme::BG);
                let at = pos2(wf.right() - g.size().x - 14.0, wf.top() + 6.0);
                painter.rect_filled(Rect::from_min_size(at - vec2(6.0, 2.0), g.size() + vec2(12.0, 4.0)), 3.0, theme::SELECT);
                painter.galley(at, g, theme::BG);
            }
        }
        let row_h = wf.height() / visible as f32;
        let x_hz = |f: f64| full.left() + ((f - f0) / span) as f32 * full.width();
        let mut boxes: Vec<(Rect, &SignalRecord)> = Vec::new();
        let squelched = |r: &SignalRecord| self.squelch && self.known.iter().any(|(lo, hi, _)| (*lo..=*hi).contains(&r.center_hz()));
        // Rows back from the top row for a record's end and start: by row
        // sequence normally, by time when rows run on a clock.
        let timed = self.row_period.is_some();
        let top_time = self.row_times.get(self.scroll).copied();
        let rows_back = |r: &SignalRecord| -> Option<(f32, f32)> {
            if timed {
                let top = top_time?;
                if r.t_start > top {
                    return None;
                }
                let idx = |t: std::time::SystemTime| self.row_times.partition_point(|rt| *rt > t).saturating_sub(self.scroll) as f32;
                Some((idx(r.t_end), idx(r.t_start) + 1.0))
            } else {
                if r.first_seq > newest {
                    return None;
                }
                Some((newest.saturating_sub(r.last_seq) as f32, newest.saturating_sub(r.first_seq) as f32 + 1.0))
            }
        };
        for r in records {
            if r.f_hi_hz < f0 || r.f_lo_hz > f1 || squelched(r) {
                continue;
            }
            let Some((back_end, back_start)) = rows_back(r) else { continue };
            if back_end >= visible as f32 {
                continue;
            }
            let (mut x0, mut x1) = (x_hz(r.f_lo_hz), x_hz(r.f_hi_hz));
            if x1 - x0 < 6.0 {
                let c = (x0 + x1) / 2.0;
                (x0, x1) = (c - 3.0, c + 3.0);
            }
            let y0 = wf.top() + back_end * row_h;
            let y1 = (wf.top() + back_start * row_h).min(wf.bottom());
            boxes.push((Rect::from_min_max(pos2(x0 - 1.0, y0), pos2(x1 + 1.0, y1.max(y0 + 2.0))), r));
        }
        let now_sys = std::time::SystemTime::now();
        let hover = resp.hover_pos();
        let hovered = hover.and_then(|p| {
            boxes.iter().filter(|(b, _)| b.expand(2.0).contains(p)).min_by(|a, b| a.0.area().total_cmp(&b.0.area())).map(|(_, r)| r.id)
        });
        // Detections are soft tints over the signal rather than outlines: the
        // waterfall stays readable and the protocol colour still reads. Only
        // the hovered/selected ones get an outline.
        let focused = |r: &SignalRecord| {
            selected == Some(r.id) || hovered == Some(r.id) || (selected_emitter.is_some() && r.emitter == selected_emitter)
        };
        for (b, r) in &boxes {
            let class = r.display_class();
            let identified = class.is_some_and(|c| c.protocol.is_some());
            let base = theme::class_color(class).unwrap_or(theme::RECORD);
            let alpha = match (focused(r), self.subtle_boxes, identified) {
                (true, _, _) => 0.40,
                (false, true, _) => 0.10,
                (false, false, true) => 0.28,
                (false, false, false) => 0.13,
            };
            painter.rect_filled(*b, 1.5, base.gamma_multiply(alpha));
            if focused(r) {
                painter.rect_stroke(*b, 1.5, Stroke::new(1.0, theme::SELECT), egui::StrokeKind::Outside);
            } else if let Some(age) = r.emitter_first_seen.and_then(|t| now_sys.duration_since(t).ok()).filter(|a| *a <= self.new_within) {
                // New emitter: an outline that fades over the window.
                let k = 1.0 - age.as_secs_f32() / self.new_within.as_secs_f32().max(1.0);
                painter.rect_stroke(*b, 1.5, Stroke::new(1.0, theme::NEW.gamma_multiply(0.35 + 0.65 * k)), egui::StrokeKind::Outside);
            }
        }
        // Labels on top: one per emitter (its newest visible burst), only for
        // identified protocols, plus whatever is hovered/selected. Labels that
        // would overlap an earlier one are skipped.
        let mut order: Vec<&(Rect, &SignalRecord)> = boxes.iter().collect();
        // Deterministic order: records arrive in a different order each frame
        // (open tracks get reshuffled), and active bursts all touch the top
        // row, so without a full tie-break the overlap test picked a different
        // winner each frame and labels flickered.
        order.sort_by(|a, b| {
            focused(b.1)
                .cmp(&focused(a.1))
                .then(a.0.top().total_cmp(&b.0.top()))
                .then(a.0.left().total_cmp(&b.0.left()))
                .then(a.1.id.cmp(&b.1.id))
        });
        let mut labelled = std::collections::HashSet::new();
        let mut placed: Vec<Rect> = Vec::new();
        for (b, r) in order {
            let class = r.display_class();
            let identified = class.is_some_and(|c| c.protocol.is_some());
            let focus = focused(r);
            let key = r.emitter.unwrap_or(r.id);
            if !focus && (self.subtle_boxes || !identified || b.height() < 8.0 || labelled.contains(&key)) {
                continue;
            }
            let text = match class {
                Some(c) => {
                    let mut t = c.label.clone();
                    if let Some(f) = &c.fields {
                        if let Some(id) = &f.id {
                            t += &format!(" · {id}");
                        }
                        if let Some(a) = f.activity {
                            t += &format!(" · {}", a.name());
                        }
                        if let Some(talk) = &f.talk {
                            t += &format!(" · {talk}");
                        }
                    }
                    t
                }
                None => format!("#{}", r.id),
            };
            let g = painter.layout_no_wrap(text, mono(10.0), theme::BG);
            let at = pos2(b.left(), (b.top() - g.size().y - 1.0).max(wf.top()));
            let chip = Rect::from_min_size(at, g.size() + vec2(6.0, 0.0));
            if !focus && placed.iter().any(|p| p.intersects(chip)) {
                continue;
            }
            let bg = if focus { theme::SELECT } else { theme::class_color(class).unwrap_or(theme::RECORD) };
            painter.rect_filled(chip, 2.0, bg);
            painter.galley(at + vec2(3.0, 0.0), g, theme::BG);
            labelled.insert(key);
            placed.push(chip);
        }

        // --- monitor markers: a slim line through the pane and a tag on the spectrum ---
        let mut tags: Vec<Rect> = Vec::new();
        let mut markers = self.markers.clone();
        markers.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (f, label) in &markers {
            if *f < f0 || *f > f1 {
                continue;
            }
            let x = x_hz(*f);
            painter.vline(x, full.y_range(), Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.55)));
            let g = painter.layout_no_wrap(label.clone(), mono(10.0), theme::BG);
            // Stack tags downwards when neighbours would overlap.
            let mut at = pos2((x + 4.0).min(full.right() - g.size().x - 6.0), spec.top() + 4.0);
            let mut tag = Rect::from_min_size(at - vec2(3.0, 1.0), g.size() + vec2(6.0, 2.0));
            while tags.iter().any(|t| t.intersects(tag)) {
                at.y += g.size().y + 4.0;
                tag = tag.translate(vec2(0.0, g.size().y + 4.0));
            }
            painter.rect_filled(tag, 2.0, theme::ACCENT);
            painter.galley(at, g, theme::BG);
            tags.push(tag);
        }

        // --- hover readout ---
        if let Some(r) = hovered.and_then(|id| records.iter().find(|r| r.id == id)) {
            let p = hover.unwrap_or_default();
            let mut text = format!(
                "#{}{}\n{:.4} MHz · {:.1} kHz\n{:.2} s · SNR {:.0}/{:.0} dB\npeak {:.1} dBFS",
                r.id,
                if r.active { "  ACTIVE" } else { "" },
                r.center_hz() / 1e6,
                r.bandwidth_hz() / 1e3,
                r.duration_s(),
                r.mean_snr_db,
                r.max_snr_db,
                r.peak_dbfs
            );
            if let Some(e) = r.emitter {
                text += &format!("\nemitter E{e}");
            }
            match r.display_class() {
                Some(c) => {
                    let f = &c.features;
                    text += &format!("\n\n{}  ({:.0}%)", c.label, c.confidence * 100.0);
                    if let Some(s) = &c.sync {
                        text += &format!("\nsync ×{}{}", s.count, if s.inverted { " (inverted)" } else { "" });
                    }
                    if let Some(f) = &c.fields {
                        if let Some(a) = f.activity {
                            text += &format!(
                                "
{}{}",
                                a.name(),
                                f.talk.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
                            );
                        }
                        let kinds: Vec<String> = f.kinds.iter().take(3).map(|(k, n)| format!("{k}×{n}")).collect();
                        text += &format!(
                            "\n{} · {} · {}/{} frames decoded",
                            f.id.as_deref().unwrap_or("-"),
                            kinds.join(", "),
                            f.decoded,
                            f.tried
                        );
                    }
                    text += &format!(
                        "\nlevels {} · {} · ±{:.1} kHz\nenvelope CV {:.2} · {:.2} s analysed",
                        if f.fsk_levels == 0 { "-".to_string() } else { f.fsk_levels.to_string() },
                        f.symbol_rate_raw.map(|v| format!("{v:.0} Bd")).unwrap_or("no symbol rate".into()),
                        f.deviation_hz / 1e3,
                        f.envelope_cv,
                        c.analysed_s
                    );
                }
                None => text += "\n\nnot classified yet",
            }
            let g = painter.layout_no_wrap(text, mono(11.0), theme::TEXT);
            let at = pos2((p.x + 14.0).min(full.right() - g.size().x - 10.0), (p.y + 14.0).min(full.bottom() - g.size().y - 8.0));
            painter.rect(
                Rect::from_min_size(at - vec2(6.0, 4.0), g.size() + vec2(12.0, 8.0)),
                4.0,
                theme::BG,
                Stroke::new(1.0, theme::BORDER2),
                egui::StrokeKind::Inside,
            );
            painter.galley(at, g, theme::TEXT);
        } else if let Some(p) = resp.hover_pos() {
            let fx = f0 + ((p.x - full.left()) / full.width()) as f64 * span;
            painter.vline(p.x, full.y_range(), Stroke::new(1.0, Color32::from_rgba_unmultiplied(230, 237, 243, 90)));
            let (full0, full1) = self.full_span().unwrap_or((f0, f1));
            let i = (((fx - full0) / (full1 - full0)).clamp(0.0, 1.0) * self.trace.len() as f64) as usize;
            let db = self.trace.get(i.min(self.trace.len().saturating_sub(1))).copied().unwrap_or(f32::NAN);
            let mut label = format!("{:.4} MHz  {:.1} dBFS", fx / 1e6, db);
            if self.zoom.is_some() {
                label += &format!("  ·  zoom ×{:.1} (right double-click resets)", self.zoom_factor());
            }
            if let Some((_, _, k)) = self.known.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&fx)) {
                label += &format!("  ·  {k}");
            }
            let galley = painter.layout_no_wrap(label, mono(11.0), theme::TEXT);
            let at = pos2((p.x + 10.0).min(full.right() - galley.size().x - 8.0), spec.top() + 6.0);
            painter.rect_filled(Rect::from_min_size(at - vec2(5.0, 2.0), galley.size() + vec2(10.0, 4.0)), 3.0, theme::BG);
            painter.galley(at, galley, theme::TEXT);
        }
        let click = resp.interact_pointer_pos().filter(|_| resp.clicked());
        let record = click.and(hovered);
        ShowResult { freq_hz: click.map(|p| f0 + ((p.x - full.left()) / full.width()) as f64 * span), record }
    }
}

/// What a click on the view hit.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShowResult {
    pub freq_hz: Option<f64>,
    pub record: Option<u64>,
}

pub const HEADER_H: f32 = 26.0;

fn mono(size: f32) -> egui::FontId {
    egui::FontId::monospace(size)
}

fn nice_step(span: f64, n: f64) -> f64 {
    let target = span / n.max(2.0);
    let p = 10f64.powf(target.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if m * p >= target {
            return m * p;
        }
    }
    10.0 * p
}

fn decimals(step: f64) -> usize {
    let mut d = 0;
    while d < 6 && ((step * 10f64.powi(d as i32)).round() - step * 10f64.powi(d as i32)).abs() > 1e-6 {
        d += 1;
    }
    d
}
