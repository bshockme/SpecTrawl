//! The records list: every record the snapshot carries, filtered and sorted, and a search of the store's history.
//! Clicking a row selects it; the inspector shows it in full. Reads `api` types only.

use std::cmp::Ordering;
use std::sync::Arc;

use api::{Command, Evidence, RecordQuery, SignalRecord, Snapshot, fmt};
use egui::{RichText, Sense, vec2};

use crate::selection::Selection;
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Time,
    Receiver,
    Mhz,
    Khz,
    Duration,
    Peak,
    Snr,
    Headline,
    Emitter,
}

const COLS: [(Col, &str, f32); 9] = [
    (Col::Time, "Time (UTC)", 74.0),
    (Col::Receiver, "Rx", 28.0),
    (Col::Mhz, "MHz", 84.0),
    (Col::Khz, "kHz", 56.0),
    (Col::Duration, "Dur", 56.0),
    (Col::Peak, "Peak dBFS", 70.0),
    (Col::Snr, "SNR dB", 56.0),
    (Col::Headline, "What", 170.0),
    (Col::Emitter, "Emitter", 60.0),
];

const ROW_H: f32 = 18.0;

pub struct RecordsPane {
    pub filter: String,
    pub sort: Col,
    pub descending: bool,
    pub search: String,
    /// The sequence number of the last history search sent; its answer is the snapshot's `query` with this `seq`.
    pub search_seq: Option<u64>,
    /// Show the search's answer instead of the live list.
    pub showing_history: bool,
    /// List occupants too, not only events (spec §5: by default the table shows events).
    pub show_occupants: bool,
}

impl Default for RecordsPane {
    fn default() -> Self {
        RecordsPane::new()
    }
}

/// What a record headlines as: its headline claim's label, else "unknown".
pub fn headline(r: &SignalRecord) -> &str {
    r.headline().map_or("unknown", |c| c.label.as_str())
}

/// The "What" column: the headline, or "unknown" with what its shape suggests (spec §4.5: "unknown" with the
/// features), and whether it is that muted kind.
pub fn what(r: &SignalRecord) -> (String, bool) {
    if let Some(h) = r.headline() {
        return (h.label.clone(), false);
    }
    let shape = r.claims.iter().filter(|c| matches!(c.by, Evidence::Features { .. })).max_by(|a, b| a.confidence.total_cmp(&b.confidence));
    (shape.map_or_else(|| "unknown".to_string(), |c| format!("unknown · looks like {}", c.label)), true)
}

impl RecordsPane {
    pub fn new() -> RecordsPane {
        RecordsPane {
            filter: String::new(),
            sort: Col::Time,
            descending: true,
            search: String::new(),
            search_seq: None,
            showing_history: false,
            show_occupants: false,
        }
    }

