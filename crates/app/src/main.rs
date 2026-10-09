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
    /// Records for the boxes, refetched only when new rows arrive.
    records: (Option<u64>, std::time::Instant, std::sync::Arc<[engine::SignalRecord]>),
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
    selected_emitter: Option<(Role, u64)>,
    list_tab: ListTab,
    last_frame: std::time::Instant,
    /// Imported names (DSD+, trunk-recorder). Labels only: never replaces
    /// anything decoded.
    catalog: catalog::Catalog,
    catalog_msg: Option<(std::time::Instant, String)>,
    /// Running SigMF export (description, handle) and the last result.
    export: Option<(String, std::thread::JoinHandle<Result<engine::sigmf::ExportResult, String>>)>,
    export_msg: Option<String>,
    /// Emitter table, refreshed a few times a second rather than every frame.
    emitter_cache: (std::time::Instant, Vec<(Role, engine::Emitter)>),
}

/// Target frame interval (~60 fps).
const FRAME_TIME: std::time::Duration = std::time::Duration::from_micros(16_667);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListTab {
    Emitters,
    Records,
    P25,
}

/// Command line: `--sim` (simulator only), `--freq <MHz>` (focus centre),
/// `--serial <hex>` (focus device), `--no-sweep`, `--sweep <lo> <hi>` (MHz),
/// `--tab emitters|records|p25` (bottom panel).
struct Args {
    sim: bool,
    freq_mhz: Option<f64>,
    serial: Option<String>,
    sweep: bool,
    sweep_range: Option<(f64, f64)>,
    tab: Option<ListTab>,
}

