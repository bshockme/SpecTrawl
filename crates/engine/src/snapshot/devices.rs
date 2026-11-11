//! The device registry: what the backends enumerate, which devices are open, and the commands that change them.

use std::collections::BTreeMap;
use std::sync::Arc;

use api::{Capabilities, Command, DeviceEntry, DeviceInfo, GainSetting, LogKind, Outcome, fmt};
use sdr_core::Backend;

use super::Log;
use crate::budget::{BudgetConfig, Mode, SourceId};
use crate::engine::Engine;

/// An open receiver as the registry remembers it.
pub(crate) struct Opened {
    pub letter: char,
    pub info: DeviceInfo,
    pub caps: Arc<Capabilities>,
    pub gain: Option<GainSetting>,
}

pub(crate) struct Registry {
    backends: Vec<Box<dyn Backend>>,
    entries: Vec<DeviceEntry>,
    open: BTreeMap<SourceId, Opened>,
}

impl Registry {
    /// Adopts every source already in the engine (lowest letters first, in id order), then enumerates.
    pub fn new(engine: &Engine, backends: Vec<Box<dyn Backend>>, log: &mut Log) -> Registry {
        let mut r = Registry { backends, entries: Vec::new(), open: BTreeMap::new() };
        let mut sources = engine.sources();
        sources.sort_by_key(|s| s.id());
        for s in sources {
            let Some(letter) = r.free_letter() else { break };
            let snap = s.snapshot();
            r.open.insert(s.id(), Opened { letter, info: snap.info, caps: Arc::new(snap.caps), gain: None });
        }
        r.enumerate(log);
        r
    }

    /// Every backend's devices; a failing backend is a log line. An open device stays listed even when no longer enumerated.
    pub fn enumerate(&mut self, log: &mut Log) {
        let mut entries = Vec::new();
        for b in &self.backends {
            match b.enumerate() {
                Ok(found) => entries.extend(found.into_iter().map(|info| DeviceEntry { info, open_as: None })),
                Err(e) => log.push(None, LogKind::Device, format!("{}: enumeration failed: {e}", b.name())),
            }
        }
        self.entries = entries;
        self.sync();
    }

    /// The device list with `open_as` as of now.
    pub fn entries(&self) -> &[DeviceEntry] {
        &self.entries
    }

    /// Open receivers in letter order.
    pub fn open_by_letter(&self) -> Vec<(SourceId, &Opened)> {
        let mut v: Vec<(SourceId, &Opened)> = self.open.iter().map(|(&id, o)| (id, o)).collect();
        v.sort_by_key(|(_, o)| o.letter);
        v
    }

    pub fn ids(&self) -> Vec<SourceId> {
        self.open.keys().copied().collect()
    }

    pub fn letter(&self, id: SourceId) -> Option<char> {
        self.open.get(&id).map(|o| o.letter)
    }

    /// The lowest of `A..=Z` not in use, skipping `S` (reserved for the sweep, step 4a).
    fn free_letter(&self) -> Option<char> {
        ('A'..='Z').filter(|&l| l != 'S').find(|&l| !self.open.values().any(|o| o.letter == l))
    }

    /// Sets each entry's `open_as` and lists open devices no backend enumerated.
    fn sync(&mut self) {
        for e in &mut self.entries {
            e.open_as = self.open.iter().find(|(_, o)| same_device(&o.info, &e.info.backend, &e.info.serial)).map(|(&id, _)| id);
        }
        for (&id, o) in &self.open {
            if !self.entries.iter().any(|e| same_device(&o.info, &e.info.backend, &e.info.serial)) {
                self.entries.push(DeviceEntry { info: o.info.clone(), open_as: Some(id) });
            }
        }
    }

