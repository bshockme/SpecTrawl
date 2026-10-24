//! The engine's log, oldest first, kept scrolled to the newest line.

use api::Snapshot;

use crate::theme;

pub fn show(ui: &mut egui::Ui, snap: &Snapshot) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if snap.log.is_empty() {
            ui.label(egui::RichText::new("Nothing logged yet.").color(theme::MUTED));
        }
        for e in &snap.log {
            let line = match e.receiver {
                Some(letter) => format!("[{letter}] {}", e.text),
                None => e.text.clone(),
            };
            ui.label(egui::RichText::new(line).monospace());
        }
    });
}
