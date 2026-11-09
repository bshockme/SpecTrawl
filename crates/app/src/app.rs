//! The window: a top bar, the dock of panes, and the loop that turns snapshots into panes and clicks into commands.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use api::{Command, Handle, RecordId, Snapshot, SourceId};
use egui_dock::{DockArea, DockState, NodeIndex, Tree};

use crate::catalogue_pane::CataloguePane;
use crate::devices_pane::{self, BudgetDraft};
use crate::emitters_pane::EmittersPane;
use crate::health::History;
use crate::inspector::Inspector;
use crate::log_pane::LogPane;
use crate::pages_pane::PagesPane;
use crate::receiver_pane::{DEFAULT_DB_RANGE, ReceiverPane};
use crate::records_pane::RecordsPane;
use crate::selection::Selection;
use crate::status_bar::StatusBar;
use crate::tabs::Tab;
use crate::theme;

/// The eframe storage key everything this app persists lives under.
const STORAGE_KEY: &str = "spectrawl-next";

/// The Devices column's share of the window: wide enough for a row with `open as A` and `Close`.
const DEVICES_SHARE: f32 = 0.3;

/// Devices' share of the left column; the inspector takes the rest.
const INSPECTOR_SPLIT: f32 = 0.3;

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
    records: RecordsPane,
    emitters: EmittersPane,
    pages: PagesPane,
    catalogue: CataloguePane,
    log: LogPane,
    status: StatusBar,
    /// What is selected, shared by every pane and shown by the inspector.
    selection: Selection,
    inspector: Inspector,
    /// The health plots' history.
    health: History,
    /// The house theme and type are installed in this context.
    styled: bool,
}

impl App {
    /// Devices at the left, Log at the right; the first receiver opens above the Log.
    pub fn new(handle: Handle) -> App {
        let snap = handle.latest();
        let mut dock = DockState::new(vec![Tab::Devices]);
        let [left, _] = dock.main_surface_mut().split_right(
            NodeIndex::root(),
            DEVICES_SHARE,
            vec![Tab::Log, Tab::Records, Tab::Emitters, Tab::Systems, Tab::Channels, Tab::Pages, Tab::Catalogue],
        );
        // The inspector under Devices (docs/ui-design.md §4: docked left).
        dock.main_surface_mut().split_below(left, INSPECTOR_SPLIT, vec![Tab::Inspector]);
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
            records: RecordsPane::new(),
            emitters: EmittersPane::default(),
            pages: PagesPane::default(),
            catalogue: CataloguePane::default(),
            log: LogPane::default(),
            status: StatusBar::default(),
            selection: Selection::None,
            inspector: Inspector::default(),
            health: History::default(),
            styled: false,
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
        // A context the launcher didn't style (a test's) gets the theme here; fonts take effect from the next frame, so
        // this frame draws nothing.
        if !self.styled {
            let ctx = ui.ctx().clone();
            if ctx.fonts(|f| f.definitions().families.contains_key(&theme::label_family())) {
                self.styled = true;
            } else {
                theme::apply(&ctx);
                ctx.request_repaint();
                return;
            }
        }
        self.snap = self.handle.latest();
        if self.seen_version != Some(self.snap.version) {
            self.sync();
        }
        self.health.sample(&self.snap, std::time::Instant::now());

        egui::Panel::top("top_bar").frame(theme::bar_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::wordmark(ui);
                ui.add_space(12.0);
                ui.menu_button("View ⏷", |ui| {
                    for tab in [
                        Tab::Devices,
                        Tab::Inspector,
                        Tab::Log,
                        Tab::Records,
                        Tab::Emitters,
                        Tab::Systems,
                        Tab::Channels,
                        Tab::Pages,
                        Tab::Catalogue,
                    ] {
                        let shown = self.dock.find_tab(&tab).is_some();
                        if ui.add_enabled(!shown, egui::Button::new(tab.name())).clicked() {
                            reopen(&mut self.dock, tab);
                            ui.close();
                        }
                    }
                });
            });
        });
        let snap = self.snap.clone();
        egui::Panel::top("status_bar").frame(theme::status_frame()).show(ui, |ui| self.status.show(ui, &snap));

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
        for c in std::mem::take(&mut self.out) {
            let query = matches!(c, Command::QueryRecords(_));
            let seq = self.handle.send(c);
            if query {
                self.records.search_seq = Some(seq);
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn pane(&self, id: SourceId) -> Option<&ReceiverPane> {
        self.panes.get(&id)
    }

    pub fn records_pane(&self) -> &RecordsPane {
        &self.records
    }

    pub fn records_pane_mut(&mut self) -> &mut RecordsPane {
        &mut self.records
    }

    /// The selected record, when a record is selected.
    pub fn selected(&self) -> Option<RecordId> {
        self.selection.record()
    }

    /// Selects a record, or clears the selection.
    pub fn select(&mut self, id: Option<RecordId>) {
        self.selection = id.map_or(Selection::None, Selection::Record);
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    pub fn set_selection(&mut self, s: Selection) {
        self.selection = s;
    }

    pub fn inspector(&self) -> &Inspector {
        &self.inspector
    }

    /// Queues a command as a pane would (sent at the end of the next frame).
    /// Brings `tab` to the front of its dock node, reopening it if it was closed.
    pub fn show_tab(&mut self, tab: Tab) {
        if self.dock.find_tab(&tab).is_none() {
            reopen(&mut self.dock, tab.clone());
        }
        if let Some(path) = self.dock.find_tab(&tab) {
            let _ = self.dock.set_active_tab(path);
        }
    }

    pub fn push_command(&mut self, c: Command) {
        self.out.push(c);
    }

    /// New receivers get a pane and a tab; gone ones lose both. New pages are taken.
    fn sync(&mut self) {
        let snap = self.snap.clone();
        self.pages.ingest(&snap, std::time::SystemTime::now());
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
        remove_tabs(&mut self.dock, |t| matches!(t, Tab::Receiver(id) if !alive(id)));
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
                    let mine: Vec<_> = snap.records.records.iter().filter(|x| x.source == r.id).cloned().collect();
                    let decoders: Vec<_> = snap.decoders.iter().filter(|d| d.source == r.id).cloned().collect();
                    p.show(ui, r, &mine, &decoders, &mut self.selection, &mut self.out);
                }
            }
            Tab::Devices => devices_pane::show(ui, &snap, &mut self.budget_draft, &mut self.out),
            Tab::Log => self.log.show(ui, &snap),
            Tab::Records => self.records.show(ui, &snap, &mut self.selection, &mut self.out),
            Tab::Emitters => self.emitters.show(ui, &snap, &mut self.selection),
            Tab::Pages => self.pages.show(ui, &snap, &mut self.selection),
            Tab::Catalogue => self.catalogue.show(ui, &snap),
            Tab::Inspector => self.inspector.show(ui, &snap, &mut self.selection, &mut self.out, &self.health),
            Tab::Systems => crate::systems_pane::show(ui, &snap, &mut self.selection),
            Tab::Channels => crate::channels_pane::show(ui, &snap, &mut self.selection),
        }
    }

    /// Receivers are closed from their `…` menu, which closes the device.
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