    /// Applies one command to the engine and the registry.
    pub fn apply(&mut self, engine: &Engine, c: Command, log: &mut Log) -> Result<Outcome, String> {
        match c {
            Command::Enumerate => {
                self.enumerate(log);
                Ok(Outcome::Done)
            }
            Command::Open { backend, serial } => self.open(engine, &backend, &serial, log),
            Command::Close(id) => {
                let o = self.open.remove(&id).ok_or_else(|| no_receiver(id))?;
                engine.remove_source(id);
                self.sync();
                log.push(Some(o.letter), LogKind::Device, format!("{} closed", o.letter));
                Ok(Outcome::Done)
            }
            Command::Tune { source, center_hz } => {
                let (letter, src) = self.target(engine, source)?;
                if src.mode() == Mode::Sweep {
                    return Err(format!("{letter} is sweeping: set its span, or switch it to Live"));
                }
                let caps = &self.open[&source].caps;
                if !caps.freq_ranges_hz.iter().any(|&(lo, hi)| (lo..=hi).contains(&center_hz)) {
                    return Err(format!("{} is outside the device's range", fmt::mhz(center_hz)));
                }
                src.set_center(center_hz).map_err(|e| e.to_string())?;
                log.push(Some(letter), LogKind::Tune, format!("{letter} tuned to {}", fmt::mhz(center_hz)));
                Ok(Outcome::Done)
            }
            Command::SetGain { source, gain } => {
                let (letter, src) = self.target(engine, source)?;
                src.set_gain(gain).map_err(|e| e.to_string())?;
                if let Some(o) = self.open.get_mut(&source) {
                    o.gain = Some(gain);
                }
                log.push(Some(letter), LogKind::Tune, format!("{letter} gain set"));
                Ok(Outcome::Done)
            }
            Command::Tag { record, tag } | Command::Untag { record, tag } if tag.trim().is_empty() => {
                Err(format!("record {}: an empty tag", record.0))
            }
            Command::Tag { record, tag } => found(engine.records().tag(record, tag.trim(), true) || engine.store().is_some(), record),
            Command::Untag { record, tag } => found(engine.records().tag(record, tag.trim(), false) || engine.store().is_some(), record),
            Command::Note { record, text } => found(engine.records().note(record, &text) || engine.store().is_some(), record),
            // The publisher answers it (it needs the command's number).
            Command::QueryRecords(_)
            | Command::Play { .. }
            | Command::StopPlay(_)
            | Command::SaveIq { .. }
            | Command::StartWorker { .. }
            | Command::StopWorker(_) => Err("the publisher's to apply".into()),
            Command::SetMode { source, mode } => {
                let (letter, src) = self.target(engine, source)?;
                src.set_mode(mode);
                // `S` is the sweep's letter (spec §9): a receiver takes it while sweeping, if free, and a free letter back.
                let now = match mode {
                    Mode::Sweep if letter != 'S' && !self.open.values().any(|o| o.letter == 'S') => 'S',
                    Mode::Live if letter == 'S' => self.free_letter().unwrap_or(letter),
                    _ => letter,
                };
                if let Some(o) = self.open.get_mut(&source) {
                    o.letter = now;
                }
                let name = match mode {
                    Mode::Live => "Live",
                    Mode::Sweep => "Sweep",
                };
                let text = if now == letter { format!("{letter} mode {name}") } else { format!("{letter} mode {name}, now {now}") };
                log.push(Some(now), LogKind::Tune, text);
                Ok(Outcome::Done)
            }
            Command::SetSweep { source, start_hz, stop_hz, segments } => {
                let (letter, src) = self.target(engine, source)?;
                let caps = &self.open[&source].caps;
                let lo = caps.freq_ranges_hz.iter().map(|r| r.0).fold(f64::MAX, f64::min);
                let hi = caps.freq_ranges_hz.iter().map(|r| r.1).fold(f64::MIN, f64::max);
                if start_hz.partial_cmp(&stop_hz) != Some(std::cmp::Ordering::Less) || stop_hz < lo || start_hz > hi {
                    return Err(format!("{}–{} is not a span this device can sweep", fmt::mhz(start_hz), fmt::mhz(stop_hz)));
                }
                let plan = crate::sweep::SweepPlan {
                    start_hz: start_hz.max(lo),
                    stop_hz: stop_hz.min(hi),
                    segments,
                    spur_grid_hz: caps.spur_grid_hz,
                    ..Default::default()
                }
                .normalised();
                log.push(Some(letter), LogKind::Tune, format!("{letter} sweeps {}–{}", fmt::mhz(plan.start_hz), fmt::mhz(plan.stop_hz)));
                src.set_sweep_plan(plan);
                Ok(Outcome::Done)
            }
            Command::SetBudget { guaranteed_secs, ceiling_bytes } => {
                let sweep_secs = engine.config().budget.sweep_secs;
                engine.keeper().budget().set_config(BudgetConfig { guaranteed_secs, ceiling_bytes, sweep_secs });
                log.push(
                    None,
                    LogKind::Device,
                    format!("budget: guaranteed {}, ceiling {} MB", fmt::secs(guaranteed_secs), ceiling_bytes >> 20),
                );
                Ok(Outcome::Done)
            }
        }
    }

