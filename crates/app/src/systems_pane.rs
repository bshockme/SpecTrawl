//! SYSTEMS, the thing a trunking decoder's domain cares about (spec §5), and the decoders' lamps. Every field is the
//! snapshot's: a system row is its emitter's decoded claim, so it reads what the record and the emitter read.

use api::{DecoderView, Lamp, Snapshot, fmt};
use egui::RichText;

use crate::theme;

/// The lamp's word and colour.
pub fn lamp(l: Lamp) -> (&'static str, egui::Color32) {
    match l {
        Lamp::Searching => ("Searching", theme::MUTED),
        Lamp::Syncing => ("Syncing", theme::SELECT),
        Lamp::Locked => ("Locked", theme::VOICE),
    }
}

/// One decoder: its lamp and its sentence.
pub fn decoder_line(ui: &mut egui::Ui, d: &DecoderView) {
    let (word, colour) = lamp(d.lamp);
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(colour));
        ui.label(RichText::new(word).color(colour).small());
        ui.label(&d.summary);
    });
}

pub fn show(ui: &mut egui::Ui, snap: &Snapshot) {
    ui.label(RichText::new("SYSTEMS").strong());
    if snap.systems.systems.is_empty() {
        ui.label(RichText::new("none yet: a system appears once a control channel's broadcasts are decoded").italics().color(theme::MUTED));
    }
    egui::Grid::new("systems").striped(true).show(ui, |ui| {
        if !snap.systems.systems.is_empty() {
            for h in ["Mode", "Control", "Identity", "Records", "Last seen"] {
                ui.label(RichText::new(h).small().color(theme::MUTED));
            }
            ui.end_row();
        }
        for s in &snap.systems.systems {
            ui.label(&s.mode);
            ui.label(fmt::mhz(s.control_hz));
            ui.label(RichText::new(s.identity.line()).strong());
            ui.label(s.records.len().to_string());
            ui.label(fmt::clock(s.last_seen));
            ui.end_row();
        }
    });
    ui.add_space(8.0);
    ui.label(RichText::new("DECODERS").strong());
    if snap.decoders.is_empty() {
        ui.label(RichText::new("none running").italics().color(theme::MUTED));
    }
    for d in &snap.decoders {
        decoder_line(ui, d);
    }
}

/// The classify pool's budget in one line (spec §4.5: a number on screen).
pub fn budget_line(snap: &Snapshot) -> String {
    let c = &snap.classify;
    if c.workers == 0 {
        return "detector bank off".into();
    }
    let load = if c.looked_secs > 0.0 { 100.0 * c.work_secs / c.looked_secs } else { 0.0 };
    format!(
        "Looked at {} records ({} waiting, {} missed: IQ gone, {} dropped: queue full; {} survey, no IQ) · {} threads at {load:.0}% of real time",
        c.looked, c.waiting, c.missed, c.dropped, c.survey, c.workers
    )
}
