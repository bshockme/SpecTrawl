//! The window: a top bar, the dock of panes, and the loop that turns snapshots into panes and clicks into commands.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use api::{Command, Handle, RecordId, Snapshot, SourceId};
use egui_dock::{DockArea, DockState, NodeIndex, Tree};
use serde_json::Value;

use crate::catalogue_pane::CataloguePane;
use crate::devices_pane::{self, BudgetDraft};
use crate::emitters_pane::EmittersPane;
use crate::health::History;
use crate::inspector::Inspector;
use crate::log_pane::LogPane;
use crate::pages_pane::PagesPane;
use crate::palette::{Action, Palette};
use crate::receiver_pane::{DEFAULT_DB_RANGE, ReceiverPane};
use crate::records_pane::RecordsPane;
use crate::selection::Selection;
use crate::status_bar::StatusBar;
use crate::tabs::Tab;
use crate::theme;
use crate::workspace::{self, DEVICES_SHARE, INSPECTOR_SPLIT};

/// The eframe storage key everything this app persists lives under.
const STORAGE_KEY: &str = "spectrawl-next";

/// A tab in a window of its own (docs/ui-design.md §5): an immediate viewport with its own dock.
struct PopOut {
    id: u64,
    title: String,
    dock: DockState<Tab>,
}

/// A change to the docks asked for while they were being drawn.
enum DockReq {
    PopOut(Tab),
    Home(Tab),
}

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
    /// Saved layouts, and the page (workspace) in use.
    workspaces: workspace::Saved,
    page: String,
    save_as: String,
    popouts: Vec<PopOut>,
    next_popout: u64,
    /// The pop-out being drawn, while one is.
    drawing: Option<u64>,
    dock_requests: Vec<DockReq>,
    palette: Palette,
}

impl App {
    /// The Survey layout: Devices over the inspector at the left, the tables at the right; the first receiver opens
    /// above them.
    pub fn new(handle: Handle) -> App {
        let snap = handle.latest();
        App {
            handle,
            snap,
            seen_version: None,
            dock: workspace::preset("Survey").expect("a preset"),
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
            workspaces: workspace::Saved::default(),
            page: "Survey".into(),
            save_as: String::new(),
            popouts: Vec::new(),
            next_popout: 1,
            drawing: None,
            dock_requests: Vec::new(),
            palette: Palette::default(),
        }
    }

    /// The ring budget the engine was started with, as the Devices pane's starting draft.
    pub fn with_budget(mut self, draft: BudgetDraft) -> App {
        self.budget_draft = draft;
        self
    }

    /// Reads the dB ranges saved by serial, the saved layouts, and the layout last used.
    pub fn restore(&mut self, storage: &dyn eframe::Storage) {
        if let Some(text) = storage.get_string(STORAGE_KEY) {
            match serde_json::from_str::<Persisted>(&text) {
                Ok(p) => {
                    self.db_ranges = p.db_ranges;
                    self.workspaces = p.workspaces;
                    if let Some(dock) = self.workspaces.last.as_ref().and_then(workspace::load) {
                        self.dock = dock;
                        self.page = self.workspaces.active.clone().unwrap_or_else(|| "Survey".into());
                    }
                }
                Err(e) => log::warn!("ignoring the saved settings: {e}"),
            }
        }
    }

    /// Whether the command palette is open.
    pub fn palette_open(&self) -> bool {
        self.palette.open
    }

    /// The tabs in pop-out windows.
    pub fn popped_out(&self) -> Vec<Tab> {
        self.popouts.iter().flat_map(|p| p.dock.iter_all_tabs().map(|(_, t)| t.clone())).collect()
    }

    /// The page in use: a preset's name or a saved layout's.
    pub fn page(&self) -> &str {
        &self.page
    }

    /// Switches to a preset or a saved layout; the receivers open now are placed back in it.
    pub fn load_page(&mut self, name: &str) {
        let dock = match workspace::preset(name) {
            Some(d) => d,
            None => match self.workspaces.named.get(name).and_then(workspace::load) {
                Some(d) => d,
                None => return,
            },
        };
        self.dock = dock;
        // Tabs in pop-out windows stay there; the main dock gives up its copies of them.
        let popped: Vec<Tab> = self.popouts.iter().flat_map(|p| p.dock.iter_all_tabs().map(|(_, t)| t.clone())).collect();
        remove_tabs(&mut self.dock, |t| popped.contains(t));
        for r in self.snap.receivers.iter().map(|r| Tab::Receiver(r.id)) {
            if !popped.contains(&r) {
                place_receiver(self.dock.main_surface_mut(), r);
            }
        }
        self.page = name.to_string();
    }

    /// Saves the current layout under `name` (a preset's name can't be taken).
    pub fn save_page(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() || workspace::PRESETS.contains(&name) {
            return;
        }
        if let Some(v) = workspace::save(&self.dock) {
            self.workspaces.named.insert(name.to_string(), v);
            self.page = name.to_string();
        }
    }

