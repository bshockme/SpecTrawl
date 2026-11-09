//! CHANNELS, the thing the air band's domain cares about (spec §5): every raster channel something was heard on, built
//! from the records' claims by the engine (its air band decoder), so a row reads what its records read. Clicking a
//! row selects its newest record, which the Records tab plays.

use api::{RecordId, Snapshot, fmt};
use egui::RichText;

use crate::theme;

pub fn show(ui: &mut egui::Ui, snap: &Snapshot, selected: &mut Option<RecordId>) {
    ui.label(theme::label("Channels"));
    let rows = &snap.channels.channels;
    if rows.is_empty() {
        ui.label(
            RichText::new("none yet: a channel appears once a call on it measures as AM, or a decoder names it (ACARS)")
                .italics()
                .color(theme::INK2),
        );
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        egui::Grid::new("channels").striped(true).show(ui, |ui| {
            for h in ["", "Channel", "Voice", "Data", "Carrier", "On air", "Peak", "Last heard", "What"] {
                ui.label(theme::label(h));
            }
            ui.end_row();
            for c in rows {
                let newest = c.records.last().copied();
                let is_selected = newest.is_some() && *selected == newest;
                theme::dot(ui, if c.open { theme::INK } else { theme::INK3 }, c.open);
                let name = RichText::new(format!("{} {}", c.plan, c.name)).strong();
                if ui.selectable_label(is_selected, name).on_hover_text(format!("{} records; select the newest", c.records.len())).clicked()
                {
                    *selected = newest;
                }
                ui.label(c.voice.to_string());
                ui.label(if c.data > 0 { format!("{} {}", c.data, c.modes.join(", ")) } else { "0".into() });
                ui.label(RichText::new(c.carrier.to_string()).color(theme::INK2));
                ui.label(fmt::secs(c.airtime_s));
                ui.label(format!("{:.0} dBFS", c.peak_dbfs));
                ui.label(fmt::clock(c.last_heard));
                ui.label(&c.summary);
                ui.end_row();
            }
        });
    });
}
