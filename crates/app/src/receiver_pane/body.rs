//! The receiver pane's body: the spectrum trace over the ring-texture waterfall, with zoom, pan and drag-to-tune.

use std::sync::Arc;

use api::{Command, Receiver, RecordId, Row, SignalRecord};
use egui::{Color32, ColorImage, Mesh, Pos2, Rect, Sense, Shape, Stroke, TextureHandle, TextureOptions, pos2, vec2};

use crate::theme;
use crate::trace::{self, Trace};
use crate::waterfall::{self, HIST_W, History, Lut, Pushed, ROWS, View};

/// The spectrum plot's height, in points.
const SPECTRUM_H: f32 = 160.0;
/// Room under the plot for the frequency labels.
const LABELS_H: f32 = 14.0;
/// Seconds between the waterfall's time marks.
const MARK_SECS: f64 = 10.0;
/// Each wheel notch narrows the window by this much.
const WHEEL_ZOOM: f64 = 1.25;

/// What the texture holds: its width, the view it was coloured with and the span it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Built {
    w: usize,
    view: View,
    span: (f64, f64),
}

pub(super) struct Body {
    pub(super) history: History,
    trace: Trace,
    zoom: Option<(f64, f64)>,
    lut: Lut,
    tex: Option<TextureHandle>,
    built: Option<Built>,
    /// The texture row holding the newest row drawn.
    write_row: usize,
    /// Rows taken in since the pause, newer than the texture's top row.
    behind: usize,
    /// The texture must be rebuilt from history (a retune, or the end of a pause).
    dirty: bool,
    /// The spectrum drag in progress, in points.
    tune_drag: f32,
    cols: Vec<f32>,
    line: Vec<Color32>,
}

impl Body {
    pub(super) fn new() -> Body {
        Body {
            history: History::new(),
            trace: Trace::new(),
            zoom: None,
            lut: Lut(theme::waterfall_lut()),
            tex: None,
            built: None,
            write_row: 0,
            behind: 0,
            dirty: true,
            tune_drag: 0.0,
            cols: Vec::new(),
            line: Vec::new(),
        }
    }

    /// The newest row, reduced to history columns.
    pub(super) fn newest_db(&self) -> &[f32] {
        self.history.newest().map_or(&[], |r| r.db.as_slice())
    }

    /// Adds the snapshot's rows to the history; unpaused, each new row goes to the texture as it arrives.
    pub(super) fn ingest(&mut self, rows: &[std::sync::Arc<Row>], paused: bool) {
        for row in rows {
            let added = match self.history.push(row) {
                Pushed::Skipped => continue,
                Pushed::Cleared => {
                    self.trace.reset();
                    self.dirty = true;
                    self.behind = 0;
                    0
                }
                Pushed::Appended => 1,
                Pushed::Hole => 2,
            };
            if paused {
                self.behind = (self.behind + added).min(ROWS);
            } else if !self.dirty {
                // The hole's NaN row first, then the row itself.
                for k in (0..added).rev() {
                    self.push_tex_row(k);
                }
            }
            if let Some(newest) = self.history.newest() {
                self.trace.update(&newest.db);
            }
        }
    }

