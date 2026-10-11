mod audio;
mod logging;
mod settings;
mod spectrum_view;
mod theme;

use std::time::{SystemTime, UNIX_EPOCH};

use egui_dock::{DockState, NodeIndex, SurfaceIndex};
use engine::{Mode, Source, SourceConfig, SweepPlan};
use sdr_core::{Backend, DeviceInfo, GainSetting};
use serde::{Deserialize, Serialize};
use spectrum_view::SpectrumView;

fn main() -> eframe::Result {
    let settings = settings::Settings::load();
    logging::init(settings.log.clone());
    spawn_deadlock_watchdog();
    // The window opens where it was left (the workspace remembers it);
    // the first run fills the screen, and the layout built on that first
    // frame is sized to the monitor.
    // (Maximizing is asked for on the first frame: a window created
    // maximized keeps its creation size on Windows.)
    let mut viewport = egui::ViewportBuilder::default().with_title("SpecTrawl");
    viewport = match load_workspace().and_then(|w| w.window) {
        Some(w) => viewport.with_position(w.pos).with_inner_size(w.size),
        None => viewport.with_inner_size([1600.0, 960.0]),
    };
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("SpecTrawl", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// A receiver's letter, the handle for it everywhere in the UI: `S` is the
/// sweeper, `A`, `B`, ... are live receivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

/// Waterfall dB range for one receiver.
fn db_ui(ui: &mut egui::Ui, p: &mut Receiver) {
    ui.spacing_mut().slider_width = 110.0;
    ui.add(egui::Slider::new(&mut p.view.db_max, -80.0..=0.0).text("top dB"));
    ui.add(egui::Slider::new(&mut p.view.db_min, -160.0..=-40.0).text("floor dB"));
    if ui.small_button("Auto range").clicked() {
        p.view.auto_range();
    }
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
    /// The dock tree, lent to the tab viewer while it draws (None then).
    dock: Option<DockState<Tab>>,
    dock_requests: Vec<DockReq>,
    popouts: Vec<PopOut>,
    next_popout: u64,
    /// Where the main window is, kept up to date for the workspace.
    window: Option<WindowRect>,
    /// Maximize on the first frame (a first run, or left maximized).
    maximize: bool,
    /// The named workspace in use, if one was loaded or saved.
    workspace_name: Option<String>,
    workspace_new_name: String,
    calls_view: CallsView,
    /// Talkgroup typed into the follow-list flyout.
    follow_add: u32,
    /// The command palette (Ctrl+K).
    palette_open: bool,
    palette_text: String,
    palette_sel: usize,
    palette_focus: bool,
    /// Volume before muting (M), to come back to.
    muted_volume: f32,
    /// Which pop-out window is being drawn (None: the main window), for
    /// the tab context menu.
    drawing: Option<u64>,
    /// (id, title) of every pop-out, for the context menu while they're
    /// on loan.
    popout_names: Vec<(u64, String)>,
    /// `--tab`: shown once the dock exists.
    start_tab: Option<Tab>,
    frame: FrameInfo,
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
    /// General settings (⚙), saved when changed.
    settings: settings::Settings,
    /// A survey span change from the keyboard, applied once the keys rest.
    sweep_dirty: Option<std::time::Instant>,
    /// Diagnostics log view: text filter.
    log_filter: String,
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

/// A dockable pane. Plain identifiers: the dock tree serialises as these
/// and the bodies are `App` methods.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Tab {
    Receiver(ReceiverId),
    Sources,
    Inspector,
    Emitters,
    Records,
    #[serde(alias = "P25")]
    Trunking,
    Calls,
    Pages,
    Diagnostics,
}

impl Tab {
    /// Every pane that isn't a receiver.
    const PANELS: [Tab; 8] = [Tab::Sources, Tab::Inspector, Tab::Emitters, Tab::Records, Tab::Trunking, Tab::Calls, Tab::Pages, Tab::Diagnostics];

    fn name(&self) -> String {
        match self {
            Tab::Receiver(id) => format!("Receiver {id}"),
            Tab::Sources => "Sources".into(),
            Tab::Inspector => "Inspector".into(),
            Tab::Emitters => "Emitters".into(),
            Tab::Records => "Records".into(),
            Tab::Trunking => "Trunking".into(),
            Tab::Calls => "Calls".into(),
            Tab::Pages => "Pages".into(),
            Tab::Diagnostics => "Diagnostics".into(),
        }
    }

    /// Lives in the tables group of the default layout.
    fn is_table(&self) -> bool {
        matches!(self, Tab::Emitters | Tab::Records | Tab::Trunking | Tab::Calls | Tab::Pages | Tab::Diagnostics)
    }
}

/// A change to the dock, queued while the dock is on loan to the viewer.
enum DockReq {
    /// Focus the tab, adding it if it isn't open.
    Show(Tab),
    Remove(Tab),
    AddReceiver(ReceiverId),
    Reset,
    /// Into a new OS window of its own.
    PopOut(Tab),
    /// Into the window with that id, or the main window.
    MoveTo(Tab, Option<u64>),
}

/// A popped-out OS window: its own dock, so it can hold several tabs.
#[derive(Serialize, Deserialize)]
struct PopOut {
    id: u64,
    title: String,
    dock: DockState<Tab>,
    /// Outer position and inner size, in points, once the window has shown.
    rect: Option<[f32; 4]>,
}

/// The main window's outer position and inner size in points, and whether
/// it was maximized (then `pos`/`size` are what it restores to).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WindowRect {
    pos: [f32; 2],
    size: [f32; 2],
    maximized: bool,
}

/// Saved layout: `Documents/SpecTrawl/workspaces/last.json`.
#[derive(Serialize, Deserialize)]
struct Workspace {
    main: DockState<Tab>,
    tune_target: char,
    #[serde(default)]
    popouts: Vec<PopOut>,
    #[serde(default)]
    window: Option<WindowRect>,
}

/// What the Calls tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallsView {
    Calls,
    /// The raw control-channel grants the calls are built from.
    Grants,
}

/// Built-in layouts, listed with the saved workspaces.
const PRESETS: [&str; 3] = ["survey", "trunking", "paging"];

/// What a command-palette entry does.
enum Cmd {
    Tune(ReceiverId, f64),
    Open(DeviceInfo, ReceiverId),
    Listen(ReceiverId, engine::ListenSpec),
    Follow(ReceiverId),
    Stop,
    Mute,
    Pause,
    Squelch,
    Target(ReceiverId),
    ExportEmitters,
    Workspace(String),
    Show(Tab),
    PopOut(Tab),
    Priority(u32),
    Lockout(u32),
    Reset,
}

/// Per-frame facts the receiver panes share (live spans for the survey's
/// brackets, the next tune target).
#[derive(Default)]
struct FrameInfo {
    live_spans: Vec<(ReceiverId, (f64, f64))>,
    spans: Vec<(f64, f64)>,
    next_target: Option<ReceiverId>,
}

