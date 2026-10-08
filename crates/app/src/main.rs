mod spectrum_view;
mod theme;

use std::time::{SystemTime, UNIX_EPOCH};

use engine::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::{Backend, DeviceInfo, GainSetting};
use spectrum_view::SpectrumView;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    spawn_deadlock_watchdog();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 960.0]).with_title("SpecTrawl"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("SpecTrawl", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Survey,
    Focus,
}

impl Role {
    fn chip(self) -> &'static str {
        match self {
            Role::Survey => "SURVEY",
            Role::Focus => "FOCUS A",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GainMode {
    Linearity,
    Sensitivity,
    Agc,
}

struct Pane {
    role: Role,
    source: Source,
    view: SpectrumView,
    gain_mode: GainMode,
    /// Preset step 0..=21 (Airspy's linearity/sensitivity tables).
    gain_step: u8,
}

impl Pane {
    fn gain_setting(&self) -> GainSetting {
        let v = self.gain_step as f32 / 21.0;
        match self.gain_mode {
            GainMode::Linearity => GainSetting::Linearity(v),
            GainMode::Sensitivity => GainSetting::Sensitivity(v),
            GainMode::Agc => GainSetting::Auto,
        }
    }
}

/// Gain controls for one pane; applies changes to the device.
fn gain_ui(ui: &mut egui::Ui, p: &mut Pane) -> Option<String> {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Gain");
        for (m, name) in [(GainMode::Linearity, "Linearity"), (GainMode::Sensitivity, "Sensitivity"), (GainMode::Agc, "AGC")] {
            changed |= ui.selectable_value(&mut p.gain_mode, m, name).changed();
        }
    });
    if p.gain_mode != GainMode::Agc {
        changed |= ui.add(egui::Slider::new(&mut p.gain_step, 0..=21).text("step")).changed();
    } else if p.role == Role::Survey {
        ui.label(egui::RichText::new("AGC makes hops inconsistent; fixed gain is better for surveys").small().color(theme::MUTED));
    }
    if changed {
        return p.source.set_gain(p.gain_setting()).err().map(|e| e.to_string());
    }
    None
}

struct App {
    backends: Vec<Box<dyn Backend>>,
    devices: Vec<DeviceInfo>,
    panes: Vec<Pane>,
    paused: bool,
    tune_mhz: f64,
    sweep_lo_mhz: f64,
    sweep_hi_mhz: f64,
    error: Option<String>,
    selected: Option<u64>,
}

/// Command line: `--sim` (simulator only), `--freq <MHz>` (focus centre),
/// `--serial <hex>` (focus device), `--no-sweep`, `--sweep <lo> <hi>` (MHz).
struct Args {
    sim: bool,
    freq_mhz: Option<f64>,
    serial: Option<String>,
    sweep: bool,
    sweep_range: Option<(f64, f64)>,
}

fn parse_args() -> Args {
    let mut a = Args { sim: false, freq_mhz: None, serial: None, sweep: true, sweep_range: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sim" => a.sim = true,
            "--freq" => a.freq_mhz = it.next().and_then(|v| v.parse().ok()),
            "--serial" => a.serial = it.next(),
            "--no-sweep" => a.sweep = false,
            "--sweep" => {
                let lo = it.next().and_then(|v| v.parse().ok());
                let hi = it.next().and_then(|v| v.parse().ok());
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    a.sweep_range = Some((lo, hi));
                }
            }
            _ => log::warn!("ignoring argument {arg}"),
        }
    }
    a
}

