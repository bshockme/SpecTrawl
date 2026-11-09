//! Emitters: every recently seen transmitter, occupants included (spec §5: occupants have their own view), with its
//! name, frequency, activity and when it was last heard. One not heard for a minute is drawn as a ghost with its age
//! (the guide's D5, D7). Clicking a row selects it; the inspector shows its records.

use std::time::SystemTime;

use api::{EmitterView, Snapshot, fmt};
use egui::{RichText, Sense, vec2};

use crate::selection::Selection;
use crate::theme;

const ROW_H: f32 = 18.0;
/// Not heard for this long: drawn as a ghost.
const STALE_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Receiver,
    Mhz,
    Khz,
    Records,
    OnAir,
    Duty,
    LastSeen,
    What,
}

const COLS: [(Col, &str, f32); 8] = [
    (Col::Receiver, "Rx", 28.0),
    (Col::Mhz, "MHz", 84.0),
    (Col::Khz, "kHz", 56.0),
    (Col::Records, "Records", 60.0),
    (Col::OnAir, "On air", 64.0),
    (Col::Duty, "Duty %", 56.0),
    (Col::LastSeen, "Last seen", 90.0),
    (Col::What, "What", 220.0),
];

pub struct EmittersPane {
    pub filter: String,
    pub sort: Col,
    pub descending: bool,
}

impl Default for EmittersPane {
    fn default() -> Self {
        EmittersPane { filter: String::new(), sort: Col::LastSeen, descending: true }
    }
}

/// "12 s ago", "3 min ago", "2.1 h ago".
fn age(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map_or(0, |d| d.as_secs());
    match s {
        0..90 => format!("{s} s ago"),
        90..5400 => format!("{} min ago", s / 60),
        _ => format!("{:.1} h ago", s as f64 / 3600.0),
    }
}

impl EmittersPane {
    /// The emitters to list, filtered by the words (MHz, name, receiver letter) and sorted.
    pub fn rows(&self, snap: &Snapshot) -> Vec<std::sync::Arc<EmitterView>> {
        let letter = |e: &EmitterView| snap.receivers.iter().find(|r| *r.device.serial == *e.serial).map(|r| r.letter);
        let name = |e: &EmitterView| e.headline.as_ref().map_or("unknown".to_string(), |c| c.label.clone());
        let words: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        let mut rows: Vec<_> = snap
            .records
            .emitters
            .iter()
            .filter(|e| {
                words.iter().all(|w| {
                    format!("{:.4}", e.center_hz / 1e6).contains(w.as_str())
                        || name(e).to_lowercase().contains(w.as_str())
                        || letter(e).is_some_and(|l| l.to_ascii_lowercase().to_string() == *w)
                })
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            let o = match self.sort {
                Col::Receiver => letter(a).cmp(&letter(b)),
                Col::Mhz => a.center_hz.total_cmp(&b.center_hz),
                Col::Khz => a.bandwidth_hz.total_cmp(&b.bandwidth_hz),
                Col::Records => a.records.cmp(&b.records),
                Col::OnAir => a.on_time_s.total_cmp(&b.on_time_s),
                Col::Duty => a.duty.total_cmp(&b.duty),
                Col::LastSeen => a.last_seen.cmp(&b.last_seen),
                Col::What => name(a).cmp(&name(b)),
            }
            .then(a.id.cmp(&b.id));
            if self.descending { o.reverse() } else { o }
        });
        rows
    }

    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot, sel: &mut Selection) {
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("MHz, name, receiver…").desired_width(180.0));
        });
        let rows = self.rows(snap);
        let named = rows.iter().filter(|e| e.headline.is_some()).count();
        ui.label(
            RichText::new(format!("{} ({named} named) of {}", fmt::count(rows.len(), "emitter"), snap.records.emitters.len()))
                .color(theme::INK2)
                .small(),
        );
        ui.separator();
        ui.horizontal(|ui| {
            for (col, name, w) in COLS {
                let arrow = if self.sort == col { if self.descending { " ⏷" } else { " ⏶" } } else { "" };
                let resp = ui.add_sized(vec2(w, ROW_H), egui::Button::new(theme::label(&format!("{name}{arrow}"))).frame(false));
                if resp.clicked() {
                    if self.sort == col {
                        self.descending = !self.descending;
                    } else {
                        (self.sort, self.descending) = (col, true);
                    }
                }
            }
        });
        let letter = |e: &EmitterView| snap.receivers.iter().find(|r| *r.device.serial == *e.serial).map_or('–', |r| r.letter);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for e in &rows[range] {
                let me = Selection::Emitter(e.id);
                let is_sel = *sel == me;
                let stale = SystemTime::now().duration_since(e.last_seen).is_ok_and(|d| d.as_secs() >= STALE_SECS);
                let resp = ui
                    .horizontal(|ui| {
                        let cells = [
                            letter(e).to_string(),
                            format!("{:.4}", e.center_hz / 1e6),
                            format!("{:.1}", e.bandwidth_hz / 1e3),
                            e.records.to_string(),
                            fmt::secs(e.on_time_s),
                            format!("{:.0}", e.duty * 100.0),
                            age(e.last_seen),
                            e.headline.as_ref().map_or("unknown".to_string(), |c| c.label.clone()),
                        ];
                        for ((col, _, w), text) in COLS.iter().zip(cells) {
                            let colour = match col {
                                _ if is_sel => theme::ACCENT,
                                _ if stale => theme::INK2,
                                Col::What if e.headline.is_none() => theme::INK2,
                                _ => theme::INK,
                            };
                            let mut t = RichText::new(text).color(colour);
                            if *col != Col::What {
                                t = t.monospace();
                            } else if e.headline.is_none() {
                                t = t.italics();
                            }
                            ui.add_sized(vec2(*w, ROW_H), egui::Label::new(t).truncate());
                        }
                    })
                    .response
                    .interact(Sense::click());
                if resp.clicked() {
                    sel.toggle(me);
                }
            }
        });
    }
}
