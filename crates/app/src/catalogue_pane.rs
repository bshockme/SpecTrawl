//! The catalogue (spec §5): every mode the engine can name or decode, grouped by domain, each with how far SpecTrawl
//! gets with it, a line on what it reads, and how many of the recent records it named; a search.

use api::{CatalogueEntry, Domain, Reach, Snapshot, fmt};
use egui::RichText;

use crate::theme;

#[derive(Default)]
pub struct CataloguePane {
    pub search: String,
}

/// How many recent records a mode named (their headline is it), and how many more it was only suggested for.
pub fn counts(e: &CatalogueEntry, snap: &Snapshot) -> (usize, usize) {
    let mut named = 0;
    let mut suggested = 0;
    for r in &snap.records.records {
        if r.headline().is_some_and(|c| e.matches(&c.label)) {
            named += 1;
        } else if r.claims.iter().any(|c| e.matches(&c.label)) {
            suggested += 1;
        }
    }
    (named, suggested)
}

impl CataloguePane {
    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.label("Search");
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("mode, domain, words…").desired_width(180.0));
        });
        let words: Vec<String> = self.search.split_whitespace().map(str::to_lowercase).collect();
        let hit = |e: &CatalogueEntry| {
            words.iter().all(|w| {
                e.mode.to_lowercase().contains(w.as_str())
                    || e.blurb.to_lowercase().contains(w.as_str())
                    || e.domain.name().to_lowercase().contains(w.as_str())
                    || e.reach.name().contains(w.as_str())
            })
        };
        let shown: Vec<&CatalogueEntry> = snap.catalogue.iter().filter(|e| hit(e)).collect();
        let decodes = snap.catalogue.iter().filter(|e| e.reach == Reach::Decodes).count();
        ui.label(
            RichText::new(format!(
                "{} ({decodes} decoded, the rest named by sync or shape) · counts are over the {} recent records",
                fmt::count(snap.catalogue.len(), "mode"),
                snap.records.records.len()
            ))
            .color(theme::INK2)
            .small(),
        );
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut domains: Vec<Domain> = shown.iter().map(|e| e.domain).collect();
            domains.dedup();
            // One grid for every domain, so the columns line up down the page.
            egui::Grid::new("catalogue").num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for d in domains {
                    ui.add_space(4.0);
                    ui.end_row();
                    ui.label(theme::label(d.name()));
                    ui.end_row();
                    for e in shown.iter().filter(|e| e.domain == d) {
                        let (named, suggested) = counts(e, snap);
                        ui.label(RichText::new(e.mode).size(14.0));
                        theme::chip(ui, e.reach.name(), theme::CHIP, theme::INK2);
                        let count = match (named, suggested) {
                            (0, 0) => RichText::new("none yet").color(theme::INK2),
                            (n, 0) => RichText::new(format!("{n} named")).monospace(),
                            (n, s) => RichText::new(format!("{n} named, {s} suggested")).monospace(),
                        };
                        ui.label(count);
                        ui.label(RichText::new(e.blurb).color(theme::INK2));
                        ui.end_row();
                    }
                }
            });
        });
    }
}