    /// The end of a pause: the texture catches up from history.
    pub(super) fn resume(&mut self) {
        self.behind = 0;
        self.dirty = true;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        r: &Receiver,
        records: &[Arc<SignalRecord>],
        selected: &mut Option<RecordId>,
        db_range: (f32, f32),
        paused: &mut bool,
        out: &mut Vec<Command>,
    ) {
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, theme::PLOT_BG);
        let Some(span) = self.history.span() else {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "waiting for rows…", egui::FontId::proportional(13.0), theme::MUTED);
            return;
        };
        let spec = Rect::from_min_size(rect.min, vec2(rect.width(), SPECTRUM_H.min(rect.height())));
        let wf = Rect::from_min_max(pos2(rect.left(), spec.bottom() + 1.0), rect.max);
        let spec_resp = ui.interact(spec, ui.id().with(("spectrum", r.id)), Sense::click_and_drag());
        let wf_resp = ui.interact(wf, ui.id().with(("waterfall", r.id)), Sense::click_and_drag());

        let mut view = View { db_min: db_range.0, db_max: db_range.1, zoom: self.zoom };
        self.interact(ui, &spec_resp, &wf_resp, span, &mut view, r, out);
        if ui.rect_contains_pointer(rect) && !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            *paused = !*paused;
            if !*paused {
                self.resume();
            }
        }
        self.zoom = view.zoom;

        if wf.height() > 1.0 {
            self.draw_waterfall(ui, &painter, wf, span, view, r.letter);
            let boxes = self.draw_records(&painter, wf, span, &view, records, *selected);
            // A click (not a drag) picks the smallest box under the pointer, or clears the selection.
            if wf_resp.clicked()
                && let Some(p) = wf_resp.interact_pointer_pos()
            {
                *selected = boxes
                    .iter()
                    .filter(|(_, b)| b.expand(2.0).contains(p))
                    .min_by(|a, b| a.1.area().total_cmp(&b.1.area()))
                    .map(|(id, _)| *id);
            }
        }
        self.draw_spectrum(&painter, spec, span, &view);
        painter.hline(rect.x_range(), spec.bottom() + 0.5, Stroke::new(1.0, theme::BORDER));
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
    }

    #[allow(clippy::too_many_arguments)]
    fn interact(
        &mut self,
        ui: &egui::Ui,
        spec: &egui::Response,
        wf: &egui::Response,
        span: (f64, f64),
        view: &mut View,
        r: &Receiver,
        out: &mut Vec<Command>,
    ) {
        // Wheel over either: zoom, keeping the frequency under the cursor where it is.
        let hovered = if spec.hovered() { Some(spec.rect) } else { wf.hovered().then_some(wf.rect) };
        if let (Some(area), Some(p)) = (hovered, ui.input(|i| i.pointer.hover_pos())) {
            let notches = ui.input(|i| wheel_notches(&i.events));
            if notches != 0.0 {
                let hz = view.x_to_hz(span, area, p.x);
                view.zoom_about(span, hz, WHEEL_ZOOM.powf(notches));
                let moved = view.x_to_hz(span, area, p.x);
                view.pan(span, hz - moved);
            }
        }
        if wf.dragged_by(egui::PointerButton::Primary) {
            let (lo, hi) = view.window(span);
            view.pan(span, -(wf.drag_delta().x / wf.rect.width()) as f64 * (hi - lo));
        }
        let primary = egui::PointerButton::Primary;
        let (offset, release) = tune_drag_step(
            self.tune_drag,
            spec.drag_started_by(primary),
            spec.dragged_by(primary),
            spec.drag_stopped_by(primary),
            spec.drag_delta().x,
        );
        self.tune_drag = offset;
        if let Some(dx) = release {
            let (lo, hi) = view.window(span);
            let dhz = (dx / spec.rect.width()) as f64 * (hi - lo);
            let hz = round_to(r.center_hz - dhz, r.caps.tuning_step_hz);
            if hz != r.center_hz {
                out.push(Command::Tune { source: r.id, center_hz: hz });
            }
        }
        if spec.double_clicked() || wf.double_clicked() {
            view.zoom = None;
        }
    }

    fn draw_spectrum(&mut self, painter: &egui::Painter, spec: Rect, span: (f64, f64), view: &View) {
        let plot = Rect::from_min_max(spec.min + vec2(0.0, 4.0), spec.max - vec2(0.0, LABELS_H));
        let (db_min, db_max) = (view.db_min, view.db_max);
        let y_of = |db: f32| plot.bottom() - ((db - db_min) / (db_max - db_min)).clamp(0.0, 1.0) * plot.height();
        let font = egui::FontId::monospace(10.0);

        // dB grid.
        let step = trace::nice_step((db_max - db_min) as f64, 6.0);
        let dec = if step < 1.0 { 1 } else { 0 };
        let mut db = (db_min as f64 / step).ceil() * step;
        while db <= db_max as f64 {
            let y = y_of(db as f32);
            painter.hline(plot.x_range(), y, Stroke::new(1.0, theme::GRID));
            painter.text(pos2(plot.left() + 4.0, y - 1.0), egui::Align2::LEFT_BOTTOM, format!("{db:.dec$}"), font.clone(), theme::MUTED);
            db += step;
        }
        // Frequency ticks, in MHz.
        let (lo, hi) = view.window(span);
        let step = trace::nice_step(hi - lo, 8.0);
        let dec = trace::decimals(step);
        let mut f = (lo / step).ceil() * step;
        while f <= hi {
            let x = view.hz_to_x(span, spec, f);
            painter.vline(x, plot.y_range(), Stroke::new(1.0, theme::GRID));
            if x > spec.left() + 24.0 && x < spec.right() - 24.0 {
                painter.text(
                    pos2(x, spec.bottom() - 2.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!("{:.dec$}", f / 1e6),
                    font.clone(),
                    theme::MUTED,
                );
            }
            f += step;
        }

        // Hold, then the trace over it, shifted by a tuning drag in progress.
        let (c0, c1) = view.cols(span, HIST_W);
        let n = (c1 - c0).min((plot.width().round() as usize).max(2));
        let col_hz = |c: f64| span.0 + c * (span.1 - span.0) / HIST_W as f64;
        let xs: Vec<f32> = (0..n)
            .map(|i| {
                let r = waterfall::column(i, n, c1 - c0);
                view.hz_to_x(span, spec, col_hz(c0 as f64 + (r.start + r.end) as f64 / 2.0)) + self.tune_drag
            })
            .collect();
        for (line, colour, width) in [(&self.trace.hold, theme::MUTED, 1.0), (&self.trace.smooth, theme::ACCENT, 1.3)] {
            if line.len() != HIST_W {
                continue;
            }
            self.cols.resize(n, 0.0);
            waterfall::resample_max(&line[c0..c1], &mut self.cols);
            let pts = self.cols.iter().zip(&xs).map(|(&v, &x)| (!v.is_nan()).then(|| pos2(x, y_of(v))));
            for run in runs(pts) {
                painter.add(Shape::line(run, Stroke::new(width, colour)));
            }
        }
    }

    fn draw_waterfall(&mut self, ui: &egui::Ui, painter: &egui::Painter, wf: Rect, span: (f64, f64), view: View, letter: char) {
        let w = ((wf.width() * ui.ctx().pixels_per_point()).round() as usize).clamp(1, HIST_W);
        let want = Built { w, view, span };
        if self.dirty || self.tex.is_none() || self.built != Some(want) {
            self.rebuild(ui.ctx(), letter, want);
        }
        let Some(tex) = &self.tex else { return };
        // The texture holds whole history columns [c0, c1); show just the window of them.
        let (c0, c1) = view.cols(span, HIST_W);
        let (lo, hi) = view.window(span);
        let col_hz = |c: usize| span.0 + c as f64 * (span.1 - span.0) / HIST_W as f64;
        let (h0, h1) = (col_hz(c0), col_hz(c1));
        let (u0, u1) = (((lo - h0) / (h1 - h0)) as f32, ((hi - h0) / (h1 - h0)) as f32);
        let mut mesh = Mesh::with_texture(tex.id());
        for (uv, (top, bottom)) in waterfall::quads(self.write_row).into_iter().flatten() {
            let screen = Rect::from_x_y_ranges(wf.x_range(), wf.top() + top * wf.height()..=wf.top() + bottom * wf.height());
            let uv = Rect::from_min_max(pos2(u0, uv.min.y), pos2(u1, uv.max.y));
            mesh.add_rect_with_uv(screen, uv, Color32::WHITE);
        }
        painter.add(Shape::mesh(mesh));

        // Time marks every 10 s at the left edge.
        let row_h = wf.height() as f64 / ROWS as f64;
        let every = MARK_SECS / self.history.secs_per_row();
        let font = egui::FontId::monospace(10.0);
        let mut k = 1;
        loop {
            let y = wf.top() + (k as f64 * every * row_h) as f32;
            if y.is_nan() || y >= wf.bottom() - 6.0 || k > 200 {
                break;
            }
            // A tick at the mark, its label just above it.
            painter.hline(wf.left()..=wf.left() + 10.0, y, Stroke::new(1.0, theme::TEXT2));
            painter.text(
                pos2(wf.left() + 3.0, y - 1.0),
                egui::Align2::LEFT_BOTTOM,
                format!("−{} s", k * MARK_SECS as u64),
                font.clone(),
                theme::TEXT2,
            );
            k += 1;
        }
    }

    /// Outlines each record over the rows that saw it: neutral for "unknown" (step 6 colours by the kind of claim),
    /// the selection's colour when selected, no top edge while still open. Returns each drawn box.
    fn draw_records(
        &self,
        painter: &egui::Painter,
        wf: Rect,
        span: (f64, f64),
        view: &View,
        records: &[Arc<SignalRecord>],
        selected: Option<RecordId>,
    ) -> Vec<(RecordId, Rect)> {
        let row_h = wf.height() / ROWS as f32;
        let behind = self.behind as isize;
        let mut out = Vec::new();
        for rec in records {
            let Some((top, bottom)) = self.history.rows_of(rec.first_sample, rec.end_sample) else { continue };
            let (top, bottom) = ((top as isize - behind).max(0), bottom as isize - behind);
            if bottom < 0 {
                continue;
            }
            let (mut x0, mut x1) = (view.hz_to_x(span, wf, rec.f_lo_hz), view.hz_to_x(span, wf, rec.f_hi_hz));
            if x1 < wf.left() || x0 > wf.right() {
                continue;
            }
            // At least a few points wide: a 600 Hz line on a 10 MHz span is a fraction of a pixel.
            if x1 - x0 < 4.0 {
                let c = (x0 + x1) / 2.0;
                (x0, x1) = (c - 2.0, c + 2.0);
            }
            let (y0, y1) = (wf.top() + top as f32 * row_h, wf.top() + (bottom + 1) as f32 * row_h);
            let b = Rect::from_min_max(pos2(x0, y0), pos2(x1, y1.max(y0 + 2.0))).intersect(wf);
            let colour = if selected == Some(rec.id) { theme::SELECT } else { theme::RECORD.gamma_multiply(0.7) };
            let stroke = Stroke::new(if selected == Some(rec.id) { 1.5 } else { 1.0 }, colour);
            painter.line_segment([b.left_bottom(), b.right_bottom()], stroke);
            painter.line_segment([b.left_top(), b.left_bottom()], stroke);
            painter.line_segment([b.right_top(), b.right_bottom()], stroke);
            if !rec.open {
                painter.line_segment([b.left_top(), b.right_top()], stroke);
            }
            out.push((rec.id, b));
        }
        out
    }

    /// Recolours the whole texture from history (skipping rows that arrived during a pause).
    fn rebuild(&mut self, ctx: &egui::Context, letter: char, want: Built) {
        let Built { w, view, span } = want;
        let (c0, c1) = view.cols(span, HIST_W);
        let mut px = vec![theme::PLOT_BG; w * ROWS];
        self.cols.resize(w, 0.0);
        for (i, row) in self.history.iter().skip(self.behind).take(ROWS).enumerate() {
            waterfall::resample_max(&row.db[c0..c1], &mut self.cols);
            waterfall::colorize(&self.cols, &view, &self.lut, &mut self.line);
            px[i * w..(i + 1) * w].copy_from_slice(&self.line);
        }
        let img = ColorImage::new([w, ROWS], px);
        match &mut self.tex {
            Some(t) if t.size() == [w, ROWS] => t.set(img, TextureOptions::LINEAR),
            _ => self.tex = Some(ctx.load_texture(format!("waterfall-{letter}"), img, TextureOptions::LINEAR)),
        }
        self.write_row = 0;
        self.built = Some(want);
        self.dirty = false;
    }

    /// History row `k` (0 = newest) into the texture, one row up the ring.
    fn push_tex_row(&mut self, k: usize) {
        let (Some(built), Some(tex), Some(row)) = (self.built, self.tex.as_mut(), self.history.iter().nth(k)) else { return };
        let (c0, c1) = built.view.cols(built.span, HIST_W);
        self.cols.resize(built.w, 0.0);
        waterfall::resample_max(&row.db[c0..c1], &mut self.cols);
        waterfall::colorize(&self.cols, &built.view, &self.lut, &mut self.line);
        self.write_row = waterfall::next_write_row(self.write_row);
        tex.set_partial([0, self.write_row], ColorImage::new([built.w, 1], self.line.clone()), TextureOptions::LINEAR);
    }
}