/// Command line: `--sim` (simulator only), `--freq <MHz>` (focus centre),
/// `--serial <hex>` (focus device), `--no-sweep`, `--sweep <lo> <hi>` (MHz),
/// `--tab emitters|records|trunking|calls|pages` (the table shown first).
struct Args {
    sim: bool,
    freq_mhz: Option<f64>,
    serial: Option<String>,
    sweep: bool,
    sweep_range: Option<(f64, f64)>,
    tab: Option<Tab>,
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
                    Some("records") => Some(Tab::Records),
                    Some("p25") | Some("trunking") => Some(Tab::Trunking),
                    Some("pages") => Some(Tab::Pages),
                    Some("calls") => Some(Tab::Calls),
                    Some("diagnostics") | Some("diag") => Some(Tab::Diagnostics),
                    _ => Some(Tab::Emitters),
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
            dock: None,
            dock_requests: Vec::new(),
            popouts: Vec::new(),
            next_popout: 1,
            window: None,
            maximize: true,
            workspace_name: None,
            workspace_new_name: String::new(),
            calls_view: CallsView::Calls,
            follow_add: 0,
            palette_open: false,
            palette_text: String::new(),
            palette_sel: 0,
            palette_focus: false,
            muted_volume: 0.0,
            drawing: None,
            popout_names: Vec::new(),
            start_tab: args.tab.clone(),
            frame: FrameInfo::default(),
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
            settings: settings::Settings::load(),
            sweep_dirty: None,
            log_filter: String::new(),
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
        // The last layout, if there is one (else the default, built on the
        // first frame when the window's width is known).
        if let Some(w) = load_workspace() {
            let t = ReceiverId(w.tune_target);
            if app.receiver(t).is_some() {
                app.tune_target = t;
            }
            app.dock = Some(w.main);
            app.window = w.window;
            app.maximize = w.window.is_none_or(|r| r.maximized);
            app.popouts = w.popouts.into_iter().filter(|p| p.dock.iter_all_tabs().next().is_some()).collect();
            app.next_popout = app.popouts.iter().map(|p| p.id + 1).max().unwrap_or(1);
            let open: Vec<ReceiverId> = app.panes.iter().map(|p| p.id).collect();
            app.dock_requests.extend(open.into_iter().map(DockReq::AddReceiver));
            if let Some(t) = app.start_tab.take() {
                app.dock_requests.push(DockReq::Show(t));
            }
        }
        app
    }

    fn workspace(&self) -> Option<Workspace> {
        let main = self.dock.as_ref()?;
        Some(Workspace {
            main: main.clone(),
            tune_target: self.tune_target.0,
            popouts: self.popouts.iter().map(|p| PopOut { id: p.id, title: p.title.clone(), dock: p.dock.clone(), rect: p.rect }).collect(),
            window: self.window,
        })
    }

    /// Notes the main window's geometry (skipped while minimized, when
    /// Windows reports it off-screen).
    fn track_window(&mut self, ctx: &egui::Context) {
        let (outer, inner, maximized, minimized) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized.unwrap_or(false), v.minimized.unwrap_or(false))
        });
        if minimized {
            return;
        }
        let Some((outer, inner)) = outer.zip(inner) else { return };
        match (&mut self.window, maximized) {
            (Some(w), true) => w.maximized = true,
            // A maximized window's own rect hangs off the screen edge; what's
            // noted is where it would restore to.
            (w, max) => *w = Some(WindowRect { pos: [outer.min.x.max(0.0), outer.min.y.max(0.0)], size: [inner.width(), inner.height()], maximized: max }),
        }
    }

    /// Writes the layout as `last.json` (what the next start loads).
    fn save_workspace(&self) {
        if let Some(w) = self.workspace() {
            write_workspace(&workspace_path(), &w);
        }
    }

    fn save_workspace_as(&mut self, name: &str) {
        let name = engine::sigmf::sanitize(name.trim());
        if name.is_empty() {
            return;
        }
        if let Some(w) = self.workspace() {
            write_workspace(&workspace_dir().join(format!("{name}.json")), &w);
            self.workspace_name = Some(name);
        }
    }

    /// Switches to a saved workspace, or a preset, by name.
    fn load_workspace_named(&mut self, name: &str, width: f32) {
        let ids: Vec<ReceiverId> = self.panes.iter().map(|p| p.id).collect();
        let w = match read_workspace(&workspace_dir().join(format!("{name}.json"))) {
            Some(w) => w,
            None if PRESETS.contains(&name) => Workspace { main: preset_dock(name, width, &ids), tune_target: self.tune_target.0, popouts: Vec::new(), window: self.window },
            None => return,
        };
        self.dock = Some(w.main);
        self.popouts = w.popouts.into_iter().filter(|p| p.dock.iter_all_tabs().next().is_some()).collect();
        self.next_popout = self.popouts.iter().map(|p| p.id + 1).max().unwrap_or(1);
        self.dock_requests.extend(ids.into_iter().map(DockReq::AddReceiver));
        self.workspace_name = Some(name.to_string());
    }

    /// The workspace menu: the layout in use, saved and preset layouts,
    /// and the panels that can be shown.
    fn workspace_menu(&mut self, ui: &mut egui::Ui, width: f32) {
        ui.set_min_width(220.0);
        let current = self.workspace_name.clone();
        ui.label(egui::RichText::new(format!("Workspace: {}", current.as_deref().unwrap_or("unsaved"))).small().color(theme::MUTED));
        if let Some(n) = &current {
            if ui.button(format!("Save \"{n}\"")).clicked() {
                self.save_workspace_as(&n.clone());
                ui.close();
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.workspace_new_name).desired_width(120.0).hint_text("name"));
            if ui.button("Save as").clicked() && !self.workspace_new_name.trim().is_empty() {
                let n = self.workspace_new_name.clone();
                self.save_workspace_as(&n);
                self.workspace_new_name.clear();
                ui.close();
            }
        });
        ui.separator();
        let saved = list_workspaces();
        let mut load: Option<String> = None;
        let mut delete: Option<String> = None;
        for n in &saved {
            ui.horizontal(|ui| {
                if ui.selectable_label(current.as_deref() == Some(n.as_str()), n).clicked() {
                    load = Some(n.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖").on_hover_text("Delete this workspace").clicked() {
                        delete = Some(n.clone());
                    }
                });
            });
        }
        for n in PRESETS {
            if !saved.iter().any(|s| s == n) && ui.selectable_label(current.as_deref() == Some(n), format!("{n} (preset)")).clicked() {
                load = Some(n.to_string());
            }
        }
        if let Some(n) = load {
            self.load_workspace_named(&n, width);
            ui.close();
        }
        if let Some(n) = delete {
            let _ = std::fs::remove_file(workspace_dir().join(format!("{n}.json")));
            if current.as_deref() == Some(n.as_str()) {
                self.workspace_name = None;
            }
        }
        ui.separator();
        if ui.button("Reset layout").on_hover_text("The default arrangement for this window width").clicked() {
            self.dock_requests.push(DockReq::Reset);
            self.workspace_name = None;
            ui.close();
        }
        let next = self.next_live_id();
        if ui.button(format!("Add receiver slot {next}")).clicked() {
            self.dock_requests.push(DockReq::AddReceiver(next));
            ui.close();
        }
        let present: Vec<Tab> = self.dock.as_ref().map(|d| d.iter_all_tabs().map(|(_, t)| t.clone()).collect()).unwrap_or_default();
        let present: Vec<Tab> = present.into_iter().chain(self.popouts.iter().flat_map(|p| p.dock.iter_all_tabs().map(|(_, t)| t.clone()))).collect();
        for t in Tab::PANELS {
            if !present.contains(&t) && ui.button(format!("Show {}", t.name())).clicked() {
                self.dock_requests.push(DockReq::Show(t));
                ui.close();
            }
        }
        ui.label(egui::RichText::new("Drag a tab to move it, or off the strip to float it;\nright-click a tab to pop it out to its own window").small().color(theme::MUTED));
    }

    /// Priority and lockout talkgroups, editable.
    fn follow_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        ui.label(egui::RichText::new("Call following").small().color(theme::MUTED));
        let mut toggle: Option<(u32, bool)> = None;
        for (list, mark, what, priority) in [
            (self.follow_policy.priority.clone(), "★", "Priority (followed first, interrupting others)", true),
            (self.follow_policy.lockout.clone(), "⊘", "Locked out (never followed)", false),
        ] {
            ui.label(egui::RichText::new(format!("{mark} {what}")).small());
            if list.is_empty() {
                ui.label(egui::RichText::new("none").small().color(theme::MUTED));
            }
            ui.horizontal_wrapped(|ui| {
                for tg in list {
                    if ui.small_button(format!("TG {tg} ✖")).on_hover_text("Remove").clicked() {
                        toggle = Some((tg, priority));
                    }
                }
            });
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.follow_add).range(0..=65535).prefix("TG "));
            if ui.small_button("★ add").clicked() && !self.follow_policy.priority.contains(&self.follow_add) {
                toggle = Some((self.follow_add, true));
            }
            if ui.small_button("⊘ add").clicked() && !self.follow_policy.lockout.contains(&self.follow_add) {
                toggle = Some((self.follow_add, false));
            }
        });
        ui.label(egui::RichText::new("Also: right-click a call in the Calls tab, or select a talkgroup").small().color(theme::MUTED));
        if let Some((tg, priority)) = toggle {
            self.toggle_policy(tg, priority);
        }
    }

    /// Where SpecTrawl keeps its files.
    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(340.0);
        let mut changed = false;
        ui.label(egui::RichText::new("Look-back").small().color(theme::MUTED));
        let rate = self.panes.iter().map(|p| p.source.snapshot().sample_rate).fold(0.0, f64::max).max(2.5e6);
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(&mut self.settings.ring_seconds).speed(1.0).range(2.0..=600.0).suffix(" s")).changed();
            let per = self.settings.ring_bytes(rate) / 1e9;
            let n = self.panes.len().max(1);
            ui.label(egui::RichText::new(format!("≈ {per:.2} GB per device at {:.0} MS/s · {:.1} GB for {n}", rate / 1e6, per * n as f64)).small().color(theme::MUTED));
        });
        ui.horizontal(|ui| {
            let cur: Vec<f64> = self.panes.iter().map(|p| p.source.config().ring_seconds).collect();
            let stale = cur.iter().any(|s| (*s - self.settings.ring_seconds).abs() > 0.5);
            ui.label(egui::RichText::new("Applies to devices opened from now on.").small().color(theme::MUTED));
            if stale && ui.small_button("Reopen devices now").on_hover_text("Restarts every open device with the new ring length (emitters and records start over)").clicked() {
                self.reopen_all();
                ui.close();
            }
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Keyboard nudges").small().color(theme::MUTED));
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(&mut self.settings.nudge_khz).speed(5.0).range(1.0..=5000.0).suffix(" kHz")).changed();
            ui.label(egui::RichText::new("← → step the target receiver (Shift ×10, Alt ÷10)").small().color(theme::MUTED));
        });
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(&mut self.settings.survey_nudge_mhz).speed(0.5).range(0.1..=100.0).suffix(" MHz")).changed();
            ui.label(egui::RichText::new("Ctrl ← → shift the survey span, Ctrl ↑ ↓ widen / narrow it").small().color(theme::MUTED));
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Logging").small().color(theme::MUTED));
        let mut levels = self.settings.log.clone();
        let mut lv_changed = false;
        egui::Grid::new("log_levels").num_columns(4).spacing([10.0, 2.0]).show(ui, |ui| {
            let names = ["off", "error", "warn", "info", "debug", "trace"];
            let combo = |ui: &mut egui::Ui, id: &str, cur: &mut log::LevelFilter| -> bool {
                let mut ch = false;
                egui::ComboBox::from_id_salt(id).selected_text(logging::level_str(*cur)).width(70.0).show_ui(ui, |ui| {
                    for n in names {
                        let l = logging::parse_level(n).unwrap();
                        ch |= ui.selectable_value(cur, l, n).changed();
                    }
                });
                ch
            };
            ui.label(egui::RichText::new("default").small());
            lv_changed |= combo(ui, "lvl_default", &mut levels.default);
            ui.end_row();
            for (k, area) in logging::AREAS.iter().enumerate() {
                let mut l = levels.level(area);
                ui.label(egui::RichText::new(*area).small());
                if combo(ui, &format!("lvl_{area}"), &mut l) {
                    levels.set(area, l);
                    lv_changed = true;
                }
                if k % 2 == 1 {
                    ui.end_row();
                }
            }
        });
        if lv_changed {
            self.settings.log = levels;
            logging::set_levels(self.settings.log.clone());
            changed = true;
        }
        if changed {
            self.settings.save();
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Folders").small().color(theme::MUTED));
        for (what, path) in [
            ("Recordings", engine::recordings_dir()),
            ("Captures (SigMF, CSV)", engine::sigmf::default_dir()),
            ("Workspaces", workspace_dir()),
            ("Names catalog", catalog_path()),
            ("Follow lists", follow_policy_path()),
            ("Logs", logging::log_dir()),
            ("Settings", settings::path()),
        ] {
            ui.horizontal(|ui| {
                if ui.small_button("Open").clicked() {
                    let dir = if path.is_dir() || path.extension().is_none() { path.clone() } else { path.parent().map(|p| p.to_path_buf()).unwrap_or(path.clone()) };
                    let _ = std::fs::create_dir_all(&dir);
                    open_path(&dir);
                }
                ui.label(egui::RichText::new(what).small());
                ui.label(egui::RichText::new(path.display().to_string()).small().color(theme::MUTED));
            });
        }
    }

    fn toggle_mute(&mut self) {
        if self.volume > 0.0 {
            self.muted_volume = self.volume;
            self.volume = 0.0;
        } else {
            self.volume = if self.muted_volume > 0.0 { self.muted_volume } else { 0.6 };
        }
        if let Some(a) = &self.audio_out {
            a.set_volume(self.volume);
        }
    }

    /// The commands matching `q`: fixed ones whose label or keywords hold
    /// every word of it, plus those parsed from a number in it (a frequency
    /// or a talkgroup) and a receiver letter.
    fn palette_commands(&self, q: &str) -> Vec<(String, Cmd)> {
        let ql = q.to_lowercase();
        let words: Vec<&str> = ql.split_whitespace().collect();
        let num = words.iter().find_map(|w| w.parse::<f64>().ok());
        let letter = words
            .iter()
            .filter(|w| w.len() == 1)
            .filter_map(|w| w.chars().next())
            .filter(|c| c.is_ascii_alphabetic())
            .map(|c| ReceiverId(c.to_ascii_uppercase()))
            .find(|r| self.receiver(*r).is_some());
        let work = letter.filter(|r| !r.is_sweep()).unwrap_or(self.working_id());
        let live = self.live_ids();
        let mut v: Vec<(String, &str, Cmd)> = Vec::new();
        if let Some(n) = num {
            if (24.0..=1800.0).contains(&n) && self.receiver(work).is_some() {
                v.push((format!("Tune {work} to {n:.4} MHz"), "tune", Cmd::Tune(work, n)));
                let spec = self.spec_for(n * 1e6, "", 0.0);
                v.push((format!("Listen on {work} at {n:.4} MHz ({})", spec.mode.name()), "listen", Cmd::Listen(work, spec)));
            }
            if n.fract() == 0.0 && (0.0..=65535.0).contains(&n) {
                let tg = n as u32;
                v.push((format!("Priority: TG {tg}"), "priority star follow", Cmd::Priority(tg)));
                v.push((format!("Lock out: TG {tg}"), "lockout lock out", Cmd::Lockout(tg)));
            }
        }
        for id in &live {
            v.push((format!("Follow calls on {id}"), "follow calls p25 trunk", Cmd::Follow(*id)));
            v.push((format!("Tune target: {id}"), "target click tunes", Cmd::Target(*id)));
        }
        v.push(("Stop listening".into(), "stop audio", Cmd::Stop));
        v.push((if self.volume > 0.0 { "Mute" } else { "Unmute" }.into(), "mute unmute volume", Cmd::Mute));
        v.push((if self.paused { "Resume" } else { "Pause" }.into(), "pause resume freeze", Cmd::Pause));
        v.push((format!("Squelch known: {}", if self.squelch { "off" } else { "on" }), "squelch known fm tv lte", Cmd::Squelch));
        v.push(("Export emitters and records (CSV)".into(), "export csv emitters records", Cmd::ExportEmitters));
        v.push(("Reset layout".into(), "reset layout", Cmd::Reset));
        for t in Tab::PANELS {
            v.push((format!("Show {}", t.name()), "show panel tab", Cmd::Show(t.clone())));
            v.push((format!("Pop out {}", t.name()), "pop out window", Cmd::PopOut(t)));
        }
        for id in self.panes.iter().map(|p| p.id) {
            v.push((format!("Pop out receiver {id}"), "pop out window receiver", Cmd::PopOut(Tab::Receiver(id))));
        }
        let next = self.next_live_id();
        for d in self.devices.iter().filter(|d| !self.panes.iter().any(|p| same_device(p.source.info(), d))) {
            v.push((format!("Open {} ({}) as {next}", d.label, d.serial), "open device focus live", Cmd::Open(d.clone(), next)));
            v.push((format!("Open {} ({}) as survey", d.label, d.serial), "open device survey sweep", Cmd::Open(d.clone(), ReceiverId::SWEEP)));
        }
        let mut names = list_workspaces();
        for p in PRESETS {
            if !names.iter().any(|n| n == p) {
                names.push(p.to_string());
            }
        }
        for n in names {
            v.push((format!("Workspace: {n}"), "workspace layout load", Cmd::Workspace(n)));
        }
        let filter: Vec<&str> = words.iter().copied().filter(|w| w.parse::<f64>().is_err() && letter.is_none_or(|l| *w != l.0.to_ascii_lowercase().to_string())).collect();
        v.into_iter()
            .filter(|(label, keys, _)| {
                let hay = format!("{} {keys}", label.to_lowercase());
                filter.iter().all(|w| hay.contains(w))
            })
            .map(|(label, _, c)| (label, c))
            .collect()
    }

    fn run_command(&mut self, cmd: Cmd, width: f32) {
        match cmd {
            Cmd::Tune(rx, mhz) => self.retune(rx, mhz),
            Cmd::Open(d, id) => self.open_as(&d, id),
            Cmd::Listen(rx, spec) => self.start_listen(rx, spec),
            Cmd::Follow(rx) => {
                let hz = self.receiver(rx).and_then(|p| p.view.span()).map(|(lo, hi)| (lo + hi) / 2.0).unwrap_or(self.listen_mhz * 1e6);
                self.start_listen(rx, engine::ListenSpec { center_hz: hz, mode: engine::DemodMode::P25, follow: true, tdma: false, slot: None });
            }
            Cmd::Stop => {
                if let Some(pb) = &self.playback {
                    pb.stop();
                } else {
                    self.stop_listen();
                }
            }
            Cmd::Mute => self.toggle_mute(),
            Cmd::Pause => self.paused = !self.paused,
            Cmd::Squelch => self.squelch = !self.squelch,
            Cmd::Target(id) => self.tune_target = id,
            Cmd::ExportEmitters => {
                self.export_msg = Some(match export_emitters(&self.panes) {
                    Ok((path, e, r)) => format!("Exported {e} emitters, {r} records to {}", path.display()),
                    Err(err) => format!("Emitter export failed: {err}"),
                });
            }
            Cmd::Workspace(n) => self.load_workspace_named(&n, width),
            Cmd::Show(t) => self.dock_requests.push(DockReq::Show(t)),
            Cmd::PopOut(t) => self.dock_requests.push(DockReq::PopOut(t)),
            Cmd::Priority(tg) => {
                if !self.follow_policy.priority.contains(&tg) {
                    self.toggle_policy(tg, true);
                }
            }
            Cmd::Lockout(tg) => {
                if !self.follow_policy.lockout.contains(&tg) {
                    self.toggle_policy(tg, false);
                }
            }
            Cmd::Reset => {
                self.dock_requests.push(DockReq::Reset);
                self.workspace_name = None;
            }
        }
    }

    /// The command palette: a text box and the commands it matches; Enter
    /// runs the highlighted one, Esc closes.
    fn palette(&mut self, ctx: &egui::Context, width: f32) {
        if !self.palette_open {
            return;
        }
        let cmds = self.palette_commands(&self.palette_text.clone());
        let n = cmds.len().min(12);
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if up {
            self.palette_sel = self.palette_sel.saturating_sub(1);
        }
        if down && n > 0 {
            self.palette_sel = (self.palette_sel + 1).min(n - 1);
        }
        let mut run: Option<usize> = None;
        let mut close = esc;
        egui::Window::new("palette")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .fixed_size([600.0, 0.0])
            .anchor(egui::Align2::CENTER_TOP, [0.0, 72.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.palette_text)
                        .desired_width(f32::INFINITY)
                        .hint_text("tune a 772.8 · listen 155.475 · follow a · priority 1042 · workspace trunking · show pages"),
                );
                if self.palette_focus {
                    r.request_focus();
                    self.palette_focus = false;
                }
                if r.changed() {
                    self.palette_sel = 0;
                }
                ui.separator();
                if cmds.is_empty() {
                    ui.label(egui::RichText::new("No matching command").small().color(theme::MUTED));
                }
                for (k, (label, _)) in cmds.iter().take(12).enumerate() {
                    if ui.selectable_label(k == self.palette_sel, label).clicked() {
                        run = Some(k);
                    }
                }
                if enter && n > 0 {
                    run = Some(self.palette_sel.min(n - 1));
                }
            });
        if let Some(k) = run {
            if let Some((_, cmd)) = cmds.into_iter().nth(k) {
                self.run_command(cmd, width);
            }
            close = true;
        }
        if close {
            self.palette_open = false;
            self.palette_text.clear();
            self.palette_sel = 0;
        }
    }

    /// The icon rail: devices, names, follow lists, workspaces, settings.
    fn rail(&mut self, ui: &mut egui::Ui, width: f32) {
        ui.add_space(4.0);
        ui.spacing_mut().button_padding = egui::vec2(4.0, 4.0);
        ui.vertical_centered(|ui| {
            let icon = |ui: &mut egui::Ui, glyph: &str, tip: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.menu_button(egui::RichText::new(glyph).size(15.0), |ui| add(ui)).response.on_hover_text(tip);
                ui.add_space(6.0);
            };
            icon(ui, "📡", "Devices", &mut |ui| {
                ui.set_min_width(280.0);
                self.sources_panel(ui);
            });
            icon(ui, "🏷", "Imported names", &mut |ui| self.import_menu(ui));
            icon(ui, "★", "Follow lists", &mut |ui| self.follow_menu(ui));
            icon(ui, "☰", "Workspaces", &mut |ui| self.workspace_menu(ui, width));
            icon(ui, "⚙", "Settings", &mut |ui| self.settings_menu(ui));
        });
    }

    /// Applies queued dock changes (the dock must be home, not on loan).
    fn apply_dock_requests(&mut self, width: f32) {
        if self.dock_requests.is_empty() {
            return;
        }
        let reqs = std::mem::take(&mut self.dock_requests);
        if self.dock.is_none() {
            return;
        }
        for r in reqs {
            match r {
                DockReq::Reset => {
                    let ids: Vec<ReceiverId> = self.panes.iter().map(|p| p.id).collect();
                    self.dock = Some(default_dock(width, &ids));
                    self.popouts.clear();
                }
                DockReq::Show(tab) => {
                    let dock = self.dock.as_mut().unwrap();
                    match dock.find_tab(&tab) {
                        Some(path) => {
                            let _ = dock.set_active_tab(path);
                        }
                        None if self.popouts.iter().any(|p| p.dock.find_tab(&tab).is_some()) => {}
                        None => place_tab(dock, tab),
                    }
                }
                DockReq::Remove(tab) => self.take_tab(&tab),
                DockReq::AddReceiver(id) => {
                    let tab = Tab::Receiver(id);
                    if self.dock.as_ref().unwrap().find_tab(&tab).is_none() && !self.popouts.iter().any(|p| p.dock.find_tab(&tab).is_some()) {
                        place_tab(self.dock.as_mut().unwrap(), tab);
                    }
                }
                DockReq::PopOut(tab) => {
                    self.take_tab(&tab);
                    let id = self.next_popout;
                    self.next_popout += 1;
                    self.popouts.push(PopOut { id, title: tab.name(), dock: DockState::new(vec![tab]), rect: None });
                }
                DockReq::MoveTo(tab, to) => {
                    self.take_tab(&tab);
                    match to.and_then(|id| self.popouts.iter_mut().find(|p| p.id == id)) {
                        Some(po) => po.dock.push_to_first_leaf(tab),
                        None => place_tab(self.dock.as_mut().unwrap(), tab),
                    }
                }
            }
        }
        self.save_workspace();
    }

    /// Removes a tab from wherever it is; a pop-out left empty closes.
    fn take_tab(&mut self, tab: &Tab) {
        if let Some(d) = self.dock.as_mut() {
            if let Some(p) = d.find_tab(tab) {
                d.remove_tab(p);
            }
        }
        for po in &mut self.popouts {
            if let Some(p) = po.dock.find_tab(tab) {
                po.dock.remove_tab(p);
            }
        }
        self.popouts.retain(|po| po.dock.iter_all_tabs().next().is_some());
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
        let segs = self.settings.survey_segments.iter().map(|(lo, hi)| (lo * 1e6, hi * 1e6)).collect();
        SweepPlan::with_segments(self.sweep_lo_mhz * 1e6, self.sweep_hi_mhz * 1e6, segs)
    }

    /// The survey's segment list: editable ranges within the span; empty
    /// means the whole span. Returns true when Apply was clicked.
    fn segments_ui(&mut self, ui: &mut egui::Ui) -> bool {
        ui.set_min_width(300.0);
        ui.label(egui::RichText::new("Survey segments (MHz). Empty = the whole span.").small().color(theme::MUTED));
        let (lo_lim, hi_lim) = (self.sweep_lo_mhz, self.sweep_hi_mhz);
        let mut remove: Option<usize> = None;
        let mut changed = false;
        for (k, (lo, hi)) in self.settings.survey_segments.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= ui.add(egui::DragValue::new(lo).speed(0.5).range(lo_lim..=hi_lim).max_decimals(3)).changed();
                ui.label("–");
                changed |= ui.add(egui::DragValue::new(hi).speed(0.5).range(lo_lim..=hi_lim).max_decimals(3)).changed();
                if ui.small_button("✕").clicked() {
                    remove = Some(k);
                }
            });
        }
        if let Some(k) = remove {
            self.settings.survey_segments.remove(k);
            changed = true;
        }
        let mut apply = false;
        ui.horizontal(|ui| {
            if ui.small_button("+ Add").clicked() {
                let last = self.settings.survey_segments.last().map(|s| s.1).unwrap_or(lo_lim);
                let lo = last.max(lo_lim).min(hi_lim - 1.0);
                self.settings.survey_segments.push((lo, (lo + 10.0).min(hi_lim)));
                changed = true;
            }
            if !self.settings.survey_segments.is_empty() && ui.small_button("Clear").clicked() {
                self.settings.survey_segments.clear();
                changed = true;
            }
            if ui.small_button("Apply").clicked() {
                apply = true;
            }
        });
        if changed {
            self.settings.save();
        }
        apply
    }

    /// Restarts the sweeper on the current range.
    fn apply_sweep(&mut self) {
        if self.sweep_hi_mhz <= self.sweep_lo_mhz + 1.0 {
            self.sweep_hi_mhz = self.sweep_lo_mhz + 10.0;
        }
        let plan = self.sweep_plan();
        if let Some(p) = self.receiver_mut(ReceiverId::SWEEP) {
            p.source.set_mode(Mode::Sweep(plan));
            p.view = survey_view();
        }
    }

    fn close(&mut self, info: &DeviceInfo) {
        if self.panes.iter().any(|p| same_device(p.source.info(), info)) {
            log::info!("close {} ({})", info.label, info.serial);
        }
        self.panes.retain(|p| !same_device(p.source.info(), info));
    }

    /// Reopens every open device (after a change to the ring length).
    fn reopen_all(&mut self) {
        let open: Vec<(DeviceInfo, ReceiverId, f64)> = self.panes.iter().map(|p| (p.source.info().clone(), p.id, p.source.snapshot().center_hz)).collect();
        for (d, id, hz) in open {
            self.open_as(&d, id);
            if !id.is_sweep() {
                self.retune(id, hz / 1e6);
            }
        }
    }

    /// Steps a focus receiver's centre by `steps` nudges.
    fn nudge(&mut self, id: ReceiverId, steps: f64) {
        let Some(p) = self.receiver(id) else { return };
        let mhz = p.source.snapshot().center_hz / 1e6 + steps * self.settings.nudge_khz / 1e3;
        self.retune(id, (mhz.clamp(24.0, 1800.0) * 1e6).round() / 1e6);
    }

    /// Shifts (`shift`) or widens (`grow`) the survey span by nudges; the
    /// sweep restarts once the keys have rested for a moment.
    fn nudge_sweep(&mut self, shift: f64, grow: f64) {
        let d = self.settings.survey_nudge_mhz;
        let (mut lo, mut hi) = (self.sweep_lo_mhz + shift * d - grow * d, self.sweep_hi_mhz + shift * d + grow * d);
        lo = lo.clamp(24.0, 1790.0);
        hi = hi.clamp(lo + 1.0, 1800.0);
        if (lo, hi) != (self.sweep_lo_mhz, self.sweep_hi_mhz) {
            self.sweep_lo_mhz = lo;
            self.sweep_hi_mhz = hi;
            self.sweep_dirty = Some(std::time::Instant::now());
        }
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
            Source::start(dev, SourceConfig { ring_seconds: self.settings.ring_seconds, ..SourceConfig::default() }, mode)
        });
        log::info!("open {} ({}) as {id}: {}", info.label, info.serial, if result.is_ok() { "ok" } else { "failed" });
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
                self.dock_requests.push(DockReq::AddReceiver(id));
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
                let width = ui.ctx().content_rect().width();
                let ws = format!("Workspace: {} ⏷", self.workspace_name.as_deref().unwrap_or("default"));
                ui.menu_button(ws, |ui| self.workspace_menu(ui, width));
                if ui.button("Ctrl+K").on_hover_text("Command palette: tune, listen, follow, open, priority, workspace, show…").clicked() {
                    self.palette_open = true;
                    self.palette_focus = true;
                }
                if let Some((t, m)) = &self.catalog_msg {
                    if t.elapsed() < std::time::Duration::from_secs(12) {
                        ui.label(egui::RichText::new(m).small().color(theme::TEXT2));
                    }
                }
                if let Some(e) = &self.error {
                    ui.label(egui::RichText::new(e).small().color(theme::ALERT));
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

    /// Devices, and which receiver slot each one fills.
    fn sources_panel(&mut self, ui: &mut egui::Ui) {
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

        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x70, 0x96), e);
        }
    }

    /// The demodulation a signal's label suggests.
    fn auto_mode(&self, hz: f64, label: &str, bw: f64) -> engine::DemodMode {
        let broadcast = self.known.1.iter().any(|(lo, hi, k)| (*lo..=*hi).contains(&hz) && k.starts_with("FM broadcast"));
        if broadcast || label.contains("Wideband FM") || bw > 100e3 {
            engine::DemodMode::Wfm
        } else if label.contains("P25") && !label.contains("Phase 2") {
            engine::DemodMode::P25
        } else if label.starts_with("AM") || label.contains(" AM") {
            engine::DemodMode::Am
        } else {
            engine::DemodMode::Nfm
        }
    }

    /// How to listen to a signal: the chosen mode, else the one its label
    /// suggests.
    fn spec_for(&self, hz: f64, label: &str, bw: f64) -> engine::ListenSpec {
        let tdma = if self.listen_mode.is_none() { label.contains("Phase 2") } else { self.listen_tdma };
        let mode = self.listen_mode.unwrap_or_else(|| if tdma { engine::DemodMode::P25 } else { self.auto_mode(hz, label, bw) });
        engine::ListenSpec { center_hz: hz, mode, follow: false, tdma, slot: None }
    }

    /// Plays receiver `rx` demodulated as `spec` (retuning it there first if
    /// the channel is outside its span). One audio output: any other
    /// receiver's listener and a playing recording stop.
    fn start_listen(&mut self, rx: ReceiverId, spec: engine::ListenSpec) {
        if let Some(focus) = self.receiver(rx).filter(|_| !spec.follow) {
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

    fn stop_listen(&mut self) {
        for p in self.panes.iter().filter(|p| p.source.listening().is_some()) {
            p.source.listen(None);
        }
        if let Some(a) = &self.audio_out {
            a.set_source(None);
        }
    }

    /// What's playing, on one line: live listening or a recording, with
    /// the mode popover, volume and Stop; the engine summary at the right.
    fn transport_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let listening: Option<(ReceiverId, engine::ListenSpec)> = self.panes.iter().find_map(|p| p.source.listening().map(|l| (p.id, l)));
        let rx = listening.map(|(r, _)| r).unwrap_or(self.working_id());
        let mut start: Option<(ReceiverId, engine::ListenSpec)> = None;
        let mut stop = false;
        ui.horizontal_centered(|ui| {
            if let Some(pb) = &self.playback {
                let name = pb.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                ui.label(egui::RichText::new(format!("♪ {name} · {:.1} s", pb.seconds)).monospace().small().color(theme::VOICE));
            } else if let Some((rx, l)) = &listening {
                theme::chip(ui, &rx.to_string(), theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG);
                if let Some(p) = self.receiver(*rx) {
                    if l.mode == engine::DemodMode::P25 {
                        let v = p.source.voice();
                        let what = match (v.center_hz, v.slot) {
                            (Some(hz), Some(ts)) => format!("{:.5} MHz · P25 Phase 2 TS{}", hz / 1e6, ts + 1),
                            (Some(hz), None) if l.tdma => format!("{:.5} MHz · P25 Phase 2", hz / 1e6),
                            (Some(hz), None) => format!("{:.5} MHz · P25", hz / 1e6),
                            (None, _) => "waiting for a granted call".into(),
                        };
                        let head = if l.follow { format!("Following · {what}") } else { what };
                        ui.label(egui::RichText::new(head).monospace().small().strong());
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
                            ui.label(egui::RichText::new("● rec").small().color(theme::ALERT));
                        }
                    } else {
                        let open = p.source.audio().open.load(std::sync::atomic::Ordering::Relaxed);
                        let (txt, col) = if open { ("● OPEN", theme::VOICE) } else { ("○ squelched", theme::MUTED) };
                        ui.label(egui::RichText::new(format!("{:.4} MHz · {}", l.center_hz / 1e6, l.mode.name())).monospace().small().strong());
                        ui.label(egui::RichText::new(txt).small().color(col));
                    }
                }
            } else {
                ui.label(egui::RichText::new("■ not listening").small().color(theme::MUTED));
            }
            if let Some(e) = &self.audio_err {
                ui.label(egui::RichText::new(e).small().color(theme::ALERT));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (mut ring_mb, mut gaps) = (0.0, 0);
                for p in &self.panes {
                    let st = p.source.stats();
                    ring_mb += st.ring_capacity as f64 * 4.0 / 1e6;
                    gaps += st.gaps;
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{} rx · rings {:.0} MB · drops {gaps} · {:.0} fps",
                        self.panes.len(),
                        ring_mb,
                        1.0 / ctx.input(|i| i.stable_dt).max(1e-3)
                    ))
                    .monospace()
                    .small()
                    .color(theme::MUTED),
                );
                ui.separator();
                if ui.add_enabled(listening.is_some() || self.playback.is_some(), egui::Button::new("Stop").small()).clicked() {
                    stop = true;
                }
                ui.spacing_mut().slider_width = 80.0;
                if ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.5).show_value(false)).changed() {
                    if let Some(a) = &self.audio_out {
                        a.set_volume(self.volume);
                    }
                }
                ui.label(egui::RichText::new("vol").small().color(theme::MUTED));
                let mode_name = self.listen_mode.map(|m| m.name()).unwrap_or("Auto");
                ui.menu_button(egui::RichText::new(format!("{mode_name} ⏷")).small(), |ui| {
                    ui.set_min_width(240.0);
                    ui.label(egui::RichText::new(format!("Mode (listening on {rx})")).small().color(theme::MUTED));
                    ui.horizontal(|ui| {
                        for (m, name) in [
                            (None, "Auto"),
                            (Some(engine::DemodMode::Nfm), "NFM"),
                            (Some(engine::DemodMode::Am), "AM"),
                            (Some(engine::DemodMode::Wfm), "WFM"),
                            (Some(engine::DemodMode::P25), "P25"),
                        ] {
                            if ui.selectable_value(&mut self.listen_mode, m, egui::RichText::new(name).small()).changed() {
                                // Re-apply to what's playing.
                                if let Some((r, l)) = listening.filter(|(_, l)| !l.follow) {
                                    let mode = m.unwrap_or(l.mode);
                                    start = Some((r, engine::ListenSpec { center_hz: l.center_hz, mode, follow: false, tdma: self.listen_tdma, slot: None }));
                                }
                            }
                        }
                    });
                    if self.listen_mode == Some(engine::DemodMode::P25) {
                        ui.checkbox(&mut self.listen_tdma, egui::RichText::new("Phase 2").small())
                            .on_hover_text("TDMA: plays whichever timeslot starts talking");
                    }
                    ui.separator();
                    let following = listening.is_some_and(|(_, l)| l.follow);
                    if ui
                        .add_enabled(!following, egui::Button::new(format!("Follow calls on {rx}")))
                        .on_hover_text("Play P25 calls (Phase 1 and 2) as the control channel grants them (the receiver on the control channel's span)")
                        .clicked()
                    {
                        let hz = self.receiver(rx).and_then(|p| p.view.span()).map(|(lo, hi)| (lo + hi) / 2.0).unwrap_or(self.listen_mhz * 1e6);
                        start = Some((rx, engine::ListenSpec { center_hz: hz, mode: engine::DemodMode::P25, follow: true, tdma: false, slot: None }));
                        ui.close();
                    }
                    let fp = &self.follow_policy;
                    if !fp.priority.is_empty() || !fp.lockout.is_empty() {
                        let list = |s: &std::collections::BTreeSet<u32>| s.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
                        ui.label(egui::RichText::new(format!("★ {}  ⊘ {}", list(&fp.priority), list(&fp.lockout))).small().color(theme::MUTED))
                            .on_hover_text("Priority (★) and locked-out (⊘) talkgroups; right-click a call in the Calls tab to change");
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.listen_mhz).speed(0.0125).range(24.0..=1800.0).max_decimals(4).suffix(" MHz"));
                        if ui.button("Listen here").clicked() {
                            let hz = self.listen_mhz * 1e6;
                            start = Some((rx, self.spec_for(hz, "", 0.0)));
                            ui.close();
                        }
                    });
                    if ui.small_button("Recordings folder").on_hover_text(engine::recordings_dir().display().to_string()).clicked() {
                        let dir = engine::recordings_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        open_path(&dir);
                        ui.close();
                    }
                });
            });
        });
        if let Some((rx, spec)) = start {
            self.start_listen(rx, spec);
        }
        if stop {
            if let Some(pb) = &self.playback {
                pb.stop();
            } else {
                self.stop_listen();
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
            .map(|k| (k.occupant.lo_hz, k.occupant.hi_hz, k.label()))
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

    /// Once a frame, before the panes draw: feed the views, and note the
    /// live spans (for the survey's brackets) and the next tune target.
    fn frame_prep(&mut self) {
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
        let live = self.live_ids();
        let target = self.tune_target;
        let live_spans: Vec<(ReceiverId, (f64, f64))> = live.iter().filter_map(|id| Some((*id, self.receiver(*id)?.view.span()?))).collect();
        self.frame = FrameInfo {
            spans: live_spans.iter().map(|(_, s)| *s).collect(),
            live_spans,
            next_target: live.iter().position(|i| *i == target).map(|k| live[(k + 1) % live.len()]).or(live.first().copied()),
        };
    }

    /// One receiver's pane: spectrum and waterfall with the header toolbar,
    /// or a device picker for an empty slot.
    fn receiver_pane(&mut self, ui: &mut egui::Ui, id: ReceiverId) {
        if self.receiver(id).is_none() {
            let mut open = None;
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(egui::RichText::new(format!("Receiver {id}: no device")).strong());
                let free: Vec<DeviceInfo> =
                    self.devices.iter().filter(|d| !self.panes.iter().any(|p| same_device(p.source.info(), d))).cloned().collect();
                if free.is_empty() {
                    ui.label(egui::RichText::new("Every device is in use; close one from its pane menu (…).").small().color(theme::MUTED));
                }
                for d in free {
                    if ui.button(format!("Open {} ({}) as {id}", d.label, d.serial)).clicked() {
                        open = Some(d);
                    }
                }
                ui.add_space(8.0);
                if ui.small_button("Remove this slot").clicked() {
                    self.dock_requests.push(DockReq::Remove(Tab::Receiver(id)));
                }
            });
            if let Some(d) = open {
                self.open_as(&d, id);
            }
            return;
        }
        let r = ui.available_rect_before_wrap();
        let selection = self.selection.clone();
        let target = self.tune_target;
        let next_target = self.frame.next_target;
        enum Act {
            Tune(ReceiverId, f64),
            Select(Selection),
            ApplySweep,
            Target(ReceiverId),
            Close(DeviceInfo),
            Err(String),
        }
        let mut acts: Vec<Act> = Vec::new();
        let segments = self.settings.survey_segments.len();
        let mut open_inspector = false;
        {
            let App { panes, sweep_lo_mhz, sweep_hi_mhz, frame, .. } = &mut *self;
            let Some(p) = panes.iter_mut().find(|p| p.id == id) else { return };
            let records = if id.is_sweep() { survey_outside_focus(pane_records(p), &frame.spans) } else { pane_records(p) };
            // Highlight the selected record, and its emitter (or the
            // selected emitter), on this receiver only.
            let sel_record = selection.record().filter(|(rx, _)| *rx == id).map(|(_, rid)| rid);
            let sel_emitter = selection
                .emitter()
                .filter(|(rx, _)| *rx == id)
                .map(|(_, e)| e)
                .or_else(|| sel_record.and_then(|rid| records.iter().find(|r| r.id == rid).and_then(|r| r.emitter)));
            let info = p.source.info().clone();
            let center_mhz = p.source.snapshot().center_hz / 1e6;
            if id.is_sweep() {
                let st = p.source.sweep_status();
                let title = match &st {
                    Some(s) if !s.segments.is_empty() => format!(
                        "S · {} · {:.0}–{:.0} MHz · {} segments, {} of {} hops · {:.0} MHz/s · revisit {:.1} s",
                        info.label,
                        s.start_hz / 1e6,
                        s.stop_hz() / 1e6,
                        s.segments.len(),
                        s.measured_hops,
                        s.hops,
                        s.mhz_per_sec(),
                        s.last_pass_s
                    ),
                    Some(s) => format!(
                        "S · {} · {:.0}–{:.0} MHz · {} hops · {:.0} MHz/s · revisit {:.1} s",
                        info.label,
                        s.start_hz / 1e6,
                        s.stop_hz() / 1e6,
                        s.hops,
                        s.mhz_per_sec(),
                        s.last_pass_s
                    ),
                    None => format!("S · {}", info.label),
                };
                let res = p.view.show(ui, r, &title, st.as_ref(), &records, sel_record, sel_emitter);
                for (rx, (lo, hi)) in &frame.live_spans {
                    p.view.bracket(ui, r, *lo, *hi, &rx.to_string());
                }
                // Survey click: tune the target there; on a record, also
                // select it and centre on the record, not the click point.
                if let Some(rid) = res.record {
                    acts.push(Act::Select(Selection::Record(id, rid)));
                    if let Some(f) = records.iter().find(|r| r.id == rid).map(|r| r.center_hz()) {
                        acts.push(Act::Tune(target, f));
                    }
                } else if let Some(f) = res.freq_hz {
                    acts.push(Act::Tune(target, f));
                }
            } else {
                let snap = p.source.snapshot();
                let title = format!("{id} · {} · {:.3} MHz · {:.1} MS/s", info.label, snap.center_hz / 1e6, snap.sample_rate / 1e6);
                let res = p.view.show(ui, r, &title, None, &records, sel_record, sel_emitter);
                // Live click: select a record if one was hit, otherwise
                // recentre this receiver.
                match (res.record, res.freq_hz) {
                    (Some(rid), _) => acts.push(Act::Select(Selection::Record(id, rid))),
                    (None, Some(f)) => acts.push(Act::Tune(id, f)),
                    _ => {}
                }
            }
            // Clicking the title selects the receiver (the inspector shows it).
            let hdr = egui::Rect::from_min_size(r.min, egui::vec2(r.width(), spectrum_view::HEADER_H)).shrink2(egui::vec2(6.0, 2.0));
            let title_rect = egui::Rect::from_min_size(hdr.min, egui::vec2(hdr.width() * 0.4, hdr.height()));
            if ui.interact(title_rect, ui.id().with(("header", id.0)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                acts.push(Act::Select(Selection::Receiver(id)));
            }
            if selection == Selection::Receiver(id) {
                ui.painter_at(r).hline(r.x_range(), r.top() + spectrum_view::HEADER_H - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
            }
            // The header toolbar: this receiver's own controls, right-aligned
            // over the painted title.
            ui.scope_builder(egui::UiBuilder::new().max_rect(hdr).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.menu_button(egui::RichText::new("…").small(), |ui| {
                    if ui.button("Close device").clicked() {
                        acts.push(Act::Close(info.clone()));
                        ui.close();
                    }
                });
                ui.menu_button(egui::RichText::new("dB ⏷").small(), |ui| {
                    ui.set_min_width(230.0);
                    db_ui(ui, p);
                });
                ui.menu_button(egui::RichText::new("Gain ⏷").small(), |ui| {
                    ui.set_min_width(260.0);
                    if let Some(e) = gain_ui(ui, p) {
                        acts.push(Act::Err(e));
                    }
                });
                if id.is_sweep() {
                    if let Some(next) = next_target {
                        if ui
                            .add(egui::Button::new(egui::RichText::new(format!("click tunes {target}")).small()))
                            .on_hover_text("Where a click on the survey tunes; click to pick the next receiver (keys 1, 2, 3)")
                            .clicked()
                        {
                            acts.push(Act::Target(next));
                        }
                    }
                    if ui.small_button("Apply").clicked() {
                        acts.push(Act::ApplySweep);
                    }
                    if ui
                        .small_button(if segments == 0 { "Segments".to_string() } else { format!("{segments} segments") })
                        .on_hover_text("Sweep only chosen ranges of the span (edited in the Inspector)")
                        .clicked()
                    {
                        acts.push(Act::Select(Selection::Receiver(id)));
                        open_inspector = true;
                    }
                    ui.add(egui::DragValue::new(sweep_hi_mhz).speed(1.0).range(34.0..=1800.0).suffix(" MHz"));
                    ui.label(egui::RichText::new("–").small());
                    ui.add(egui::DragValue::new(sweep_lo_mhz).speed(1.0).range(24.0..=1790.0).suffix(" MHz"));
                } else {
                    let mut mhz = center_mhz;
                    if ui.add(egui::DragValue::new(&mut mhz).speed(0.1).range(24.0..=1800.0).max_decimals(6).suffix(" MHz")).changed() {
                        acts.push(Act::Tune(id, mhz * 1e6));
                    }
                    ui.label(egui::RichText::new("Centre").small().color(theme::MUTED));
                    if ui
                        .selectable_label(target == id, egui::RichText::new("⌖").small())
                        .on_hover_text("Tune target: survey clicks and double-clicks land here (keys 1, 2, 3)")
                        .clicked()
                    {
                        acts.push(Act::Target(id));
                    }
                }
            });
        }
        if open_inspector {
            self.dock_requests.push(DockReq::Show(Tab::Inspector));
        }
        for a in acts {
            match a {
                Act::Tune(id, f) => self.retune(id, (f / 1e3).round() / 1e3),
                Act::Select(sel) => self.selection = sel,
                Act::ApplySweep => self.apply_sweep(),
                Act::Target(id) => self.tune_target = id,
                Act::Close(d) => self.close(&d),
                Act::Err(e) => self.error = Some(e),
            }
        }
    }

    /// Finishes a SigMF export that has completed.
    fn poll_export(&mut self) {
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
    }

    /// One panel for whatever is selected: a receiver, a detection, an
    /// emitter, a talkgroup, a radio or a capcode.
    fn inspector(&mut self, ui: &mut egui::Ui) {
        self.poll_export();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("INSPECTOR").small().strong().color(theme::MUTED));
            if self.selection != Selection::None {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Clear the selection (Esc)").clicked() {
                        self.selection = Selection::None;
                    }
                });
            }
        });
        let sel = self.selection.clone();
        theme::card(ui, |ui| match sel {
            Selection::None => {
                ui.label(egui::RichText::new("Click a box, a list row or a receiver heading to select it.").small().color(theme::MUTED));
                ui.separator();
                let rx = self.working_id();
                self.export_footer(ui, rx);
            }
            Selection::Receiver(id) => self.inspect_receiver(ui, id),
            Selection::Record(rx, id) => self.inspect_signal(ui, rx, Some(id), None),
            Selection::Emitter(rx, id) => self.inspect_signal(ui, rx, None, Some(id)),
            Selection::Talkgroup { system, tg } => self.inspect_call_party(ui, system, tg, true),
            Selection::Radio { system, id } => self.inspect_call_party(ui, system, id, false),
            Selection::Capcode { protocol, capcode } => self.inspect_capcode(ui, &protocol, capcode),
        });
    }

    /// A receiver: identity, its tuning, gain and range, and diagnostics
    /// folded away.
    fn inspect_receiver(&mut self, ui: &mut egui::Ui, id: ReceiverId) {
        let target = self.tune_target;
        let mut tune = None;
        let mut apply = false;
        let mut set_target = false;
        let mut err = None;
        let mut show_segments = false;
        {
            let App { panes, sweep_lo_mhz, sweep_hi_mhz, .. } = &mut *self;
            let Some(p) = panes.iter_mut().find(|p| p.id == id) else {
                ui.label(egui::RichText::new(format!("Receiver {id} is not open.")).color(theme::MUTED));
                return;
            };
            let info = p.source.info().clone();
            let snap = p.source.snapshot();
            let st = p.source.stats();
            let cfg = p.source.config().clone();
            let load = p.source.classifier_load();
            let sweep = p.source.sweep_status();
            ui.horizontal(|ui| {
                let (bg, fg) = if id.is_sweep() {
                    (egui::Color32::from_rgb(0x0f, 0x2b, 0x2b), egui::Color32::from_rgb(0x7f, 0xe9, 0xe5))
                } else {
                    (theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG)
                };
                theme::chip(ui, &id.chip(), bg, fg);
                ui.label(egui::RichText::new(&info.label).strong());
            });
            ui.label(egui::RichText::new(format!("{} · {}", info.backend, info.serial)).small().color(theme::MUTED));
            if id.is_sweep() {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(sweep_lo_mhz).speed(1.0).range(24.0..=1790.0).suffix(" MHz"));
                    ui.label("to");
                    ui.add(egui::DragValue::new(sweep_hi_mhz).speed(1.0).range(34.0..=1800.0).suffix(" MHz"));
                    apply = ui.button("Apply range").clicked();
                });
                show_segments = true;
            } else {
                ui.horizontal(|ui| {
                    ui.label("Centre");
                    let mut mhz = snap.center_hz / 1e6;
                    if ui.add(egui::DragValue::new(&mut mhz).speed(0.1).range(24.0..=1800.0).max_decimals(6).suffix(" MHz")).changed() {
                        tune = Some(mhz);
                    }
                    if target == id {
                        ui.label(egui::RichText::new("tune target").small().color(theme::ACCENT))
                            .on_hover_text("Survey clicks and double-clicks tune here (keys 1, 2, 3 switch)");
                    } else if ui.small_button("Make tune target").clicked() {
                        set_target = true;
                    }
                });
            }
            kv(ui, "Rate", &format!("{:.1} MS/s · {}-bit", snap.sample_rate / 1e6, snap.caps.adc_bits));
            if let Some(e) = gain_ui(ui, p) {
                err = Some(e);
            }
            db_ui(ui, p);
            egui::CollapsingHeader::new(egui::RichText::new("Diagnostics").small()).id_salt(("diag", id)).show(ui, |ui| match &sweep {
                Some(s) => {
                    kv(ui, "Hops", &format!("{} × {:.1} MHz", s.hops, s.hop_hz / 1e6));
                    kv(ui, "Rate", &format!("{:.0} hops/s · {:.0} MHz/s", s.hops_per_sec, s.mhz_per_sec()));
                    kv(ui, "Revisit", &format!("{:.2} s", s.last_pass_s));
                    kv(ui, "Re-measured", &format!("{} settle · {} gap", s.settle_retries, s.redone_hops));
                    if s.tuner_lock {
                        kv(ui, "Lock waits", &s.lock_waits.to_string());
                    } else {
                        kv(ui, "Slow hops", &format!("{} of {}", s.slow_hops, s.hops));
                    }
                    kv(ui, "Gaps", &format!("{} ({} samples)", st.gaps, st.lost_samples));
                }
                None => {
                    let rate = snap.sample_rate;
                    kv(ui, "FFT", &format!("{} pt · {:.0}/s · {:.0} Hz", cfg.fft_size, st.ffts_per_sec, rate / cfg.fft_size as f64));
                    kv(ui, "Look-back", &format!("{:.1} / {:.0} s", st.ring_samples as f64 / rate, cfg.ring_seconds));
                    kv(ui, "Gaps", &format!("{} ({} samples)", st.gaps, st.lost_samples));
                    kv(ui, "Classifier", &format!("{:.1}/s · {:.0} ms · {:.0}%", load.jobs_per_sec, load.avg_job_ms, load.busy * 100.0));
                }
            });
        }
        if let Some(m) = tune {
            self.retune(id, m);
        }
        if show_segments {
            ui.separator();
            apply |= self.segments_ui(ui);
        }
        if apply {
            self.apply_sweep();
        }
        if set_target {
            self.tune_target = id;
        }
        if err.is_some() {
            self.error = err;
        }
        if !id.is_sweep() {
            ui.separator();
            self.export_footer(ui, id);
        }
    }

    /// A detection (record) or an emitter on receiver `rx`: what it is,
    /// Listen, and SigMF export of its channel.
    fn inspect_signal(&mut self, ui: &mut egui::Ui, rx: ReceiverId, record: Option<u64>, emitter: Option<u64>) {
        let Some(focus) = self.receiver(rx) else {
            ui.label(egui::RichText::new(format!("Receiver {rx} is not open.")).color(theme::MUTED));
            return;
        };
        let records: Vec<engine::SignalRecord> = focus.records.2.iter().cloned().collect();
        let record = record.and_then(|id| {
            records.iter().find(|r| r.id == id).cloned().or_else(|| focus.source.recent_records(3000).into_iter().find(|r| r.id == id))
        });
        let emitter = emitter.and_then(|id| focus.source.emitters().into_iter().find(|e| e.id == id));
        let busy = self.export.is_some();
        let dir = engine::sigmf::default_dir();
        let mut req: Option<(String, engine::sigmf::ExportRequest)> = None;
        let mut listen: Option<(f64, String, f64)> = None;
        if let Some(r) = &record {
            let label = r.display_class().map(|c| c.label.clone()).unwrap_or_else(|| "unclassified".into());
            ui.label(egui::RichText::new(format!("Record #{} · {}", r.id, label)).strong());
            ui.label(
                egui::RichText::new(format!("{:.4} MHz · {:.1} kHz · {:.2} s · on {rx}", r.center_hz() / 1e6, r.bandwidth_hz() / 1e3, r.duration_s()))
                    .monospace()
                    .color(theme::TEXT2),
            );
            kv(ui, "SNR", &format!("{:.0} / {:.0} dB · peak {:.1} dBFS", r.mean_snr_db, r.max_snr_db, r.peak_dbfs));
            kv(ui, "Seen", &format!("{} – {}{}", hms(r.t_start), hms(r.t_end), if r.active { " · active" } else { "" }));
            if let Some(e) = r.emitter {
                kv(ui, "Emitter", &format!("E{e}"));
            }
            if ui.button("Listen").clicked() {
                listen = Some((r.center_hz(), label.clone(), r.bandwidth_hz()));
            }
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
                            annotations: annotations(focus, std::slice::from_ref(r)),
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
                            annotations: annotations(focus, &overlapping),
                        },
                    ));
                }
            });
        } else if let Some(em) = &emitter {
            let label = em.label().unwrap_or_else(|| "unclassified".into());
            ui.label(egui::RichText::new(format!("Emitter E{} · {}", em.id, label)).strong());
            ui.label(
                egui::RichText::new(format!("{:.4} MHz · {:.1} kHz · {} bursts · on {rx}", em.center_hz / 1e6, em.bandwidth_hz / 1e3, em.records))
                    .monospace()
                    .color(theme::TEXT2),
            );
            kv(ui, "Seen", &format!("{} – {}{}", hms(em.first_seen), hms(em.last_seen), if em.active { " · active" } else { "" }));
            kv(ui, "Duty", &format!("{:.0}% · peak SNR {:.0} dB", em.duty() * 100.0, em.peak_snr_db));
            if let Some(a) = em.activity() {
                kv(ui, "Activity", &format!("{}{}", a.name(), em.last_talk.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()));
            }
            let mut others: Vec<(&String, &u32)> = em.tally.iter().filter(|(l, _)| **l != label).collect();
            others.sort_by(|a, b| b.1.cmp(a.1));
            if !others.is_empty() {
                let also: Vec<String> = others.iter().take(3).map(|(l, n)| format!("{l}×{n}")).collect();
                kv(ui, "Also seen", &also.join(", "));
            }
            if ui.button("Listen").clicked() {
                listen = Some((em.center_hz, label.clone(), em.bandwidth_hz));
            }
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
                        annotations: annotations(focus, &mine),
                    },
                ));
            }
        } else {
            ui.label(egui::RichText::new("Gone: the selection is no longer in the receiver's history.").small().color(theme::MUTED));
        }
        if let Some((what, r)) = req {
            let h = focus.source.export(r);
            self.export = Some((what, h));
            self.export_msg = None;
        }
        if let Some((hz, label, bw)) = listen {
            let spec = self.spec_for(hz, &label, bw);
            self.start_listen(rx, spec);
        }
        ui.separator();
        self.export_footer(ui, rx);
    }

    /// Look-back export of receiver `rx`'s whole buffer, and export status.
    fn export_footer(&mut self, ui: &mut egui::Ui, rx: ReceiverId) {
        let busy = self.export.is_some();
        let dir = engine::sigmf::default_dir();
        if let Some(focus) = self.receiver(rx) {
            if ui
                .add_enabled(!busy, egui::Button::new("Export look-back (full band)"))
                .on_hover_text(format!("Everything in the {rx} look-back buffer (~10 s, ~400 MB)"))
                .clicked()
            {
                let (s, e) = focus.source.ring_range();
                let snap = focus.source.snapshot();
                let records: Vec<engine::SignalRecord> = focus.records.2.iter().cloned().collect();
                let r = engine::sigmf::ExportRequest {
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
                    annotations: annotations(focus, &records),
                };
                let h = focus.source.export(r);
                self.export = Some(("look-back".into(), h));
                self.export_msg = None;
            }
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
    }

    /// A talkgroup (`group`) or a radio: its imported name, following
    /// controls, and its last calls.
    fn inspect_call_party(&mut self, ui: &mut egui::Ui, system: Option<engine::SystemKey>, id: u32, group: bool) {
        let systems: Vec<engine::TrunkSystem> = self.panes.iter().flat_map(|p| p.source.systems()).collect();
        let sys = system_for(&systems, &system);
        let name = match (&sys, group) {
            (Some((k, cc)), true) => self.catalog.group(k, *cc, id).map(|g| g.name().to_string()),
            (Some((k, cc)), false) => self.catalog.radio(k, *cc, id).map(str::to_string),
            (None, _) => None,
        }
        .filter(|n| !n.is_empty());
        let head = if group { format!("TG {id}") } else { format!("Radio {id}") };
        ui.label(egui::RichText::new(match &name {
            Some(n) => format!("{head} · {n}"),
            None => head,
        })
        .strong());
        if let Some((k, _)) = &sys {
            ui.label(egui::RichText::new(format!("{} · {}", k.kind().name(), k.short())).small().color(theme::MUTED));
        }
        let mut toggle: Option<(u32, bool)> = None;
        if group {
            let fp = &self.follow_policy;
            ui.horizontal(|ui| {
                if ui.selectable_label(fp.priority.contains(&id), "★ Priority").on_hover_text("Followed first, interrupting other talkgroups").clicked() {
                    toggle = Some((id, true));
                }
                if ui.selectable_label(fp.lockout.contains(&id), "⊘ Lock out").on_hover_text("Never followed").clicked() {
                    toggle = Some((id, false));
                }
            });
        }
        let mut calls: Vec<engine::CallEvent> = self
            .panes
            .iter()
            .flat_map(|p| p.source.calls(2000))
            .filter(|c| c.call.group == group && c.call.target == id || (!group && c.call.source == Some(id)))
            .collect();
        calls.sort_by(|a, b| b.last.cmp(&a.last));
        calls.dedup_by(|a, b| a.first == b.first && a.call.target == b.call.target);
        ui.separator();
        ui.label(egui::RichText::new(format!("{} calls · last:", calls.len())).small().color(theme::MUTED));
        let mut play = None;
        for c in calls.iter().take(5) {
            ui.horizontal(|ui| {
                let secs = c.last.duration_since(c.first).unwrap_or_default().as_secs();
                let who = if group { c.call.source.map(|s| format!("← {s}")).unwrap_or_default() } else { format!("TG {}", c.call.target) };
                let flags = format!("{}{}", if c.call.emergency { " EMERG" } else { "" }, if c.call.encrypted { " enc" } else { "" });
                ui.label(egui::RichText::new(format!("{} · {secs}s · {who}{flags}", hms(c.first))).monospace().small().color(theme::TEXT2));
                if let Some(last) = c.recordings.last() {
                    if ui.small_button("▶").on_hover_text(last.display().to_string()).clicked() {
                        play = Some(last.clone());
                    }
                }
            });
        }
        if let Some((tg, priority)) = toggle {
            self.toggle_policy(tg, priority);
        }
        if let Some(p) = play {
            self.play_recording(&p);
        }
    }

    /// A pager capcode: its protocol and last messages.
    fn inspect_capcode(&mut self, ui: &mut egui::Ui, protocol: &str, capcode: u64) {
        ui.label(egui::RichText::new(format!("Capcode {capcode:09} · {protocol}")).strong());
        let mut pages: Vec<engine::PageEvent> = self.panes.iter().flat_map(|p| p.source.pages(5000)).filter(|p| p.capcode == capcode).collect();
        pages.sort_by(|a, b| b.last.cmp(&a.last));
        if let Some(p) = pages.first() {
            ui.label(egui::RichText::new(format!("{:.4} MHz", p.center_hz / 1e6)).small().color(theme::MUTED));
        }
        if ui.small_button("Show in Pages").clicked() {
            self.page_filter = format!("{capcode:09}");
            self.dock_requests.push(DockReq::Show(Tab::Pages));
        }
        ui.separator();
        ui.label(egui::RichText::new(format!("{} pages · last:", pages.len())).small().color(theme::MUTED));
        for p in pages.iter().take(5) {
            let body = format!("{} · {}{}", hms(p.last), if p.damaged { "[damaged] " } else { "" }, p.text);
            ui.add(egui::Label::new(egui::RichText::new(&body).monospace().small().color(theme::TEXT2)).truncate()).on_hover_text(&body);
        }
    }

    /// Moves a talkgroup in or out of the priority (`true`) or lockout list
    /// and tells the sources.
    fn toggle_policy(&mut self, tg: u32, priority: bool) {
        let fp = &mut self.follow_policy;
        let (list, other) = if priority { (&mut fp.priority, &mut fp.lockout) } else { (&mut fp.lockout, &mut fp.priority) };
        if !list.remove(&tg) {
            list.insert(tg);
            other.remove(&tg);
        }
        self.policy_gen.0 += 1;
        save_follow_policy(&self.follow_policy);
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
                "{} emitters · {active} active · {identified} identified{}{} · click selects · double-click tunes {}",
                rows.len(),
                if hidden > 0 { format!(" · {hidden} unconfirmed not shown") } else { String::new() },
                if self.emitter_squelched > 0 { format!(" · {} known squelched", self.emitter_squelched) } else { String::new() },
                self.tune_target
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

    /// Every trunked system identified on any receiver, any protocol, with
    /// the name imports that label them.
    fn trunking_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.menu_button(egui::RichText::new("Import names ⏷").small(), |ui| self.import_menu(ui));
            let names = if self.catalog.sources.is_empty() { "no names imported".to_string() } else { format!("names: {}", self.catalog.sources.join(" · ")) };
            ui.label(egui::RichText::new(names).small().color(theme::MUTED));
        });
        let rows: Vec<(ReceiverId, engine::TrunkSystem)> = self.panes.iter().flat_map(|p| p.source.systems().into_iter().map(move |s| (p.id, s))).collect();
        if rows.is_empty() {
            let monitors = self.panes.iter().flat_map(|p| p.source.monitors()).filter(|m| m.label.starts_with("CC")).count();
            ui.label(
                egui::RichText::new(if monitors == 0 {
                    "No control channel identified yet. P25 control channels are monitored automatically once one is in a receiver's span; DMR, NXDN, LTR and EDACS systems will list here as their decoders arrive."
                } else {
                    "Monitoring a control channel; waiting for its first messages…"
                })
                .color(theme::MUTED),
            );
            return;
        }
        let cat = &self.catalog;
        let mut tune = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("trunking")
            .striped(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::exact(28.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(330.0))
            .column(egui_extras::Column::exact(220.0))
            .column(egui_extras::Column::exact(100.0))
            .column(egui_extras::Column::exact(56.0))
            .column(egui_extras::Column::exact(72.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["Src", "System", "Identity", "Site · network (imported)", "Control MHz", "Grants", "Last", "Adjacent sites"] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).small().color(theme::MUTED));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let (rx, sys) = &rows[row.index()];
                    let text = |ui: &mut egui::Ui, s: String, c: egui::Color32| {
                        ui.add(egui::Label::new(egui::RichText::new(s).monospace().color(c)).selectable(false).truncate());
                    };
                    let recent = sys.last_activity.is_some_and(|t| std::time::SystemTime::now().duration_since(t).map(|d| d.as_secs() < 10).unwrap_or(true));
                    let c = if recent { theme::TEXT } else { theme::TEXT2 };
                    let site = sys.site.and_then(|(r, t)| cat.site(&sys.key, r, t)).unwrap_or("");
                    let net = cat.network(&sys.key).unwrap_or("");
                    let names = match (site.is_empty(), net.is_empty()) {
                        (false, false) => format!("{site} · {net}"),
                        (false, true) => site.to_string(),
                        (true, false) => net.to_string(),
                        (true, true) => String::new(),
                    };
                    let adjacent: Vec<String> = sys
                        .adjacent
                        .iter()
                        .map(|(l, f)| {
                            let name = l
                                .split_once('-')
                                .and_then(|(r, t)| Some((r.parse::<u8>().ok()?, t.parse::<u8>().ok()?)))
                                .and_then(|(r, t)| cat.site(&sys.key, r, t))
                                .map(|n| format!(" {n}"))
                                .unwrap_or_default();
                            match f {
                                Some(f) => format!("{l}{name} {:.4}", f / 1e6),
                                None => format!("{l}{name}"),
                            }
                        })
                        .collect();
                    row.col(|ui| text(ui, rx.to_string(), c));
                    row.col(|ui| text(ui, sys.kind.name().to_string(), theme::class_color(None).unwrap_or(egui::Color32::from_rgb(0xf2, 0xa3, 0x3a))));
                    row.col(|ui| text(ui, sys.identity.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · "), c));
                    row.col(|ui| text(ui, names, theme::TEXT2));
                    row.col(|ui| text(ui, sys.control_hz.iter().map(|f| format!("{:.5}", f / 1e6)).collect::<Vec<_>>().join(", "), c));
                    row.col(|ui| text(ui, sys.grants.to_string(), theme::MUTED));
                    row.col(|ui| text(ui, sys.last_activity.map(hms).unwrap_or_default(), theme::MUTED));
                    row.col(|ui| {
                        let a = adjacent.join(", ");
                        ui.add(egui::Label::new(egui::RichText::new(&a).monospace().color(theme::MUTED)).selectable(false).truncate()).on_hover_text(&a);
                    });
                    if row.response().double_clicked() {
                        tune = sys.control_hz.first().copied();
                    }
                });
            });
        if let Some(f) = tune {
            self.retune_target((f / 1e3).round() / 1e3);
        }
    }

    /// The raw control-channel grants, every receiver, newest first.
    fn grants_table(&mut self, ui: &mut egui::Ui) {
        let systems: Vec<engine::TrunkSystem> = self.panes.iter().flat_map(|p| p.source.systems()).collect();
        let mut grants: Vec<engine::GrantEvent> = self.panes.iter().flat_map(|p| p.source.p25_grants(2000)).collect();
        grants.sort_by(|a, b| b.last.cmp(&a.last));
        if grants.is_empty() {
            ui.label(egui::RichText::new("No grants yet.").color(theme::MUTED));
            return;
        }
        let cat = &self.catalog;
        let p25 = egui::Color32::from_rgb(0xf2, 0xa3, 0x3a);
        let mut tune = None;
        let row_h = 18.0;
        egui_extras::TableBuilder::new(ui)
            .id_salt("grants")
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
                for t in ["Last", "First", "Talkgroup", "Name (imported)", "Voice MHz", "Slot", "Radio ID", "Radio (imported)", "×", "System"] {
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
                    let key = engine::SystemKey::P25 { wacn: None, system: None, nac: g.nac };
                    let sys = systems.iter().find(|s| s.key.same(&key));
                    let (key, cc) = match sys {
                        Some(s) => (s.key.clone(), s.control_hz.first().copied()),
                        None => (key, None),
                    };
                    let tg_name = cat.group(&key, cc, g.talkgroup as u32).map(|i| i.name().to_string()).unwrap_or_default();
                    let radio_name = g.source.and_then(|r| cat.radio(&key, cc, r)).unwrap_or("").to_string();
                    row.col(|ui| text(ui, hms(g.last), c));
                    row.col(|ui| text(ui, hms(g.first), theme::MUTED));
                    row.col(|ui| text(ui, g.talkgroup.to_string(), if recent { p25 } else { c }));
                    row.col(|ui| text(ui, tg_name, theme::TEXT2));
                    row.col(|ui| text(ui, g.freq_hz.map(|f| format!("{:.5}", f / 1e6)).unwrap_or("?".into()), c));
                    row.col(|ui| text(ui, g.slot.map(|s| s.to_string()).unwrap_or_default(), c));
                    row.col(|ui| text(ui, g.source.map(|s| s.to_string()).unwrap_or_default(), c));
                    row.col(|ui| text(ui, radio_name, theme::TEXT2));
                    row.col(|ui| text(ui, g.count.to_string(), theme::MUTED));
                    row.col(|ui| text(ui, key.short(), theme::MUTED));
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
        let system_of = |c: &engine::CallEvent| system_for(&systems, &c.system);
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
        let following = self.panes.iter().find(|p| p.source.listening().is_some_and(|l| l.follow)).map(|p| p.id);
        let mut view = self.calls_view;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut view, CallsView::Calls, egui::RichText::new("Calls").small());
            ui.selectable_value(&mut view, CallsView::Grants, egui::RichText::new("Grants").small()).on_hover_text("The raw control-channel grants calls are built from");
            ui.separator();
            ui.label(egui::RichText::new("Filter").small().color(theme::MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.call_filter).desired_width(200.0).hint_text("talkgroup, radio or name"));
            ui.label(
                egui::RichText::new(format!("{} calls · {} heard on their voice channel", calls.len(), calls.iter().filter(|c| c.heard).count()))
                    .small()
                    .monospace()
                    .color(theme::MUTED),
            );
            ui.separator();
            let fp = &self.follow_policy;
            let list = |s: &std::collections::BTreeSet<u32>| s.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
            let state = match following {
                Some(rx) => format!("following on {rx}"),
                None => "not following".to_string(),
            };
            ui.label(egui::RichText::new(format!("{state} · ★ {} · ⊘ {}", list(&fp.priority), list(&fp.lockout))).small().color(theme::MUTED))
                .on_hover_text("Priority (★) and locked-out (⊘) talkgroups: right-click a call, or the ★ rail button");
        });
        self.calls_view = view;
        if view == CallsView::Grants {
            self.grants_table(ui);
            return;
        }
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
            .column(egui_extras::Column::exact(36.0))
            .column(egui_extras::Column::exact(220.0))
            .column(egui_extras::Column::exact(90.0))
            .column(egui_extras::Column::exact(160.0))
            .column(egui_extras::Column::exact(70.0))
            .column(egui_extras::Column::exact(48.0))
            .column(egui_extras::Column::remainder())
            .header(row_h, |mut h| {
                for t in ["Start", "Secs", "MHz", "Protocol", "Talkgroup", "★⊘", "Name (imported)", "Radio", "Radio (imported)", "Flags", "Audio", "Seen"] {
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
                        (true, true, _) => "★",
                        (true, _, true) => "⊘",
                        _ => "",
                    };
                    let target = if c.call.group { format!("TG {}", c.call.target) } else { format!("→ {}", c.call.target) };
                    row.col(|ui| text(ui, target, if recent { accent } else { col }));
                    row.col(|ui| text(ui, mark.into(), if mark == "★" { theme::SELECT } else { theme::MUTED }));
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
            self.toggle_policy(tg, priority);
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


    /// Every receiver's statistics in full, then audio and the UI itself.
    fn diagnostics_panel(&mut self, ui: &mut egui::Ui) {
        fn section(ui: &mut egui::Ui, title: &str) {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(title).small().strong().color(theme::MUTED));
        }
        let fps = 1.0 / ui.ctx().input(|i| i.stable_dt).max(1e-3);
        let (inner, monitor, ppp) = ui.ctx().input(|i| (i.viewport().inner_rect, i.viewport().monitor_size, i.pixels_per_point()));
        egui::ScrollArea::vertical().id_salt("diagnostics").show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.columns(2, |cols| {
                for (k, p) in self.panes.iter().enumerate() {
                    let ui = &mut cols[k % 2];
                    let id = p.id;
                    let info = p.source.info();
                    let snap = p.source.snapshot();
                    let st = p.source.stats();
                    let cfg = p.source.config();
                    let load = p.source.classifier_load();
                    let rate = snap.sample_rate;
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let (bg, fg) = if id.is_sweep() {
                            (egui::Color32::from_rgb(0x0f, 0x2b, 0x2b), egui::Color32::from_rgb(0x7f, 0xe9, 0xe5))
                        } else {
                            (theme::CHIP_FOCUS_BG, theme::CHIP_FOCUS_FG)
                        };
                        theme::chip(ui, &id.chip(), bg, fg);
                        ui.label(egui::RichText::new(&info.label).strong());
                        ui.label(egui::RichText::new(format!("{} · {}", info.backend, info.serial)).small().color(theme::MUTED));
                    });
                    section(ui, "DEVICE");
                    kv(ui, "Centre", &format!("{:.4} MHz", snap.center_hz / 1e6));
                    kv(ui, "Rate", &format!("{:.2} MS/s · {}-bit", rate / 1e6, snap.caps.adc_bits));
                    kv(ui, "Samples", &format!("{:.1} G · {:.0} s", st.total_samples as f64 / 1e9, st.total_samples as f64 / rate));
                    kv(ui, "Overloads", &st.overloads.to_string());
                    if st.device_lost {
                        ui.label(egui::RichText::new("Device lost: no samples are arriving").color(theme::ALERT));
                    }
                    section(ui, "STREAM");
                    kv(ui, "Gaps", &format!("{} · {} samples lost", st.gaps, st.lost_samples));
                    kv(
                        ui,
                        "Look-back ring",
                        &format!("{:.1} of {:.1} s · {:.0} MB", st.ring_samples as f64 / rate, st.ring_capacity as f64 / rate, st.ring_capacity as f64 * 4.0 / 1e6),
                    );
                    section(ui, "PROCESSING");
                    kv(ui, "FFT", &format!("{} pt · {:.0}/s · {:.0} Hz/bin", cfg.fft_size, st.ffts_per_sec, rate / cfg.fft_size as f64));
                    kv(ui, "Classifier", &format!("{:.1} jobs/s · {:.0} ms · {:.0}% busy", load.jobs_per_sec, load.avg_job_ms, load.busy * 100.0));
                    let monitors = p.source.monitors();
                    if monitors.is_empty() {
                        kv(ui, "Monitors", "none");
                    } else {
                        let names: Vec<String> = monitors.iter().map(|m| m.label.clone()).collect();
                        kv(ui, "Monitors", &names.join(" · "));
                    }
                    if let Some(s) = p.source.sweep_status() {
                        section(ui, "SWEEP");
                        kv(ui, "Range", &format!("{:.1} – {:.1} MHz", s.start_hz / 1e6, s.stop_hz() / 1e6));
                        kv(ui, "Hops", &format!("{} × {:.2} MHz · at {}", s.hops, s.hop_hz / 1e6, s.current_hop));
                        kv(ui, "Rate", &format!("{:.1} hops/s · {:.0} MHz/s", s.hops_per_sec, s.mhz_per_sec()));
                        kv(ui, "Revisit", &format!("{:.2} s", s.last_pass_s));
                        kv(ui, "Re-measured", &format!("{} settle · {} gap", s.settle_retries, s.redone_hops));
                        if s.tuner_lock {
                            kv(ui, "Tuner lock", &format!("reported · {} waits", s.lock_waits));
                        } else {
                            kv(ui, "Tuner lock", &format!("not reported · {} slow hops", s.slow_hops));
                        }
                        // Age of each hop's last measurement: bright is fresh,
                        // fading to the floor over one pass; dark is unmeasured.
                        let (resp, painter) = ui.allocate_painter(egui::vec2(ui.available_width(), 12.0), egui::Sense::hover());
                        let r = resp.rect;
                        let n = s.hop_times.len().max(1) as f32;
                        let w = r.width() / n;
                        let pass = s.last_pass_s.max(0.1) as f32;
                        for (i, t) in s.hop_times.iter().enumerate() {
                            let col = match t {
                                Some(t) => {
                                    let a = (t.elapsed().as_secs_f32() / pass).clamp(0.0, 1.0);
                                    theme::ACCENT.lerp_to_gamma(theme::MUTED, a)
                                }
                                None => egui::Color32::from_gray(40),
                            };
                            let x0 = r.min.x + w * i as f32;
                            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(x0, r.min.y), egui::pos2(x0 + w - 0.5, r.max.y)), 0.0, col);
                        }
                        if let Some(pos) = resp.hover_pos() {
                            let i = (((pos.x - r.min.x) / w) as usize).min(s.hop_times.len().saturating_sub(1));
                            let age = s.hop_times.get(i).copied().flatten().map_or("not yet".to_string(), |t| format!("{:.2} s ago", t.elapsed().as_secs_f64()));
                            resp.on_hover_text(format!("hop {i} · {:.2} MHz · {age}", (s.start_hz + s.hop_hz * (i as f64 + 0.5)) / 1e6));
                        }
                    }
                    ui.add_space(6.0);
                }
            });
            ui.separator();
            ui.columns(2, |cols| {
                let ui = &mut cols[0];
                section(ui, "AUDIO");
                match (&self.audio_out, &self.audio_err) {
                    (Some(_), _) => kv(ui, "Output", &format!("open · volume {:.0}%", self.volume * 100.0)),
                    (None, Some(e)) => kv(ui, "Output", e),
                    (None, None) => kv(ui, "Output", "not opened yet"),
                }
                let mut any = false;
                for p in &self.panes {
                    if let Some(l) = p.source.listening() {
                        any = true;
                        let (depth, starved) = p.source.audio().depth();
                        kv(
                            ui,
                            &format!("Listening {}", p.id),
                            &format!("{:.4} MHz · {}{}{}", l.center_hz / 1e6, l.mode.name(), if l.follow { " · follow" } else { "" }, if l.tdma { " · TDMA" } else { "" }),
                        );
                        kv(ui, "Queue", &format!("{:.2} s{}", depth, if starved { " · gathering" } else { "" }));
                        let v = p.source.voice();
                        if v.active || v.center_hz.is_some() {
                            let mut parts = vec![if v.active { "voice" } else { "idle" }.to_string()];
                            if let Some(hz) = v.center_hz {
                                parts.push(format!("{:.4} MHz", hz / 1e6));
                            }
                            if let Some(s) = v.slot {
                                parts.push(format!("slot {s}"));
                            }
                            if let Some(e) = v.encrypted {
                                parts.push(e.to_string());
                            }
                            if v.recording {
                                parts.push("rec".into());
                            }
                            kv(ui, "Digital", &parts.join(" · "));
                        }
                    }
                }
                if let Some(pb) = &self.playback {
                    any = true;
                    let (depth, _) = pb.queue.depth();
                    kv(ui, "Playing", &format!("{} · {:.1} s · queue {:.2} s", pb.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), pb.seconds, depth));
                }
                if !any {
                    kv(ui, "Listening", "nothing");
                }
                let ui = &mut cols[1];
                section(ui, "UI");
                kv(ui, "Frame", &format!("{fps:.0} fps"));
                if let Some(r) = inner {
                    kv(ui, "Window", &format!("{:.0} × {:.0} pt · {ppp:.2} px/pt", r.width(), r.height()));
                }
                if let Some(m) = monitor {
                    kv(ui, "Monitor", &format!("{:.0} × {:.0} pt", m.x, m.y));
                }
                let tabs = self.dock.as_ref().map_or(0, |d| d.iter_all_tabs().count()) + self.popouts.iter().map(|p| p.dock.iter_all_tabs().count()).sum::<usize>();
                kv(ui, "Layout", &format!("{} tabs · {} pop-out windows · {}", tabs, self.popouts.len(), self.workspace_name.as_deref().unwrap_or("unsaved")));
                let names = if self.catalog.sources.is_empty() { "none".to_string() } else { self.catalog.sources.join(" · ") };
                kv(ui, "Names", &names);
            });
            ui.separator();
            self.log_view(ui);
        });
    }

    /// The newest log entries (levels are set in ⚙ → Logging).
    fn log_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("LOG").small().strong().color(theme::MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.log_filter).desired_width(200.0).hint_text("filter: area, module or text"));
            if ui.small_button("Clear").clicked() {
                logging::clear();
            }
            if ui.small_button("Open log folder").clicked() {
                let d = logging::log_dir();
                let _ = std::fs::create_dir_all(&d);
                open_path(&d);
            }
            ui.label(egui::RichText::new(format!("levels: {}", logging::AREAS.iter().map(|a| format!("{a} {}", logging::level_str(self.settings.log.level(a)))).collect::<Vec<_>>().join(" · "))).small().color(theme::MUTED));
        });
        let (entries, _) = logging::recent(400);
        let q = self.log_filter.to_lowercase();
        let shown: Vec<&logging::Entry> = entries
            .iter()
            .filter(|e| q.is_empty() || e.area.contains(&q) || e.target.to_lowercase().contains(&q) || e.message.to_lowercase().contains(&q) || e.level.as_str().to_lowercase() == q)
            .collect();
        let row_h = ui.text_style_height(&egui::TextStyle::Small);
        egui::ScrollArea::vertical().id_salt("log_rows").max_height(260.0).stick_to_bottom(true).show_rows(ui, row_h, shown.len(), |ui, range| {
            for e in &shown[range] {
                let col = match e.level {
                    log::Level::Error => theme::ALERT,
                    log::Level::Warn => theme::ACCENT,
                    log::Level::Info => theme::TEXT,
                    _ => theme::TEXT2,
                };
                ui.label(
                    egui::RichText::new(format!("{} {:<5} [{}] {}: {}", &logging::stamp(e.time)[11..], e.level, e.area, e.target, e.message))
                        .small()
                        .monospace()
                        .color(col),
                );
            }
        });
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
        // Ctrl+K: the command palette, from anywhere.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            self.palette_open = !self.palette_open;
            self.palette_focus = self.palette_open;
        }
        // 1/2/3 pick the live receiver tuning lands on; Space pauses, M
        // mutes; Esc clears the selection.
        if !ctx.egui_wants_keyboard_input() && !self.palette_open {
            let live = self.live_ids();
            let mut mute = false;
            let mut nudge: Option<f64> = None;
            let mut nudge_sweep: Option<(f64, f64)> = None;
            ctx.input(|i| {
                for (n, k) in [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3].into_iter().enumerate() {
                    if i.key_pressed(k) {
                        if let Some(id) = live.get(n) {
                            self.tune_target = *id;
                        }
                    }
                }
                if i.key_pressed(egui::Key::Space) {
                    self.paused = !self.paused;
                }
                if i.key_pressed(egui::Key::M) {
                    mute = true;
                }
                if i.key_pressed(egui::Key::Escape) {
                    self.selection = Selection::None;
                }
                // Arrows nudge: ← → step the target receiver's centre (Shift
                // ×10, Alt ÷10); with Ctrl they shift the survey span, and
                // Ctrl ↑ ↓ widen or narrow it.
                let (left, right, up, down) = (
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                );
                if left || right || up || down {
                    let dir = if right || up { 1.0 } else { -1.0 };
                    if i.modifiers.command {
                        if left || right {
                            nudge_sweep = Some((dir, 0.0));
                        } else {
                            nudge_sweep = Some((0.0, dir));
                        }
                    } else if left || right {
                        let scale = if i.modifiers.shift { 10.0 } else if i.modifiers.alt { 0.1 } else { 1.0 };
                        nudge = Some(dir * scale);
                    }
                }
                if i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::P) {
                    if let Some(d) = self.dock.as_mut() {
                        if let Some((_, t)) = d.find_active_focused() {
                            self.dock_requests.push(DockReq::PopOut(t.clone()));
                        }
                    }
                }
            });
            if mute {
                self.toggle_mute();
            }
            if let Some(steps) = nudge {
                self.nudge(self.tune_target, steps);
            }
            if let Some((shift, grow)) = nudge_sweep {
                self.nudge_sweep(shift, grow);
            }
        }
        if self.sweep_dirty.is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(600)) {
            self.sweep_dirty = None;
            self.apply_sweep();
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
        egui::Panel::bottom("transport").exact_size(26.0).frame(theme::bar_frame()).show(ui, |ui| self.transport_bar(ui, &ctx));
        let width = ctx.content_rect().width();
        egui::Panel::left("rail").exact_size(36.0).frame(theme::rail_frame()).show(ui, |ui| self.rail(ui, width));
        self.palette(&ctx, width);
        self.frame_prep();
        if std::mem::take(&mut self.maximize) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        } else {
            self.track_window(&ctx);
        }
        if self.dock.is_none() {
            // No saved layout: build one for the screen this window is on
            // (the window is maximized on a first run; the monitor's width
            // in points is what it will have once it has settled).
            let ids: Vec<ReceiverId> = self.panes.iter().map(|p| p.id).collect();
            let screen = ctx.input(|i| i.viewport().monitor_size).map_or(width, |m| m.x);
            self.dock = Some(default_dock(screen, &ids));
            if let Some(t) = self.start_tab.take() {
                self.dock_requests.push(DockReq::Show(t));
            }
        }
        self.apply_dock_requests(width);
        // The dock is lent to the viewer (us) while it draws.
        self.popout_names = self.popouts.iter().map(|p| (p.id, p.title.clone())).collect();
        self.drawing = None;
        let mut dock = self.dock.take().expect("dock");
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| {
            egui_dock::DockArea::new(&mut dock)
                .id(egui::Id::new("main_dock"))
                .style(dock_style(ui))
                .show_add_buttons(false)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, self);
        });
        self.dock = Some(dock);
        // Popped-out windows, each an immediate viewport with its own dock
        // (an embedded window where the backend can't open OS windows).
        let mut pops = std::mem::take(&mut self.popouts);
        let mut closed: Vec<usize> = Vec::new();
        for (i, po) in pops.iter_mut().enumerate() {
            let pid = po.id;
            let vid = egui::ViewportId::from_hash_of(("popout", pid));
            let mut vb = egui::ViewportBuilder::default().with_title(format!("SpecTrawl · {}", po.title));
            vb = match po.rect {
                Some([x, y, w, h]) => vb.with_position([x, y]).with_inner_size([w, h]),
                None => vb.with_inner_size([900.0, 600.0]),
            };
            self.drawing = Some(pid);
            let mut dock = std::mem::replace(&mut po.dock, DockState::new(Vec::new()));
            let (close, rect) = ctx.show_viewport_immediate(vid, vb, |ui, _class| {
                let (close, rect) = ui.input(|i| (i.viewport().close_requested(), i.viewport().outer_rect.zip(i.viewport().inner_rect)));
                egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| {
                    egui_dock::DockArea::new(&mut dock)
                        .id(egui::Id::new(("popout_dock", pid)))
                        .style(dock_style(ui))
                        .show_add_buttons(false)
                        .show_leaf_close_all_buttons(false)
                        .show_leaf_collapse_buttons(false)
                        .show_inside(ui, self);
                });
                (close, rect)
            });
            po.dock = dock;
            if let Some((outer, inner)) = rect {
                po.rect = Some([outer.min.x, outer.min.y, inner.width(), inner.height()]);
            }
            if close || po.dock.iter_all_tabs().next().is_none() {
                closed.push(i);
            }
        }
        self.drawing = None;
        // A closed window's tabs come home.
        for i in closed.into_iter().rev() {
            let po = pops.remove(i);
            for (_, t) in po.dock.iter_all_tabs() {
                self.dock_requests.push(DockReq::Show(t.clone()));
            }
        }
        self.popouts = pops;
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

    fn on_exit(&mut self) {
        self.save_workspace();
    }
}

