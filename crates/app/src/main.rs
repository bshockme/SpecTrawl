mod audio;
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

/// A receiver's letter, the handle for it everywhere in the UI: `S` is the
/// sweeper, `A`, `B`, ... are live receivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ReceiverId(char);

impl ReceiverId {
    const SWEEP: ReceiverId = ReceiverId('S');

    fn is_sweep(self) -> bool {
        self == Self::SWEEP
    }

    fn chip(self) -> String {
        if self.is_sweep() { "SURVEY".into() } else { format!("FOCUS {}", self.0) }
    }

    /// The nth live letter: A, B, C, ... (skipping S).
    fn live(n: usize) -> ReceiverId {
        ReceiverId(('A'..='Z').filter(|c| *c != 'S').nth(n).unwrap_or('Z'))
    }
}

impl std::fmt::Display for ReceiverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the user is looking at: drives the inspector, listen target and
/// export buttons.
#[derive(Debug, Clone, PartialEq, Default)]
enum Selection {
    #[default]
    None,
    Receiver(ReceiverId),
    Record(ReceiverId, u64),
    Emitter(ReceiverId, u64),
    Talkgroup { system: Option<engine::SystemKey>, tg: u32 },
    Radio { system: Option<engine::SystemKey>, id: u32 },
    Capcode { protocol: String, capcode: u64 },
}

impl Selection {
    /// The receiver the selection belongs to.
    fn receiver(&self) -> Option<ReceiverId> {
        match self {
            Self::Receiver(r) | Self::Record(r, _) | Self::Emitter(r, _) => Some(*r),
            _ => None,
        }
    }

    fn record(&self) -> Option<(ReceiverId, u64)> {
        match self {
            Self::Record(r, id) => Some((*r, *id)),
            _ => None,
        }
    }

