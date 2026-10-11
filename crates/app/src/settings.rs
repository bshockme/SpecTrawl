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
    /// Survey segments (MHz); empty = the whole span.
    pub survey_segments: Vec<(f64, f64)>,
    /// Auto-explore filters and timings (`docs/auto-explore.md`).
    pub explore: ExploreSettings,
}

/// Serialisable form of `engine::explore::Config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExploreSettings {
    /// Ranges (MHz) to explore; empty = anywhere in the survey.
    pub ranges_mhz: Vec<(f64, f64)>,
    /// Preset name → rule name ("exclude", "prioritise", ...), for presets
    /// that aren't ignored.
    pub presets: std::collections::BTreeMap<String, String>,
    pub settle_s: f64,
    pub silence_s: f64,
    pub cap_s: f64,
}

impl Default for ExploreSettings {
    fn default() -> Self {
        Self::from_config(&engine::explore::Config::default())
    }
}

impl ExploreSettings {
    pub fn from_config(c: &engine::explore::Config) -> Self {
        Self {
            ranges_mhz: c.ranges_mhz.clone(),
            presets: c.presets.iter().map(|(k, r)| (k.clone(), r.name().to_string())).collect(),
            settle_s: c.settle_s,
            silence_s: c.silence_s,
            cap_s: c.cap_s,
        }
    }

    pub fn to_config(&self) -> engine::explore::Config {
        engine::explore::Config {
            ranges_mhz: self.ranges_mhz.clone(),
            presets: self.presets.iter().filter_map(|(k, v)| engine::explore::Rule::parse(v).map(|r| (k.clone(), r))).collect(),
            settle_s: self.settle_s.clamp(0.0, 10.0),
            silence_s: self.silence_s.clamp(0.5, 60.0),
            cap_s: self.cap_s.clamp(2.0, 300.0),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log: LogLevels::default(),
            ring_seconds: 10.0,
            nudge_khz: 100.0,
            survey_nudge_mhz: 1.0,
            new_signal_s: 120.0,
            survey_segments: Vec::new(),
            explore: ExploreSettings::default(),
        }
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