impl egui_dock::TabViewer for App {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(&*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Receiver(id) => match self.receiver(*id) {
                Some(p) => format!("{id} · {}", p.source.info().label),
                None => format!("{id} · empty"),
            }
            .into(),
            t => t.name().into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Receiver(id) => self.receiver_pane(ui, *id),
            Tab::Sources => {
                egui::ScrollArea::vertical().show(ui, |ui| self.sources_panel(ui));
            }
            Tab::Inspector => {
                egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui));
            }
            Tab::Emitters => self.emitter_list(ui),
            Tab::Records => self.signal_list(ui),
            Tab::Trunking => self.trunking_panel(ui),
            Tab::Calls => self.calls_panel(ui),
            Tab::Pages => self.pages_panel(ui),
            Tab::Diagnostics => self.diagnostics_panel(ui),
        }
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        if ui.button("Pop out to a new window").on_hover_text("Ctrl+Shift+P on the focused tab").clicked() {
            self.dock_requests.push(DockReq::PopOut(tab.clone()));
            ui.close();
        }
        if self.drawing.is_some() && ui.button("Move to the main window").clicked() {
            self.dock_requests.push(DockReq::MoveTo(tab.clone(), None));
            ui.close();
        }
        for (id, title) in self.popout_names.clone() {
            if Some(id) != self.drawing && ui.button(format!("Move to window: {title}")).clicked() {
                self.dock_requests.push(DockReq::MoveTo(tab.clone(), Some(id)));
                ui.close();
            }
        }
    }

    /// Receiver slots are closed from the pane itself, not the tab.
    fn closeable(&mut self, tab: &mut Tab) -> bool {
        !matches!(tab, Tab::Receiver(_))
    }

    fn allowed_in_windows(&self, _tab: &mut Tab) -> bool {
        true
    }

    /// Tables and spectrum views manage their own scrolling.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}

