//! The records list: every record the snapshot carries, filtered and sorted, the selected one's claims, tags and notes,
//! and a search of the store's history. Reads `api` types only and edits through commands.

use std::cmp::Ordering;
use std::sync::Arc;

use api::{
    AudioMode, Claim, Command, Evidence, PlayState, PlayTarget, PlayTo, PlayView, RecordId, RecordQuery, SignalRecord, Snapshot, fmt,
};
use egui::{RichText, Sense, vec2};

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
    tag_draft: String,
    note_draft: String,
    /// The mode Play asks for; `None` lets the engine choose from the frequency.
    pub play_mode: Option<AudioMode>,
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

/// Inferred from where a signal is or what is next to it, not from the signal (spec §5: muted, with the word).
fn inferred(c: &Claim) -> bool {
    matches!(c.by, Evidence::BandPlan { .. } | Evidence::Neighbour { .. } | Evidence::Position(_))
}

/// One claim as the inspector shows it: decoded solid with its summary, a sync-bank claim with its counts, inferred
/// ones muted and marked.
fn claim_line(ui: &mut egui::Ui, c: &Claim) {
    let pct = c.confidence * 100.0;
    match &c.by {
        Evidence::Decoder { summary, locked, .. } => {
            let lock = if *locked { "" } else { ", not locked now" };
            ui.label(RichText::new(format!("{} · decoded{lock} · {pct:.0}%", c.label)).strong());
            if !summary.is_empty() {
                ui.label(RichText::new(format!("    {summary}")).small());
            }
        }
        Evidence::SyncBank { syncs, frames_tried, frames_decoded, .. } => {
            let frames = if *frames_tried > 0 { format!(", {frames_decoded} of {frames_tried} frames decoded") } else { String::new() };
            ui.label(format!("{} · sync bank: {syncs} syncs{frames} · {pct:.0}%", c.label));
        }
        _ if inferred(c) => {
            let entry = match &c.by {
                Evidence::BandPlan { entry } => format!(" ({entry})"),
                _ => String::new(),
            };
            ui.label(RichText::new(format!("{}{entry} · inferred from {} · {pct:.0}%", c.label, evidence_kind(c))).color(theme::MUTED));
        }
        _ => {
            ui.label(format!("{} · {} · {pct:.0}%", c.label, evidence_kind(c)));
        }
    }
}