    fn emitter(&self) -> Option<(ReceiverId, u64)> {
        match self {
            Self::Emitter(r, id) => Some((*r, *id)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GainMode {
    Linearity,
    Sensitivity,
    Agc,
}

struct Receiver {
    id: ReceiverId,
    source: Source,
    view: SpectrumView,
    /// Records for the boxes, refetched only when new rows arrive.
    records: (Option<u64>, std::time::Instant, std::sync::Arc<[engine::SignalRecord]>),
    gain_mode: GainMode,
    /// Preset step 0..=21 (Airspy's linearity/sensitivity tables).
    gain_step: u8,
}

impl Receiver {
    fn gain_setting(&self) -> GainSetting {
        let v = self.gain_step as f32 / 21.0;
        match self.gain_mode {
            GainMode::Linearity => GainSetting::Linearity(v),
            GainMode::Sensitivity => GainSetting::Sensitivity(v),
            GainMode::Agc => GainSetting::Auto,
        }
    }
}

/// Gain controls for one receiver; applies changes to the device.
fn gain_ui(ui: &mut egui::Ui, p: &mut Receiver) -> Option<String> {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Gain");
        for (m, name) in [(GainMode::Linearity, "Linearity"), (GainMode::Sensitivity, "Sensitivity"), (GainMode::Agc, "AGC")] {
            changed |= ui.selectable_value(&mut p.gain_mode, m, name).changed();
        }
    });
    if p.gain_mode != GainMode::Agc {
        changed |= ui.add(egui::Slider::new(&mut p.gain_step, 0..=21).text("step")).changed();
    } else if p.id.is_sweep() {
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
    panes: Vec<Receiver>,
    paused: bool,
    /// Where survey clicks and table double-clicks tune (keys 1/2/3 pick it).
    tune_target: ReceiverId,
    tune_mhz: f64,
    sweep_lo_mhz: f64,
    sweep_hi_mhz: f64,
    error: Option<String>,
    selection: Selection,
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
    /// Also holds how many emitters the view leaves out as unconfirmed.
    emitter_cache: (std::time::Instant, Vec<(ReceiverId, engine::Emitter)>, usize),
    /// Emitters the view leaves out as known occupants (squelch on).
    emitter_squelched: usize,
    emitter_view: EmitterView,
    /// Spectrum squelch: hide and dim known wideband occupants.
    squelch: bool,
    /// Audio output (opened on first listen), its error, and listen settings.
    audio_out: Option<audio::AudioOut>,
    audio_err: Option<String>,
    /// A recording being played (takes the audio output while it plays).
    playback: Option<engine::Playback>,
    /// Talkgroups following prefers or ignores (saved in follow.txt), and
    /// (generation, panes) last given to the sources.
    follow_policy: engine::FollowPolicy,
    policy_gen: (u64, u64, usize),
    /// None = pick the mode from the signal's label.
    listen_mode: Option<engine::DemodMode>,
    /// P25: Phase 2 TDMA (picked from the label when the mode is Auto).
    listen_tdma: bool,
    listen_mhz: f64,
    /// Pages tab filter: capcode or text.
    page_filter: String,
    /// Calls tab filter: talkgroup, radio or name.
    call_filter: String,
    volume: f32,
    /// Known occupants from every pane, merged: (lo Hz, hi Hz, label).
    known: (std::time::Instant, Vec<(f64, f64, String)>),
}

/// What the emitter table shows and in what order.
struct EmitterView {
    sort: EmitterSort,
    descending: bool,
    /// None = every receiver.
    source: Option<ReceiverId>,
    /// Also list emitters that might still be noise (seen once, weak, briefly).
    unconfirmed: bool,
}

impl Default for EmitterView {
    fn default() -> Self {
        // The survey's coarse bins can't separate channels and its emitters
        // run to hundreds; it marks activity on its waterfall instead.
        Self { sort: EmitterSort::Freq, descending: false, source: Some(ReceiverId::live(0)), unconfirmed: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitterSort {
    Freq,
    Class,
    Bandwidth,
    Bursts,
    Duty,
    First,
    Last,
    Snr,
}

impl EmitterSort {
    fn cmp(self, a: &engine::Emitter, b: &engine::Emitter) -> std::cmp::Ordering {
        match self {
            Self::Freq => a.center_hz.total_cmp(&b.center_hz),
            Self::Class => a.label().cmp(&b.label()),
            Self::Bandwidth => a.bandwidth_hz.total_cmp(&b.bandwidth_hz),
            Self::Bursts => a.records.cmp(&b.records),
            Self::Duty => a.duty().total_cmp(&b.duty()),
            Self::First => a.first_seen.cmp(&b.first_seen),
            Self::Last => a.last_seen.cmp(&b.last_seen),
            Self::Snr => a.peak_snr_db.total_cmp(&b.peak_snr_db),
        }
    }

    /// Direction on first click: numbers and times high first, names and
    /// frequency low first.
    fn default_descending(self) -> bool {
        !matches!(self, Self::Freq | Self::Class)
    }
}

/// Target frame interval (~60 fps).
const FRAME_TIME: std::time::Duration = std::time::Duration::from_micros(16_667);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListTab {
    Emitters,
    Records,
    P25,
    Calls,
    Pages,
}

/// Command line: `--sim` (simulator only), `--freq <MHz>` (focus centre),
/// `--serial <hex>` (focus device), `--no-sweep`, `--sweep <lo> <hi>` (MHz),
/// `--tab emitters|records|p25|calls|pages` (bottom panel).
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
                    Some("pages") => Some(ListTab::Pages),
                    Some("calls") => Some(ListTab::Calls),
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
            tune_target: ReceiverId::live(0),
            tune_mhz: args.freq_mhz.unwrap_or(if args.sim { 855.25 } else { 100.0 }),
            sweep_lo_mhz: lo,
            sweep_hi_mhz: hi,
            error: None,
            selection: Selection::None,
            list_tab: args.tab.unwrap_or(ListTab::Emitters),
            emitter_cache: (std::time::Instant::now() - std::time::Duration::from_secs(1), Vec::new(), 0),
            emitter_squelched: 0,
            emitter_view: EmitterView::default(),
            squelch: true,
            audio_out: None,
            playback: None,
            follow_policy: load_follow_policy(),
            policy_gen: (1, 0, 0),
            audio_err: None,
            listen_mode: None,
            listen_tdma: false,
            listen_mhz: 100.0,
            page_filter: String::new(),
            call_filter: String::new(),
            volume: 0.6,
            known: (std::time::Instant::now() - std::time::Duration::from_secs(5), Vec::new()),
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
            app.open_as(f, ReceiverId::live(0));
        }
        if args.sweep && !args.sim {
            if let Some(s) = hw.iter().find(|d| focus.as_ref().is_none_or(|f| !same_device(d, f))).cloned() {
                app.open_as(&s, ReceiverId::SWEEP);
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

    fn receiver(&self, id: ReceiverId) -> Option<&Receiver> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn receiver_mut(&mut self, id: ReceiverId) -> Option<&mut Receiver> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// The live receiver tuning lands on.
    fn target(&self) -> Option<&Receiver> {
        self.receiver(self.tune_target)
    }

    fn sweeper(&self) -> Option<&Receiver> {
        self.receiver(ReceiverId::SWEEP)
    }

    /// Live receivers' letters, in order.
    fn live_ids(&self) -> Vec<ReceiverId> {
        let mut v: Vec<ReceiverId> = self.panes.iter().map(|p| p.id).filter(|i| !i.is_sweep()).collect();
        v.sort();
        v
    }

    /// The first live letter nothing is open as.
    fn next_live_id(&self) -> ReceiverId {
        (0..24).map(ReceiverId::live).find(|i| self.receiver(*i).is_none()).unwrap_or(ReceiverId::live(0))
    }

    /// The live receiver a listen or export applies to: the selection's,
    /// else the tune target.
    fn working_id(&self) -> ReceiverId {
        self.selection.receiver().filter(|r| !r.is_sweep()).unwrap_or(self.tune_target)
    }

    fn sweep_plan(&self) -> SweepPlan {
        SweepPlan { start_hz: self.sweep_lo_mhz * 1e6, stop_hz: self.sweep_hi_mhz * 1e6, ..Default::default() }
    }

    fn close(&mut self, info: &DeviceInfo) {
        self.panes.retain(|p| !same_device(p.source.info(), info));
    }

    fn open_as(&mut self, info: &DeviceInfo, id: ReceiverId) {
        // A device fills one slot; a slot holds one device.
        self.close(info);
        self.panes.retain(|p| p.id != id);
        let Some(backend) = self.backends.iter().find(|b| b.name() == info.backend) else { return };
        let mode = if id.is_sweep() { Mode::Sweep(self.sweep_plan()) } else { Mode::Live };
        let tune = self.tune_mhz * 1e6;
        let result = backend.open(&info.serial).and_then(|mut dev| {
            if !id.is_sweep() {
                if let Err(e) = dev.set_center(tune) {
                    log::warn!("initial tune: {e}");
                }
            }
            Source::start(dev, SourceConfig::default(), mode)
        });
        match result {
            Ok(source) => {
                if !id.is_sweep() {
                    self.tune_mhz = source.snapshot().center_hz / 1e6;
                    // The first live receiver (or a reopened target) takes tuning.
                    if self.target().is_none() || self.tune_target == id {
                        self.tune_target = id;
                    }
                }
                let view = if id.is_sweep() { survey_view() } else { SpectrumView::new() };
                // Matches the backend's opening default (Airspy linearity 13/21).
                self.panes.push(Receiver {
                    id,
                    source,
                    view,
                    records: (None, std::time::Instant::now(), Vec::new().into()),
                    gain_mode: GainMode::Linearity,
                    gain_step: 13,
                });
                self.panes.sort_by_key(|p| (!p.id.is_sweep(), p.id));
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", info.label)),
        }
    }

    /// Tunes the target receiver.
    fn retune_target(&mut self, mhz: f64) {
        self.retune(self.tune_target, mhz);
    }

    fn retune(&mut self, id: ReceiverId, mhz: f64) {
        if id == self.tune_target {
            self.tune_mhz = mhz;
        }
        if let Some(p) = self.receiver_mut(id) {
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
                let n = self.known.1.len();
                ui.toggle_value(&mut self.squelch, format!("Squelch known ({n})")).on_hover_text(
                    "Dim FM broadcast, TV and LTE/NR carriers the app has identified, and hide their detections, so unknown signals stand out",
                );
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
                    let open: Vec<(DeviceInfo, ReceiverId)> = self.panes.iter().map(|p| (p.source.info().clone(), p.id)).collect();
                    self.panes.clear();
                    self.rescan();
                    for (d, r) in open {
                        self.open_as(&d, r);
                    }
                }
            });
        });

        enum Action {
            Open(DeviceInfo, ReceiverId),
            Close(DeviceInfo),
        }
        let mut action = None;
        let next_live = self.next_live_id();
        for d in &self.devices {
            let role = self.panes.iter().find(|p| same_device(p.source.info(), d)).map(|p| p.id);
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&d.label).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match role {
                        Some(r) => {
                            let (bg, fg) = if r.is_sweep() {
                                (egui::Color32::from_rgb(0x0f, 0x2b, 0x2b), egui::Color32::from_rgb(0x7f, 0xe9, 0xe5))
                            } else {
                                (theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG)
                            };
                            theme::chip(ui, &r.chip(), bg, fg);
                        }
                        None => {
                            if ui.small_button(format!("Focus {next_live}")).clicked() {
                                action = Some(Action::Open(d.clone(), next_live));
                            }
                            if ui.small_button("Survey").clicked() {
                                action = Some(Action::Open(d.clone(), ReceiverId::SWEEP));
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
        if let Some(p) = self.panes.iter_mut().find(|p| p.id.is_sweep()) {
            let st = p.source.sweep_status();
            let mut apply = false;
            let mut err = None;
            ui.add_space(8.0);
            let mut pick = false;
            ui.horizontal(|ui| {
                let sel = self.selection == Selection::Receiver(ReceiverId::SWEEP);
                let col = if sel { theme::ACCENT } else { theme::MUTED };
                pick = ui.add(egui::Label::new(egui::RichText::new("SURVEY").small().strong().color(col)).sense(egui::Sense::click())).clicked();
            });
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
                    if s.tuner_lock {
                        kv(ui, "Lock waits", &s.lock_waits.to_string());
                    } else {
                        kv(ui, "Slow hops", &format!("{} of {}", s.slow_hops, s.hops));
                    }
                }
            });
            if err.is_some() {
                self.error = err;
            }
            if pick {
                self.selection = Selection::Receiver(ReceiverId::SWEEP);
            }
            if apply {
                if self.sweep_hi_mhz <= self.sweep_lo_mhz + 1.0 {
                    self.sweep_hi_mhz = self.sweep_lo_mhz + 10.0;
                }
                let plan = self.sweep_plan();
                if let Some(p) = self.receiver_mut(ReceiverId::SWEEP) {
                    p.source.set_mode(Mode::Sweep(plan));
                    p.view = survey_view();
                }
            }
        }

        let live = self.live_ids();
        for id in live {
            let target = self.tune_target == id;
            let Some(p) = self.panes.iter_mut().find(|p| p.id == id) else { continue };
            let snap = p.source.snapshot();
            let st = p.source.stats();
            let cfg = p.source.config().clone();
            let load = p.source.classifier_load();
            let mut tune = None;
            let mut err = None;
            ui.add_space(8.0);
            let sel = self.selection == Selection::Receiver(id);
            let mut pick = false;
            ui.horizontal(|ui| {
                let col = if sel { theme::ACCENT } else { theme::MUTED };
                pick = ui.add(egui::Label::new(egui::RichText::new(id.chip()).small().strong().color(col)).sense(egui::Sense::click())).clicked();
                if target {
                    ui.label(egui::RichText::new("tune target").small().color(theme::ACCENT)).on_hover_text("Survey clicks and double-clicks tune here (keys 1, 2, 3 switch)");
                }
            });
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Centre");
                    let mut mhz = snap.center_hz / 1e6;
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
                if target {
                    ui.label(egui::RichText::new("Tip: click the survey to tune here").small().color(theme::MUTED));
                }
            });
            if err.is_some() {
                self.error = err;
            }
            if pick {
                self.selection = Selection::Receiver(id);
            }
            if let Some(m) = tune {
                self.retune(id, m);
            }
        }

        self.listen_card(ui);
        self.selection_card(ui);
        ui.add_space(8.0);
        ui.label(egui::RichText::new("DISPLAY").small().strong().color(theme::MUTED));
        theme::card(ui, |ui| {
            ui.spacing_mut().slider_width = 110.0;
            for p in self.panes.iter_mut() {
                ui.label(egui::RichText::new(p.id.chip()).small().color(theme::MUTED));
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

    /// Demodulated audio from a live receiver (the selection's, else the
    /// tune target): the selected signal (mode from its label unless
    /// overridden) or a typed frequency.
    fn listen_card(&mut self, ui: &mut egui::Ui) {
        let rx = self.working_id();
        let Some(focus) = self.receiver(rx) else { return };
        let listening = focus.source.listening();
        // What's selected, and the mode its label suggests.
        let emitter = self.selection.emitter().filter(|(r, _)| *r == rx).and_then(|(_, id)| focus.source.emitters().into_iter().find(|e| e.id == id));
        let record = self.selection.record().filter(|(r, _)| *r == rx).and_then(|(_, id)| focus.records.2.iter().find(|r| r.id == id).cloned());
        let target: Option<(f64, String, f64)> =
            emitter.as_ref().map(|e| (e.center_hz, e.label().unwrap_or_default(), e.bandwidth_hz)).or_else(|| {
                record.as_ref().map(|r| (r.center_hz(), r.display_class().map(|c| c.label.clone()).unwrap_or_default(), r.bandwidth_hz()))
            });
        let known = self.known.1.clone();
        let auto_mode = |hz: f64, label: &str, bw: f64| {
            let broadcast = known.iter().any(|(lo, hi, k)| (*lo..=*hi).contains(&hz) && k.starts_with("FM broadcast"));
            if broadcast || label.contains("Wideband FM") || bw > 100e3 {
                engine::DemodMode::Wfm
            } else if label.contains("P25") && !label.contains("Phase 2") {
                engine::DemodMode::P25
            } else if label.starts_with("AM") || label.contains(" AM") {
                engine::DemodMode::Am
            } else {
                engine::DemodMode::Nfm
            }
        };
        let open = focus.source.audio().open.load(std::sync::atomic::Ordering::Relaxed);
        let voice = focus.source.voice();
        let span_mid = focus.view.span().map(|(lo, hi)| (lo + hi) / 2.0);
        let mut start: Option<engine::ListenSpec> = None;
        let mut stop = false;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("LISTEN").small().strong().color(theme::MUTED));
            ui.label(egui::RichText::new(format!("on {rx}")).small().color(theme::MUTED));
        });
        theme::card(ui, |ui| {
            match listening {
                Some(l) if l.mode == engine::DemodMode::P25 => {
                    let v = &voice;
                    ui.horizontal(|ui| {
                        let what = match (v.center_hz, v.slot) {
                            (Some(hz), Some(ts)) => format!("{:.5} MHz P25 Phase 2 TS{}", hz / 1e6, ts + 1),
                            (Some(hz), None) if l.tdma => format!("{:.5} MHz P25 Phase 2", hz / 1e6),
                            (Some(hz), None) => format!("{:.5} MHz P25", hz / 1e6),
                            (None, _) => "waiting for a granted call".into(),
                        };
                        let head = if l.follow { format!("Following · {what}") } else { what };
                        ui.label(egui::RichText::new(head).monospace().strong());
                    });
                    ui.horizontal(|ui| {
                        let (txt, col) = match (v.encrypted, v.active) {
                            (Some(alg), _) => (format!("encrypted ({alg}), silent"), theme::ALERT),
                            (None, true) => ("● voice".to_string(), theme::VOICE),
                            (None, false) => ("○ no voice".to_string(), theme::MUTED),
                        };
                        ui.label(egui::RichText::new(txt).small().color(col));
                        if let Some(c) = v.call {
                            ui.label(egui::RichText::new(c.summary()).small().monospace());
                        }
                        if v.recording {
                            ui.label(egui::RichText::new("rec").small().color(theme::ALERT));
                        }
                    });
                }
                Some(l) => {
                    ui.horizontal(|ui| {
                        let (txt, col) = if open { ("● OPEN", theme::VOICE) } else { ("○ squelched", theme::MUTED) };
                        ui.label(egui::RichText::new(format!("{:.4} MHz {}", l.center_hz / 1e6, l.mode.name())).monospace().strong());
                        ui.label(egui::RichText::new(txt).small().color(col));
                    });
                }
                None => {
                    ui.label(egui::RichText::new("Not listening").color(theme::MUTED));
                }
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Mode").small().color(theme::MUTED));
                for (m, name) in [
                    (None, "Auto"),
                    (Some(engine::DemodMode::Nfm), "NFM"),
                    (Some(engine::DemodMode::Am), "AM"),
                    (Some(engine::DemodMode::Wfm), "WFM"),
                    (Some(engine::DemodMode::P25), "P25"),
                ] {
                    if ui.selectable_value(&mut self.listen_mode, m, egui::RichText::new(name).small()).changed() {
                        // Re-apply to what's playing.
                        if let Some(l) = listening.filter(|l| !l.follow) {
                            let mode = m.unwrap_or(l.mode);
                            start = Some(engine::ListenSpec { center_hz: l.center_hz, mode, follow: false, tdma: self.listen_tdma, slot: None });
                        }
                    }
                }
                if self.listen_mode == Some(engine::DemodMode::P25) {
                    ui.checkbox(&mut self.listen_tdma, egui::RichText::new("Phase 2").small())
                        .on_hover_text("TDMA: plays whichever timeslot starts talking");
                }
            });
            ui.horizontal(|ui| {
                let sel = target.clone();
                if ui.add_enabled(sel.is_some(), egui::Button::new("Listen to selection")).clicked() {
                    if let Some((hz, label, bw)) = sel {
                        let tdma = if self.listen_mode.is_none() { label.contains("Phase 2") } else { self.listen_tdma };
                        let mode = self.listen_mode.unwrap_or_else(|| if tdma { engine::DemodMode::P25 } else { auto_mode(hz, &label, bw) });
                        start = Some(engine::ListenSpec { center_hz: hz, mode, follow: false, tdma, slot: None });
                    }
                }
                if ui.add_enabled(listening.is_some(), egui::Button::new("Stop")).clicked() {
                    stop = true;
                }
            });
            ui.horizontal(|ui| {
                let following = listening.is_some_and(|l| l.follow);
                if ui
                    .add_enabled(!following, egui::Button::new("Follow calls"))
                    .on_hover_text("Play P25 calls (Phase 1 and 2) as the control channel grants them (Focus on the control channel's span)")
                    .clicked()
                {
                    let hz = span_mid.unwrap_or(self.listen_mhz * 1e6);
                    start = Some(engine::ListenSpec { center_hz: hz, mode: engine::DemodMode::P25, follow: true, tdma: false, slot: None });
                }
                let fp = &self.follow_policy;
                if !fp.priority.is_empty() || !fp.lockout.is_empty() {
                    let list = |s: &std::collections::BTreeSet<u32>| s.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
                    ui.label(egui::RichText::new(format!("★ {}  ⊘ {}", list(&fp.priority), list(&fp.lockout))).small().color(theme::MUTED))
                        .on_hover_text("Priority (★) and locked-out (⊘) talkgroups; right-click a call in the Calls tab to change");
                }
                if ui.small_button("Recordings").on_hover_text(engine::recordings_dir().display().to_string()).clicked() {
                    let dir = engine::recordings_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    open_path(&dir);
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.listen_mhz).speed(0.0125).range(24.0..=1800.0).max_decimals(4).suffix(" MHz"));
                if ui.button("Listen here").clicked() {
                    let hz = self.listen_mhz * 1e6;
                    start = Some(engine::ListenSpec {
                        center_hz: hz,
                        mode: self.listen_mode.unwrap_or_else(|| auto_mode(hz, "", 0.0)),
                        follow: false,
                        tdma: self.listen_tdma && self.listen_mode == Some(engine::DemodMode::P25),
                        slot: None,
                    });
                }
            });
            if ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.5).text("volume")).changed() {
                if let Some(a) = &self.audio_out {
                    a.set_volume(self.volume);
                }
            }
            if let Some(e) = &self.audio_err {
                ui.label(egui::RichText::new(e).small().color(theme::MUTED));
            }
            if let Some(pb) = &self.playback {
                ui.horizontal(|ui| {
                    let name = pb.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    ui.label(egui::RichText::new(format!("▶ {name} ({:.1} s)", pb.seconds)).small().monospace().color(theme::VOICE));
                    if ui.small_button("Stop").clicked() {
                        pb.stop();
                    }
                });
            }
        });
        if let Some(spec) = start {
            if let Some(focus) = self.receiver(rx).filter(|_| !spec.follow) {
                // Outside the tuned span: retune the focus there first.
                if focus.view.span().is_none_or(|(lo, hi)| spec.center_hz < lo + 0.5e6 || spec.center_hz > hi - 0.5e6) {
                    self.retune(rx, (spec.center_hz / 1e3).round() / 1e3);
                }
            }
            if self.audio_out.is_none() {
                match audio::AudioOut::new() {
                    Ok(a) => {
                        a.set_volume(self.volume);
                        self.audio_out = Some(a);
                        self.audio_err = None;
                    }
                    Err(e) => self.audio_err = Some(format!("Audio output: {e}")),
                }
            }
            self.playback = None;
            // One audio output: another receiver's listener stops.
            for p in self.panes.iter().filter(|p| p.id != rx && p.source.listening().is_some()) {
                p.source.listen(None);
            }
            if let Some(focus) = self.receiver(rx) {
                focus.source.listen(Some(spec));
                if let Some(a) = &self.audio_out {
                    a.set_source(Some(focus.source.audio()));
                }
                self.listen_mhz = spec.center_hz / 1e6;
            }
        }
        if stop {
            if let Some(focus) = self.receiver(rx) {
                focus.source.listen(None);
            }
            if let Some(a) = &self.audio_out {
                a.set_source(None);
            }
        }
    }

    /// Plays a recording, taking the audio output from live listening until
    /// it ends.
    fn play_recording(&mut self, path: &std::path::Path) {
        if self.audio_out.is_none() {
            match audio::AudioOut::new() {
                Ok(a) => {
                    a.set_volume(self.volume);
                    self.audio_out = Some(a);
                }
                Err(e) => {
                    self.audio_err = Some(format!("Audio output: {e}"));
                    return;
                }
            }
        }
        match engine::Playback::start(path) {
            Ok(pb) => {
                if let Some(a) = &self.audio_out {
                    a.set_source(Some(pb.queue.clone()));
                }
                self.playback = Some(pb);
                self.audio_err = None;
            }
            Err(e) => self.audio_err = Some(format!("{}: {e}", path.display())),
        }
    }

    /// A finished recording gives the audio output back to live listening.
    fn tick_playback(&mut self, ctx: &egui::Context) {
        if !self.playback.as_ref().is_some_and(|p| p.finished()) {
            if self.playback.is_some() {
                // Keep repainting while it plays so the end is noticed.
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            return;
        }
        self.playback = None;
        let live = self.panes.iter().find(|f| f.source.listening().is_some()).map(|f| f.source.audio());
        if let Some(a) = &self.audio_out {
            a.set_source(live);
        }
    }

    /// Merges every pane's known occupants (refreshed once a second). Where
    /// panes overlap, the same occupant seen by both is kept once: the wider
    /// extent wins (Focus sees HD sidebands the survey's bins can't).
    fn refresh_known(&mut self) {
        if self.known.0.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        let mut all: Vec<(f64, f64, String)> = self
            .panes
            .iter()
            .flat_map(|p| p.source.occupants())
            .map(|k| (k.occupant.lo_hz, k.occupant.hi_hz, k.occupant.kind.label()))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64, String)> = Vec::new();
        for k in all {
            match merged.last_mut() {
                // Same occupant: overlapping and centred within 50 kHz.
                Some(m) if k.0 < m.1 && ((k.0 + k.1) / 2.0 - (m.0 + m.1) / 2.0).abs() < 50e3 => {
                    if k.1 - k.0 > m.1 - m.0 {
                        *m = k;
                    }
                }
                _ => merged.push(k),
            }
        }
        self.known = (std::time::Instant::now(), merged);
    }

    /// The known occupant covering `hz`, if any.
    fn known_at(&self, hz: f64) -> Option<&str> {
        self.known.1.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&hz)).map(|k| k.2.as_str())
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        self.refresh_known();
        for p in self.panes.iter_mut() {
            p.view.known = self.known.1.clone();
            p.view.squelch = self.squelch;
            p.view.paused = self.paused;
            p.view.markers = p.source.monitors().iter().map(|m| (m.center_hz, m.label.clone())).collect();
        }
        if !self.paused {
            for p in self.panes.iter_mut() {
                p.view.ingest(&p.source);
            }
        }
        let area = ui.available_rect_before_wrap();
        if self.panes.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("No sources open. Pick a device on the left.").color(theme::MUTED))
            });
            return;
        }
        // The sweeper on top, live receivers sharing the rest.
        let live = self.live_ids();
        let mut rects: Vec<(ReceiverId, egui::Rect)> = Vec::new();
        let mut top = area.top();
        if self.sweeper().is_some() {
            let h = if live.is_empty() { area.height() } else { (area.height() * 0.38).round() };
            rects.push((ReceiverId::SWEEP, egui::Rect::from_min_size(egui::pos2(area.left(), top), egui::vec2(area.width(), h))));
            top += h + 6.0;
        }
        let n = live.len() as f32;
        let h = ((area.bottom() - top) - 6.0 * (n - 1.0).max(0.0)) / n.max(1.0);
        for id in &live {
            rects.push((*id, egui::Rect::from_min_size(egui::pos2(area.left(), top), egui::vec2(area.width(), h))));
            top += h + 6.0;
        }

        let live_spans: Vec<(ReceiverId, (f64, f64))> = live.iter().filter_map(|id| Some((*id, self.receiver(*id)?.view.span()?))).collect();
        let spans: Vec<(f64, f64)> = live_spans.iter().map(|(_, s)| *s).collect();
        let selection = self.selection.clone();
        let target = self.tune_target;
        let mut tune_to: Option<(ReceiverId, f64)> = None;
        let mut select = None;
        for (id, r) in rects {
            let Some(p) = self.receiver_mut(id) else { continue };
            let records = if id.is_sweep() { survey_outside_focus(pane_records(p), &spans) } else { pane_records(p) };
            // Highlight the selected record, and its emitter (or the
            // selected emitter), on this receiver only.
            let sel_record = selection.record().filter(|(rx, _)| *rx == id).map(|(_, rid)| rid);
            let sel_emitter = selection
                .emitter()
                .filter(|(rx, _)| *rx == id)
                .map(|(_, e)| e)
                .or_else(|| sel_record.and_then(|rid| records.iter().find(|r| r.id == rid).and_then(|r| r.emitter)));
            if id.is_sweep() {
                let st = p.source.sweep_status();
                let title = match &st {
                    Some(s) => format!(
                        "SURVEY · {} · {:.0}–{:.0} MHz · {} hops · {:.0} MHz/s · revisit {:.1} s · click tunes {target}",
                        p.source.info().label,
                        s.start_hz / 1e6,
                        s.stop_hz() / 1e6,
                        s.hops,
                        s.mhz_per_sec(),
                        s.last_pass_s
                    ),
                    None => format!("SURVEY · {}", p.source.info().label),
                };
                let res = p.view.show(ui, r, &title, st.as_ref(), &records, sel_record, sel_emitter);
                for (rx, (lo, hi)) in &live_spans {
                    p.view.bracket(ui, r, *lo, *hi, &rx.to_string());
                }
                // Survey click: tune the target there; on a record, also
                // select it and centre on the record, not the click point.
                if let Some(rid) = res.record {
                    select = Some(Selection::Record(id, rid));
                    tune_to = records.iter().find(|r| r.id == rid).map(|r| (target, r.center_hz()));
                } else if let Some(f) = res.freq_hz {
                    tune_to = Some((target, f));
                }
            } else {
                let snap = p.source.snapshot();
                let title = format!("FOCUS {id} · {} · {:.3} MHz · {:.1} MS/s", snap.info.label, snap.center_hz / 1e6, snap.sample_rate / 1e6);
                let res = p.view.show(ui, r, &title, None, &records, sel_record, sel_emitter);
                // Live click: select a record if one was hit, otherwise
                // recentre this receiver.
                match (res.record, res.freq_hz) {
                    (Some(rid), _) => select = Some(Selection::Record(id, rid)),
                    (None, Some(f)) => tune_to = Some((id, f)),
                    _ => {}
                }
            }
        }
        if let Some(sel) = select {
            self.selection = sel;
        }
        if let Some((id, f)) = tune_to {
            self.retune(id, (f / 1e3).round() / 1e3);
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
        let rx = self.working_id();
        let Some(focus) = self.receiver(rx) else { return };
        let records: Vec<engine::SignalRecord> = focus.records.2.iter().cloned().collect();
        let record = self.selection.record().filter(|(r, _)| *r == rx).and_then(|(_, id)| {
            records.iter().find(|r| r.id == id).cloned().or_else(|| focus.source.recent_records(3000).into_iter().find(|r| r.id == id))
        });
        let emitter = self.selection.emitter().filter(|(r, _)| *r == rx).and_then(|(_, id)| focus.source.emitters().into_iter().find(|e| e.id == id));
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
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SELECTION").small().strong().color(theme::MUTED));
            ui.label(egui::RichText::new(format!("on {rx}")).small().color(theme::MUTED));
        });
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
                ui.label(egui::RichText::new("Click a box or a list row to select it.").small().color(theme::MUTED));
            }
            ui.separator();
            if ui
                .add_enabled(!busy, egui::Button::new("Export look-back (full band)"))
                .on_hover_text(format!("Everything in the {rx} look-back buffer (~10 s, ~400 MB)"))
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
            ui.selectable_value(&mut self.list_tab, ListTab::Calls, "Calls");
            ui.selectable_value(&mut self.list_tab, ListTab::Pages, "Pages");
            ui.label(egui::RichText::new(format!("click to select · double-click to tune {}", self.tune_target)).small().color(theme::MUTED));
        });
        match self.list_tab {
            ListTab::Emitters => self.emitter_list(ui),
            ListTab::Records => self.signal_list(ui),
            ListTab::P25 => self.p25_panel(ui),
            ListTab::Calls => self.calls_panel(ui),
            ListTab::Pages => self.pages_panel(ui),
        }
    }

    /// Emitters from the chosen panes, in a stable order (frequency unless a
    /// column header was clicked). Unconfirmed ones (maybe noise) are left
    /// out unless asked for. Virtualised: only rows on screen are laid out.
    fn emitter_list(&mut self, ui: &mut egui::Ui) {
        // P25 traffic channels: the talkgroup comes from the control
        // channel's grant for that frequency.
        let (grants, systems) = match self.target() {
            Some(p) => (p.source.p25_grants(500), p.source.p25_systems()),
            None => (Vec::new(), Vec::new()),
        };
        let grant_talk = |e: &engine::Emitter| -> Option<String> {
            let g = grants.iter().filter(|g| g.freq_hz.is_some_and(|f| (f - e.center_hz).abs() < 3e3)).max_by_key(|g| g.last)?;
            let sys = systems.iter().find(|(n, _)| *n == g.nac).map(|(_, s)| s);
            let alias =
                sys.and_then(|s| self.catalog.p25_group(s.wacn, s.system, s.control_hz, g.talkgroup)).map(|i| format!(" {}", i.alias));
            Some(format!("TG {}{}{}", g.talkgroup, alias.unwrap_or_default(), g.source.map(|s| format!(" ← {s}")).unwrap_or_default()))
        };
        let ids: Vec<ReceiverId> = self.panes.iter().map(|p| p.id).collect();
        let v = &mut self.emitter_view;
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Source").small().color(theme::MUTED));
            refresh |= ui.selectable_value(&mut v.source, None, egui::RichText::new("All").small()).changed();
            for id in &ids {
                refresh |= ui.selectable_value(&mut v.source, Some(*id), egui::RichText::new(id.to_string()).small()).changed();
            }
            ui.separator();
            refresh |= ui.checkbox(&mut v.unconfirmed, egui::RichText::new("show unconfirmed").small()).changed();
            ui.separator();
            if ui
                .button(egui::RichText::new("Export CSV").small())
                .on_hover_text("All emitters and records, every source, unfiltered")
                .clicked()
            {
                self.export_msg = Some(match export_emitters(&self.panes) {
                    Ok((path, e, r)) => format!("Exported {e} emitters, {r} records to {}", path.display()),
                    Err(err) => format!("Emitter export failed: {err}"),
                });
            }
            if let Some(m) = self.export_msg.as_deref().filter(|m| m.contains("mitter")) {
                ui.label(egui::RichText::new(m).small().color(theme::TEXT2));
            }
        });
        if refresh || self.emitter_cache.0.elapsed() > std::time::Duration::from_millis(300) {
            let v = &self.emitter_view;
            let spans: Vec<(f64, f64)> = self.panes.iter().filter(|q| !q.id.is_sweep()).filter_map(|q| q.view.span()).collect();
            let mut rows: Vec<(ReceiverId, engine::Emitter)> = Vec::new();
            let mut hidden = 0;
            let mut squelched = 0;
            // With the chosen receiver not open, show every receiver rather than nothing.
            let source = v.source.filter(|r| self.panes.iter().any(|p| p.id == *r));
            for p in self.panes.iter().filter(|p| source.is_none_or(|r| r == p.id)) {
                for e in p.source.emitters() {
                    if p.id.is_sweep() && in_focus_zone(e.center_hz, &spans) {
                        continue;
                    }
                    if !e.confirmed && !v.unconfirmed {
                        hidden += 1;
                        continue;
                    }
                    if self.squelch && self.known_at(e.center_hz).is_some() {
                        squelched += 1;
                        continue;
                    }
                    rows.push((p.id, e));
                }
            }
            let (sort, desc) = (v.sort, v.descending);
            rows.sort_by(|a, b| {
                let o = sort.cmp(&a.1, &b.1).then(a.1.center_hz.total_cmp(&b.1.center_hz));
                if desc { o.reverse() } else { o }
            });
            self.emitter_cache = (std::time::Instant::now(), rows, hidden);
            self.emitter_squelched = squelched;
        }
        let rows = std::mem::take(&mut self.emitter_cache.1);
        let active = rows.iter().filter(|r| r.1.active).count();
        let identified = rows.iter().filter(|r| r.1.has_protocol()).count();
        let hidden = self.emitter_cache.2;
        ui.label(
            egui::RichText::new(format!(
                "{} emitters · {active} active · {identified} identified{}{}",
                rows.len(),
                if hidden > 0 { format!(" · {hidden} unconfirmed not shown") } else { String::new() },
                if self.emitter_squelched > 0 { format!(" · {} known squelched", self.emitter_squelched) } else { String::new() }
            ))
            .small()
            .monospace()
            .color(theme::MUTED),
        );
        let mut sort_click = None;
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
            .column(egui_extras::Column::exact(170.0))
            .column(egui_extras::Column::exact(170.0))
            .columns(egui_extras::Column::exact(64.0), 3)
            .columns(egui_extras::Column::exact(72.0), 2)
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                use EmitterSort as S;
                let (cur, desc) = (self.emitter_view.sort, self.emitter_view.descending);
                for (t, key) in [
                    ("", None),
                    ("Src", None),
                    ("MHz", Some(S::Freq)),
                    ("Class", Some(S::Class)),
                    ("ID", None),
                    ("Activity", None),
                    ("Also seen", None),
                    ("BW kHz", Some(S::Bandwidth)),
                    ("Bursts", Some(S::Bursts)),
                    ("Duty %", Some(S::Duty)),
                    ("First", Some(S::First)),
                    ("Last", Some(S::Last)),
                    ("SNR dB", Some(S::Snr)),
                ] {
                    h.col(|ui| {
                        let Some(key) = key else {
                            ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                            return;
                        };
                        let text = if key == cur { format!("{t} {}", if desc { "▼" } else { "▲" }) } else { t.to_string() };
                        let col = if key == cur { theme::TEXT } else { theme::MUTED };
                        let r = ui.add(egui::Label::new(egui::RichText::new(text).small().color(col)).sense(egui::Sense::click()));
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            sort_click = Some(key);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let (role, e) = &rows[row.index()];
                    let sel = self.selection == Selection::Emitter(*role, e.id);
                    row.set_selected(sel);
                    let c = if sel { theme::SELECT } else { theme::TEXT2 };
                    let text = |ui: &mut egui::Ui, s: String, col: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(col)).selectable(false).truncate());
                    };
                    // Inside a known occupant (FM, TV, LTE): that's what it is.
                    let known = self.known_at(e.center_hz);
                    let label = match known {
                        Some(k) if !e.has_protocol() => k.to_string(),
                        _ => e.label().unwrap_or_else(|| if role.is_sweep() { "-".into() } else { "…".into() }),
                    };
                    let class_col = if sel {
                        theme::SELECT
                    } else if known.is_some() && !e.has_protocol() {
                        theme::KNOWN
                    } else {
                        theme::class_color(e.best.as_deref()).unwrap_or(theme::MUTED)
                    };
                    let mut others: Vec<(&String, &u32)> = e.tally.iter().filter(|(l, _)| **l != label).collect();
                    others.sort_by(|a, b| b.1.cmp(a.1));
                    let also: Vec<String> = others.iter().take(2).map(|(l, n)| format!("{l}×{n}")).collect();
                    row.col(|ui| text(ui, if e.active { "●".into() } else { String::new() }, theme::ACCENT));
                    row.col(|ui| text(ui, role.to_string(), c));
                    row.col(|ui| text(ui, format!("{:.4}", e.center_hz / 1e6), c));
                    row.col(|ui| text(ui, label, class_col));
                    row.col(|ui| text(ui, e.id_label(), c));
                    // Voice / control / data, and the latest call heard.
                    let talk = e.last_talk.clone().or_else(|| grant_talk(e));
                    let act = match (e.activity(), talk) {
                        (Some(a), Some(t)) => format!("{} · {t}", a.name()),
                        (Some(a), None) => a.name().to_string(),
                        (None, _) => String::new(),
                    };
                    let act_col = match e.activity() {
                        Some(detect::decode::Activity::Voice) if !sel => theme::VOICE,
                        _ => c,
                    };
                    row.col(|ui| text(ui, act, act_col));
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
        if let Some(key) = sort_click {
            let v = &mut self.emitter_view;
            v.descending = if v.sort == key { !v.descending } else { key.default_descending() };
            v.sort = key;
            // Re-sort on the next frame rather than waiting for the refresh.
            self.emitter_cache.0 -= std::time::Duration::from_secs(1);
        }
        if let Some((rx, id)) = clicked {
            self.selection = Selection::Emitter(rx, id);
        }
        if let Some(f) = tune {
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }

    /// P25 systems and talkgroup grants learned by the control-channel monitors.
    fn p25_panel(&mut self, ui: &mut egui::Ui) {
        let Some(focus) = self.target() else {
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
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }

    /// Calls from every pane: control-channel grants and voice-channel
    /// signalling merged per call, newest first.
    fn calls_panel(&mut self, ui: &mut egui::Ui) {
        let mut play: Option<std::path::PathBuf> = None;
        let selected = self.selection.clone();
        // (talkgroup, list: true = priority, false = lockout) to toggle.
        let mut toggle: Option<(u32, bool)> = None;
        let (prio, locked) = (self.follow_policy.priority.clone(), self.follow_policy.lockout.clone());
        let mut calls: Vec<engine::CallEvent> = self.panes.iter().flat_map(|p| p.source.calls(5000)).collect();
        calls.sort_by(|a, b| b.last.cmp(&a.last));
        // Names from the imported catalog, by each call's own system. A call
        // only heard on its voice channel names no system; with one system
        // known, it's that one.
        let systems: Vec<engine::TrunkSystem> = self.panes.iter().flat_map(|p| p.source.systems()).collect();
        let cat = &self.catalog;
        let system_of = |c: &engine::CallEvent| -> Option<(engine::SystemKey, Option<f64>)> {
            let s = match &c.system {
                Some(k) => systems.iter().find(|s| s.key.same(k)),
                None if systems.len() == 1 => systems.first(),
                None => None,
            };
            match (s, &c.system) {
                (Some(s), _) => Some((s.key.clone(), s.control_hz.first().copied())),
                (None, Some(k)) => Some((k.clone(), None)),
                (None, None) => None,
            }
        };
        let tg_name = |c: &engine::CallEvent| -> String {
            match (c.call.group, system_of(c)) {
                (true, Some((k, cc))) => cat.group(&k, cc, c.call.target).map(|i| i.name().to_string()).unwrap_or_default(),
                _ => String::new(),
            }
        };
        let radio_name = |c: &engine::CallEvent| -> String {
            match (c.call.source, system_of(c)) {
                (Some(r), Some((k, cc))) => cat.radio(&k, cc, r).unwrap_or("").to_string(),
                _ => String::new(),
            }
        };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Filter").small().color(theme::MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.call_filter).desired_width(200.0).hint_text("talkgroup, radio or name"));
            ui.label(
                egui::RichText::new(format!(
                    "{} calls · {} heard on their voice channel · times are approximate (voice channels are sampled)",
                    calls.len(),
                    calls.iter().filter(|c| c.heard).count()
                ))
                .small()
                .monospace()
                .color(theme::MUTED),
            );
        });
        if calls.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No calls yet. Calls appear from P25 control-channel grants (Focus on the control channel) and from \
                     voice channels whose signalling names the talkgroup (P25 Phase 1/2, DMR).",
                )
                .color(theme::MUTED),
            );
            return;
        }
        let f = self.call_filter.trim().to_lowercase();
        if !f.is_empty() {
            calls.retain(|c| {
                c.call.target.to_string().contains(&f)
                    || c.call.source.is_some_and(|s| s.to_string().contains(&f))
                    || tg_name(c).to_lowercase().contains(&f)
                    || radio_name(c).to_lowercase().contains(&f)
            });
        }
        let accent = egui::Color32::from_rgb(0xf2, 0xa3, 0x3a);
        let mut tune = None;
        let mut select = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("calls")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(48.0))
            .column(egui_extras::Column::exact(84.0))
            .column(egui_extras::Column::exact(110.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(220.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(160.0))
            .column(egui_extras::Column::exact(70.0))
            .column(egui_extras::Column::exact(48.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["Start", "Secs", "MHz", "Protocol", "Talkgroup", "Name (imported)", "Radio", "Radio (imported)", "Flags", "Audio", "Seen"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, calls.len(), |mut row| {
                    let c = &calls[row.index()];
                    let text = |ui: &mut egui::Ui, s: String, col: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(col)).selectable(false).truncate());
                    };
                    row.set_selected(match &selected {
                        Selection::Talkgroup { tg, .. } => c.call.group && c.call.target == *tg,
                        Selection::Radio { id, .. } => !c.call.group && c.call.target == *id,
                        _ => false,
                    });
                    let recent = std::time::SystemTime::now().duration_since(c.last).map(|d| d.as_secs() < 10).unwrap_or(true);
                    let col = if recent { theme::TEXT } else { theme::TEXT2 };
                    row.col(|ui| text(ui, hms(c.first), col));
                    row.col(|ui| text(ui, format!("{:.0}", c.last.duration_since(c.first).unwrap_or_default().as_secs_f64()), theme::MUTED));
                    row.col(|ui| text(ui, c.center_hz.map(|f| format!("{:.5}", f / 1e6)).unwrap_or("?".into()), col));
                    row.col(|ui| text(ui, c.protocol.clone(), theme::TEXT2));
                    let mark = match (c.call.group, prio.contains(&c.call.target), locked.contains(&c.call.target)) {
                        (true, true, _) => "★ ",
                        (true, _, true) => "⊘ ",
                        _ => "",
                    };
                    let target = if c.call.group { format!("{mark}TG {}", c.call.target) } else { format!("→ {}", c.call.target) };
                    row.col(|ui| text(ui, target, if recent { accent } else { col }));
                    row.col(|ui| text(ui, tg_name(c), theme::TEXT2));
                    row.col(|ui| text(ui, c.call.source.map(|s| s.to_string()).unwrap_or_default(), col));
                    row.col(|ui| text(ui, radio_name(c), theme::TEXT2));
                    let flags = format!("{}{}", if c.call.emergency { "EMERG " } else { "" }, if c.call.encrypted { "enc" } else { "" });
                    row.col(|ui| text(ui, flags, if c.call.emergency { theme::ALERT } else { theme::MUTED }));
                    row.col(|ui| {
                        if let Some(last) = c.recordings.last() {
                            let n = c.recordings.len();
                            let label = if n > 1 { format!("▶ {n}") } else { "▶".into() };
                            let b = ui.small_button(label).on_hover_text(format!("Play {}\nRight-click: open in the system player", last.display()));
                            if b.clicked() {
                                play = Some(last.clone());
                            }
                            if b.secondary_clicked() {
                                open_path(last);
                            }
                        }
                    });
                    let seen = match (c.granted, c.heard) {
                        (true, true) => "granted + heard",
                        (true, false) => "granted",
                        _ => "heard",
                    };
                    row.col(|ui| text(ui, seen.into(), theme::MUTED));
                    let resp = row.response();
                    if resp.clicked() {
                        let system = system_of(c).map(|(k, _)| k);
                        select = Some(if c.call.group {
                            Selection::Talkgroup { system, tg: c.call.target }
                        } else {
                            Selection::Radio { system, id: c.call.target }
                        });
                    }
                    if resp.double_clicked() {
                        tune = c.center_hz;
                    }
                    if c.call.group {
                        let tg = c.call.target;
                        resp.context_menu(|ui| {
                            let p = if prio.contains(&tg) { format!("Remove TG {tg} from priority") } else { format!("Priority: TG {tg}") };
                            if ui.button(p).clicked() {
                                toggle = Some((tg, true));
                                ui.close();
                            }
                            let l = if locked.contains(&tg) { format!("Unlock TG {tg}") } else { format!("Lock out: TG {tg}") };
                            if ui.button(l).clicked() {
                                toggle = Some((tg, false));
                                ui.close();
                            }
                        });
                    }
                });
            });
        if let Some((tg, priority)) = toggle {
            let fp = &mut self.follow_policy;
            let (list, other) = if priority { (&mut fp.priority, &mut fp.lockout) } else { (&mut fp.lockout, &mut fp.priority) };
            if !list.remove(&tg) {
                list.insert(tg);
                other.remove(&tg);
            }
            self.policy_gen.0 += 1;
            save_follow_policy(&self.follow_policy);
        }
        if let Some(sel) = select {
            self.selection = sel;
        }
        if let Some(p) = play {
            self.play_recording(&p);
        }
        if let Some(f) = tune {
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }

    /// Pager messages (FLEX, POCSAG) decoded by the monitors, newest first.
    fn pages_panel(&mut self, ui: &mut egui::Ui) {
        let mut pages: Vec<engine::PageEvent> = self.panes.iter().flat_map(|p| p.source.pages(5000)).collect();
        pages.sort_by(|a, b| b.last.cmp(&a.last));
        let pagers = self.panes.iter().flat_map(|p| p.source.monitors()).filter(|m| !m.label.starts_with("CC")).count();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Filter").small().color(theme::MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.page_filter).desired_width(200.0).hint_text("capcode or text"));
            ui.label(
                egui::RichText::new(format!("{} pages · {pagers} pager channel{} monitored", pages.len(), if pagers == 1 { "" } else { "s" }))
                    .small()
                    .monospace()
                    .color(theme::MUTED),
            );
        });
        if pages.is_empty() {
            ui.label(
                egui::RichText::new(if pagers == 0 {
                    "No pager channel identified yet. FLEX and POCSAG channels are monitored automatically once identified."
                } else {
                    "Monitoring; waiting for the first page…"
                })
                .color(theme::MUTED),
            );
            return;
        }
        let f = self.page_filter.trim().to_lowercase();
        if !f.is_empty() {
            pages.retain(|p| format!("{:09}", p.capcode).contains(&f) || p.text.to_lowercase().contains(&f));
        }
        let pager = egui::Color32::from_rgb(0x6c, 0xc4, 0xe0);
        let selected = self.selection.clone();
        let mut tune = None;
        let mut select = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("pages")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::exact(84.0))
            .column(egui_extras::Column::exact(60.0))
            .column(egui_extras::Column::exact(84.0))
            .column(egui_extras::Column::exact(84.0))
            .column(egui_extras::Column::exact(32.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["Time", "MHz", "Protocol", "Capcode", "Type", "×", "Message (hover for all)"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, pages.len(), |mut row| {
                    let p = &pages[row.index()];
                    let text = |ui: &mut egui::Ui, s: String, c: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(c)).selectable(false).truncate());
                    };
                    row.set_selected(matches!(&selected, Selection::Capcode { capcode, .. } if *capcode == p.capcode));
                    let recent = std::time::SystemTime::now().duration_since(p.last).map(|d| d.as_secs() < 10).unwrap_or(true);
                    let c = if recent { theme::TEXT } else { theme::TEXT2 };
                    row.col(|ui| text(ui, hms(p.last), c));
                    row.col(|ui| text(ui, format!("{:.4}", p.center_hz / 1e6), theme::TEXT2));
                    row.col(|ui| text(ui, p.protocol.to_string(), pager));
                    row.col(|ui| text(ui, format!("{:09}", p.capcode), c));
                    row.col(|ui| text(ui, p.kind.clone(), theme::MUTED));
                    row.col(|ui| text(ui, p.count.to_string(), theme::MUTED));
                    row.col(|ui| {
                        let body = format!("{}{}", if p.damaged { "[damaged] " } else { "" }, p.text);
                        ui.add(egui::Label::new(egui::RichText::new(&body).monospace().color(c)).selectable(false).truncate())
                            .on_hover_text(&body);
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        select = Some(Selection::Capcode { protocol: p.protocol.to_string(), capcode: p.capcode });
                    }
                    if resp.double_clicked() {
                        tune = Some(p.center_hz);
                    }
                });
            });
        if let Some(sel) = select {
            self.selection = sel;
        }
        if let Some(f) = tune {
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }

    /// Newest records from all panes, most recent first. Virtualised.
    fn signal_list(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<(ReceiverId, engine::SignalRecord)> = Vec::new();
        for p in &self.panes {
            rows.extend(p.source.recent_records(500).into_iter().map(|r| (p.id, r)));
        }
        rows.sort_by(|a, b| b.1.active.cmp(&a.1.active).then(b.1.t_end.cmp(&a.1.t_end)));
        let before = rows.len();
        if self.squelch {
            rows.retain(|r| self.known_at(r.1.center_hz()).is_none());
        }
        let squelched = before - rows.len();
        let active = rows.iter().filter(|r| r.1.active).count();
        ui.label(
            egui::RichText::new(format!(
                "{} records shown · {active} active{}",
                rows.len(),
                if squelched > 0 { format!(" · {squelched} known squelched") } else { String::new() }
            ))
            .small()
            .monospace()
            .color(theme::MUTED),
        );
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
                    let sel = self.selection == Selection::Record(*role, r.id);
                    row.set_selected(sel);
                    let c = if sel { theme::SELECT } else { theme::TEXT2 };
                    let text = |ui: &mut egui::Ui, s: String, col: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(col)).selectable(false).truncate());
                    };
                    let (class, class_col) = match r.display_class() {
                        Some(k) => {
                            let act = k.fields.as_ref().and_then(|f| f.activity).map(|a| format!(" · {}", a.name())).unwrap_or_default();
                            (format!("{}{act}", k.label), if sel { theme::SELECT } else { theme::class_color(Some(k)).unwrap_or(c) })
                        }
                        None => (if role.is_sweep() { "-".into() } else { "…".into() }, theme::MUTED),
                    };
                    row.col(|ui| text(ui, r.id.to_string(), c));
                    row.col(|ui| text(ui, role.to_string(), c));
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
                        clicked = Some((*role, r.id));
                    }
                    if resp.double_clicked() {
                        tune = Some(r.center_hz());
                    }
                });
            });
        if let Some((rx, id)) = clicked {
            self.selection = Selection::Record(rx, id);
        }
        if let Some(f) = tune {
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tick_playback(&ctx);
        // 1/2/3 pick the live receiver tuning lands on; Esc clears the selection.
        if !ctx.egui_wants_keyboard_input() {
            let live = self.live_ids();
            ctx.input(|i| {
                for (n, k) in [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3].into_iter().enumerate() {
                    if i.key_pressed(k) {
                        if let Some(id) = live.get(n) {
                            self.tune_target = *id;
                        }
                    }
                }
                if i.key_pressed(egui::Key::Escape) {
                    self.selection = Selection::None;
                }
            });
        }
        // Give the follow policy to every source when it changes (or a
        // source is added).
        if (self.policy_gen.0, self.panes.len()) != (self.policy_gen.1, self.policy_gen.2) {
            for p in &self.panes {
                p.source.set_follow_policy(self.follow_policy.clone());
            }
            self.policy_gen = (self.policy_gen.0, self.policy_gen.0, self.panes.len());
        }
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
fn pane_records(p: &mut Receiver) -> std::sync::Arc<[engine::SignalRecord]> {
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

fn in_focus_zone(f: f64, spans: &[(f64, f64)]) -> bool {
    spans.iter().any(|(lo, hi)| f > lo - FOCUS_ZONE_MARGIN_HZ && f < hi + FOCUS_ZONE_MARGIN_HZ)
}

fn survey_outside_focus(records: std::sync::Arc<[engine::SignalRecord]>, spans: &[(f64, f64)]) -> std::sync::Arc<[engine::SignalRecord]> {
    if spans.is_empty() {
        return records;
    }
    records.iter().filter(|r| !in_focus_zone(r.center_hz(), spans)).cloned().collect()
}

/// `<Documents>/SpecTrawl/catalog.json`
/// Writes every pane's emitters and records (unfiltered, unconfirmed
/// included) to two CSVs next to the SigMF exports. Returns the emitter
/// file's path and the counts.
fn export_emitters(panes: &[Receiver]) -> std::io::Result<(std::path::PathBuf, usize, usize)> {
    use std::io::Write;
    let dir = engine::sigmf::default_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let unix = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    // Labels and tallies can hold commas.
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let role = |r: ReceiverId| if r.is_sweep() { "survey".to_string() } else { format!("focus {r}") };

    let em_path = dir.join(format!("emitters_{stamp}.csv"));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&em_path)?);
    writeln!(
        f,
        "source,serial,emitter,center_mhz,bw_khz,records,confirmed,active,duty,on_time_s,first_seen,last_seen,peak_snr_db,label,system_id,tally"
    )?;
    let mut n_em = 0;
    for p in panes {
        let serial = p.source.info().serial.clone();
        let mut em = p.source.emitters();
        em.sort_by(|a, b| a.center_hz.total_cmp(&b.center_hz));
        for e in &em {
            let tally: Vec<String> = e.tally.iter().map(|(k, n)| format!("{k}x{n}")).collect();
            writeln!(
                f,
                "{},{serial},{},{:.6},{:.2},{},{},{},{:.4},{:.3},{:.3},{:.3},{:.1},{},{},{}",
                role(p.id),
                e.id,
                e.center_hz / 1e6,
                e.bandwidth_hz / 1e3,
                e.records,
                e.confirmed as u8,
                e.active as u8,
                e.duty(),
                e.on_time_s,
                unix(e.first_seen),
                unix(e.last_seen),
                e.peak_snr_db,
                q(&e.label().unwrap_or_default()),
                q(&e.id_label()),
                q(&tally.join("; "))
            )?;
        }
        n_em += em.len();
    }
    f.flush()?;

    let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join(format!("records_{stamp}.csv")))?);
    writeln!(
        f,
        "source,record,emitter,center_mhz,bw_khz,bin_hz,t_start,t_end,rows,mean_snr_db,max_snr_db,peak_dbfs,active,tuner_mhz,class"
    )?;
    let mut n_rec = 0;
    for p in panes {
        let recs = p.source.records_since(0);
        for r in &recs {
            writeln!(
                f,
                "{},{},{},{:.6},{:.2},{:.1},{:.3},{:.3},{},{:.1},{:.1},{:.1},{},{:.6},{}",
                role(p.id),
                r.id,
                r.emitter.map(|e| e.to_string()).unwrap_or_default(),
                r.center_hz() / 1e6,
                r.bandwidth_hz() / 1e3,
                r.bin_hz,
                unix(r.t_start),
                unix(r.t_end),
                r.rows,
                r.mean_snr_db,
                r.max_snr_db,
                r.peak_dbfs,
                r.active as u8,
                r.tuner_center_hz / 1e6,
                q(r.display_class().map(|c| c.label.as_str()).unwrap_or(""))
            )?;
        }
        n_rec += recs.len();
    }
    f.flush()?;
    Ok((em_path, n_em, n_rec))
}