/// Removes every tab matching `gone`, one at a time: `remove_tab` moves a promoted sibling's whole
/// subtree, where `retain_tabs` moves only its top node and can orphan the tabs below it.
fn remove_tabs(dock: &mut DockState<Tab>, gone: impl Fn(&Tab) -> bool) {
    while let Some(path) = dock.find_tab_from(&gone) {
        dock.remove_tab(path);
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
        Tab::Systems | Tab::Channels | Tab::Emitters | Tab::Pages | Tab::Catalogue => {
            match tree.find_tab(&Tab::Records).or_else(|| tree.find_tab(&Tab::Log)) {
                Some((node, _)) => tree[node].append_tab(tab),
                None => {
                    tree.split_right(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
                }
            }
        }
        Tab::Records => match tree.find_tab(&Tab::Log) {
            Some((node, _)) => tree[node].append_tab(tab),
            None => match tree.find_tab_from(|t| matches!(t, Tab::Receiver(_))) {
                Some((node, _)) => {
                    tree.split_below(node, 0.7, vec![tab]);
                }
                None => {
                    tree.split_right(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
                }
            },
        },
        Tab::Inspector => match tree.find_tab(&Tab::Devices) {
            Some((node, _)) => {
                tree.split_below(node, INSPECTOR_SPLIT, vec![tab]);
            }
            None => {
                tree.split_left(NodeIndex::root(), DEVICES_SHARE, vec![tab]);
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-empty node below the root hangs from a split, and the tree's tabs are the ones given.
    fn assert_whole(dock: &DockState<Tab>, tabs: &[Tab]) {
        let nodes: Vec<_> = dock.main_surface().iter().collect();
        for (i, n) in nodes.iter().enumerate().skip(1) {
            if !n.is_empty() {
                assert!(nodes[(i - 1) / 2].is_parent(), "node {i} is orphaned");
            }
        }
        for t in tabs {
            assert!(dock.find_tab(t).is_some(), "{t:?} is gone");
        }
        let leaf_tabs: usize = nodes.iter().filter_map(|n| n.tabs()).map(|t| t.len()).sum();
        assert_eq!(leaf_tabs, tabs.len());
    }

    #[test]
    fn closing_the_last_receiver_keeps_devices_under_log() {
        let rx = Tab::Receiver(SourceId(0));
        let mut dock = DockState::new(vec![Tab::Devices]);
        dock.main_surface_mut().split_right(NodeIndex::root(), DEVICES_SHARE, vec![Tab::Log]);
        place_receiver(dock.main_surface_mut(), rx.clone());
        // The user drags Devices into a split under the Log: receiver over (Log over Devices).
        let path = dock.find_tab(&Tab::Devices).unwrap();
        dock.remove_tab(path);
        let (log, _) = dock.main_surface().find_tab(&Tab::Log).unwrap();
        dock.main_surface_mut().split_below(log, 0.5, vec![Tab::Devices]);
        assert_whole(&dock, &[rx.clone(), Tab::Log, Tab::Devices]);

        remove_tabs(&mut dock, |t| *t == rx);
        assert_whole(&dock, &[Tab::Log, Tab::Devices]);
    }
}
