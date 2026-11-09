//! One receiver: its header toolbar (ui-design §3.1), its status line, then its spectrum and waterfall ([`body`]).

mod body;

use api::{Command, GainSetting, Health, Mode, Receiver, Status};
use egui::PopupCloseBehavior;
use egui::containers::menu::{MenuButton, MenuConfig};

use crate::{theme, trace};

/// The display range a new pane starts with when nothing was saved for its device, in dB.
pub const DEFAULT_DB_RANGE: (f32, f32) = (-110.0, -45.0);

pub struct ReceiverPane {
    last_status: Status,
    /// `(center_hz, sample_rate)` at the last snapshot.
    last_tuning: Option<(f64, f64)>,
    /// `(floor, top)` in dB.
    db_range: (f32, f32),
    paused: bool,
    /// The centre drag value's copy, in MHz; follows the snapshot except while it is being edited.
    centre_mhz: f64,
    centre_editing: bool,
    /// The sweep span fields' copies, in MHz; follow the snapshot except while either is being edited.
    span_mhz: (f64, f64),
    span_editing: bool,
    gain: GainDraft,
    gain_open: bool,
    body: body::Body,
}

/// The steps of the linearity and sensitivity presets, 0 to this: what Airspy's and HydraSDR's own tools show. The api
/// carries the setting as a fraction (0 to 1) for any backend; the popover shows steps.
const PRESET_STEPS: u8 = 21;

fn to_step(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * PRESET_STEPS as f32).round() as u8
}

fn from_step(step: u8) -> f32 {
    step.min(PRESET_STEPS) as f32 / PRESET_STEPS as f32
}

/// The `Gain ⏷` popover's controls, synced from the snapshot while the popover is closed.
#[derive(Debug, Clone, Copy)]
struct GainDraft {
    /// Preset steps, 0 to [`PRESET_STEPS`].
    linearity: u8,
    sensitivity: u8,
    manual: [u8; 3],
}

impl GainDraft {
    fn sync(&mut self, g: Option<GainSetting>) {
        match g {
            Some(GainSetting::Linearity(v)) => self.linearity = to_step(v),
            Some(GainSetting::Sensitivity(v)) => self.sensitivity = to_step(v),
            Some(GainSetting::Manual { lna, mixer, vga }) => self.manual = [lna, mixer, vga],
            Some(GainSetting::Auto) | None => {}
        }
    }
}

impl Default for ReceiverPane {
    fn default() -> Self {
        ReceiverPane::new()
    }
}

impl ReceiverPane {
    pub fn new() -> ReceiverPane {
        ReceiverPane {
            last_status: Status { health: Health::Ok, text: String::new(), reason: None },
            last_tuning: None,
            db_range: DEFAULT_DB_RANGE,
            paused: false,
            centre_mhz: 0.0,
            centre_editing: false,
            span_mhz: (24.0, 1800.0),
            span_editing: false,
            gain: GainDraft { linearity: 13, sensitivity: 13, manual: [8, 8, 8] },
            gain_open: false,
            body: body::Body::new(),
        }
    }

    /// Takes in a new snapshot's view of this receiver, its rows into the waterfall's history.
    pub fn ingest(&mut self, r: &Receiver) {
        self.last_status = r.status.clone();
        self.last_tuning = Some((r.center_hz, r.sample_rate));
        self.body.ingest(&r.rows, self.paused);
    }

