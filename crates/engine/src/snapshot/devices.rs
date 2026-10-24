//! The device registry: what the backends enumerate, which devices are open, and the commands that change them.

use std::collections::BTreeMap;
use std::sync::Arc;

use api::{Capabilities, Command, DeviceEntry, DeviceInfo, GainSetting, Outcome, fmt};
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
                Err(e) => log.push(None, format!("{}: enumeration failed: {e}", b.name())),
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
                log.push(Some(o.letter), format!("{} closed", o.letter));
                Ok(Outcome::Done)
            }
            Command::Tune { source, center_hz } => {
                let (letter, src) = self.target(engine, source)?;
                let caps = &self.open[&source].caps;
                if !caps.freq_ranges_hz.iter().any(|&(lo, hi)| (lo..=hi).contains(&center_hz)) {
                    return Err(format!("{} is outside the device's range", fmt::mhz(center_hz)));
                }
                src.set_center(center_hz).map_err(|e| e.to_string())?;
                log.push(Some(letter), format!("{letter} tuned to {}", fmt::mhz(center_hz)));
                Ok(Outcome::Done)
            }
            Command::SetGain { source, gain } => {
                let (letter, src) = self.target(engine, source)?;
                src.set_gain(gain).map_err(|e| e.to_string())?;
                if let Some(o) = self.open.get_mut(&source) {
                    o.gain = Some(gain);
                }
                log.push(Some(letter), format!("{letter} gain set"));
                Ok(Outcome::Done)
            }
            Command::SetMode { source, mode } => {
                let (letter, src) = self.target(engine, source)?;
                src.set_mode(mode);
                let name = match mode {
                    Mode::Live => "Live",
                    Mode::Sweep => "Sweep",
                };
                log.push(Some(letter), format!("{letter} mode {name}"));
                Ok(Outcome::Done)
            }
            Command::SetBudget { guaranteed_secs, ceiling_bytes } => {
                let sweep_secs = engine.config().budget.sweep_secs;
                engine.keeper().budget().set_config(BudgetConfig { guaranteed_secs, ceiling_bytes, sweep_secs });
                log.push(None, format!("budget: guaranteed {}, ceiling {} MB", fmt::secs(guaranteed_secs), ceiling_bytes >> 20));
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
        log.push(Some(letter), format!("opened {} as {letter}", snap.info.label));
        self.open.insert(source.id(), Opened { letter, info: snap.info, caps: Arc::new(snap.caps), gain: None });
        self.sync();
        Ok(Outcome::Opened(source.id()))
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

fn no_receiver(id: SourceId) -> String {
    format!("no receiver {}", id.0)
}
