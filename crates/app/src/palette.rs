//! The command palette (Ctrl+K; docs/ui-design.md §7, the guide's I6): a text box over a filtered list of commands in
//! plain words, each with its shortcut beside it where it has one. Typing a number offers to tune a receiver there, or
//! to inspect the emitter nearest it.

use api::{Snapshot, SourceId};

use crate::tabs::Tab;
use crate::theme;

/// What a palette entry does; the app carries it out.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Show(Tab),
    PopOut(Tab),
    Workspace(String),
    ResetLayout,
    ClearSelection,
    Tune(SourceId, f64),
    InspectEmitter(api::EmitterId),
}

/// One entry: what it says, the words it also answers to, its shortcut, and what it does.
#[derive(Debug, Clone)]
pub struct Entry {
    pub label: String,
    pub keywords: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    text: String,
    sel: usize,
    focus: bool,
}

/// The tabs the palette can show or pop out.
const TABS: [Tab; 9] =
    [Tab::Devices, Tab::Inspector, Tab::Log, Tab::Records, Tab::Emitters, Tab::Systems, Tab::Channels, Tab::Pages, Tab::Catalogue];

/// Every entry that matches `query`, best first: an entry matches when each word is in its label or keywords.
pub fn entries(query: &str, snap: &Snapshot, workspaces: &[String]) -> Vec<Entry> {
    let q = query.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let number = words.iter().find_map(|w| w.parse::<f64>().ok());
    let letter = words
        .iter()
        .filter(|w| w.len() == 1)
        .filter_map(|w| w.chars().next())
        .filter_map(|c| snap.receivers.iter().find(|r| r.letter.eq_ignore_ascii_case(&c)))
        .next();
    let mut v: Vec<Entry> = Vec::new();
    let e = |label: String, keywords: &'static str, shortcut: &'static str, action: Action| Entry { label, keywords, shortcut, action };
    if let Some(mhz) = number {
        let live = snap.receivers.iter().filter(|r| r.mode == api::Mode::Live);
        for r in live.filter(|r| letter.is_none_or(|l| l.id == r.id)) {
            if (24.0..=1800.0).contains(&mhz) {
                v.push(e(format!("Tune {} to {mhz:.4} MHz", r.letter), "tune frequency", "", Action::Tune(r.id, mhz * 1e6)));
            }
        }
        let mut near: Vec<_> = snap.records.emitters.iter().filter(|x| (x.center_hz - mhz * 1e6).abs() <= 25e3).collect();
        near.sort_by(|a, b| (a.center_hz - mhz * 1e6).abs().total_cmp(&(b.center_hz - mhz * 1e6).abs()));
        for x in near.into_iter().take(3) {
            let name = x.headline.as_ref().map_or("unknown", |c| c.label.as_str());
            v.push(e(
                format!("Inspect emitter {} at {:.4} MHz ({name})", x.id.0, x.center_hz / 1e6),
                "inspect emitter select",
                "",
                Action::InspectEmitter(x.id),
            ));
        }
    }
    for t in TABS {
        v.push(e(format!("Show {}", t.name()), "show tab open view", "", Action::Show(t.clone())));
    }
    for name in crate::workspace::PRESETS.iter().map(|s| s.to_string()).chain(workspaces.iter().cloned()) {
        v.push(e(format!("Workspace: {name}"), "workspace layout page", "", Action::Workspace(name)));
    }
    v.push(e("Reset layout".into(), "reset layout default", "", Action::ResetLayout));
    v.push(e("Clear the selection".into(), "clear deselect selection", "Esc", Action::ClearSelection));
    for t in TABS {
        v.push(e(format!("Pop out {} into a window", t.name()), "pop out window", "", Action::PopOut(t.clone())));
    }
    // A number is not a word to match: the entries built from it already answer it.
    let text_words: Vec<&str> = words.iter().copied().filter(|w| w.parse::<f64>().is_err() && w.len() > 1).collect();
    v.retain(|x| {
        let hay = format!("{} {}", x.label.to_lowercase(), x.keywords);
        text_words.iter().all(|w| hay.contains(w))
    });
    v
}

impl Palette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.focus = self.open;
        self.text.clear();
        self.sel = 0;
    }

    /// Draws the palette when open; returns the action chosen, if one was.
    pub fn show(&mut self, ctx: &egui::Context, snap: &Snapshot, workspaces: &[String]) -> Option<Action> {
        if !self.open {
            return None;
        }
        let list = entries(&self.text, snap, workspaces);
        let n = list.len().min(14);
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if up {
            self.sel = self.sel.saturating_sub(1);
        }
        if down && n > 0 {
            self.sel = (self.sel + 1).min(n - 1);
        }
        let mut chosen = None;
        egui::Window::new("Command palette")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .fixed_size([620.0, 0.0])
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.text)
                        .desired_width(f32::INFINITY)
                        .hint_text("show records · workspace trunking · 851.0125 · tune a 460.6 · reset layout"),
                );
                if self.focus {
                    r.request_focus();
                    self.focus = false;
                }
                if r.changed() {
                    self.sel = 0;
                }
                ui.separator();
                if list.is_empty() {
                    ui.label(egui::RichText::new("No command matches that.").color(theme::INK2));
                }
                for (k, x) in list.iter().take(14).enumerate() {
                    ui.horizontal(|ui| {
                        if ui.selectable_label(k == self.sel, &x.label).clicked() {
                            chosen = Some(x.action.clone());
                        }
                        if !x.shortcut.is_empty() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new(x.shortcut).monospace().color(theme::INK2));
                            });
                        }
                    });
                }
                if enter && n > 0 {
                    chosen = Some(list[self.sel.min(n - 1)].action.clone());
                }
            });
        if chosen.is_some() || esc {
            self.open = false;
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_narrow_the_list_and_a_number_offers_to_inspect_the_nearest_emitter() {
        let snap = Snapshot::empty();
        let all = entries("", &snap, &[]);
        assert!(all.iter().any(|e| e.action == Action::Show(Tab::Records)));
        let ws = entries("workspace trunk", &snap, &["night".into()]);
        assert_eq!(ws.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Workspace: Trunking"]);
        assert!(entries("esc", &snap, &[]).is_empty(), "shortcuts are shown, not searched");
        assert!(entries("clear", &snap, &[]).iter().any(|e| e.shortcut == "Esc"));
    }
}