    /// `records`: this receiver's, from the snapshot; `selected` is shared with the records list.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        r: &Receiver,
        records: &[std::sync::Arc<api::SignalRecord>],
        decoders: &[api::DecoderView],
        selected: &mut crate::selection::Selection,
        bands: &[api::BandSpan],
        out: &mut Vec<Command>,
    ) {
        self.header(ui, r, selected, out);
        status_lines(ui, r);
        // One sentence per decoder on this receiver, with its lamp (spec §5's status line and lamps).
        for d in decoders {
            crate::systems_pane::decoder_line(ui, d);
        }
        ui.add_space(4.0);
        self.body.show(ui, r, records, selected, bands, self.db_range, &mut self.paused, out);
    }

    pub fn last_status(&self) -> &Status {
        &self.last_status
    }

    /// `(center_hz, sample_rate)` at the last snapshot taken in.
    pub fn last_tuning(&self) -> Option<(f64, f64)> {
        self.last_tuning
    }

    /// Waterfall rows held.
    pub fn history_len(&self) -> usize {
        self.body.history.len()
    }

    /// The frequencies the waterfall's rows cover (`None` before the first row).
    pub fn history_span(&self) -> Option<(f64, f64)> {
        self.body.history.span()
    }

    /// The waterfall rows (0 = newest) a record's box spans, if any held row saw it.
    pub fn record_rows(&self, r: &api::SignalRecord) -> Option<(usize, usize)> {
        self.body.history.rows_of(r.first_sample, r.end_sample)
    }

    /// All-NaN rows the history holds, one per discontinuity between snapshot rows.
    pub fn history_holes(&self) -> usize {
        self.body.history.iter().filter(|r| r.db.iter().all(|v| v.is_nan())).count()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// `(floor, top)` in dB.
    pub fn db_range(&self) -> (f32, f32) {
        self.db_range
    }

    pub fn set_db_range(&mut self, range: (f32, f32)) {
        self.db_range = range;
    }

    fn header(&mut self, ui: &mut egui::Ui, r: &Receiver, selected: &mut crate::selection::Selection, out: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            let letter = r.letter.to_string();
            // The letter is an identifier, not a state: a neutral chip.
            theme::chip(ui, &letter, theme::CHIP, theme::INK);
            // The receiver's name selects it: the inspector shows its ring, stage and decoders.
            let me = crate::selection::Selection::Receiver(r.id);
            if ui.selectable_label(*selected == me, &r.device.label).on_hover_text("Show this receiver in the inspector").clicked() {
                selected.toggle(me);
            }
            for mode in [Mode::Live, Mode::Sweep] {
                if ui.add(egui::Button::new(mode_name(mode)).selected(r.mode == mode)).clicked() && r.mode != mode {
                    out.push(Command::SetMode { source: r.id, mode });
                }
            }
            ui.separator();
            match r.mode {
                Mode::Live => self.centre(ui, r, out),
                Mode::Sweep => self.span(ui, r, out),
            }
            self.gain_menu(ui, r, out);
            self.db_menu(ui);
            if ui.add(egui::Button::new("Pause").selected(self.paused)).on_hover_text("Freeze the waterfall (Space)").clicked() {
                self.paused = !self.paused;
                if !self.paused {
                    self.body.resume();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("…", |ui| {
                    if ui.button("Close device").clicked() {
                        out.push(Command::Close(r.id));
                        ui.close();
                    }
                });
            });
        });
    }

    fn centre(&mut self, ui: &mut egui::Ui, r: &Receiver, out: &mut Vec<Command>) {
        if !self.centre_editing {
            self.centre_mhz = r.center_hz / 1e6;
        }
        let (lo, hi) = r.caps.freq_ranges_hz.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &(a, b)| (lo.min(a), hi.max(b)));
        let mut dv = egui::DragValue::new(&mut self.centre_mhz).speed(0.001).fixed_decimals(3).suffix(" MHz");
        if lo < hi {
            dv = dv.range(lo / 1e6..=hi / 1e6);
        }
        let resp = ui.add(dv).on_hover_text("Centre frequency");
        self.centre_editing = resp.has_focus() || resp.dragged();
        if resp.drag_stopped() || resp.lost_focus() {
            let step = r.caps.tuning_step_hz;
            let hz = self.centre_mhz * 1e6;
            let hz = if step > 0.0 { (hz / step).round() * step } else { hz };
            if hz != r.center_hz {
                out.push(Command::Tune { source: r.id, center_hz: hz });
            }
        }
    }

    /// A sweeping receiver's span, from–to in MHz; an edit is sent once it settles.
    fn span(&mut self, ui: &mut egui::Ui, r: &Receiver, out: &mut Vec<Command>) {
        if !self.span_editing
            && let Some(s) = &r.sweep
        {
            self.span_mhz = (s.start_hz / 1e6, s.stop_hz / 1e6);
        }
        let (lo, hi) = r.caps.freq_ranges_hz.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &(a, b)| (lo.min(a), hi.max(b)));
        let range = if lo < hi { lo / 1e6..=hi / 1e6 } else { 0.0..=1e5 };
        let a = ui.add(egui::DragValue::new(&mut self.span_mhz.0).speed(1.0).fixed_decimals(1).suffix(" MHz").range(range.clone()));
        ui.label("–");
        let b = ui.add(egui::DragValue::new(&mut self.span_mhz.1).speed(1.0).fixed_decimals(1).suffix(" MHz").range(range));
        self.span_editing = a.has_focus() || a.dragged() || b.has_focus() || b.dragged();
        if settled(&a) || settled(&b) {
            let (start, stop) = (self.span_mhz.0.min(self.span_mhz.1) * 1e6, self.span_mhz.0.max(self.span_mhz.1) * 1e6);
            if stop > start {
                out.push(Command::SetSweep { source: r.id, start_hz: start, stop_hz: stop, segments: vec![] });
            }
        }
    }

    fn gain_menu(&mut self, ui: &mut egui::Ui, r: &Receiver, out: &mut Vec<Command>) {
        if !self.gain_open {
            self.gain.sync(r.gain);
        }
        let g = &mut self.gain;
        let (_, inner) = popover("Gain ⏷").ui(ui, |ui| {
            let mut set = None;
            // Each `SetGain` logs a line: send a drag's value once, when it ends.
            // In the presets' own steps, 0 to 21, as Airspy's and HydraSDR's tools number them.
            if settled(&ui.add(egui::Slider::new(&mut g.linearity, 0..=PRESET_STEPS).text("Linearity"))) {
                set = Some(GainSetting::Linearity(from_step(g.linearity)));
            }
            if settled(&ui.add(egui::Slider::new(&mut g.sensitivity, 0..=PRESET_STEPS).text("Sensitivity"))) {
                set = Some(GainSetting::Sensitivity(from_step(g.sensitivity)));
            }
            let now = match r.gain {
                Some(GainSetting::Linearity(v)) => format!("Now: linearity step {}", to_step(v)),
                Some(GainSetting::Sensitivity(v)) => format!("Now: sensitivity step {}", to_step(v)),
                Some(GainSetting::Auto) => "Now: automatic (AGC)".into(),
                Some(GainSetting::Manual { lna, mixer, vga }) => format!("Now: by hand, LNA {lna}, mixer {mixer}, VGA {vga}"),
                None => "Now: the device's default".into(),
            };
            ui.label(egui::RichText::new(now).color(theme::INK2));
            if ui.button("Auto").clicked() {
                set = Some(GainSetting::Auto);
            }
            ui.separator();
            ui.label(egui::RichText::new("Manual").color(theme::INK2));
            if r.caps.gain_stages.is_empty() {
                ui.label(egui::RichText::new("This device has no hand-set stages.").color(theme::INK2));
            }
            for (i, stage) in r.caps.gain_stages.iter().take(3).enumerate() {
                ui.horizontal(|ui| {
                    let range = stage.min.max(0.0) as u8..=stage.max.clamp(0.0, 255.0) as u8;
                    if settled(&ui.add(egui::DragValue::new(&mut g.manual[i]).range(range))) {
                        let [lna, mixer, vga] = g.manual;
                        set = Some(GainSetting::Manual { lna, mixer, vga });
                    }
                    ui.label(&stage.name);
                });
            }
            if let Some(gain) = set {
                out.push(Command::SetGain { source: r.id, gain });
            }
        });
        self.gain_open = inner.is_some();
    }

    fn db_menu(&mut self, ui: &mut egui::Ui) {
        let newest = self.body.newest_db();
        let (floor, top) = &mut self.db_range;
        popover("dB range ⏷").ui(ui, |ui| {
            egui::Grid::new("db_range").num_columns(2).show(ui, |ui| {
                let lo = *floor + 1.0;
                ui.label("Top");
                ui.add(egui::DragValue::new(top).range(lo..=20.0).speed(0.5).suffix(" dB"));
                ui.end_row();
                let hi = *top - 1.0;
                ui.label("Floor");
                ui.add(egui::DragValue::new(floor).range(-200.0..=hi).speed(0.5).suffix(" dB"));
                ui.end_row();
            });
            if ui.button("Auto").on_hover_text("Fit the range to the newest row").clicked() {
                (*floor, *top) = trace::auto_range(newest);
            }
        });
    }
}

