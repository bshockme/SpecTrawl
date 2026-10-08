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
/// Resolution rows are kept at for recolouring; display width never exceeds it.
const HIST_W: usize = 4096;

pub struct SpectrumView {
    pub db_min: f32,
    pub db_max: f32,
    pub smoothing: f32,
    /// Waterfall row height in points; `None` fits the whole history.
    pub row_px: Option<f32>,
    /// Draw unselected record boxes faintly and without labels (dense views).
    pub subtle_boxes: bool,
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

    /// Puts the noise floor near the bottom of the colour map.
    pub fn auto_range(&mut self) {
        if self.trace.is_empty() {
            return;
        }
        let mut v = self.trace.clone();
        v.sort_by(f32::total_cmp);
        let floor = v[v.len() / 5];
        self.db_min = (floor - 6.0).round();
        self.db_max = self.db_min + 65.0;
    }

    /// Pulls new spectrum rows from the source.
    pub fn ingest(&mut self, src: &Source) {
        let rows = src.rows_after(self.last_seq);
        let skip = rows.len().saturating_sub(ROWS);
        for row in rows.iter().skip(skip) {
            self.last_seq = Some(row.seq);
            if self.tuning != Some(row.tuning) || self.trace.len() != row.db.len() {
                // Retune or FFT size change: restart the traces.
                let first = self.tuning.is_none();
                self.tuning = Some(row.tuning);
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
            let mut dec = vec![0.0; row.db.len().min(HIST_W)];
            dsp::decimate_max(&row.db, &mut dec);
            if self.history.len() == ROWS {
                self.history.pop_back();
            }
            self.history.push_front(dec);
            self.push_tex_row();
        }
    }

    fn colorize(&self, db: &[f32], out: &mut Vec<Color32>) {
        let lo = self.db_min;
        let k = 255.0 / (self.db_max - self.db_min).max(1.0);
        out.clear();
        out.extend(db.iter().map(|&v| self.lut[((v - lo) * k).clamp(0.0, 255.0) as usize]));
    }

    fn push_tex_row(&mut self) {
        let (Some(tex), Some(row)) = (self.tex.as_mut(), self.history.front()) else { return };
        if self.tex_w == 0 {
            return;
        }
        let mut cols = vec![0.0; self.tex_w];
        dsp::decimate_max(row, &mut cols);
        let mut px = Vec::with_capacity(self.tex_w);
        // Borrow juggling: colourise with a copy of the parameters.
        let (lo, k, lut) = (self.db_min, 255.0 / (self.db_max - self.db_min).max(1.0), &self.lut);
        px.extend(cols.iter().map(|&v| lut[((v - lo) * k).clamp(0.0, 255.0) as usize]));
        self.write_row = (self.write_row + ROWS - 1) % ROWS;
        let img = egui::ColorImage::new([self.tex_w, 1], px);
        tex.set_partial([0, self.write_row], img, TextureOptions::LINEAR);
    }

    /// Rebuilds the whole texture from history (on resize or range change).
    fn rebuild_tex(&mut self, ctx: &egui::Context, width: usize) {
        self.tex_w = width;
        self.tex_range = (self.db_min, self.db_max);
        self.write_row = 0;
        let mut px = vec![theme::PLOT_BG; width * ROWS];
        let mut cols = vec![0.0; width];
        let mut line = Vec::with_capacity(width);
        for (r, row) in self.history.iter().enumerate() {
            dsp::decimate_max(row, &mut cols);
            self.colorize(&cols, &mut line);
            px[r * width..(r + 1) * width].copy_from_slice(&line);
        }
        let img = egui::ColorImage::new([width, ROWS], px);
        match &mut self.tex {
            Some(t) if t.size() == [width, ROWS] => t.set(img, TextureOptions::LINEAR),
            _ => self.tex = Some(ctx.load_texture("waterfall", img, TextureOptions::LINEAR)),
        }
    }

    /// Frequency extent currently displayed, in Hz.
    pub fn span(&self) -> Option<(f64, f64)> {
        self.tuning.map(|t| (t.center_hz - t.span_hz / 2.0, t.center_hz + t.span_hz / 2.0))
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
                let age = now.duration_since(*t).as_secs_f64() / pass;
                let a = if h == s.current_hop { 255 } else { (200.0 * (1.0 - age.min(1.0) * 0.8)) as u8 };
                let x0 = strip.left() + strip.width() * h as f32 / s.hops as f32;
                let x1 = strip.left() + strip.width() * (h + 1) as f32 / s.hops as f32;
                p.rect_filled(Rect::from_x_y_ranges(x0..=(x1 - 1.0).max(x0 + 0.5), strip.y_range()), 0.0, Color32::from_rgba_unmultiplied(0x38, 0xd3, 0xcf, a));
            }
            full.min.y += 6.0;
        }
        let resp = ui.allocate_rect(full, egui::Sense::click());
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
        if self.tex.is_none() || want_w != self.tex_w || self.tex_range != (self.db_min, self.db_max) {
            self.rebuild_tex(ui.ctx(), want_w);
        }
        if let Some(tex) = &self.tex {
            // Newest row at the top. Show `visible` rows starting at the ring's
            // write position, wrapping once past the end of the texture.
            let start = self.write_row;
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
                painter.text(pos2(x, spec.bottom() - 3.0), egui::Align2::CENTER_BOTTOM, format!("{:.*}", dec, f / 1e6), mono(10.0), theme::MUTED);
            }
            f += step;
        }

        let cols = (spec.width() as usize).max(2);
        self.scratch.resize(cols, 0.0);
        let x_of = |i: usize| spec.left() + i as f32 * spec.width() / (cols - 1) as f32;
        // Peak hold.
        dsp::decimate_max(&self.hold, &mut self.scratch);
        let hold_pts: Vec<Pos2> = self.scratch.iter().enumerate().map(|(i, &v)| pos2(x_of(i), y_of(v))).collect();
        // Trace with fill.
        dsp::decimate_max(&self.trace, &mut self.scratch);
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
        painter.add(Shape::line(hold_pts, Stroke::new(1.0, Color32::from_rgba_unmultiplied(242, 163, 58, 110))));
        painter.add(Shape::line(pts, Stroke::new(1.3, theme::ACCENT)));
        painter.hline(full.x_range(), spec.bottom() + 0.5, Stroke::new(1.0, theme::BORDER));

        // --- record boxes ---
        let newest = self.last_seq.unwrap_or(0);
        let row_h = wf.height() / visible as f32;
        let x_hz = |f: f64| full.left() + ((f - f0) / span) as f32 * full.width();
        let mut boxes: Vec<(Rect, &SignalRecord)> = Vec::new();
        for r in records {
            if r.f_hi_hz < f0 || r.f_lo_hz > f1 || r.first_seq > newest {
                continue;
            }
            let back_end = newest.saturating_sub(r.last_seq) as f32;
            let back_start = newest.saturating_sub(r.first_seq) as f32 + 1.0;
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
        let hover = resp.hover_pos();
        let hovered = hover.and_then(|p| boxes.iter().filter(|(b, _)| b.expand(2.0).contains(p)).min_by(|a, b| a.0.area().total_cmp(&b.0.area())).map(|(_, r)| r.id));
        for (b, r) in &boxes {
            let sel = selected == Some(r.id) || hovered == Some(r.id);
            let mut c = if sel { theme::SELECT } else if r.active { theme::ACCENT } else { theme::RECORD };
            if self.subtle_boxes && !sel {
                c = c.gamma_multiply(0.35);
            }
            painter.rect_stroke(*b, 2.0, Stroke::new(if sel { 2.0 } else { 1.0 }, c), egui::StrokeKind::Outside);
            if sel || (b.width() >= 46.0 && !self.subtle_boxes) {
                let g = painter.layout_no_wrap(format!("#{}", r.id), mono(10.0), theme::BG);
                let at = pos2(b.left(), (b.top() - g.size().y - 1.0).max(wf.top()));
                painter.rect_filled(Rect::from_min_size(at, g.size() + vec2(6.0, 0.0)), 2.0, c);
                painter.galley(at + vec2(3.0, 0.0), g, theme::BG);
            }
        }

        // --- hover readout ---
        if let Some(r) = hovered.and_then(|id| records.iter().find(|r| r.id == id)) {
            let p = hover.unwrap_or_default();
            let text = format!(
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
            let g = painter.layout_no_wrap(text, mono(11.0), theme::TEXT);
            let at = pos2((p.x + 14.0).min(full.right() - g.size().x - 10.0), (p.y + 14.0).min(full.bottom() - g.size().y - 8.0));
            painter.rect(Rect::from_min_size(at - vec2(6.0, 4.0), g.size() + vec2(12.0, 8.0)), 4.0, theme::BG, Stroke::new(1.0, theme::BORDER2), egui::StrokeKind::Inside);
            painter.galley(at, g, theme::TEXT);
        } else if let Some(p) = resp.hover_pos() {
            let fx = f0 + ((p.x - full.left()) / full.width()) as f64 * span;
            painter.vline(p.x, full.y_range(), Stroke::new(1.0, Color32::from_rgba_unmultiplied(230, 237, 243, 90)));
            let i = (((p.x - spec.left()) / spec.width()) * self.trace.len() as f32) as usize;
            let db = self.trace.get(i.min(self.trace.len().saturating_sub(1))).copied().unwrap_or(f32::NAN);
            let label = format!("{:.4} MHz  {:.1} dBFS", fx / 1e6, db);
            let galley = painter.layout_no_wrap(label, mono(11.0), theme::TEXT);
            let at = pos2((p.x + 10.0).min(full.right() - galley.size().x - 8.0), spec.top() + 6.0);
            painter.rect_filled(Rect::from_min_size(at - vec2(5.0, 2.0), galley.size() + vec2(10.0, 4.0)), 3.0, theme::BG);
            painter.galley(at, galley, theme::TEXT);
        }
        let click = resp.interact_pointer_pos().filter(|_| resp.clicked());
        ShowResult {
            freq_hz: click.map(|p| f0 + ((p.x - full.left()) / full.width()) as f64 * span),
            record: click.and(hovered),
        }
    }
}

/// What a click on the view hit.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShowResult {
    pub freq_hz: Option<f64>,
    pub record: Option<u64>,
}

const HEADER_H: f32 = 26.0;

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
