//! PAGES, the thing the pager domain cares about (spec §5): every page the POCSAG and FLEX monitors decode, newest
//! first, gathered by the app from each monitor's newest lines. A line is a page the first time it is seen (its time is
//! when the app saw it); a fuller version of a page cut short replaces it. The text is shown here, live, and nowhere
//! else: tests and screenshots don't carry real pages.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use api::{EmitterId, Snapshot, SourceId, fmt};
use egui::{RichText, Sense, vec2};

use crate::selection::Selection;
use crate::theme;

/// Pages kept.
const KEPT: usize = 2000;
const ROW_H: f32 = 18.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub heard: SystemTime,
    pub source: SourceId,
    pub emitter: EmitterId,
    pub protocol: String,
    pub center_hz: f64,
    /// Capcode or address, with its function or phase ("1234567 (A)", "001234567 A alpha").
    pub who: String,
    pub text: String,
    pub damaged: bool,
    line: String,
}

/// Splits a monitor's line ("1234567 (A) [damaged]: text") into who, text and whether it was damaged.
pub fn parse(line: &str) -> (String, String, bool) {
    let (who, text) = line.split_once(": ").unwrap_or((line, ""));
    let damaged = who.contains("[damaged]");
    let who = who.replace("[damaged]", "").replace("[continued]", "").trim().to_string();
    (who, text.to_string(), damaged)
}

#[derive(Default)]
pub struct PagesPane {
    /// Newest last.
    pub pages: VecDeque<Page>,
    /// Lines already taken, by monitor.
    seen: HashMap<(SourceId, EmitterId, String), VecDeque<String>>,
    pub filter: String,
}

impl PagesPane {
    /// Takes the new lines from every pager monitor in `snap`.
    pub fn ingest(&mut self, snap: &Snapshot, now: SystemTime) {
        for d in snap.decoders.iter().filter(|d| d.mode == "POCSAG" || d.mode == "FLEX") {
            let key = (d.source, d.emitter, d.mode.clone());
            let seen = self.seen.entry(key).or_default();
            for line in &d.recent {
                if seen.contains(line) {
                    continue;
                }
                seen.push_back(line.clone());
                while seen.len() > 64 {
                    seen.pop_front();
                }
                let (who, text, damaged) = parse(line);
                // A fuller version of a page cut short takes its place.
                let mine = |p: &Page| p.source == d.source && p.emitter == d.emitter && p.protocol == d.mode;
                if let Some(p) = self.pages.iter_mut().rev().find(|p| mine(p) && line.starts_with(&p.line)) {
                    (p.who, p.text, p.damaged, p.line) = (who, text, damaged, line.clone());
                    continue;
                }
                self.pages.push_back(Page {
                    heard: now,
                    source: d.source,
                    emitter: d.emitter,
                    protocol: d.mode.clone(),
                    center_hz: d.center_hz,
                    who,
                    text,
                    damaged,
                    line: line.clone(),
                });
                while self.pages.len() > KEPT {
                    self.pages.pop_front();
                }
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot, sel: &mut Selection) {
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("capcode, words, MHz…").desired_width(180.0));
        });
        let words: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        let rows: Vec<&Page> = self
            .pages
            .iter()
            .rev()
            .filter(|p| {
                words.iter().all(|w| {
                    p.who.to_lowercase().contains(w.as_str())
                        || p.text.to_lowercase().contains(w.as_str())
                        || format!("{:.4}", p.center_hz / 1e6).contains(w.as_str())
                        || p.protocol.to_lowercase() == *w
                })
            })
            .collect();
        let monitors = snap.decoders.iter().filter(|d| d.mode == "POCSAG" || d.mode == "FLEX").count();
        ui.label(
            RichText::new(format!(
                "{} of {} since the app started · {} running",
                fmt::count(rows.len(), "page"),
                self.pages.len(),
                fmt::count(monitors, "pager monitor")
            ))
            .color(theme::INK2)
            .small(),
        );
        if self.pages.is_empty() {
            ui.label(RichText::new("none yet: a page appears once a POCSAG or FLEX monitor decodes one").italics().color(theme::INK2));
            return;
        }
        ui.separator();
        const COLS: [(&str, f32); 5] = [("Heard", 74.0), ("Rx", 28.0), ("Protocol", 64.0), ("MHz", 84.0), ("Capcode", 150.0)];
        ui.horizontal(|ui| {
            for (name, w) in COLS {
                ui.add_sized(vec2(w, ROW_H), egui::Label::new(theme::label(name)));
            }
            ui.label(theme::label("Message"));
        });
        let letter = |id: SourceId| snap.receivers.iter().find(|r| r.id == id).map_or('–', |r| r.letter);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for p in &rows[range] {
                let me = Selection::Emitter(p.emitter);
                let ink = if *sel == me { theme::ACCENT } else { theme::INK };
                let resp = ui
                    .horizontal(|ui| {
                        let cells = [
                            fmt::clock(p.heard),
                            letter(p.source).to_string(),
                            p.protocol.clone(),
                            format!("{:.4}", p.center_hz / 1e6),
                            p.who.clone(),
                        ];
                        for ((_, w), text) in COLS.iter().zip(cells) {
                            ui.add_sized(vec2(*w, ROW_H), egui::Label::new(RichText::new(text).monospace().color(ink)).truncate());
                        }
                        if p.damaged {
                            ui.label(RichText::new("damaged").color(theme::INK2).italics().small());
                        }
                        ui.add(egui::Label::new(RichText::new(&p.text).color(ink)).truncate());
                    })
                    .response
                    .interact(Sense::click());
                if resp.on_hover_text("Select its channel's emitter").clicked() {
                    sel.toggle(me);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_splits_into_who_text_and_damage() {
        assert_eq!(parse("1234567 (A) [damaged]: SYNTHETIC TEST"), ("1234567 (A)".into(), "SYNTHETIC TEST".into(), true));
        assert_eq!(
            parse("001246802 A alpha: MAINTENANCE WINDOW 0200-0400"),
            ("001246802 A alpha".into(), "MAINTENANCE WINDOW 0200-0400".into(), false)
        );
    }

    #[test]
    fn a_page_is_kept_once_and_a_fuller_version_replaces_the_cut_one() {
        let mut snap = Snapshot::empty();
        let d = api::DecoderView {
            source: SourceId(1),
            emitter: EmitterId(3),
            mode: "POCSAG".into(),
            center_hz: 931.7125e6,
            lamp: api::Lamp::Locked,
            summary: String::new(),
            blocks_ok: 1,
            blocks_bad: 0,
            level_dbfs: None,
            channel: None,
            recent: vec!["1234567 (A): SYNTHETIC".into()],
        };
        snap.decoders.push(d.clone());
        let mut pane = PagesPane::default();
        let t = SystemTime::UNIX_EPOCH;
        pane.ingest(&snap, t);
        pane.ingest(&snap, t);
        assert_eq!(pane.pages.len(), 1);
        snap.decoders[0].recent = vec!["1234567 (A): SYNTHETIC TEST PAGE".into(), "7654321 (D): SECOND".into()];
        pane.ingest(&snap, t);
        let texts: Vec<_> = pane.pages.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["SYNTHETIC TEST PAGE", "SECOND"]);
    }
}