/// The default arrangement for a window `width` points wide: Sources over
/// the inspector at the left, receivers stacked in the middle, the tables
/// below (beside, from 2560 points: 1440p, or 4K at 150%).
fn default_dock(width: f32, receivers: &[ReceiverId]) -> DockState<Tab> {
    let mut rx: Vec<ReceiverId> = receivers.to_vec();
    rx.sort_by_key(|r| (!r.is_sweep(), *r));
    if rx.is_empty() {
        rx.push(ReceiverId::live(0));
    }
    let mut ds = DockState::new(vec![Tab::Receiver(rx[0])]);
    let tree = ds.main_surface_mut();
    let [center, left] = tree.split_left(NodeIndex::root(), if width < 1600.0 { 0.22 } else { 0.16 }, vec![Tab::Sources]);
    tree.split_below(left, 0.38, vec![Tab::Inspector]);
    let tables: Vec<Tab> = Tab::PANELS.iter().filter(|t| t.is_table()).cloned().collect();
    let [mut node, _] = if width >= 2560.0 { tree.split_right(center, 0.62, tables) } else { tree.split_below(center, 0.64, tables) };
    // Receivers stack: the sweeper keeps 38%, live receivers share equally.
    for (k, id) in rx.iter().enumerate().skip(1) {
        let remaining = (rx.len() - k) as f32;
        let frac = if rx[k - 1].is_sweep() { 0.38 } else { 1.0 / (remaining + 1.0) };
        let [_, new] = tree.split_below(node, frac, vec![Tab::Receiver(*id)]);
        node = new;
    }
    ds
}

