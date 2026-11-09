//! The health slot (spec §5): three small plots for the selected decoder or receiver, kept by the app from the
//! snapshots it sees. Each shows its current value with its unit and the window in words (the guide's D1, D7).
//!
//! A decoder's plots are its signal-to-noise ratio (its emitter's newest record), the share of frames that decoded in
//! each interval, and its bus channel's lag. A receiver's are its worst channel lag, how far the FFT runs behind, and
//! the seconds of history it holds.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use api::{EmitterId, Snapshot, SourceId};
use egui::{Color32, RichText, Stroke, pos2, vec2};

use crate::theme;

/// How much history a plot keeps.
pub const WINDOW: Duration = Duration::from_secs(120);
/// How often a point is taken.
pub const EVERY: Duration = Duration::from_millis(500);

/// One plotted quantity: points as (seconds since the history began, value).
#[derive(Default, Debug, Clone)]
pub struct Series {
    pub points: VecDeque<(f64, f32)>,
}

impl Series {
    fn push(&mut self, t: f64, v: f32) {
        if v.is_finite() {
            self.points.push_back((t, v));
        }
        while self.points.front().is_some_and(|p| t - p.0 > WINDOW.as_secs_f64()) {
            self.points.pop_front();
        }
    }

    pub fn last(&self) -> Option<f32> {
        self.points.back().map(|p| p.1)
    }
}

#[derive(Default, Debug, Clone)]
pub struct DecoderSeries {
    pub snr_db: Series,
    pub ok_pct: Series,
    pub lag_s: Series,
    counts: Option<(u64, u64)>,
}

#[derive(Default, Debug, Clone)]
pub struct ReceiverSeries {
    pub lag_s: Series,
    pub behind_s: Series,
    pub held_s: Series,
}

/// A decoder's key: its receiver, its emitter and its mode (one decoder per protocol per emitter).
pub type DecoderKey = (SourceId, EmitterId, String);

pub struct History {
    start: Instant,
    last: Option<Instant>,
    pub decoders: HashMap<DecoderKey, DecoderSeries>,
    pub receivers: HashMap<SourceId, ReceiverSeries>,
}

impl Default for History {
    fn default() -> Self {
        History { start: Instant::now(), last: None, decoders: HashMap::new(), receivers: HashMap::new() }
    }
}

impl History {
    /// Takes a point from `snap` if [`EVERY`] has passed since the last; forgets decoders and receivers gone.
    pub fn sample(&mut self, snap: &Snapshot, now: Instant) {
        if self.last.is_some_and(|l| now.duration_since(l) < EVERY) {
            return;
        }
        self.last = Some(now);
        let t = now.duration_since(self.start).as_secs_f64();
        let lag_of = |source: SourceId, ch: api::ChannelId| {
            snap.receivers.iter().find(|r| r.id == source).and_then(|r| r.consumers.iter().find(|c| c.id == ch)).map(|c| c.lag_secs)
        };
        let mut alive = Vec::new();
        for d in &snap.decoders {
            let key = (d.source, d.emitter, d.mode.clone());
            let s = self.decoders.entry(key.clone()).or_default();
            alive.push(key);
            let snr = snap
                .records
                .records
                .iter()
                .filter(|r| r.emitter == Some(d.emitter))
                .max_by_key(|r| r.first_sample)
                .map_or(f32::NAN, |r| r.mean_snr_db);
            s.snr_db.push(t, snr);
            if let Some((ok, bad)) = s.counts {
                let (dok, dbad) = (d.blocks_ok.saturating_sub(ok), d.blocks_bad.saturating_sub(bad));
                if dok + dbad > 0 {
                    s.ok_pct.push(t, 100.0 * dok as f32 / (dok + dbad) as f32);
                }
            }
            s.counts = Some((d.blocks_ok, d.blocks_bad));
            s.lag_s.push(t, d.channel.and_then(|ch| lag_of(d.source, ch)).map_or(f32::NAN, |l| l as f32));
        }
        self.decoders.retain(|k, _| alive.contains(k));
        for r in &snap.receivers {
            let s = self.receivers.entry(r.id).or_default();
            s.lag_s.push(t, r.consumers.iter().map(|c| c.lag_secs as f32).fold(0.0, f32::max));
            s.behind_s.push(t, r.stage.behind_secs as f32);
            s.held_s.push(t, r.ring.held_secs as f32);
        }
        self.receivers.retain(|id, _| snap.receivers.iter().any(|r| r.id == *id));
    }
}

/// One small plot: its key, its current value large with the unit, the window in words, and the line in a black well.
/// `fixed` gives the y range (frames ok % is 0–100); otherwise it fits the data.
pub fn plot(ui: &mut egui::Ui, key: &str, unit: &str, s: &Series, digits: usize, fixed: Option<(f32, f32)>) {
    ui.horizontal(|ui| {
        ui.label(theme::label(key));
        match s.last() {
            Some(v) => ui.label(theme::value_unit(&format!("{v:.digits$}"), unit)),
            None => ui.label(RichText::new("no data yet").color(theme::INK2)),
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("last {} min", WINDOW.as_secs() / 60)).color(theme::INK2).small());
        });
    });
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, theme::WELL);
    let Some(&(t_end, _)) = s.points.back() else { return };
    let t0 = t_end - WINDOW.as_secs_f64();
    let (lo, hi) = fixed.unwrap_or_else(|| {
        let (mut lo, mut hi) = s.points.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        if hi - lo < 1e-3 {
            (lo, hi) = (lo - 1.0, hi + 1.0);
        }
        (lo, hi)
    });
    let x = |t: f64| rect.left() + ((t - t0) / WINDOW.as_secs_f64()) as f32 * rect.width();
    let y = |v: f32| rect.bottom() - 3.0 - (v - lo) / (hi - lo) * (rect.height() - 6.0);
    let line: Vec<_> = s.points.iter().map(|&(t, v)| pos2(x(t), y(v.clamp(lo, hi)))).collect();
    if line.len() == 1 {
        painter.circle_filled(line[0], 1.5, theme::TRACE);
    } else {
        painter.add(egui::Shape::line(line, Stroke::new(1.2, theme::TRACE)));
    }
    let scale = |v: f32| RichText::new(format!("{v:.digits$}")).size(10.0).color(theme::INK2);
    let ink = |t: RichText| egui::WidgetText::from(t).into_galley(ui, None, f32::INFINITY, egui::FontSelection::Default);
    let (top, bottom) = (ink(scale(hi)), ink(scale(lo)));
    painter.galley(rect.left_top() + vec2(3.0, 1.0), top, Color32::WHITE);
    painter.galley(pos2(rect.left() + 3.0, rect.bottom() - 12.0), bottom, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_keeps_its_window_and_skips_non_numbers() {
        let mut s = Series::default();
        for k in 0..400 {
            s.push(k as f64 * 0.5, if k % 10 == 0 { f32::NAN } else { k as f32 });
        }
        assert_eq!(s.last(), Some(399.0));
        let first = s.points.front().unwrap().0;
        assert!(199.5 - first <= WINDOW.as_secs_f64(), "kept from {first}");
        assert!(s.points.iter().all(|p| p.1.is_finite()));
    }
}
