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
    /// Survey waterfall row clock in seconds; 0 = one row per sweep pass.
    pub survey_row_s: f64,
    /// Scanner mode rules.
    pub scan: ScanSettings,
}

/// Which active signal a scanning receiver goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ScanRule {
    /// The transmission that started most recently; a newer one preempts.
    #[default]
    Newest,
    /// The range (segment or preset) with the most activity, newest within it.
    MostSignals,
    /// Stay on the current one until it ends.
    Stay,
}

impl ScanRule {
    pub const ALL: [ScanRule; 3] = [ScanRule::Newest, ScanRule::MostSignals, ScanRule::Stay];

    pub fn name(self) -> &'static str {
        match self {
            ScanRule::Newest => "always newest",
            ScanRule::MostSignals => "busiest range",
            ScanRule::Stay => "stay until it ends",
        }
    }
}

/// Scanner settings. The explore filters (ranges, presets) choose what is
/// eligible; these say how the scanner moves between the active ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    pub rule: ScanRule,
    /// Aviation scanner: only the Aviation preset, AM with carrier squelch.
    pub aviation: bool,
    /// Leave a channel after this long without activity.
    pub silence_s: f64,
    /// Longest stay on one channel.
    pub max_s: f64,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self { rule: ScanRule::Newest, aviation: false, silence_s: 3.0, max_s: 180.0 }
    }
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
    /// Save a channel capture per visit: "off", "unknown" (visits that end
    /// unknown) or "all".
    pub capture: String,
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
            capture: "off".into(),
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
            survey_row_s: 0.0,
            scan: ScanSettings::default(),
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