    fn saved_names(&self) -> Vec<String> {
        self.workspaces.named.keys().cloned().collect()
    }

    /// The page bar: the presets in workflow order, then the saved layouts; the one in use carries the house gold
    /// underline (the guide's signature §2).
    fn page_bar(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = workspace::PRESETS.iter().map(|s| s.to_string()).chain(self.saved_names()).collect();
        for name in names {
            let here = name == self.page;
            let text = egui::RichText::new(&name).color(if here { theme::INK } else { theme::INK2 });
            let resp = ui.add(egui::Button::new(text).frame(false)).on_hover_text(if here {
                "The layout in use"
            } else {
                "Switch to this layout"
            });
            if here {
                let r = resp.rect;
                ui.painter().line_segment(
                    [egui::pos2(r.left() + 4.0, r.bottom() + 1.0), egui::pos2(r.right() - 4.0, r.bottom() + 1.0)],
                    egui::Stroke::new(2.0, theme::HOUSE),
                );
            }
            if resp.clicked() && !here {
                self.load_page(&name);
            }
        }
    }

    /// The `View ⏷` menu: the panes to show, saving and deleting layouts, Reset layout, and the palette.
    fn view_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(240.0);
        ui.label(theme::label("Panes"));
        for tab in
            [Tab::Devices, Tab::Inspector, Tab::Log, Tab::Records, Tab::Emitters, Tab::Systems, Tab::Channels, Tab::Pages, Tab::Catalogue]
        {
            let shown = self.tab_shown(&tab);
            if ui.add_enabled(!shown, egui::Button::new(tab.name())).clicked() {
                reopen(&mut self.dock, tab);
                ui.close();
            }
        }
        ui.separator();
        ui.label(theme::label("Layouts"));
        if self.workspaces.named.contains_key(&self.page) && ui.button(format!("Save over \"{}\"", self.page)).clicked() {
            let name = self.page.clone();
            self.save_page(&name);
            ui.close();
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.save_as).desired_width(130.0).hint_text("name"));
            let name = self.save_as.trim().to_string();
            let ok = !name.is_empty() && !workspace::PRESETS.contains(&name.as_str());
            if ui.add_enabled(ok, egui::Button::new("Save as")).clicked() {
                self.save_page(&name);
                self.save_as.clear();
                ui.close();
            }
        });
        let mut delete = None;
        for name in self.saved_names() {
            ui.horizontal(|ui| {
                ui.label(&name);
                if ui.small_button("Delete").on_hover_text("Forget this saved layout").clicked() {
                    delete = Some(name.clone());
                }
            });
        }
        if let Some(name) = delete {
            self.workspaces.named.remove(&name);
            if self.page == name {
                self.page = "Survey".into();
            }
        }
        if ui.button("Reset layout").on_hover_text("Put this page's panes back where its layout has them").clicked() {
            let page = self.page.clone();
            self.load_page(if workspace::PRESETS.contains(&page.as_str()) || self.workspaces.named.contains_key(&page) {
                &page
            } else {
                "Survey"
            });
            ui.close();
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Command palette").clicked() {
                self.palette.toggle();
                ui.close();
            }
            ui.label(egui::RichText::new("Ctrl+K").monospace().color(theme::INK2));
        });
    }

    /// Whether a tab is in the main dock or a pop-out.
    fn tab_shown(&self, tab: &Tab) -> bool {
        self.dock.find_tab(tab).is_some() || self.popouts.iter().any(|p| p.dock.find_tab(tab).is_some())
    }

    /// Carries out a palette action.
    fn run(&mut self, a: Action) {
        match a {
            Action::Show(t) => self.show_tab(t),
            Action::PopOut(t) => {
                self.show_tab(t.clone());
                self.dock_requests.push(DockReq::PopOut(t));
            }
            Action::Workspace(name) => self.load_page(&name),
            Action::ResetLayout => {
                let page = self.page.clone();
                self.load_page(&page);
            }
            Action::ClearSelection => self.selection = Selection::None,
            Action::Tune(source, center_hz) => self.out.push(Command::Tune { source, center_hz }),
            Action::InspectEmitter(e) => {
                self.selection = Selection::Emitter(e);
                self.show_tab(Tab::Inspector);
            }
        }
    }

    /// Moves tabs between the main dock and the pop-outs, as asked while drawing.
    fn apply_dock_requests(&mut self) {
        for r in std::mem::take(&mut self.dock_requests) {
            match r {
                DockReq::PopOut(tab) => {
                    let Some(path) = self.dock.find_tab(&tab) else { continue };
                    self.dock.remove_tab(path);
                    let title = match &tab {
                        Tab::Receiver(id) => self
                            .snap
                            .receivers
                            .iter()
                            .find(|r| r.id == *id)
                            .map_or("Receiver".into(), |r| format!("{} · {}", r.letter, r.device.label)),
                        t => t.name().to_string(),
                    };
                    self.popouts.push(PopOut {
                        id: self.next_popout,
                        title: format!("SpecTrawl · {title}"),
                        dock: DockState::new(vec![tab]),
                    });
                    self.next_popout += 1;
                }
                DockReq::Home(tab) => {
                    for p in &mut self.popouts {
                        if let Some(path) = p.dock.find_tab(&tab) {
                            p.dock.remove_tab(path);
                        }
                    }
                    self.come_home(tab);
                }
            }
        }
    }

    /// Puts a tab back in the main dock.
    fn come_home(&mut self, tab: Tab) {
        if self.dock.find_tab(&tab).is_some() {
            return;
        }
        match tab {
            Tab::Receiver(_) => place_receiver(self.dock.main_surface_mut(), tab),
            t => reopen(&mut self.dock, t),
        }
    }

    /// Draws every pop-out in its own window; a closed window's tabs come home.
    fn draw_popouts(&mut self, ctx: &egui::Context) {
        let mut pops = std::mem::take(&mut self.popouts);
        let mut closed = Vec::new();
        for (i, po) in pops.iter_mut().enumerate() {
            let vid = egui::ViewportId::from_hash_of(("popout", po.id));
            let vb = egui::ViewportBuilder::default().with_title(po.title.clone()).with_inner_size([960.0, 640.0]);
            self.drawing = Some(po.id);
            let mut dock = std::mem::replace(&mut po.dock, DockState::new(Vec::new()));
            let close = ctx.show_viewport_immediate(vid, vb, |ui, _class| {
                let close = ui.input(|i| i.viewport().close_requested());
                egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| {
                    DockArea::new(&mut dock)
                        .id(egui::Id::new(("popout dock", po.id)))
                        .show_add_buttons(false)
                        .show_leaf_close_all_buttons(false)
                        .show_leaf_collapse_buttons(false)
                        .show_inside(ui, self);
                });
                close
            });
            po.dock = dock;
            if close || po.dock.iter_all_tabs().next().is_none() {
                closed.push(i);
            }
        }
        self.drawing = None;
        for i in closed.into_iter().rev() {
            let po = pops.remove(i);
            for (_, t) in po.dock.iter_all_tabs() {
                self.come_home(t.clone());
            }
        }
        self.popouts = pops;
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

        // Keys: Ctrl+K opens the palette; Esc clears the selection when nothing else takes it.
        let ctx = ui.ctx().clone();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            self.palette.toggle();
        }
        if !self.palette.open && !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selection = Selection::None;
        }

        egui::Panel::top("top_bar").frame(theme::bar_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::wordmark(ui);
                ui.add_space(16.0);
                self.page_bar(ui);
                ui.add_space(8.0);
                ui.separator();
                ui.menu_button("View ⏷", |ui| self.view_menu(ui));
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
        self.draw_popouts(&ctx);
        let names = self.saved_names();
        let snap = self.snap.clone();
        if let Some(a) = self.palette.show(&ctx, &snap, &names) {
            self.run(a);
        }
        self.apply_dock_requests();

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
        if !self.tab_shown(&tab) {
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
            if !self.tab_shown(&tab) {
                place_receiver(self.dock.main_surface_mut(), tab);
            }
        }
        let alive = |id: &SourceId| snap.receivers.iter().any(|r| r.id == *id);
        self.panes.retain(|id, _| alive(id));
        remove_tabs(&mut self.dock, |t| matches!(t, Tab::Receiver(id) if !alive(id)));
        for p in &mut self.popouts {
            remove_tabs(&mut p.dock, |t| matches!(t, Tab::Receiver(id) if !alive(id)));
        }
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
    #[serde(default)]
    workspaces: workspace::Saved,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui)
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // The main window's layout as it is now; pop-outs come back in the main window next time.
        let mut ws = self.workspaces.clone();
        ws.last = workspace::save(&self.dock).map(|mut v: Value| {
            if let Ok(mut d) = serde_json::from_value::<DockState<Tab>>(v.clone()) {
                for p in &self.popouts {
                    for (_, t) in p.dock.iter_all_tabs() {
                        if !matches!(t, Tab::Receiver(_)) {
                            reopen(&mut d, t.clone());
                        }
                    }
                }
                v = serde_json::to_value(&d).unwrap_or(v);
            }
            v
        });
        ws.active = Some(self.page.clone());
        match serde_json::to_string(&Persisted { db_ranges: self.db_ranges.clone(), workspaces: ws }) {
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
                    p.show(ui, r, &mine, &decoders, &mut self.selection, &snap.band_plan, &mut self.out);
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

    /// Tabs may float inside the window they're in (drag one off its strip).
    fn allowed_in_windows(&self, _tab: &mut Tab) -> bool {
        true
    }

    /// A tab's right-click menu: pop it out into a window of its own, or send it back.
    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        if self.drawing.is_none() {
            if ui.button("Pop out into a window").clicked() {
                self.dock_requests.push(DockReq::PopOut(tab.clone()));
                ui.close();
            }
        } else if ui.button("Back to the main window").clicked() {
            self.dock_requests.push(DockReq::Home(tab.clone()));
            ui.close();
        }
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
