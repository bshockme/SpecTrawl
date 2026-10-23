//! Structured logging: every `log` record goes to stderr, to a rolling file
//! under `Documents/SpecTrawl/logs`, and to an in-memory ring the
//! Diagnostics tab shows. Levels are set per *area* (device, sweep,
//! detection, decoders, audio, ui), mapped from the record's module path, so
//! the sweep state machine can be put on debug without drowning in decoder
//! chatter. The levels live in the settings file and can be changed at run
//! time.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

/// The log areas, in the order the UI lists them.
pub const AREAS: [&str; 7] = ["device", "sweep", "detection", "decoders", "audio", "ui", "other"];

/// Entries kept for the Diagnostics tab.
const RING: usize = 3000;
/// The log file rolls over at this size; this many old files are kept.
const ROLL_BYTES: u64 = 8 << 20;
const KEEP_FILES: u32 = 5;

/// Per-area levels, saved in the settings file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLevels {
    #[serde(with = "level_name")]
    pub default: LevelFilter,
    /// Area → level, for areas that differ from the default.
    #[serde(default)]
    pub areas: std::collections::BTreeMap<String, String>,
}

impl Default for LogLevels {
    fn default() -> Self {
        // Third-party crates (wgpu, winit) chatter at info; warn is enough.
        Self { default: LevelFilter::Info, areas: [("other".to_string(), "warn".to_string())].into_iter().collect() }
    }
}

impl LogLevels {
    pub fn level(&self, area: &str) -> LevelFilter {
        self.areas.get(area).and_then(|s| parse_level(s)).unwrap_or(self.default)
    }

    pub fn set(&mut self, area: &str, level: LevelFilter) {
        if level == self.default {
            self.areas.remove(area);
        } else {
            self.areas.insert(area.to_string(), level_str(level).to_string());
        }
    }

    /// The most verbose level in use (what `log` is told to pass through).
    fn max(&self) -> LevelFilter {
        AREAS.iter().map(|a| self.level(a)).max().unwrap_or(self.default)
    }
}

pub fn parse_level(s: &str) -> Option<LevelFilter> {
    Some(match s.to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => return None,
    })
}