fn same_device(a: &DeviceInfo, b: &DeviceInfo) -> bool {
    a.backend == b.backend && a.serial == b.serial
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let args = parse_args();
        let (lo, hi) = args.sweep_range.unwrap_or((24.0, 1800.0));
        let mut app = Self {
            backends: vec![Box::new(backend_airspy::AirspyBackend::new()), Box::new(backend_sim::SimBackend)],
            devices: Vec::new(),
            panes: Vec::new(),
            paused: false,
            tune_mhz: args.freq_mhz.unwrap_or(if args.sim { 855.25 } else { 100.0 }),
            sweep_lo_mhz: lo,
            sweep_hi_mhz: hi,
            error: None,
            selected: None,
        };
        app.rescan();

        // Until the role assigner exists: R2s first (wideband antennas), the
        // simulator last. One device → Focus; two → Survey + Focus.
        let rank = |d: &DeviceInfo| match (d.backend.as_str(), d.label.as_str()) {
            ("sim", _) => 9,
            (_, "Airspy R2") => 0,
            (_, "Airspy Mini") => 5, // on the 1090 MHz antenna here
            _ => 1,
        };
        let mut hw: Vec<DeviceInfo> = app.devices.iter().filter(|d| d.backend != "sim").cloned().collect();
        hw.sort_by_key(|d| rank(d));
        hw.retain(|d| rank(d) < 5);
        let sim = app.devices.iter().find(|d| d.backend == "sim").cloned();
        let focus = match (&args.serial, args.sim) {
            (Some(s), _) => app.devices.iter().find(|d| d.serial.eq_ignore_ascii_case(s)).cloned(),
            (None, true) => sim.clone(),
            (None, false) => hw.first().cloned().or(sim.clone()),
        };
        if let Some(f) = &focus {
            app.open_as(f, Role::Focus);
        }
        if args.sweep && !args.sim {
            if let Some(s) = hw.iter().find(|d| focus.as_ref().is_none_or(|f| !same_device(d, f))).cloned() {
                app.open_as(&s, Role::Survey);
            }
        }
        app
    }

    fn rescan(&mut self) {
        self.devices.clear();
        for b in &self.backends {
            match b.enumerate() {
                Ok(mut v) => self.devices.append(&mut v),
                Err(e) => log::info!("backend {}: {e}", b.name()),
            }
        }
    }

    fn pane(&self, role: Role) -> Option<&Pane> {
        self.panes.iter().find(|p| p.role == role)
    }

    fn pane_mut(&mut self, role: Role) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.role == role)
    }

    fn sweep_plan(&self) -> SweepPlan {
        SweepPlan { start_hz: self.sweep_lo_mhz * 1e6, stop_hz: self.sweep_hi_mhz * 1e6, ..Default::default() }
    }

    fn close(&mut self, info: &DeviceInfo) {
        self.panes.retain(|p| !same_device(p.source.info(), info));
    }

    fn open_as(&mut self, info: &DeviceInfo, role: Role) {
        // A device fills one role; a role holds one device.
        self.close(info);
        self.panes.retain(|p| p.role != role);
        let Some(backend) = self.backends.iter().find(|b| b.name() == info.backend) else { return };
        let mode = match role {
            Role::Survey => Mode::Sweep(self.sweep_plan()),
            Role::Focus => Mode::Live,
        };
        let tune = self.tune_mhz * 1e6;
        let result = backend.open(&info.serial).and_then(|mut dev| {
            if role == Role::Focus {
                if let Err(e) = dev.set_center(tune) {
                    log::warn!("initial tune: {e}");
                }
            }
            Source::start(dev, SourceConfig::default(), mode)
        });
        match result {
            Ok(source) => {
                if role == Role::Focus {
                    self.tune_mhz = source.snapshot().center_hz / 1e6;
                }
                let view = if role == Role::Survey { survey_view() } else { SpectrumView::new() };
                // Matches the backend's opening default (Airspy linearity 13/21).
                self.panes.push(Pane { role, source, view, gain_mode: GainMode::Linearity, gain_step: 13 });
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", info.label)),
        }
    }

    fn retune_focus(&mut self, mhz: f64) {
        self.tune_mhz = mhz;
        if let Some(p) = self.pane_mut(Role::Focus) {
            if let Err(e) = p.source.set_center(mhz * 1e6) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(egui::RichText::new("SpecTrawl").strong().size(15.0));
            ui.add_space(8.0);
            let (txt, fg, bg) = if self.paused {
                ("PAUSED", theme::MUTED, theme::PANEL2)
            } else {
                ("● LIVE", egui::Color32::from_rgb(0xff, 0x8a, 0x8a), egui::Color32::from_rgb(0x2a, 0x12, 0x15))
            };
            egui::Frame::new().fill(bg).corner_radius(4).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
                ui.label(egui::RichText::new(txt).monospace().color(fg).size(11.0));
            });
            ui.label(egui::RichText::new(utc_now()).monospace().color(theme::TEXT2));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(if self.paused { "▶ Resume" } else { "⏸ Pause" }).clicked() {
                    self.paused = !self.paused;
                }
            });
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SOURCES").small().strong().color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Rescan").clicked() {
                    // Release open devices so they can be probed, then reopen.
                    let open: Vec<(DeviceInfo, Role)> = self.panes.iter().map(|p| (p.source.info().clone(), p.role)).collect();
                    self.panes.clear();
                    self.rescan();
                    for (d, r) in open {
                        self.open_as(&d, r);
                    }
                }
            });
        });

        enum Action {
            Open(DeviceInfo, Role),
            Close(DeviceInfo),
        }
        let mut action = None;
        for d in &self.devices {
            let role = self.panes.iter().find(|p| same_device(p.source.info(), d)).map(|p| p.role);
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&d.label).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match role {
                        Some(r) => {
                            let (bg, fg) = match r {
                                Role::Survey => (egui::Color32::from_rgb(0x0f, 0x2b, 0x2b), egui::Color32::from_rgb(0x7f, 0xe9, 0xe5)),
                                Role::Focus => (theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG),
                            };
                            theme::chip(ui, r.chip(), bg, fg);
                        }
                        None => {
                            if ui.small_button("Focus").clicked() {
                                action = Some(Action::Open(d.clone(), Role::Focus));
                            }
                            if ui.small_button("Survey").clicked() {
                                action = Some(Action::Open(d.clone(), Role::Survey));
                            }
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{} · {}", d.backend, d.serial)).small().color(theme::MUTED));
                    if role.is_some() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Close").clicked() {
                                action = Some(Action::Close(d.clone()));
                            }
                        });
                    }
                });
            });
        }
        match action {
            Some(Action::Open(d, r)) => self.open_as(&d, r),
            Some(Action::Close(d)) => self.close(&d),
            None => {}
        }

        // Borrow the pane field-wise so the closures can also touch other fields.
        if let Some(p) = self.panes.iter_mut().find(|p| p.role == Role::Survey) {
            let st = p.source.sweep_status();
            let stats = p.source.stats();
            let mut apply = false;
            let mut err = None;
            ui.add_space(8.0);
            ui.label(egui::RichText::new("SURVEY").small().strong().color(theme::MUTED));
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.sweep_lo_mhz).speed(1.0).range(24.0..=1790.0).suffix(" MHz"));
                    ui.label("to");
                    ui.add(egui::DragValue::new(&mut self.sweep_hi_mhz).speed(1.0).range(34.0..=1800.0).suffix(" MHz"));
                });
                apply = ui.button("Apply range").clicked();
                if let Some(e) = gain_ui(ui, p) {
                    err = Some(e);
                }
                if let Some(s) = &st {
                    kv(ui, "Hops", &format!("{} × {:.1} MHz", s.hops, s.hop_hz / 1e6));
                    kv(ui, "Rate", &format!("{:.0} hops/s · {:.0} MHz/s", s.hops_per_sec, s.mhz_per_sec()));
                    kv(ui, "Revisit", &format!("{:.2} s", s.last_pass_s));
                    kv(ui, "Re-done hops", &format!("{} · gaps {}", s.redone_hops, stats.gaps));
                }
            });
            if err.is_some() {
                self.error = err;
            }
            if apply {
                if self.sweep_hi_mhz <= self.sweep_lo_mhz + 1.0 {
                    self.sweep_hi_mhz = self.sweep_lo_mhz + 10.0;
                }
                let plan = self.sweep_plan();
                if let Some(p) = self.pane_mut(Role::Survey) {
                    p.source.set_mode(Mode::Sweep(plan));
                    p.view = survey_view();
                }
            }
        }

        if let Some(p) = self.panes.iter_mut().find(|p| p.role == Role::Focus) {
            let snap = p.source.snapshot();
            let st = p.source.stats();
            let cfg = p.source.config().clone();
            let mut tune = None;
            let mut err = None;
            ui.add_space(8.0);
            ui.label(egui::RichText::new("FOCUS A").small().strong().color(theme::MUTED));
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Centre");
                    let mut mhz = self.tune_mhz;
                    let r = ui.add(egui::DragValue::new(&mut mhz).speed(0.1).range(24.0..=1800.0).max_decimals(6).suffix(" MHz"));
                    if r.changed() {
                        tune = Some(mhz);
                    }
                });
                let rate = snap.sample_rate;
                kv(ui, "Rate", &format!("{:.1} MS/s · {}-bit", rate / 1e6, snap.caps.adc_bits));
                kv(ui, "FFT", &format!("{} pt · {:.0}/s · {:.0} Hz", cfg.fft_size, st.ffts_per_sec, rate / cfg.fft_size as f64));
                kv(ui, "Look-back", &format!("{:.1} / {:.0} s", st.ring_samples as f64 / rate, cfg.ring_seconds));
                kv(ui, "Gaps", &format!("{} ({} samples)", st.gaps, st.lost_samples));
                if let Some(e) = gain_ui(ui, p) {
                    err = Some(e);
                }
                ui.label(egui::RichText::new("Tip: click the survey to tune here").small().color(theme::MUTED));
            });
            if err.is_some() {
                self.error = err;
            }
            if let Some(m) = tune {
                self.retune_focus(m);
            }
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new("DISPLAY").small().strong().color(theme::MUTED));
        theme::card(ui, |ui| {
            ui.spacing_mut().slider_width = 110.0;
            for p in self.panes.iter_mut() {
                ui.label(egui::RichText::new(p.role.chip()).small().color(theme::MUTED));
                ui.add(egui::Slider::new(&mut p.view.db_max, -80.0..=0.0).text("top dB"));
                ui.add(egui::Slider::new(&mut p.view.db_min, -160.0..=-40.0).text("floor dB"));
                if ui.small_button("Auto range").clicked() {
                    p.view.auto_range();
                }
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x70, 0x96), e);
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        if !self.paused {
            for p in self.panes.iter_mut() {
                p.view.ingest(&p.source);
            }
        }
        let area = ui.available_rect_before_wrap();
        let has_survey = self.pane(Role::Survey).is_some();
        let has_focus = self.pane(Role::Focus).is_some();
        let (survey_rect, focus_rect) = match (has_survey, has_focus) {
            (true, true) => {
                let h = (area.height() * 0.38).round();
                let top = egui::Rect::from_min_size(area.min, egui::vec2(area.width(), h));
                let bottom = egui::Rect::from_min_max(egui::pos2(area.left(), top.bottom() + 6.0), area.max);
                (Some(top), Some(bottom))
            }
            (true, false) => (Some(area), None),
            (false, true) => (None, Some(area)),
            (false, false) => {
                ui.centered_and_justified(|ui| ui.label(egui::RichText::new("No sources open. Pick a device on the left.").color(theme::MUTED)));
                return;
            }
        };

        let focus_span = self.pane(Role::Focus).and_then(|p| p.view.span());
        let selected = self.selected;
        let mut tune_to = None;
        let mut select = None;
        if let (Some(r), Some(p)) = (survey_rect, self.pane_mut(Role::Survey)) {
            let st = p.source.sweep_status();
            let title = match &st {
                Some(s) => format!(
                    "SURVEY · {} · {:.0}–{:.0} MHz · {} hops · {:.0} MHz/s · revisit {:.1} s",
                    p.source.info().label,
                    s.start_hz / 1e6,
                    s.stop_hz() / 1e6,
                    s.hops,
                    s.mhz_per_sec(),
                    s.last_pass_s
                ),
                None => format!("SURVEY · {}", p.source.info().label),
            };
            let records = p.source.records_since(p.view.oldest_seq());
            let res = p.view.show(ui, r, &title, st.as_ref(), &records, selected);
            if let Some((lo, hi)) = focus_span {
                p.view.bracket(ui, r, lo, hi, "A");
            }
            // Survey click: tune the focus there; on a record, also select it
            // and centre on the record rather than the click point.
            if let Some(id) = res.record {
                select = Some(id);
                tune_to = records.iter().find(|r| r.id == id).map(|r| r.center_hz());
            } else {
                tune_to = res.freq_hz;
            }
        }
        if let (Some(r), Some(p)) = (focus_rect, self.pane_mut(Role::Focus)) {
            let snap = p.source.snapshot();
            let title = format!(
                "FOCUS A · {} · {:.3} MHz · {:.1} MS/s",
                snap.info.label,
                snap.center_hz / 1e6,
                snap.sample_rate / 1e6
            );
            let records = p.source.records_since(p.view.oldest_seq());
            let res = p.view.show(ui, r, &title, None, &records, selected);
            // Focus click: select a record if one was hit, otherwise recentre.
            match (res.record, res.freq_hz) {
                (Some(id), _) => select = Some(id),
                (None, Some(f)) => tune_to = Some(f),
                _ => {}
            }
        }
        if let Some(id) = select {
            self.selected = Some(id);
        }
        if let Some(f) = tune_to {
            self.retune_focus((f / 1e3).round() / 1e3);
        }
    }

    /// Newest records from all panes, most recent first.
    fn signal_list(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<(Role, engine::SignalRecord)> = Vec::new();
        for p in &self.panes {
            rows.extend(p.source.recent_records(300).into_iter().map(|r| (p.role, r)));
        }
        rows.sort_by(|a, b| b.1.active.cmp(&a.1.active).then(b.1.t_end.cmp(&a.1.t_end)));
        rows.truncate(300);
        let active = rows.iter().filter(|r| r.1.active).count();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SIGNALS").small().strong().color(theme::MUTED));
            ui.label(egui::RichText::new(format!("{} shown · {active} active", rows.len())).small().monospace().color(theme::MUTED));
            ui.label(egui::RichText::new("click a row to select · double-click to tune Focus A").small().color(theme::MUTED));
        });
        let mut tune = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("signals").striped(true).num_columns(10).spacing([14.0, 3.0]).show(ui, |ui| {
                for h in ["#", "Src", "Last seen", "MHz", "Class", "BW kHz", "Dur s", "SNR dB", "Peak dBFS", "State"] {
                    ui.label(egui::RichText::new(h).small().color(theme::MUTED));
                }
                ui.end_row();
                for (role, r) in &rows {
                    let sel = self.selected == Some(r.id);
                    let c = if sel { theme::SELECT } else { theme::TEXT2 };
                    let cell = |ui: &mut egui::Ui, s: String| ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(c)).sense(egui::Sense::click()));
                    let resps = [
                        cell(ui, format!("{}", r.id)),
                        cell(ui, match role { Role::Survey => "S".into(), Role::Focus => "A".into() }),
                        cell(ui, hms(r.t_end)),
                        cell(ui, format!("{:.4}", r.center_hz() / 1e6)),
                        {
                            let (txt, col) = match r.class.as_deref() {
                                Some(k) => (k.label.clone(), theme::class_color(Some(k)).unwrap_or(c)),
                                None => (if *role == Role::Survey { "-".into() } else { "…".into() }, theme::MUTED),
                            };
                            ui.add(egui::Label::new(egui::RichText::new(txt).monospace().color(if sel { theme::SELECT } else { col })).sense(egui::Sense::click()))
                        },
                        cell(ui, format!("{:.1}", r.bandwidth_hz() / 1e3)),
                        cell(ui, format!("{:.2}", r.duration_s())),
                        cell(ui, format!("{:.0} / {:.0}", r.mean_snr_db, r.max_snr_db)),
                        cell(ui, format!("{:.1}", r.peak_dbfs)),
                        cell(ui, if r.active { "active".into() } else { "done".into() }),
                    ];
                    if resps.iter().any(|x| x.clicked()) {
                        self.selected = Some(r.id);
                    }
                    if resps.iter().any(|x| x.double_clicked()) {
                        tune = Some(r.center_hz());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(f) = tune {
            self.retune_focus((f / 1e3).round() / 1e3);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top").exact_size(40.0).frame(theme::bar_frame()).show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").exact_size(24.0).frame(theme::bar_frame()).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let (mut ring_mb, mut gaps) = (0.0, 0);
                for p in &self.panes {
                    let st = p.source.stats();
                    ring_mb += st.ring_capacity as f64 * 4.0 / 1e6;
                    gaps += st.gaps;
                }
                ui.label(
                    egui::RichText::new(format!("{} sources · rings {:.0} MB · drops {}", self.panes.len(), ring_mb, gaps))
                        .monospace()
                        .small()
                        .color(theme::MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.0} fps", 1.0 / ctx.input(|i| i.stable_dt).max(1e-3)))
                            .monospace()
                            .small()
                            .color(theme::MUTED),
                    );
                });
            });
        });
        egui::Panel::bottom("signals")
            .resizable(true)
            .default_size(210.0)
            .frame(theme::side_frame())
            .show(ui, |ui| self.signal_list(ui));
        egui::Panel::left("side").exact_size(270.0).frame(theme::side_frame()).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| self.central(ui));
        ctx.request_repaint();
    }
}

fn survey_view() -> SpectrumView {
    let mut v = SpectrumView::new();
    // One row per pass (seconds apart): already averaged, and drawn thick.
    v.smoothing = 0.0;
    v.row_px = Some(3.0);
    v.subtle_boxes = true;
    v
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(k).color(theme::MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(v).monospace().color(theme::TEXT2));
        });
    });
}

/// Checks every few seconds for threads deadlocked on parking_lot locks and
/// logs their stacks (and writes them to spectrawl-deadlock.txt beside the
/// executable), so a frozen UI leaves evidence.
fn spawn_deadlock_watchdog() {
    std::thread::Builder::new()
        .name("deadlock-watchdog".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }
            let mut report = format!("{} deadlock(s) detected\n", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                for t in threads {
                    report += &format!("--- deadlock {i}, thread {:?}\n{:?}\n", t.thread_id(), t.backtrace());
                }
            }
            log::error!("{report}");
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::fs::write(exe.with_file_name("spectrawl-deadlock.txt"), &report);
            }
        })
        .expect("spawn watchdog");
}

fn hms(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn utc_now() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{} UTC", s / 3600, (s / 60) % 60, s % 60, t.subsec_millis() / 100)
}