fn parse_args() -> Args {
    let mut a = Args { sim: false, freq_mhz: None, serial: None, sweep: true, sweep_range: None, tab: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sim" => a.sim = true,
            "--freq" => a.freq_mhz = it.next().and_then(|v| v.parse().ok()),
            "--serial" => a.serial = it.next(),
            "--no-sweep" => a.sweep = false,
            "--tab" => {
                a.tab = match it.next().as_deref() {
                    Some("records") => Some(ListTab::Records),
                    Some("p25") => Some(ListTab::P25),
                    _ => Some(ListTab::Emitters),
                }
            }
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
            selected_emitter: None,
            list_tab: args.tab.unwrap_or(ListTab::Emitters),
            emitter_cache: (std::time::Instant::now() - std::time::Duration::from_secs(1), Vec::new()),
            last_frame: std::time::Instant::now(),
            catalog: catalog::Catalog::load(&catalog_path()),
            catalog_msg: None,
            export: None,
            export_msg: None,
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
                self.panes.push(Pane {
                    role,
                    source,
                    view,
                    records: (None, std::time::Instant::now(), Vec::new().into()),
                    gain_mode: GainMode::Linearity,
                    gain_step: 13,
                });
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
                ui.menu_button("Import names", |ui| self.import_menu(ui));
                if let Some((t, m)) = &self.catalog_msg {
                    if t.elapsed() < std::time::Duration::from_secs(12) {
                        ui.label(egui::RichText::new(m).small().color(theme::TEXT2));
                    }
                }
            });
        });
    }

    /// Talkgroup/radio/site names from other tools. Shown beside decoded
    /// values, never instead of them (imported files can be out of date).
    fn import_menu(&mut self, ui: &mut egui::Ui) {
        let mut result: Option<Result<String, String>> = None;
        if ui.button("DSD+ folder…").on_hover_text("Reads DSDPlus.groups, .radios, .sites and .networks").clicked() {
            if let Some(dir) = rfd::FileDialog::new().set_title("DSD+ folder").pick_folder() {
                result = Some(self.catalog.import_dsdplus_dir(&dir).map(|s| format!("DSD+: {s}")).map_err(|e| e.to_string()));
            }
        }
        if ui
            .button("trunk-recorder config.json…")
            .on_hover_text("Systems, control channels, and the talkgroup/unit files they reference")
            .clicked()
        {
            if let Some(f) = rfd::FileDialog::new().add_filter("trunk-recorder config", &["json"]).pick_file() {
                result = Some(self.catalog.import_tr_config(&f).map(|s| format!("trunk-recorder: {s}")).map_err(|e| e.to_string()));
            }
        }
        if ui.button("Talkgroup or unit CSV…").on_hover_text("trunk-recorder / RadioReference-style talkgroup CSV, or unit tags").clicked()
        {
            if let Some(f) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() {
                result = Some(self.catalog.import_csv(&f).map(|s| format!("CSV: {s}")).map_err(|e| e.to_string()));
            }
        }
        ui.separator();
        if self.catalog.sources.is_empty() {
            ui.label(egui::RichText::new("Nothing imported yet").small().color(theme::MUTED));
        } else {
            for s in &self.catalog.sources {
                ui.label(egui::RichText::new(s).small().color(theme::MUTED));
            }
            if ui.button("Clear imported names").clicked() {
                self.catalog = catalog::Catalog::default();
                result = Some(Ok("Imported names cleared".into()));
            }
        }
        if let Some(r) = result {
            let msg = match r {
                Ok(m) => match self.catalog.save(&catalog_path()) {
                    Ok(()) => m,
                    Err(e) => format!("{m} (not saved: {e})"),
                },
                Err(e) => format!("Import failed: {e}"),
            };
            self.catalog_msg = Some((std::time::Instant::now(), msg));
            ui.close();
        }
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
                    kv(ui, "Re-measured", &format!("{} settle · {} gap", s.settle_retries, s.redone_hops));
                    kv(ui, "Slow hops", &format!("{} of {}", s.slow_hops, s.hops));
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
            let load = p.source.classifier_load();
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
                kv(ui, "Classifier", &format!("{:.1}/s · {:.0} ms · {:.0}%", load.jobs_per_sec, load.avg_job_ms, load.busy * 100.0));
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

        self.selection_card(ui);
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
        for p in self.panes.iter_mut() {
            p.view.paused = self.paused;
            p.view.markers = p.source.monitors().iter().map(|m| (m.center_hz, m.label.clone())).collect();
        }
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
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("No sources open. Pick a device on the left.").color(theme::MUTED))
                });
                return;
            }
        };

        let focus_span = self.pane(Role::Focus).and_then(|p| p.view.span());
        let selected = self.selected;
        let sel_em = |role: Role| self.selected_emitter.filter(|(r, _)| *r == role).map(|(_, id)| id);
        let (sel_survey, sel_focus) = (sel_em(Role::Survey), sel_em(Role::Focus));
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
            let records = survey_outside_focus(pane_records(p), focus_span);
            let res = p.view.show(ui, r, &title, st.as_ref(), &records, selected, sel_survey);
            if let Some((lo, hi)) = focus_span {
                p.view.bracket(ui, r, lo, hi, "A");
            }
            // Survey click: tune the focus there; on a record, also select it
            // and centre on the record rather than the click point.
            if let Some(id) = res.record {
                select = Some((id, res.emitter.map(|e| (Role::Survey, e))));
                tune_to = records.iter().find(|r| r.id == id).map(|r| r.center_hz());
            } else {
                tune_to = res.freq_hz;
            }
        }
        if let (Some(r), Some(p)) = (focus_rect, self.pane_mut(Role::Focus)) {
            let snap = p.source.snapshot();
            let title = format!("FOCUS A · {} · {:.3} MHz · {:.1} MS/s", snap.info.label, snap.center_hz / 1e6, snap.sample_rate / 1e6);
            let records = pane_records(p);
            let res = p.view.show(ui, r, &title, None, &records, selected, sel_focus);
            // Focus click: select a record if one was hit, otherwise recentre.
            match (res.record, res.freq_hz) {
                (Some(id), _) => select = Some((id, res.emitter.map(|e| (Role::Focus, e)))),
                (None, Some(f)) => tune_to = Some(f),
                _ => {}
            }
        }
        if let Some((id, em)) = select {
            self.selected = Some(id);
            self.selected_emitter = em;
        }
        if let Some(f) = tune_to {
            self.retune_focus((f / 1e3).round() / 1e3);
        }
    }

    /// What's selected, and SigMF export for it.
    fn selection_card(&mut self, ui: &mut egui::Ui) {
        // Poll a running export.
        if self.export.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            let (what, h) = self.export.take().unwrap();
            self.export_msg = Some(match h.join() {
                Ok(Ok(r)) => format!(
                    "Saved {what}: {:.1} s, {:.1} MB\n{}",
                    r.seconds,
                    r.bytes as f64 / 1e6,
                    r.data_path.with_extension("").display()
                ),
                Ok(Err(e)) => format!("Export failed: {e}"),
                Err(_) => "Export crashed".into(),
            });
        }
        let Some(focus) = self.panes.iter().find(|p| p.role == Role::Focus) else { return };
        let records: Vec<engine::SignalRecord> = focus.records.2.iter().cloned().collect();
        let record = self.selected.and_then(|id| {
            records.iter().find(|r| r.id == id).cloned().or_else(|| focus.source.recent_records(3000).into_iter().find(|r| r.id == id))
        });
        let emitter = self
            .selected_emitter
            .filter(|(role, _)| *role == Role::Focus)
            .and_then(|(_, id)| focus.source.emitters().into_iter().find(|e| e.id == id));
        let busy = self.export.is_some();
        let mut req: Option<(String, engine::sigmf::ExportRequest)> = None;
        let annotate = |rs: &[engine::SignalRecord]| -> Vec<engine::sigmf::ExportAnnotation> {
            rs.iter()
                .map(|r| {
                    let (s, e) = focus.source.record_sample_range(r);
                    let label = r.display_class().map(|c| c.label.clone()).unwrap_or_else(|| format!("#{}", r.id));
                    engine::sigmf::ExportAnnotation {
                        start_sample: s,
                        end_sample: e,
                        f_lo_hz: r.f_lo_hz,
                        f_hi_hz: r.f_hi_hz,
                        label,
                        comment: format!("SpecTrawl record #{} · SNR {:.0} dB · peak {:.1} dBFS", r.id, r.max_snr_db, r.peak_dbfs),
                    }
                })
                .collect()
        };
        let dir = engine::sigmf::default_dir();
        ui.add_space(8.0);
        ui.label(egui::RichText::new("SELECTION").small().strong().color(theme::MUTED));
        theme::card(ui, |ui| {
            if let Some(r) = &record {
                let label = r.display_class().map(|c| c.label.clone()).unwrap_or_else(|| "unclassified".into());
                ui.label(egui::RichText::new(format!("Record #{} · {}", r.id, label)).strong());
                ui.label(
                    egui::RichText::new(format!(
                        "{:.4} MHz · {:.1} kHz · {:.2} s",
                        r.center_hz() / 1e6,
                        r.bandwidth_hz() / 1e3,
                        r.duration_s()
                    ))
                    .monospace()
                    .color(theme::TEXT2),
                );
                let stem = |kind: &str| {
                    engine::sigmf::sanitize(&format!("{}_{:.4}MHz_{}_{kind}", engine::sigmf::stamp(r.t_start), r.center_hz() / 1e6, label))
                };
                let (s, e) = focus.source.record_sample_range(r);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::new("Export channel"))
                        .on_hover_text("Down-converted to this signal only (cf32, ~50 kS/s)")
                        .clicked()
                    {
                        req = Some((
                            format!("record #{} channel", r.id),
                            engine::sigmf::ExportRequest {
                                start_sample: s,
                                end_sample: e,
                                kind: engine::sigmf::ExportKind::Channel { center_hz: r.center_hz(), bandwidth_hz: r.bandwidth_hz() },
                                dir: dir.clone(),
                                stem: stem("ch"),
                                description: format!("SpecTrawl record #{} ({label}), channel", r.id),
                                annotations: annotate(std::slice::from_ref(r)),
                            },
                        ));
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Full band"))
                        .on_hover_text("Raw 10 MHz capture for the record's time span (ci16)")
                        .clicked()
                    {
                        let overlapping: Vec<engine::SignalRecord> =
                            records.iter().filter(|o| o.last_row_end_sample >= s && o.first_row_end_sample <= e).cloned().collect();
                        req = Some((
                            format!("record #{} full band", r.id),
                            engine::sigmf::ExportRequest {
                                start_sample: s,
                                end_sample: e,
                                kind: engine::sigmf::ExportKind::Wideband,
                                dir: dir.clone(),
                                stem: stem("wide"),
                                description: format!("SpecTrawl record #{} ({label}), full band", r.id),
                                annotations: annotate(&overlapping),
                            },
                        ));
                    }
                });
            } else if let Some(em) = &emitter {
                let label = em.label().unwrap_or_else(|| "unclassified".into());
                ui.label(egui::RichText::new(format!("Emitter E{} · {}", em.id, label)).strong());
                ui.label(
                    egui::RichText::new(format!("{:.4} MHz · {} bursts", em.center_hz / 1e6, em.records)).monospace().color(theme::TEXT2),
                );
                if ui
                    .add_enabled(!busy, egui::Button::new("Export channel (look-back)"))
                    .on_hover_text("This emitter's channel over the whole look-back buffer")
                    .clicked()
                {
                    let (s, e) = focus.source.ring_range();
                    let mine: Vec<engine::SignalRecord> = records.iter().filter(|r| r.emitter == Some(em.id)).cloned().collect();
                    req = Some((
                        format!("emitter E{} channel", em.id),
                        engine::sigmf::ExportRequest {
                            start_sample: s,
                            end_sample: e,
                            kind: engine::sigmf::ExportKind::Channel { center_hz: em.center_hz, bandwidth_hz: em.bandwidth_hz },
                            dir: dir.clone(),
                            stem: engine::sigmf::sanitize(&format!(
                                "{}_{:.4}MHz_{}_E{}_ch",
                                engine::sigmf::stamp(std::time::SystemTime::now()),
                                em.center_hz / 1e6,
                                label,
                                em.id
                            )),
                            description: format!("SpecTrawl emitter E{} ({label}), channel", em.id),
                            annotations: annotate(&mine),
                        },
                    ));
                }
            } else {
                ui.label(egui::RichText::new("Click a box or a list row (Focus A) to select it.").small().color(theme::MUTED));
            }
            ui.separator();
            if ui
                .add_enabled(!busy, egui::Button::new("Export look-back (full band)"))
                .on_hover_text("Everything in the Focus A look-back buffer (~10 s, ~400 MB)")
                .clicked()
            {
                let (s, e) = focus.source.ring_range();
                let snap = focus.source.snapshot();
                req = Some((
                    "look-back".into(),
                    engine::sigmf::ExportRequest {
                        start_sample: s,
                        end_sample: e,
                        kind: engine::sigmf::ExportKind::Wideband,
                        dir: dir.clone(),
                        stem: engine::sigmf::sanitize(&format!(
                            "{}_{:.3}MHz_lookback_wide",
                            engine::sigmf::stamp(std::time::SystemTime::now()),
                            snap.center_hz / 1e6
                        )),
                        description: "SpecTrawl look-back buffer, full band".into(),
                        annotations: annotate(&records),
                    },
                ));
            }
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("exporting…").small().color(theme::MUTED));
                });
            } else if let Some(m) = &self.export_msg {
                ui.label(egui::RichText::new(m).small().color(theme::TEXT2));
            }
            ui.label(egui::RichText::new(format!("Saves to {}", dir.display())).small().color(theme::MUTED));
        });
        if let Some((what, r)) = req {
            let h = focus.source.export(r);
            self.export = Some((what, h));
            self.export_msg = None;
        }
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.list_tab, ListTab::Emitters, "Emitters");
            ui.selectable_value(&mut self.list_tab, ListTab::Records, "Records");
            ui.selectable_value(&mut self.list_tab, ListTab::P25, "P25");
            ui.label(egui::RichText::new("click to select · double-click to tune Focus A").small().color(theme::MUTED));
        });
        match self.list_tab {
            ListTab::Emitters => self.emitter_list(ui),
            ListTab::Records => self.signal_list(ui),
            ListTab::P25 => self.p25_panel(ui),
        }
    }

    /// Emitters from all panes: active first, then most recently seen.
    /// Virtualised: only rows on screen are laid out.
    fn emitter_list(&mut self, ui: &mut egui::Ui) {
        if self.emitter_cache.0.elapsed() > std::time::Duration::from_millis(300) {
            let mut rows: Vec<(Role, engine::Emitter)> = Vec::new();
            for p in &self.panes {
                let focus_span = self.panes.iter().find(|q| q.role == Role::Focus).and_then(|q| q.view.span());
                rows.extend(
                    p.source
                        .emitters()
                        .into_iter()
                        .filter(|e| p.role != Role::Survey || !in_focus_zone(e.center_hz, focus_span))
                        .map(|e| (p.role, e)),
                );
            }
            rows.sort_by(|a, b| b.1.active.cmp(&a.1.active).then(b.1.last_seen.cmp(&a.1.last_seen)));
            self.emitter_cache = (std::time::Instant::now(), rows);
        }
        let rows = std::mem::take(&mut self.emitter_cache.1);
        let active = rows.iter().filter(|r| r.1.active).count();
        let identified = rows.iter().filter(|r| r.1.has_protocol()).count();
        ui.label(
            egui::RichText::new(format!("{} emitters · {active} active · {identified} identified", rows.len()))
                .small()
                .monospace()
                .color(theme::MUTED),
        );
        let mut clicked = None;
        let mut tune = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("emitters")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(14.0))
            .column(egui_extras::Column::exact(28.0))
            .column(egui_extras::Column::exact(80.0))
            .column(egui_extras::Column::exact(150.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(220.0))
            .columns(egui_extras::Column::exact(64.0), 3)
            .columns(egui_extras::Column::exact(72.0), 2)
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["", "Src", "MHz", "Class", "ID", "Also seen", "BW kHz", "Bursts", "Duty %", "First", "Last", "SNR dB"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let (role, e) = &rows[row.index()];
                    let sel = self.selected_emitter == Some((*role, e.id));
                    row.set_selected(sel);
                    let c = if sel { theme::SELECT } else { theme::TEXT2 };
                    let text = |ui: &mut egui::Ui, s: String, col: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(col)).selectable(false).truncate());
                    };
                    let label = e.label().unwrap_or_else(|| if *role == Role::Survey { "-".into() } else { "…".into() });
                    let class_col = if sel { theme::SELECT } else { theme::class_color(e.best.as_deref()).unwrap_or(theme::MUTED) };
                    let mut others: Vec<(&String, &u32)> = e.tally.iter().filter(|(l, _)| **l != label).collect();
                    others.sort_by(|a, b| b.1.cmp(a.1));
                    let also: Vec<String> = others.iter().take(2).map(|(l, n)| format!("{l}×{n}")).collect();
                    row.col(|ui| text(ui, if e.active { "●".into() } else { String::new() }, theme::ACCENT));
                    row.col(|ui| text(ui, if *role == Role::Survey { "S".into() } else { "A".into() }, c));
                    row.col(|ui| text(ui, format!("{:.4}", e.center_hz / 1e6), c));
                    row.col(|ui| text(ui, label, class_col));
                    row.col(|ui| text(ui, e.id().unwrap_or("").to_string(), c));
                    row.col(|ui| text(ui, also.join(", "), theme::MUTED));
                    row.col(|ui| text(ui, format!("{:.1}", e.bandwidth_hz / 1e3), c));
                    row.col(|ui| text(ui, e.records.to_string(), c));
                    row.col(|ui| text(ui, format!("{:.0}", e.duty() * 100.0), c));
                    row.col(|ui| text(ui, hms(e.first_seen), c));
                    row.col(|ui| text(ui, hms(e.last_seen), c));
                    row.col(|ui| text(ui, format!("{:.0}", e.peak_snr_db), c));
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some((*role, e.id));
                    }
                    if resp.double_clicked() {
                        tune = Some(e.center_hz);
                    }
                });
            });
        self.emitter_cache.1 = rows;
        if let Some(sel) = clicked {
            self.selected_emitter = Some(sel);
            self.selected = None;
        }
        if let Some(f) = tune {
            self.retune_focus((f / 1e3).round() / 1e3);
        }
    }

    /// P25 systems and talkgroup grants learned by the control-channel monitors.
    fn p25_panel(&mut self, ui: &mut egui::Ui) {
        let Some(focus) = self.panes.iter().find(|p| p.role == Role::Focus) else {
            ui.label(egui::RichText::new("Open a Focus device to decode P25 control channels.").color(theme::MUTED));
            return;
        };
        let systems = focus.source.p25_systems();
        let grants = focus.source.p25_grants(2000);
        let monitors = focus.source.monitors();
        if systems.is_empty() {
            ui.label(
                egui::RichText::new(if monitors.is_empty() {
                    "No P25 control channel identified in this span yet. Monitors start automatically once one is."
                } else {
                    "Monitoring a control channel; waiting for its first messages…"
                })
                .color(theme::MUTED),
            );
            return;
        }
        let p25 = egui::Color32::from_rgb(0xf2, 0xa3, 0x3a);
        let hex = |v: Option<u32>, w: usize| v.map(|v| format!("0x{v:0w$X}")).unwrap_or("?".into());
        let cat = &self.catalog;
        let site_name = |s: &engine::P25System, rfss: u8, site: u8| match (s.wacn, s.system) {
            (Some(w), Some(y)) => cat.p25_site(w, y, rfss, site).map(str::to_string),
            _ => None,
        };
        for (nac, s) in &systems {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("P25").strong().color(p25));
                if let (Some(w), Some(y)) = (s.wacn, s.system) {
                    if let Some(n) = cat.p25_network(w, y) {
                        ui.label(egui::RichText::new(n).strong().color(theme::TEXT));
                    }
                }
                let mut parts = vec![
                    format!("NAC 0x{nac:03X}"),
                    format!("WACN {}", hex(s.wacn, 5)),
                    format!("SYS {}", hex(s.system.map(u32::from), 3)),
                    format!(
                        "RFSS {} · SITE {}",
                        s.rfss.map(|v| v.to_string()).unwrap_or("?".into()),
                        s.site.map(|v| v.to_string()).unwrap_or("?".into())
                    ),
                ];
                if let Some(cc) = s.control_hz {
                    parts.push(format!("CC {:.5} MHz", cc / 1e6));
                }
                ui.label(egui::RichText::new(parts.join(" · ")).monospace().color(theme::TEXT));
                if let (Some(r), Some(t)) = (s.rfss, s.site) {
                    if let Some(n) = site_name(s, r, t) {
                        ui.label(egui::RichText::new(format!("“{n}”")).color(theme::TEXT2));
                    }
                }
                let adj: Vec<String> = s
                    .adjacent
                    .iter()
                    .map(|((_, rfss, site), ch)| {
                        let f = s.freq(*ch).map(|f| format!("{:.4}", f / 1e6)).unwrap_or(ch.to_string());
                        match site_name(s, *rfss, *site) {
                            Some(n) => format!("{rfss}-{site} {n} {f}"),
                            None => format!("{rfss}-{site} {f}"),
                        }
                    })
                    .collect();
                if !adj.is_empty() {
                    ui.label(egui::RichText::new(format!("adjacent: {}", adj.join(", "))).small().color(theme::MUTED));
                }
            });
        }
        let by_nac: std::collections::HashMap<u16, &engine::P25System> = systems.iter().map(|(n, s)| (*n, s)).collect();
        let mut tune = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("p25grants")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(80.0))
            .column(egui_extras::Column::exact(300.0))
            .column(egui_extras::Column::exact(110.0))
            .column(egui_extras::Column::exact(44.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(160.0))
            .column(egui_extras::Column::exact(44.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in
                    ["Last", "First", "Talkgroup", "Name (imported)", "Voice MHz", "Slot", "Radio ID", "Radio (imported)", "×", "System"]
                {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, grants.len(), |mut row| {
                    let g = &grants[row.index()];
                    let text = |ui: &mut egui::Ui, s: String, c: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(c)).selectable(false).truncate());
                    };
                    let recent = std::time::SystemTime::now().duration_since(g.last).map(|d| d.as_secs() < 5).unwrap_or(true);
                    let c = if recent { theme::TEXT } else { theme::TEXT2 };
                    row.col(|ui| text(ui, hms(g.last), c));
                    row.col(|ui| text(ui, hms(g.first), theme::MUTED));
                    let sys = by_nac.get(&g.nac);
                    let (w, y, cc) = (sys.and_then(|s| s.wacn), sys.and_then(|s| s.system), sys.and_then(|s| s.control_hz));
                    let tg_name = cat.p25_group(w, y, cc, g.talkgroup).map(|i| i.name().to_string()).unwrap_or_default();
                    let radio_name = g.source.and_then(|r| cat.p25_radio(w, y, cc, r)).unwrap_or("").to_string();
                    row.col(|ui| text(ui, g.talkgroup.to_string(), if recent { p25 } else { c }));
                    row.col(|ui| text(ui, tg_name, theme::TEXT2));
                    row.col(|ui| text(ui, g.freq_hz.map(|f| format!("{:.5}", f / 1e6)).unwrap_or("?".into()), c));
                    row.col(|ui| text(ui, g.slot.map(|s| s.to_string()).unwrap_or_default(), c));
                    row.col(|ui| text(ui, g.source.map(|s| s.to_string()).unwrap_or_default(), c));
                    row.col(|ui| text(ui, radio_name, theme::TEXT2));
                    row.col(|ui| text(ui, g.count.to_string(), theme::MUTED));
                    row.col(|ui| text(ui, format!("NAC 0x{:03X}", g.nac), theme::MUTED));
                    if row.response().double_clicked() {
                        tune = g.freq_hz;
                    }
                });
            });
        if let Some(f) = tune {
            self.retune_focus((f / 1e3).round() / 1e3);
        }
    }

    /// Newest records from all panes, most recent first. Virtualised.
    fn signal_list(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<(Role, engine::SignalRecord)> = Vec::new();
        for p in &self.panes {
            rows.extend(p.source.recent_records(500).into_iter().map(|r| (p.role, r)));
        }
        rows.sort_by(|a, b| b.1.active.cmp(&a.1.active).then(b.1.t_end.cmp(&a.1.t_end)));
        let active = rows.iter().filter(|r| r.1.active).count();
        ui.label(egui::RichText::new(format!("{} records shown · {active} active", rows.len())).small().monospace().color(theme::MUTED));
        let mut clicked = None;
        let mut tune = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("records")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(56.0))
            .column(egui_extras::Column::exact(28.0))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(80.0))
            .column(egui_extras::Column::exact(150.0))
            .columns(egui_extras::Column::exact(64.0), 2)
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(80.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["#", "Src", "Last seen", "MHz", "Class", "BW kHz", "Dur s", "SNR dB", "Peak dBFS", "State"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let (role, r) = &rows[row.index()];
                    let sel = self.selected == Some(r.id);
                    row.set_selected(sel);
                    let c = if sel { theme::SELECT } else { theme::TEXT2 };
                    let text = |ui: &mut egui::Ui, s: String, col: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(col)).selectable(false).truncate());
                    };
                    let (class, class_col) = match r.display_class() {
                        Some(k) => (k.label.clone(), if sel { theme::SELECT } else { theme::class_color(Some(k)).unwrap_or(c) }),
                        None => (if *role == Role::Survey { "-".into() } else { "…".into() }, theme::MUTED),
                    };
                    row.col(|ui| text(ui, r.id.to_string(), c));
                    row.col(|ui| text(ui, if *role == Role::Survey { "S".into() } else { "A".into() }, c));
                    row.col(|ui| text(ui, hms(r.t_end), c));
                    row.col(|ui| text(ui, format!("{:.4}", r.center_hz() / 1e6), c));
                    row.col(|ui| text(ui, class, class_col));
                    row.col(|ui| text(ui, format!("{:.1}", r.bandwidth_hz() / 1e3), c));
                    row.col(|ui| text(ui, format!("{:.2}", r.duration_s()), c));
                    row.col(|ui| text(ui, format!("{:.0} / {:.0}", r.mean_snr_db, r.max_snr_db), c));
                    row.col(|ui| text(ui, format!("{:.1}", r.peak_dbfs), c));
                    row.col(|ui| text(ui, if r.active { "active".into() } else { "done".into() }, c));
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some((r.id, r.emitter.map(|e| (*role, e))));
                    }
                    if resp.double_clicked() {
                        tune = Some(r.center_hz());
                    }
                });
            });
        if let Some((id, em)) = clicked {
            self.selected = Some(id);
            self.selected_emitter = em;
        }
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
        egui::Panel::bottom("signals").resizable(true).default_size(210.0).frame(theme::side_frame()).show(ui, |ui| self.bottom_panel(ui));
        egui::Panel::left("side").exact_size(270.0).frame(theme::side_frame()).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| self.central(ui));
        // ~60 fps is plenty for a waterfall; uncapped it ran at the monitor's
        // rate (165 Hz here) and cost a full core. egui repaints at once when
        // the requested delay is under its frame time, so pace frames here.
        let since = self.last_frame.elapsed();
        if since < FRAME_TIME {
            std::thread::sleep(FRAME_TIME - since);
        }
        self.last_frame = std::time::Instant::now();
        ctx.request_repaint();
    }
}