/// Wheel notches this frame (lines; points and pages converted), positive away from the user.
fn wheel_notches(events: &[egui::Event]) -> f64 {
    events
        .iter()
        .map(|e| match e {
            egui::Event::MouseWheel { unit, delta, .. } => match unit {
                egui::MouseWheelUnit::Line => delta.y as f64,
                egui::MouseWheelUnit::Page => delta.y as f64 * 3.0,
                egui::MouseWheelUnit::Point => delta.y as f64 / 50.0,
            },
            _ => 0.0,
        })
        .sum()
}

/// One frame of the spectrum's tuning drag: the offset (points) to draw the trace shifted by, and the offset to tune by
/// when the drag ended this frame. A new drag starts from zero, and an offset left over from a drag whose release was
/// never seen (the pane was not drawn that frame) is dropped as soon as the spectrum is seen not dragging.
fn tune_drag_step(offset: f32, started: bool, dragged: bool, stopped: bool, dx: f32) -> (f32, Option<f32>) {
    let mut o = if started { 0.0 } else { offset };
    if dragged {
        o += dx;
    }
    if stopped {
        return (0.0, (o != 0.0).then_some(o));
    }
    if dragged { (o, None) } else { (0.0, None) }
}

/// `hz` to the nearest multiple of `step` (unchanged for a step of 0).
fn round_to(hz: f64, step: f64) -> f64 {
    if step > 0.0 { (hz / step).round() * step } else { hz }
}

