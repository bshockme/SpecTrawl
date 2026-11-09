//! The engine's log (spec §5's Log slot): every retune, device change, shed channel, and claim that changed an
//! emitter's headline, oldest first, kept scrolled to the newest line, with a filter by kind.

use std::collections::BTreeSet;

use api::{LogKind, Snapshot};
use egui::RichText;

use crate::theme;

/// The kinds hidden; empty shows everything.
#[derive(Default)]
pub struct LogPane {
    hidden: BTreeSet<u8>,
}

fn key(k: LogKind) -> u8 {
    LogKind::ALL.iter().position(|x| *x == k).unwrap_or(0) as u8
}

impl LogPane {
    pub fn shows(&self, k: LogKind) -> bool {
        !self.hidden.contains(&key(k))
    }

    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.label(theme::label("Show"));
            for k in LogKind::ALL {
                let on = self.shows(k);
                let n = snap.log.iter().filter(|e| e.kind == k).count();
                if ui.add(egui::Button::new(format!("{} {n}", k.name())).selected(on)).clicked() {
                    if on {
                        self.hidden.insert(key(k));
                    } else {
                        self.hidden.remove(&key(k));
                    }
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if snap.log.is_empty() {
                ui.label(RichText::new("Nothing logged yet.").color(theme::INK2));
            }
            for e in snap.log.iter().filter(|e| self.shows(e.kind)) {
                ui.horizontal(|ui| {
                    let letter = e.receiver.map_or("·".to_string(), |l| l.to_string());
                    ui.label(RichText::new(letter).monospace().color(theme::INK2));
                    let colour = if e.kind == LogKind::Error { theme::ALARM } else { theme::INK };
                    ui.label(RichText::new(&e.text).color(colour));
                });
            }
        });
    }
}
