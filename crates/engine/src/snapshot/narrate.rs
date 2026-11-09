//! The log's narration of what changed between ticks (spec §5's Log slot): every claim that changed an emitter's
//! headline, and every channel the bus shed, with the reason.

use std::collections::HashMap;

use api::{BusView, Claim, EmitterId, Evidence, LogKind, RecordsView, fmt};

use super::Log;

/// What the narrator saw last tick.
#[derive(Default)]
pub(crate) struct Narrator {
    /// The records view's version last read.
    records_version: u64,
    /// Each emitter's headline as label and evidence kind; the counts in it may change without a line.
    headlines: HashMap<EmitterId, Option<(String, &'static str)>>,
    shed: u64,
    panicked: u64,
}

/// Forget emitters beyond this many (the view keeps the recent ones; an old one that comes back is named again).
const KEPT: usize = 4096;

impl Narrator {
    /// One line per emitter whose headline changed since the last tick: named, renamed, or no longer named.
    /// `letter` gives the receiver letter for a device serial.
    pub fn emitters(&mut self, view: &RecordsView, letter: impl Fn(&str) -> Option<char>, log: &mut Log) {
        if view.version == self.records_version {
            return;
        }
        self.records_version = view.version;
        if self.headlines.len() > KEPT {
            self.headlines.clear();
        }
        for e in &view.emitters {
            let now = e.headline.as_ref().map(|c| (c.label.clone(), c.by.kind()));
            let before = self.headlines.insert(e.id, now.clone());
            let was = match before {
                Some(b) if b == now => continue,
                Some(b) => b,
                // First seen: a line only if it arrives named.
                None if now.is_none() => continue,
                None => None,
            };
            let at = format!("{:.4} MHz, emitter {}", e.center_hz / 1e6, e.id.0);
            let text = match (&e.headline, was) {
                (Some(c), None) => format!("{at}: named {}", brief(c)),
                (Some(c), Some((label, kind))) => format!("{at}: now {}; was {label} ({kind})", brief(c)),
                (None, Some((label, kind))) => format!("{at}: no longer named; was {label} ({kind})"),
                (None, None) => continue,
            };
            log.push(letter(&e.serial), LogKind::Headline, text);
        }
    }

    /// A line when the bus sheds channels or a consumer panics.
    pub fn bus(&mut self, bus: &BusView, log: &mut Log) {
        if bus.shed_total > self.shed {
            let n = bus.shed_total - self.shed;
            log.push(None, LogKind::Bus, format!("bus shed {}: its workers are saturated", fmt::count(n as usize, "channel")));
        }
        if bus.panicked_total > self.panicked {
            let n = bus.panicked_total - self.panicked;
            log.push(None, LogKind::Bus, format!("{} panicked and stopped", fmt::count(n as usize, "channel consumer")));
        }
        (self.shed, self.panicked) = (bus.shed_total, bus.panicked_total);
    }
}

/// A claim in a few words: "P25 Phase 1 (sync bank, 12 syncs)", "AM (shape, 60 %)", "P25 control (decoded)".
fn brief(c: &Claim) -> String {
    let detail = match &c.by {
        Evidence::SyncBank { syncs, .. } => format!("sync bank, {syncs} syncs"),
        Evidence::Decoder { locked: true, .. } => "decoded".into(),
        Evidence::Decoder { locked: false, .. } => "decoded, not locked now".into(),
        other => format!("{}, {:.0} %", other.kind(), c.confidence * 100.0),
    };
    format!("{} ({detail})", c.label)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::SystemTime;

    use api::EmitterView;

    use super::*;

    fn emitter(id: u64, headline: Option<Claim>) -> Arc<EmitterView> {
        Arc::new(EmitterView {
            id: EmitterId(id),
            serial: "SIM-0001".into(),
            center_hz: 851.0125e6,
            bandwidth_hz: 12.5e3,
            first_seen: SystemTime::UNIX_EPOCH,
            last_seen: SystemTime::UNIX_EPOCH,
            records: 1,
            on_time_s: 1.0,
            duty: 0.5,
            confirmed: true,
            headline,
        })
    }

    fn sync(syncs: u32) -> Claim {
        Claim::new("P25 Phase 1", 0.8, Evidence::SyncBank { protocol: "P25".into(), syncs, frames_tried: 0, frames_decoded: 0 })
    }

    fn lines(log: &Log) -> Vec<String> {
        log.lines.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn a_decoder_replacing_a_sync_bank_headline_is_one_line_saying_both() {
        let mut log = Log { lines: VecDeque::new(), kept: 100, n: 0 };
        let mut n = Narrator::default();
        let view = |version, e| RecordsView { version, records: Vec::new(), emitters: vec![e] };
        n.emitters(&view(1, emitter(2, None)), |_| Some('A'), &mut log);
        assert!(lines(&log).is_empty(), "an unnamed emitter says nothing");
        n.emitters(&view(2, emitter(2, Some(sync(12)))), |_| Some('A'), &mut log);
        // More syncs, same label and kind: no new line.
        n.emitters(&view(3, emitter(2, Some(sync(40)))), |_| Some('A'), &mut log);
        let decoded = Claim::new(
            "P25 Phase 1",
            1.0,
            Evidence::Decoder { mode: "P25 control".into(), locked: true, summary: String::new(), identity: None },
        );
        n.emitters(&view(4, emitter(2, Some(decoded))), |_| Some('A'), &mut log);
        assert_eq!(
            lines(&log),
            [
                "851.0125 MHz, emitter 2: named P25 Phase 1 (sync bank, 12 syncs)",
                "851.0125 MHz, emitter 2: now P25 Phase 1 (decoded); was P25 Phase 1 (sync bank)",
            ]
        );
        assert!(log.lines.iter().all(|l| l.kind == LogKind::Headline && l.receiver == Some('A')));
    }

    #[test]
    fn shed_channels_are_logged_once_per_increase() {
        let mut log = Log { lines: VecDeque::new(), kept: 100, n: 0 };
        let mut n = Narrator::default();
        let bus = |shed_total| BusView { workers: 4, saturated: true, shed_total, panicked_total: 0 };
        n.bus(&bus(2), &mut log);
        n.bus(&bus(2), &mut log);
        n.bus(&bus(3), &mut log);
        assert_eq!(lines(&log), ["bus shed 2 channels: its workers are saturated", "bus shed 1 channel: its workers are saturated"]);
    }
}
