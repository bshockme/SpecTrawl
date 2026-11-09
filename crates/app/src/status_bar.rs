//! The status bar under the top bar (spec §5's status line; the guide's A1 and A3): one traffic-light tile summing up
//! every receiver, the pool, the bus and the store, then the conditions as sentences, each saying what to change. A
//! condition stays while it holds; one read from a counter (samples dropped, looks missed) holds for [`HOLD`] after the
//! counter last rose.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use api::{Health, Snapshot};
use egui::{Color32, RichText};

use crate::theme;

/// How long a counter's rise keeps its condition up.
pub const HOLD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Caution,
    Alarm,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub level: Level,
    pub receiver: Option<char>,
    /// What is wrong, as a sentence.
    pub text: String,
    /// What to change.
    pub fix: Option<String>,
}

/// Counters seen last frame, and when each last rose.
#[derive(Default)]
pub struct StatusBar {
    counters: HashMap<String, (u64, Option<(Instant, u64)>)>,
    open: bool,
}

impl StatusBar {
    /// The rise of counter `key` over the last [`HOLD`], if it rose in that time.
    fn rose(&mut self, key: &str, value: u64, now: Instant) -> Option<u64> {
        let e = self.counters.entry(key.to_string()).or_insert((value, None));
        if value > e.0 {
            let since = match e.1 {
                Some((t, base)) if now.duration_since(t) < HOLD => base,
                _ => e.0,
            };
            e.1 = Some((now, since));
        }
        e.0 = value;
        match e.1 {
            Some((t, base)) if now.duration_since(t) < HOLD => Some(value - base),
            _ => None,
        }
    }

    /// Every condition holding now, worst first.
    pub fn conditions(&mut self, snap: &Snapshot, now: Instant) -> Vec<Condition> {
        let mut out = Vec::new();
        let mut add =
            |level, receiver, text: String, fix: Option<&str>| out.push(Condition { level, receiver, text, fix: fix.map(str::to_string) });
        for r in &snap.receivers {
            let l = r.letter;
            let said = match &r.status.reason {
                Some(reason) => format!("{l}: {} ({reason}).", r.status.text),
                None => format!("{l}: {}.", r.status.text),
            };
            match r.status.health {
                Health::Ok => {}
                Health::Lost => add(Level::Alarm, Some(l), said, Some("Check its USB cable, then Close and Open it in Devices.")),
                Health::Warning => {
                    let reason = r.status.reason.as_deref().unwrap_or("");
                    let fix = if reason.contains("shed") || reason.contains("behind") {
                        Some("Lower its sample rate or close a receiver.")
                    } else if r.status.text.starts_with("Waiting for samples") {
                        Some("Check it is plugged in; Close and Open it if it stays silent.")
                    } else {
                        None
                    };
                    add(Level::Caution, Some(l), said, fix)
                }
            }
            if let Some(n) = self.rose(&format!("gaps {}", r.device.serial), r.ring.device_gaps, now) {
                add(
                    Level::Alarm,
                    Some(l),
                    format!("{l}: the device dropped samples {} in the last {} s.", times(n), HOLD.as_secs()),
                    Some("Move it to a USB port on another controller, or lower its sample rate."),
                );
            }
        }
        if let Some(n) = self.rose("dropped", snap.classify.dropped, now) {
            add(
                Level::Caution,
                None,
                format!("The classifier's queue was full: {} dropped without a look.", api::fmt::count(n as usize, "record")),
                Some("Close a receiver, or wait for a quieter band."),
            );
        }
        if let Some(n) = self.rose("missed", snap.classify.missed, now) {
            add(
                Level::Caution,
                None,
                format!("{} left the ring before their look.", api::fmt::count(n as usize, "record")),
                Some("Raise the guaranteed seconds in Devices."),
            );
        }
        if snap.bus.saturated {
            add(Level::Caution, None, "The bus is saturated: channels may be shed.".into(), Some("Close a receiver."));
        }
        if snap.bus.panicked_total > 0 {
            add(
                Level::Alarm,
                None,
                format!("{} panicked and stopped.", api::fmt::count(snap.bus.panicked_total as usize, "channel consumer")),
                Some("This is a bug: the Log has the details."),
            );
        }
        if let Some(s) = &snap.store {
            if let Some(e) = &s.error {
                add(Level::Alarm, None, format!("The record store failed: {e}."), Some("Records are not being kept; check the disk."));
            } else if let Some(n) = self.rose("store dropped", s.dropped, now) {
                add(
                    Level::Caution,
                    None,
                    format!("The record store fell behind: {} not written.", api::fmt::count(n as usize, "change")),
                    Some("Check the disk is not full or slow."),
                );
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.level));
        out
    }

