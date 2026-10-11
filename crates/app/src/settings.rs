//! General settings, saved as `Documents/SpecTrawl/settings.json`. Anything
//! that is "make it configurable" lands here; the UI for it is the ⚙ menu.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::logging::LogLevels;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub log: LogLevels,
    /// Look-back ring kept per device, seconds (applies to devices opened
    /// from then on; the ⚙ menu can reopen the open ones).
    pub ring_seconds: f64,
    /// Keyboard nudge of a focus receiver's centre (← →), kHz.
    pub nudge_khz: f64,
    /// Keyboard nudge of the survey span (Ctrl + arrows), MHz.
    pub survey_nudge_mhz: f64,
    /// Emitters first seen within this many seconds are drawn as new.
    pub new_signal_s: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { log: LogLevels::default(), ring_seconds: 10.0, nudge_khz: 100.0, survey_nudge_mhz: 1.0, new_signal_s: 120.0 }
    }
}

pub fn path() -> PathBuf {
    engine::sigmf::default_dir().parent().map(|p| p.join("settings.json")).unwrap_or_else(|| "settings.json".into())
}

impl Settings {
    pub fn load() -> Self {
        let p = path();
        match std::fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: {e}; using default settings", p.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&p, s) {
                    log::warn!("settings: {e}");
                }
            }
            Err(e) => log::warn!("settings: {e}"),
        }
    }

    /// Bytes one device's ring takes at `rate` samples/s (ci16).
    pub fn ring_bytes(&self, rate: f64) -> f64 {
        self.ring_seconds * rate * 4.0
    }
}