/// A header popover that stays open while its controls are used.
/// Whether an edit is finished: a drag when it stops (not every frame of it), a click or a typed value at once.
fn settled(r: &egui::Response) -> bool {
    edit_settled(r.changed(), r.dragged(), r.drag_stopped())
}

fn edit_settled(changed: bool, dragged: bool, drag_stopped: bool) -> bool {
    drag_stopped || (changed && !dragged)
}

fn popover(text: &str) -> MenuButton<'_> {
    MenuButton::new(text).config(MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside))
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Live => "Live",
        Mode::Sweep => "Sweep",
    }
}

/// The status sentence coloured by health, then a muted clipping count when the input has clipped.
fn status_lines(ui: &mut egui::Ui, r: &Receiver) {
    let colour = match r.status.health {
        Health::Ok => theme::INK2,
        Health::Warning => theme::CAUTION,
        Health::Lost => theme::ALARM,
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(&r.status.text).color(colour));
        if let Some(reason) = &r.status.reason {
            ui.label(egui::RichText::new(format!("· {reason}")).color(theme::INK2));
        }
    });
    if r.ring.overloads > 0 {
        ui.label(egui::RichText::new(format!("clipping ({})", r.ring.overloads)).small().color(theme::INK2));
    }
}

#[cfg(test)]
mod tests {
    use super::edit_settled;

    #[test]
    fn a_drag_settles_once_when_it_stops_and_a_click_at_once() {
        // Frames of a drag: changing, then the release.
        assert!(!edit_settled(true, true, false));
        assert!(!edit_settled(false, true, false));
        assert!(edit_settled(false, false, true));
        // A click on the track, or a typed value.
        assert!(edit_settled(true, false, false));
        assert!(!edit_settled(false, false, false));
    }
}

#[cfg(test)]
mod gain_tests {
    use super::*;

    #[test]
    fn preset_steps_round_trip_through_the_fraction_the_api_carries() {
        for step in 0..=PRESET_STEPS {
            assert_eq!(to_step(from_step(step)), step);
        }
        assert_eq!(to_step(13.0 / 21.0), 13);
        assert_eq!(to_step(1.5), PRESET_STEPS);
    }
}