/// The pane's records for drawing, refetched when the view has new rows (or
/// every half second, so classifications show up) instead of every frame.
fn pane_records(p: &mut Pane) -> std::sync::Arc<[engine::SignalRecord]> {
    let newest = p.view.newest_seq();
    if p.records.0 != newest || p.records.1.elapsed() > std::time::Duration::from_millis(500) {
        p.records = (newest, std::time::Instant::now(), p.source.records_since(p.view.oldest_seq()).into());
    }
    p.records.2.clone()
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
        .spawn(|| {
            loop {
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
            }
        })
        .expect("spawn watchdog");
}

/// Inside the Focus span the focus device is the better witness: it listens
/// continuously with no hop artifacts, and the survey device hears the focus
/// R2's own local-oscillator leakage there (at the top edge, centre + 5 MHz,
/// plus weaker spurs) - confirmed by swapping the two R2s' roles. So survey
/// detections in that zone aren't drawn or listed.
const FOCUS_ZONE_MARGIN_HZ: f64 = 0.5e6;

fn in_focus_zone(f: f64, focus_span: Option<(f64, f64)>) -> bool {
    focus_span.is_some_and(|(lo, hi)| f > lo - FOCUS_ZONE_MARGIN_HZ && f < hi + FOCUS_ZONE_MARGIN_HZ)
}

fn survey_outside_focus(
    records: std::sync::Arc<[engine::SignalRecord]>,
    focus_span: Option<(f64, f64)>,
) -> std::sync::Arc<[engine::SignalRecord]> {
    if focus_span.is_none() {
        return records;
    }
    records.iter().filter(|r| !in_focus_zone(r.center_hz(), focus_span)).cloned().collect()
}

/// `<Documents>/SpecTrawl/catalog.json`
fn catalog_path() -> std::path::PathBuf {
    engine::sigmf::default_dir().parent().map(|p| p.join("catalog.json")).unwrap_or_else(|| "catalog.json".into())
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
