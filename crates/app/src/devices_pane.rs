//! The devices the engine found, opening and closing them, the ring budget and the bus.

use api::{Command, Snapshot};

use crate::theme;

/// The ring budget as edited, applied with `Apply`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetDraft {
    pub guaranteed_secs: f64,
    pub ceiling_mb: u64,
}

pub fn show(ui: &mut egui::Ui, snap: &Snapshot, draft: &mut BudgetDraft, out: &mut Vec<Command>) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(theme::label("Devices"));
            if ui.button("Refresh").clicked() {
                out.push(Command::Enumerate);
            }
        });
        if snap.devices.is_empty() {
            ui.label(egui::RichText::new("No devices found.").color(theme::INK2));
        }
        egui::Grid::new("devices").num_columns(4).striped(true).show(ui, |ui| {
            for d in &snap.devices {
                ui.label(&d.info.label);
                ui.label(egui::RichText::new(&d.info.backend).color(theme::INK2));
                ui.label(egui::RichText::new(&d.info.serial).monospace().color(theme::INK2));
                ui.horizontal(|ui| match d.open_as {
                    None => {
                        if ui.button("Open").clicked() {
                            out.push(Command::Open { backend: d.info.backend.clone(), serial: d.info.serial.clone() });
                        }
                    }
                    Some(id) => {
                        let letter = snap.receivers.iter().find(|r| r.id == id).map_or('?', |r| r.letter);
                        ui.label(egui::RichText::new(format!("open as {letter}")).color(theme::ACCENT));
                        if ui.button("Close").clicked() {
                            out.push(Command::Close(id));
                        }
                    }
                });
                ui.end_row();
            }
        });

        ui.add_space(12.0);
        ui.label(theme::label("Ring budget"));
        egui::Grid::new("ring_budget").num_columns(2).show(ui, |ui| {
            ui.label("Guaranteed");
            ui.add(egui::DragValue::new(&mut draft.guaranteed_secs).range(1.0..=3600.0).speed(1.0).suffix(" s"));
            ui.end_row();
            ui.label("Ceiling");
            ui.add(egui::DragValue::new(&mut draft.ceiling_mb).range(64..=1 << 24).speed(16.0).suffix(" MB"));
            ui.end_row();
        });
        if ui.button("Apply").clicked() {
            out.push(Command::SetBudget { guaranteed_secs: draft.guaranteed_secs, ceiling_bytes: draft.ceiling_mb << 20 });
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let b = &snap.bus;
            ui.label(
                egui::RichText::new(format!("workers {} · shed {} · panicked {}", b.workers, b.shed_total, b.panicked_total))
                    .color(theme::INK2),
            );
            if b.saturated {
                ui.label(egui::RichText::new("saturated").color(theme::CAUTION));
            }
        });
    });
}