/// Adds a tab the layout lacks: a receiver below the last receiver pane,
/// anything else beside the tables.
fn place_tab(dock: &mut DockState<Tab>, tab: Tab) {
    let last_of = |pred: &dyn Fn(&Tab) -> bool| {
        dock.iter_all_tabs().filter(|(path, t)| path.surface == SurfaceIndex::main() && pred(t)).map(|(path, _)| path).last()
    };
    match &tab {
        Tab::Receiver(_) => match last_of(&|t| matches!(t, Tab::Receiver(_))) {
            Some(path) => {
                dock.main_surface_mut().split_below(path.node, 0.5, vec![tab]);
            }
            None => dock.push_to_first_leaf(tab),
        },
        _ => match last_of(&|t| t.is_table()) {
            Some(path) => {
                dock.set_focused_node_and_surface(egui_dock::NodePath { surface: path.surface, node: path.node });
                dock.push_to_focused_leaf(tab);
            }
            None => dock.push_to_first_leaf(tab),
        },
    }
}

fn dock_style(ui: &egui::Ui) -> egui_dock::Style {
    let mut st = egui_dock::Style::from_egui(ui.style());
    st.tab_bar.height = 24.0;
    st.tab_bar.bg_fill = theme::BAR;
    st
}

fn workspace_dir() -> std::path::PathBuf {
    catalog_path().with_file_name("workspaces")
}