pub fn level_str(l: LevelFilter) -> &'static str {
    match l {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

mod level_name {
    use super::*;
    pub fn serialize<S: serde::Serializer>(l: &LevelFilter, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(level_str(*l))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<LevelFilter, D::Error> {
        let s = String::deserialize(d)?;
        parse_level(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown log level {s}")))
    }
}

/// Which area a module path belongs to.
pub fn area_of(target: &str) -> &'static str {
    let t = target;
    if t.starts_with("backend_") || t.starts_with("sdr_core") {
        "device"
    } else if t.starts_with("engine_old::sweep") {
        "sweep"
    } else if t.starts_with("engine_old::listen") || t.starts_with("app::audio") {
        "audio"
    } else if t.starts_with("engine_old::monitor")
        || t.starts_with("engine_old::calls")
        || t.starts_with("engine_old::rds")
        || t.starts_with("engine_old::cell")
        || (t.starts_with("detect::")
            && !t.starts_with("detect::spectral")
            && !t.starts_with("detect::features")
            && !t.starts_with("detect::carrier"))
    {
        "decoders"
    } else if t.starts_with("engine") || t.starts_with("detect") || t.starts_with("dsp") {
        "detection"
    } else if t.starts_with("app") || t.starts_with("spectrawl") {
        "ui"
    } else {
        "other"
    }
}

/// One record as kept for the UI.
#[derive(Debug, Clone)]
pub struct Entry {
    pub time: SystemTime,
    pub level: Level,
    pub area: &'static str,
    pub target: String,
    pub message: String,
}

struct Sink {
    file: Option<BufWriter<File>>,
    bytes: u64,
    ring: VecDeque<Entry>,
    /// Records dropped from the ring since it was last read, for the UI.
    dropped: u64,
}

struct Logger {
    levels: RwLock<LogLevels>,
    sink: Mutex<Sink>,
    dir: PathBuf,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Where the log files go.
pub fn log_dir() -> PathBuf {
    engine_old::sigmf::default_dir().parent().map(|p| p.join("logs")).unwrap_or_else(|| "logs".into())
}

fn open_file(dir: &PathBuf) -> (Option<BufWriter<File>>, u64) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("spectrawl.log");
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => (Some(BufWriter::new(f)), bytes),
        Err(e) => {
            eprintln!("log file {}: {e}", path.display());
            (None, 0)
        }
    }
}

/// spectrawl.log → spectrawl.1.log → … → spectrawl.{KEEP_FILES}.log (dropped).
fn roll(dir: &PathBuf) {
    let name = |k: u32| if k == 0 { dir.join("spectrawl.log") } else { dir.join(format!("spectrawl.{k}.log")) };
    let _ = std::fs::remove_file(name(KEEP_FILES));
    for k in (0..KEEP_FILES).rev() {
        let _ = std::fs::rename(name(k), name(k + 1));
    }
}

/// Installs the logger. `levels` come from the settings file; `RUST_LOG`
/// (a bare level name) overrides the default level for this run.
pub fn init(mut levels: LogLevels) {
    if let Some(l) = std::env::var("RUST_LOG").ok().and_then(|s| parse_level(&s)) {
        levels.default = l;
    }
    let dir = log_dir();
    let (file, bytes) = open_file(&dir);
    let max = levels.max();
    let logger = Logger {
        levels: RwLock::new(levels),
        sink: Mutex::new(Sink { file, bytes, ring: VecDeque::with_capacity(RING), dropped: 0 }),
        dir,
    };
    if LOGGER.set(logger).is_ok() {
        let _ = log::set_logger(LOGGER.get().unwrap());
        log::set_max_level(max);
        log::info!("SpecTrawl {} starting; log in {}", env!("CARGO_PKG_VERSION"), log_dir().display());
    }
}

/// Current per-area levels.
#[allow(dead_code)]
pub fn levels() -> LogLevels {
    LOGGER.get().map(|l| l.levels.read().clone()).unwrap_or_default()
}

/// Changes the per-area levels at run time.
pub fn set_levels(levels: LogLevels) {
    if let Some(l) = LOGGER.get() {
        log::set_max_level(levels.max());
        *l.levels.write() = levels;
    }
}

/// The newest entries, oldest first, and how many were dropped from the
/// ring since the last call.
pub fn recent(max: usize) -> (Vec<Entry>, u64) {
    let Some(l) = LOGGER.get() else { return (Vec::new(), 0) };
    let mut s = l.sink.lock();
    let dropped = std::mem::take(&mut s.dropped);
    let skip = s.ring.len().saturating_sub(max);
    (s.ring.iter().skip(skip).cloned().collect(), dropped)
}

pub fn clear() {
    if let Some(l) = LOGGER.get() {
        l.sink.lock().ring.clear();
    }
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.levels.read().level(area_of(m.target()))
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let area = area_of(r.target());
        let now = SystemTime::now();
        let message = r.args().to_string();
        let line = format!("{} {:<5} [{area}] {}: {message}", stamp(now), r.level(), r.target());
        eprintln!("{line}");
        let mut s = self.sink.lock();
        if let Some(f) = s.file.as_mut() {
            let _ = writeln!(f, "{line}");
            if r.level() <= Level::Warn {
                let _ = f.flush();
            }
            s.bytes += line.len() as u64 + 1;
            if s.bytes > ROLL_BYTES {
                let _ = s.file.take().map(|mut f| f.flush());
                roll(&self.dir);
                let (f, b) = open_file(&self.dir);
                s.file = f;
                s.bytes = b;
            }
        }
        if s.ring.len() >= RING {
            s.ring.pop_front();
            s.dropped += 1;
        }
        s.ring.push_back(Entry { time: now, level: r.level(), area, target: r.target().to_string(), message });
    }

    fn flush(&self) {
        if let Some(f) = self.sink.lock().file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// `2026-10-11 02:13:45.123` in UTC.
pub fn stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!("{y:04}-{m:02}-{day:02} {:02}:{:02}:{:02}.{:03}", s / 3600, (s / 60) % 60, s % 60, d.subsec_millis())
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(20_737), (2026, 10, 11));
    }

    #[test]
    fn areas() {
        assert_eq!(area_of("engine_old::sweep"), "sweep");
        assert_eq!(area_of("backend_airspy"), "device");
        assert_eq!(area_of("detect::flex"), "decoders");
        assert_eq!(area_of("detect::spectral"), "detection");
        assert_eq!(area_of("engine_old::occupancy"), "detection");
        assert_eq!(area_of("app"), "ui");
        assert_eq!(area_of("spectrawl::logging"), "ui");
        assert_eq!(area_of("wgpu_core::device"), "other");
    }

    #[test]
    fn levels_round_trip() {
        let mut l = LogLevels::default();
        l.set("sweep", LevelFilter::Debug);
        l.set("ui", LevelFilter::Info);
        let j = serde_json::to_string(&l).unwrap();
        let back: LogLevels = serde_json::from_str(&j).unwrap();
        assert_eq!(back.level("sweep"), LevelFilter::Debug);
        assert_eq!(back.level("ui"), LevelFilter::Info);
        assert!(!back.areas.contains_key("ui"));
        assert_eq!(back.max(), LevelFilter::Debug);
    }
}
