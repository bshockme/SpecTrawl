//! The window: a top bar, the dock of panes, and the loop that turns snapshots into panes and clicks into commands.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use api::{Command, Handle, Snapshot, SourceId};
use egui_dock::{DockArea, DockState, NodeIndex, Tree};

use crate::devices_pane::{self, BudgetDraft};
use crate::receiver_pane::{DEFAULT_DB_RANGE, ReceiverPane};
use crate::tabs::Tab;
use crate::{log_pane, theme};

/// The eframe storage key everything this app persists lives under.
const STORAGE_KEY: &str = "spectrawl-next";

/// The Devices column's share of the window: wide enough for a row with `open as A` and `Close`.
const DEVICES_SHARE: f32 = 0.3;

pub struct App {
    handle: Handle,
    snap: Arc<Snapshot>,
    seen_version: Option<u64>,
    dock: DockState<Tab>,
    panes: BTreeMap<SourceId, ReceiverPane>,
    budget_draft: BudgetDraft,
    /// The dB range last used for each device, by serial.
    db_ranges: HashMap<String, (f32, f32)>,
    /// Commands collected while drawing, sent once the frame is drawn.
    out: Vec<Command>,
}

impl App {
    /// Devices at the left, Log at the right; the first receiver opens above the Log.
    pub fn new(handle: Handle) -> App {
        let snap = handle.latest();
        let mut dock = DockState::new(vec![Tab::Devices]);
        dock.main_surface_mut().split_right(NodeIndex::root(), DEVICES_SHARE, vec![Tab::Log]);
        App {
            handle,
            snap,
            seen_version: None,
            dock,
            panes: BTreeMap::new(),
            // The engine's default budget where RAM cannot be read (8 GiB, half of it); `with_budget` sets the real one.
            budget_draft: BudgetDraft { guaranteed_secs: 30.0, ceiling_mb: 4096 },
            db_ranges: HashMap::new(),
            out: Vec::new(),
        }
    }

    /// The ring budget the engine was started with, as the Devices pane's starting draft.
    pub fn with_budget(mut self, draft: BudgetDraft) -> App {
        self.budget_draft = draft;
        self
    }

    /// Reads the dB ranges saved by serial.
    pub fn restore(&mut self, storage: &dyn eframe::Storage) {
        if let Some(text) = storage.get_string(STORAGE_KEY) {
            match serde_json::from_str::<Persisted>(&text) {
                Ok(p) => self.db_ranges = p.db_ranges,
                Err(e) => log::warn!("ignoring the saved settings: {e}"),
            }
        }
    }

    /// The whole frame: take in the newest snapshot, draw, send what was clicked.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        self.snap = self.handle.latest();
        if self.seen_version != Some(self.snap.version) {
            self.sync();
        }

        egui::Panel::top("top_bar").frame(theme::bar_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("SpecTrawl").strong());
                ui.menu_button("View ▾", |ui| {
                    for tab in [Tab::Devices, Tab::Log] {
                        let shown = self.dock.find_tab(&tab).is_some();
                        if ui.add_enabled(!shown, egui::Button::new(tab.name())).clicked() {
                            reopen(&mut self.dock, tab);
                            ui.close();
                        }
                    }
                });
            });
        });

        // The dock is lent to the viewer (us) while it draws.
        let mut dock = std::mem::replace(&mut self.dock, DockState::new(Vec::new()));
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| {
            DockArea::new(&mut dock)
                .id(egui::Id::new("dock"))
                .show_add_buttons(false)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, self);
        });
        self.dock = dock;

        self.remember_db_ranges();
        for c in self.out.drain(..) {
            self.handle.send(c);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn pane(&self, id: SourceId) -> Option<&ReceiverPane> {
        self.panes.get(&id)
    }

    /// New receivers get a pane and a tab; gone ones lose both.
    fn sync(&mut self) {
        let snap = self.snap.clone();
        for r in &snap.receivers {
            let pane = self.panes.entry(r.id).or_insert_with(|| {
                let mut p = ReceiverPane::new();
                if let Some(&range) = self.db_ranges.get(&r.device.serial) {
                    p.set_db_range(range);
                }
                p
            });
            pane.ingest(r);
            let tab = Tab::Receiver(r.id);
            if self.dock.find_tab(&tab).is_none() {
                place_receiver(self.dock.main_surface_mut(), tab);
            }
        }
        let alive = |id: &SourceId| snap.receivers.iter().any(|r| r.id == *id);
        self.panes.retain(|id, _| alive(id));
        self.dock.retain_tabs(|t| match t {
            Tab::Receiver(id) => alive(id),
            _ => true,
        });
        self.seen_version = Some(snap.version);
    }

    /// Keeps each open device's dB range for saving.
    fn remember_db_ranges(&mut self) {
        for r in &self.snap.receivers {
            if let Some(p) = self.panes.get(&r.id) {
                let range = p.db_range();
                if self.db_ranges.get(&r.device.serial).copied().unwrap_or(DEFAULT_DB_RANGE) != range {
                    self.db_ranges.insert(r.device.serial.clone(), range);
                }
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Persisted {
    db_ranges: HashMap<String, (f32, f32)>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui)
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        match serde_json::to_string(&Persisted { db_ranges: self.db_ranges.clone() }) {
            Ok(text) => storage.set_string(STORAGE_KEY, text),
            Err(e) => log::warn!("could not save the settings: {e}"),
        }
    }
}

impl egui_dock::TabViewer for App {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(&*tab)
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Receiver(id) => match self.snap.receivers.iter().find(|r| r.id == *id) {
                Some(r) => format!("{} · {}", r.letter, r.device.label).into(),
                None => "Receiver".into(),
            },
            t => t.name().into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let snap = self.snap.clone();
        match tab {
            Tab::Receiver(id) => {
                if let (Some(r), Some(p)) = (snap.receivers.iter().find(|r| r.id == *id), self.panes.get_mut(id)) {
                    p.show(ui, r, &mut self.out);
                }
            }
            Tab::Devices => devices_pane::show(ui, &snap, &mut self.budget_draft, &mut self.out),
            Tab::Log => log_pane::show(ui, &snap),
        }
    }

    /// Receivers are closed from their `⋯` menu, which closes the device.
    fn is_closeable(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Receiver(_))
    }

    fn allowed_in_windows(&self, _tab: &mut Tab) -> bool {
        false
    }

    /// The panes scroll themselves where they need to.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}

/// A new receiver tab joins the other receivers, opens above the Log, or failing both splits off at the right.
fn place_receiver(tree: &mut Tree<Tab>, tab: Tab) {
    if let Some((node, _)) = tree.find_tab_from(|t| matches!(t, Tab::Receiver(_))) {
        tree[node].append_tab(tab);
    } else if let Some((node, _)) = tree.find_tab(&Tab::Log) {
        tree.split_above(node, 0.7, vec![tab]);
    } else if tree.is_empty() {
        tree.push_to_first_leaf(tab);
    } else {
        tree.split_right(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
    }
}

/// Puts a closed Devices or Log tab back: Devices at the left, the Log under the receivers.
fn reopen(dock: &mut DockState<Tab>, tab: Tab) {
    let tree = dock.main_surface_mut();
    if tree.is_empty() {
        tree.push_to_first_leaf(tab);
        return;
    }
    match tab {
        Tab::Log => match tree.find_tab_from(|t| matches!(t, Tab::Receiver(_))) {
            Some((node, _)) => {
                tree.split_below(node, 0.7, vec![tab]);
            }
            None => {
                tree.split_right(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
            }
        },
        _ => {
            tree.split_left(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
        }
    }
}