/// The unbroken stretches of a line that has holes.
fn runs(pts: impl Iterator<Item = Option<Pos2>>) -> Vec<Vec<Pos2>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for p in pts {
        match p {
            Some(p) => cur.push(p),
            None if cur.len() > 1 => out.push(std::mem::take(&mut cur)),
            None => cur.clear(),
        }
    }
    if cur.len() > 1 {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tuning_drag_tunes_once_by_its_own_movement() {
        let (o, t) = tune_drag_step(0.0, true, true, false, 5.0);
        assert_eq!((o, t), (5.0, None));
        let (o, t) = tune_drag_step(o, false, true, false, 15.0);
        assert_eq!((o, t), (20.0, None));
        assert_eq!(tune_drag_step(o, false, false, true, 0.0), (0.0, Some(20.0)));
    }

    #[test]
    fn a_stale_offset_never_tunes() {
        // The release was never seen: the next frame drawn without a drag drops the offset.
        assert_eq!(tune_drag_step(20.0, false, false, false, 0.0), (0.0, None));
        // A new drag starts from zero; a press and release without movement does not tune.
        let (o, t) = tune_drag_step(20.0, true, true, false, 0.0);
        assert_eq!((o, t), (0.0, None));
        assert_eq!(tune_drag_step(o, false, false, true, 0.0), (0.0, None));
    }
}