fn workspace_path() -> std::path::PathBuf {
    workspace_dir().join("last.json")
}

/// Saved workspace names (files in the workspaces folder, without `last`).
fn list_workspaces() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(workspace_dir())
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()).filter(|_| e.path().extension().is_some_and(|x| x == "json")))
                .filter(|n| n != "last")
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn write_workspace(path: &std::path::Path, w: &Workspace) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    match serde_json::to_vec(w) {
        Ok(b) => {
            if let Err(e) = std::fs::write(path, b) {
                log::warn!("{}: {e}", path.display());
            }
        }
        Err(e) => log::warn!("workspace: {e}"),
    }
}

/// A built-in layout: the default with the table that matters in front.
fn preset_dock(name: &str, width: f32, receivers: &[ReceiverId]) -> DockState<Tab> {
    let mut ds = default_dock(width, receivers);
    let front = match name {
        "trunking" => Tab::Calls,
        "paging" => Tab::Pages,
        _ => Tab::Emitters,
    };
    if let Some(path) = ds.find_tab(&front) {
        let _ = ds.set_active_tab(path);
    }
    ds
}

fn load_workspace() -> Option<Workspace> {
    read_workspace(&workspace_path())
}

fn read_workspace(path: &std::path::Path) -> Option<Workspace> {
    let b = std::fs::read(path).ok()?;
    let mut v: serde_json::Value = match serde_json::from_slice(&b) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("workspace: {e}; using the default layout");
            return None;
        }
    };
    // egui_dock saves its (not yet laid out) rects as NaN, which JSON
    // writes as null and can't read back as a number; they're recomputed
    // on the first frame anyway.
    fn unnull(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                if m.contains_key("x") && m.contains_key("y") {
                    for k in ["x", "y"] {
                        if m[k].is_null() {
                            m[k] = serde_json::json!(0.0);
                        }
                    }
                }
                m.values_mut().for_each(unnull);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(unnull),
            _ => {}
        }
    }
    unnull(&mut v);
    match serde_json::from_value::<Workspace>(v) {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!("workspace: {e}; using the default layout");
            None
        }
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

/// SigMF annotations for records on a receiver.
fn annotations(focus: &Receiver, rs: &[engine::SignalRecord]) -> Vec<engine::sigmf::ExportAnnotation> {
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
}

/// The identified system a call belongs to (its key and control channel,
/// for name lookups). A call only heard on its voice channel names no
/// system; with one system known, it's that one.
fn system_for(systems: &[engine::TrunkSystem], key: &Option<engine::SystemKey>) -> Option<(engine::SystemKey, Option<f64>)> {
    let s = match key {
        Some(k) => systems.iter().find(|s| s.key.same(k)),
        None if systems.len() == 1 => systems.first(),
        None => None,
    };
    match (s, key) {
        (Some(s), _) => Some((s.key.clone(), s.control_hz.first().copied())),
        (None, Some(k)) => Some((k.clone(), None)),
        (None, None) => None,
    }
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