/// Opens a file or folder with the system's default application.
fn open_path(p: &std::path::Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(p).spawn();
    if let Err(e) = r {
        log::warn!("open {}: {e}", p.display());
    }
}

fn follow_policy_path() -> std::path::PathBuf {
    catalog_path().with_file_name("follow.txt")
}

/// `priority <talkgroup>` / `lockout <talkgroup>` lines.
fn load_follow_policy() -> engine::FollowPolicy {
    let mut p = engine::FollowPolicy::default();
    for line in std::fs::read_to_string(follow_policy_path()).unwrap_or_default().lines() {
        let mut w = line.split_whitespace();
        match (w.next(), w.next().and_then(|t| t.parse::<u32>().ok())) {
            (Some("priority"), Some(tg)) => {
                p.priority.insert(tg);
            }
            (Some("lockout"), Some(tg)) => {
                p.lockout.insert(tg);
            }
            _ => {}
        }
    }
    p
}

fn save_follow_policy(p: &engine::FollowPolicy) {
    let mut text = String::from("# Talkgroups for call following: priority <tg> / lockout <tg>\n");
    p.priority.iter().for_each(|t| text.push_str(&format!("priority {t}\n")));
    p.lockout.iter().for_each(|t| text.push_str(&format!("lockout {t}\n")));
    let path = follow_policy_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = std::fs::write(&path, text) {
        log::warn!("{}: {e}", path.display());
    }
}

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