    /// The tile and the first condition; the rest behind "+N more".
    pub fn show(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let now = Instant::now();
        let conditions = self.conditions(snap, now);
        ui.horizontal(|ui| {
            let (word, colour) = match conditions.first().map(|c| c.level) {
                None => ("OK", theme::OK),
                Some(Level::Caution) => ("CHECK", theme::CAUTION),
                Some(Level::Alarm) => ("PROBLEM", theme::ALARM),
            };
            let tile = tile(ui, word, colour);
            if tile.clicked() && !conditions.is_empty() {
                self.open = !self.open;
            }
            match conditions.first() {
                None => {
                    let rx = api::fmt::count(snap.receivers.len(), "receiver");
                    ui.label(RichText::new(format!("{rx} running, nothing needs attention.")).color(theme::INK2));
                }
                Some(c) => {
                    sentence(ui, c);
                    if conditions.len() > 1 && ui.link(format!("+{} more", conditions.len() - 1)).clicked() {
                        self.open = !self.open;
                    }
                }
            }
        });
        if conditions.is_empty() {
            self.open = false;
        }
        if self.open {
            egui::Window::new("Conditions")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.open)
                .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 72.0))
                .show(ui.ctx(), |ui| {
                    for c in &conditions {
                        ui.horizontal(|ui| sentence(ui, c));
                    }
                });
        }
    }
}

/// "once", "twice", "3 times".
fn times(n: u64) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

/// The traffic-light tile: a dot and a word, both in the level's colour.
fn tile(ui: &mut egui::Ui, word: &str, colour: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(theme::CHIP)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::dot(ui, colour, true);
                ui.label(theme::label(word).color(colour));
            });
        })
        .response
        .interact(egui::Sense::click())
}

/// A condition: what is wrong in ink, the fix in ink-2.
fn sentence(ui: &mut egui::Ui, c: &Condition) {
    let colour = match c.level {
        Level::Caution => theme::CAUTION,
        Level::Alarm => theme::ALARM,
    };
    theme::dot(ui, colour, false);
    ui.label(RichText::new(&c.text).color(theme::INK));
    if let Some(fix) = &c.fix {
        ui.label(RichText::new(fix).color(theme::INK2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_snapshot_has_no_conditions() {
        let mut bar = StatusBar::default();
        assert!(bar.conditions(&Snapshot::empty(), Instant::now()).is_empty());
    }

    #[test]
    fn a_counter_that_rises_holds_its_condition_for_the_hold_then_clears() {
        let mut bar = StatusBar::default();
        let t0 = Instant::now();
        let mut snap = Snapshot::empty();
        snap.classify.dropped = 5;
        // The first sight of a counter is its baseline, not a rise.
        assert!(bar.conditions(&snap, t0).is_empty());
        snap.classify.dropped = 8;
        let c = bar.conditions(&snap, t0 + Duration::from_secs(1));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].level, Level::Caution);
        assert_eq!(c[0].text, "The classifier's queue was full: 3 records dropped without a look.");
        assert!(c[0].fix.is_some(), "every condition says what to change");
        // Rising again inside the hold counts from the first rise.
        snap.classify.dropped = 10;
        let c = bar.conditions(&snap, t0 + Duration::from_secs(10));
        assert!(c[0].text.contains("5 records"), "{}", c[0].text);
        assert!(bar.conditions(&snap, t0 + Duration::from_secs(10) + HOLD).is_empty());
    }

    #[test]
    fn alarms_come_before_cautions() {
        let mut bar = StatusBar::default();
        let mut snap = Snapshot::empty();
        snap.bus.saturated = true;
        snap.bus.panicked_total = 1;
        let c = bar.conditions(&snap, Instant::now());
        assert_eq!(c.iter().map(|c| c.level).collect::<Vec<_>>(), [Level::Alarm, Level::Caution]);
    }
}