    /// The records to list: the snapshot's (or the history search's answer), filtered and sorted.
    pub fn rows(&self, snap: &Snapshot) -> Vec<Arc<SignalRecord>> {
        let source: Vec<Arc<SignalRecord>> = match (&snap.query, self.showing_history) {
            (Some(q), true) if Some(q.seq) == self.search_seq => q.records.clone(),
            _ => snap.records.records.clone(),
        };
        let letter = |r: &SignalRecord| snap.receivers.iter().find(|x| x.id == r.source).map(|x| x.letter);
        let words: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        let mut rows: Vec<Arc<SignalRecord>> = source
            .into_iter()
            .filter(|r| self.show_occupants || !r.is_occupant())
            .filter(|r| {
                words.iter().all(|w| {
                    format!("{:.4}", r.center_hz() / 1e6).contains(w.as_str())
                        || headline(r).to_lowercase().contains(w.as_str())
                        || r.claims.iter().any(|c| c.label.to_lowercase().contains(w.as_str()))
                        || r.tags.iter().any(|t| t.to_lowercase().contains(w.as_str()))
                        || r.notes.iter().any(|n| n.text.to_lowercase().contains(w.as_str()))
                        || letter(r).is_some_and(|l| l.to_ascii_lowercase().to_string() == *w)
                })
            })
            .collect();
        let key = |a: &SignalRecord, b: &SignalRecord| -> Ordering {
            match self.sort {
                Col::Time => a.t_start.cmp(&b.t_start),
                Col::Receiver => letter(a).cmp(&letter(b)),
                Col::Mhz => a.center_hz().total_cmp(&b.center_hz()),
                Col::Khz => a.bandwidth_hz().total_cmp(&b.bandwidth_hz()),
                Col::Duration => a.duration().cmp(&b.duration()),
                Col::Peak => a.peak_dbfs.total_cmp(&b.peak_dbfs),
                Col::Snr => a.max_snr_db.total_cmp(&b.max_snr_db),
                Col::Headline => headline(a).cmp(headline(b)),
                Col::Emitter => a.emitter.cmp(&b.emitter),
            }
            .then(a.id.cmp(&b.id))
        };
        rows.sort_by(|a, b| if self.descending { key(b, a) } else { key(a, b) });
        rows
    }

    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot, selected: &mut Selection, out: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("MHz, tag, note, receiver…").desired_width(180.0));
            ui.separator();
            ui.label("History");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("words in a claim, tag or note").desired_width(180.0));
            let go = ui.button("Search").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go {
                let text = self.search.trim();
                out.push(Command::QueryRecords(RecordQuery {
                    text: (!text.is_empty()).then(|| text.to_string()),
                    limit: 1000,
                    ..Default::default()
                }));
                self.showing_history = true;
            }
            if self.showing_history && ui.button("Live").clicked() {
                self.showing_history = false;
            }
            ui.separator();
            if ui
                .add(egui::Button::new("Events").selected(!self.show_occupants))
                .on_hover_text("Records shorter than a minute and narrower than 150 kHz, not broadcast or cellular")
                .clicked()
            {
                self.show_occupants = false;
            }
            if ui
                .add(egui::Button::new("All").selected(self.show_occupants))
                .on_hover_text("Occupants too: long or wideband records (the Emitters tab lists them)")
                .clicked()
            {
                self.show_occupants = true;
            }
        });
        let store_line = match &snap.store {
            Some(s) if s.error.is_some() => format!("store: {}", s.error.as_deref().unwrap_or_default()),
            Some(s) if s.dropped > 0 => format!("store: {} written, {} queued, {} dropped (queue full)", s.written, s.queued, s.dropped),
            Some(s) => format!("store: {} written, {} queued", s.written, s.queued),
            None => "records are not kept after closing (no store)".into(),
        };
        let rows = self.rows(snap);
        let source = if self.showing_history {
            match &snap.query {
                Some(q) if Some(q.seq) == self.search_seq => {
                    format!("{} from history{}", fmt::count(q.records.len(), "record"), if q.truncated { " (more matched)" } else { "" })
                }
                _ => "searching…".into(),
            }
        } else {
            let occupants = snap.records.records.iter().filter(|r| r.is_occupant()).count();
            let hidden =
                if self.show_occupants || occupants == 0 { String::new() } else { format!(" ({occupants} occupant records hidden)") };
            format!("{} of {}{hidden}", fmt::count(rows.len(), "record"), snap.records.records.len())
        };
        ui.label(RichText::new(format!("{source} · {store_line}")).color(theme::INK2).small());
        ui.label(RichText::new(crate::systems_pane::budget_line(snap)).color(theme::INK2).small());
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
        let list_h = ui.available_height().max(ROW_H * 3.0);
        let letter = |r: &SignalRecord| snap.receivers.iter().find(|x| x.id == r.source).map_or('–', |x| x.letter);
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for r in &rows[range] {
                let is_sel = selected.covers(r);
                let resp = ui
                    .horizontal(|ui| {
                        let cells = [
                            fmt::clock(r.t_start),
                            letter(r).to_string(),
                            format!("{:.4}", r.center_hz() / 1e6),
                            format!("{:.1}", r.bandwidth_hz() / 1e3),
                            if r.open { "open".into() } else { fmt::secs(r.duration().as_secs_f64()) },
                            format!("{:.1}", r.peak_dbfs),
                            format!("{:.0}", r.max_snr_db),
                            what(r).0,
                            r.emitter.map_or("–".into(), |e| e.0.to_string()),
                        ];
                        for ((col, _, w), text) in COLS.iter().zip(cells) {
                            let colour = match col {
                                // "Unknown" is an honest answer, shown as one: not styled like a claim.
                                Col::Headline if r.headline().is_none() => theme::INK2,
                                _ if is_sel => theme::ACCENT,
                                _ => theme::INK,
                            };
                            // Values and identifiers in Auggie Mono; the headline is words, in Barlow.
                            let mut t = RichText::new(text).color(colour);
                            if *col != Col::Headline {
                                t = t.monospace();
                            }
                            if *col == Col::Headline && r.headline().is_none() {
                                t = t.italics();
                            }
                            ui.add_sized(vec2(*w, ROW_H), egui::Label::new(t).truncate());
                        }
                    })
                    .response
                    .interact(Sense::click());
                if resp.clicked() {
                    selected.toggle(Selection::Record(r.id));
                }
            }
        });
    }
}