fn evidence_kind(c: &Claim) -> &'static str {
    match c.by {
        Evidence::SyncBank { .. } => "sync bank",
        Evidence::Features { .. } => "shape",
        Evidence::Decoder { .. } => "decoded",
        Evidence::BandPlan { .. } => "band plan",
        Evidence::Neighbour { .. } => "neighbour",
        Evidence::Position(_) => "position",
    }
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
            tag_draft: String::new(),
            note_draft: String::new(),
            play_mode: None,
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

    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot, selected: &mut Option<RecordId>, out: &mut Vec<Command>) {
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
            format!("{} of {}", fmt::count(rows.len(), "record"), snap.records.records.len())
        };
        ui.label(RichText::new(format!("{source} · {store_line}")).color(theme::MUTED).small());
        ui.label(RichText::new(crate::systems_pane::budget_line(snap)).color(theme::MUTED).small());
        ui.separator();

        ui.horizontal(|ui| {
            for (col, name, w) in COLS {
                let arrow = if self.sort == col { if self.descending { " ⏷" } else { " ⏶" } } else { "" };
                let resp = ui.add_sized(vec2(w, ROW_H), egui::Button::new(RichText::new(format!("{name}{arrow}")).strong()).frame(false));
                if resp.clicked() {
                    if self.sort == col {
                        self.descending = !self.descending;
                    } else {
                        (self.sort, self.descending) = (col, true);
                    }
                }
            }
        });
        let detail_h = if selected.is_some() { 200.0 } else { 0.0 };
        let list_h = (ui.available_height() - detail_h).max(ROW_H * 3.0);
        let letter = |r: &SignalRecord| snap.receivers.iter().find(|x| x.id == r.source).map_or('–', |x| x.letter);
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for r in &rows[range] {
                let is_sel = *selected == Some(r.id);
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
                                Col::Headline if r.headline().is_none() => theme::MUTED,
                                _ if is_sel => theme::SELECT,
                                _ => theme::TEXT,
                            };
                            let mut t = RichText::new(text).color(colour).monospace();
                            if *col == Col::Headline && r.headline().is_none() {
                                t = t.italics();
                            }
                            ui.add_sized(vec2(*w, ROW_H), egui::Label::new(t).truncate());
                        }
                    })
                    .response
                    .interact(Sense::click());
                if resp.clicked() {
                    *selected = if is_sel { None } else { Some(r.id) };
                }
            }
        });

        let Some(id) = *selected else { return };
        let Some(r) = rows.iter().find(|r| r.id == id).or_else(|| snap.records.records.iter().find(|r| r.id == id)).cloned() else {
            return;
        };
        ui.separator();
        self.detail(ui, snap, &r, out);
    }

    /// What Play sends for a record.
    pub fn play_command(&self, id: RecordId) -> Command {
        Command::Play { target: PlayTarget::Record(id), mode: self.play_mode, gain: 1.0, to: PlayTo::Speaker }
    }

    /// What Save audio sends: a WAV named after the record, under the engine's audio directory.
    pub fn save_command(&self, id: RecordId) -> Command {
        let to = PlayTo::Wav(format!("record-{}.wav", id.0).into());
        Command::Play { target: PlayTarget::Record(id), mode: self.play_mode, gain: 1.0, to }
    }

    /// The record's newest replay: running ones first, else the latest that ended.
    pub fn play_of(snap: &Snapshot, id: RecordId) -> Option<&PlayView> {
        let mine = || snap.audio.plays.iter().filter(move |p| p.target == PlayTarget::Record(id));
        mine().rev().find(|p| !p.state.is_over()).or_else(|| mine().next_back())
    }

    /// The line the detail shows for a replay.
    pub fn play_line(p: &PlayView) -> String {
        let mode = format!("{}{}", p.mode.name(), if p.mode_guessed { " (by band)" } else { "" });
        let from = match p.from {
            api::PlayFrom::Ring => "ring",
            api::PlayFrom::Slice => "slice",
        };
        let what = match (&p.to, &p.state) {
            (PlayTo::Wav(path), PlayState::Finished) => format!("saved {}", path.display()),
            (PlayTo::Wav(_), s) if !s.is_over() => "saving".into(),
            (_, PlayState::Preparing) => "starting".into(),
            (_, PlayState::Playing) => "playing".into(),
            (_, PlayState::Finished) => "played".into(),
            (_, PlayState::Stopped) => "stopped".into(),
            (_, PlayState::EndedEarly(why)) => format!("ended early: {why}"),
        };
        let length = if p.complete { fmt::secs(p.length_s) } else { format!("{}…", fmt::secs(p.length_s)) };
        format!("{what} · {} of {length} · {mode} from the {from}", fmt::secs(p.position_s))
    }

    fn listen(&mut self, ui: &mut egui::Ui, snap: &Snapshot, r: &SignalRecord, out: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            let modes = [(None, "Auto"), (Some(AudioMode::Am), "AM"), (Some(AudioMode::Nfm), "NFM"), (Some(AudioMode::Wfm), "WFM")];
            let current = modes.iter().find(|m| m.0 == self.play_mode).map_or("Auto", |m| m.1);
            egui::ComboBox::from_id_salt("play-mode").selected_text(current).width(64.0).show_ui(ui, |ui| {
                for (m, name) in modes {
                    ui.selectable_value(&mut self.play_mode, m, name);
                }
            });
            let running = Self::play_of(snap, r.id).filter(|p| !p.state.is_over());
            match running {
                Some(p) if p.to == PlayTo::Speaker => {
                    if ui.button("■ Stop").clicked() {
                        out.push(Command::StopPlay(p.id));
                    }
                }
                _ => {
                    if ui.button("▶ Play").on_hover_text("Hear it (from the ring, or its slice)").clicked() {
                        out.push(self.play_command(r.id));
                    }
                }
            }
            if ui.button("Save audio").on_hover_text("Write it out as a WAV").clicked() {
                out.push(self.save_command(r.id));
            }
            if let Some(p) = Self::play_of(snap, r.id) {
                let colour = if matches!(p.state, PlayState::EndedEarly(_)) { theme::ALERT } else { theme::TEXT };
                ui.label(RichText::new(Self::play_line(p)).small().color(colour));
            }
            if let Err(e) = &snap.audio.output {
                ui.label(RichText::new(format!("no sound: {e}")).small().color(theme::MUTED));
            }
        });
        // A refused play is answered with the reason.
        let refused = snap.acks.iter().rev().find_map(|a| match &a.result {
            Err(e) if e.contains(&format!("record {} ", r.id.0)) && e.contains("can't be heard") => Some(e.clone()),
            _ => None,
        });
        if let Some(e) = refused {
            ui.label(RichText::new(e).small().color(theme::ALERT));
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui, snap: &Snapshot, r: &SignalRecord, out: &mut Vec<Command>) {
        ui.label(
            RichText::new(format!(
                "Record {} · {} · {:.1} kHz · {} · samples {}..{}",
                r.id.0,
                fmt::mhz(r.center_hz()),
                r.bandwidth_hz() / 1e3,
                if r.open { "open".to_string() } else { fmt::secs(r.duration().as_secs_f64()) },
                r.first_sample,
                r.end_sample
            ))
            .strong(),
        );
        self.listen(ui, snap, r, out);
        if r.claims.is_empty() {
            ui.label(RichText::new("unknown: no claims yet").italics().color(theme::MUTED));
        }
        if r.headline().is_none() && !r.claims.is_empty() {
            ui.label(RichText::new("unknown: nothing decoded").italics().color(theme::MUTED));
        }
        for c in &r.claims {
            claim_line(ui, c);
        }
        match (&r.iq_clip, r.slice_evicted) {
            (Some(c), _) => {
                ui.label(RichText::new(format!("slice: {} ({})", c.path.display(), fmt::secs(c.samples as f64 / c.rate))).small())
            }
            (None, true) => ui.label(RichText::new("slice: removed to stay under the disk cap").small().color(theme::MUTED)),
            (None, false) => ui.label(RichText::new("slice: none").small().color(theme::MUTED)),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label("Tags");
            for t in &r.tags {
                if ui.small_button(format!("{t} ×")).clicked() {
                    out.push(Command::Untag { record: r.id, tag: t.clone() });
                }
            }
            let resp = ui.add(egui::TextEdit::singleline(&mut self.tag_draft).hint_text("add a tag").desired_width(120.0));
            if (ui.small_button("Add").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                && !self.tag_draft.trim().is_empty()
            {
                out.push(Command::Tag { record: r.id, tag: self.tag_draft.trim().to_string() });
                self.tag_draft.clear();
            }
        });
        for n in &r.notes {
            ui.label(RichText::new(format!("{} · {}", fmt::clock(n.time), n.text)).small());
        }
        ui.horizontal(|ui| {
            let resp = ui.add(egui::TextEdit::singleline(&mut self.note_draft).hint_text("add a note").desired_width(260.0));
            if (ui.small_button("Note").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                && !self.note_draft.trim().is_empty()
            {
                out.push(Command::Note { record: r.id, text: self.note_draft.trim().to_string() });
                self.note_draft.clear();
            }
        });
    }
}