    fn open(&mut self, engine: &Engine, backend: &str, serial: &str, log: &mut Log) -> Result<Outcome, String> {
        let b = self.backends.iter().find(|b| b.name() == backend).ok_or_else(|| format!("no backend named {backend}"))?;
        if let Some(o) = self.open.values().find(|o| same_device(&o.info, backend, serial)) {
            return Err(format!("already open as {}", o.letter));
        }
        let letter = self.free_letter().ok_or_else(|| "every receiver letter is in use".to_string())?;
        let device = b.open(serial).map_err(|e| e.to_string())?;
        let source = engine.add_source(device, Mode::Live).map_err(|e| e.to_string())?;
        let snap = source.snapshot();
        log.push(Some(letter), LogKind::Device, format!("opened {} as {letter}", snap.info.label));
        self.open.insert(source.id(), Opened { letter, info: snap.info, caps: Arc::new(snap.caps), gain: None });
        self.sync();
        Ok(Outcome::Opened(source.id()))
    }

    /// Each sweeping receiver blanks the other receivers' LO leaks: an Airspy radiates a little of its local
    /// oscillator at its centre + 5 MHz, which would read as carriers that follow the live receivers around.
    pub fn update_lo_masks(&self, engine: &Engine) {
        let sources: Vec<_> = self.open.iter().filter_map(|(&id, o)| Some((id, o, engine.source(id)?))).collect();
        let leaks: Vec<(SourceId, f64)> = sources
            .iter()
            .filter(|(_, o, s)| s.mode() == Mode::Live && o.info.backend == "airspy")
            .map(|(id, _, s)| (*id, s.current_segment().center_hz + LO_LEAK_OFFSET_HZ))
            .collect();
        for (id, _, s) in sources.iter().filter(|(_, _, s)| s.mode() == Mode::Sweep) {
            let mut masks: Vec<(f64, f64)> =
                leaks.iter().filter(|(l, _)| l != id).map(|(_, f)| (f - LO_LEAK_HALFWIDTH_HZ, f + LO_LEAK_HALFWIDTH_HZ)).collect();
            masks.sort_by(|a, b| a.0.total_cmp(&b.0));
            if s.lo_masks() != masks {
                s.set_lo_masks(masks);
            }
        }
    }

    /// The letter and engine source of an open receiver.
    fn target(&self, engine: &Engine, id: SourceId) -> Result<(char, Arc<crate::source::Source>), String> {
        let letter = self.letter(id).ok_or_else(|| no_receiver(id))?;
        let src = engine.source(id).ok_or_else(|| no_receiver(id))?;
        Ok((letter, src))
    }
}

/// Whether `info` names the device `backend`/`serial`. The label is not compared: a backend may label a device
/// differently when it enumerates it (for instance "in use") than when it opens it.
fn same_device(info: &DeviceInfo, backend: &str, serial: &str) -> bool {
    info.backend == backend && info.serial == serial
}

/// Where an Airspy's LO leak sits, from its centre, and how wide a sweep blanks it.
const LO_LEAK_OFFSET_HZ: f64 = 5e6;
const LO_LEAK_HALFWIDTH_HZ: f64 = 15e3;

fn no_receiver(id: SourceId) -> String {
    format!("no receiver {}", id.0)
}

/// A record edit's outcome: the book held the record, or the store will apply it to the one it keeps.
fn found(held: bool, record: api::RecordId) -> Result<Outcome, String> {
    if held { Ok(Outcome::Done) } else { Err(format!("no record {}", record.0)) }
}
