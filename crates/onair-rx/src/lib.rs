//! OnAir's `onair-server` as a SpecTrawl decoder worker (spec §4.6, `docs/decoder-worker.md`).
//!
//! OnAir is GPL-3 and runs as a separate program: this crate contains only the process supervisor and the protocol
//! client, no OnAir code, header or binding.
//!
//! - **Start.** A worker is `onair-server --config <run dir>/server.conf`, with `HOME` and `XDG_CONFIG_HOME` set to its
//!   run folder (so it writes nothing to the user's configuration), its control API on a free loopback port, and
//!   `net_radio` pointing at the engine's rtl_tcp feed. `device=rtl_tcp` makes it take that feed: without it OnAir opens
//!   the first radio it finds. The supervisor checks `/api/v1/status` names the network radio, and kills the worker if it
//!   names anything else.
//! - **Poll.** `/api/v1/telemetry` gives the lamp and the counters; `/api/v1/events` the decoded messages, from a cursor.
//! - **Restart.** A worker that exits or stops answering is restarted after 1, 2, 4 … 30 s.
//! - **Stop.** Dropping the worker kills its process.

pub mod http;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use api::{Lamp, WorkerMessage, WorkerMode};
use engine::workers::{Worker, WorkerFactory, WorkerReport, WorkerSpec};
use serde_json::Value;

/// How long a starting worker has to answer `/api/v1/status` with the network radio selected.
pub const START_TIMEOUT: Duration = Duration::from_secs(15);
/// The restart backoff's first and longest wait.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Events asked for per poll.
const EVENTS_PER_POLL: u32 = 200;

/// The modes SpecTrawl drives OnAir in, in the order the survey meets them (spec §6 step 7): `(id, name, rate)`. A stock
/// `onair-server` (3.6.0) reports their lamp and counters only: it has no message stream, so the pages, positions and
/// calls they decode need the patched OnAir (`docs/superpowers/plans/2026-11-05-rebuild-step-7.md`), as does air band,
/// which is not listed until then.
pub const MODES: &[(&str, &str, f64)] =
    &[("flex", "FLEX", 1.25e6), ("pocsag", "POCSAG", 1.25e6), ("acars", "ACARS", 1.25e6), ("ais", "AIS", 1.25e6)];

/// An installed OnAir.
#[derive(Debug, Clone)]
pub struct OnAir {
    /// `.../bin/onair-server`.
    pub server: PathBuf,
    /// "OnAir 3.6.0".
    pub label: String,
    /// Where workers' run folders go.
    pub runs: PathBuf,
    /// More `key=value` lines for every worker's settings (a test's switches; OnAir's own keys).
    pub extra: Vec<String>,
}

impl OnAir {
    pub fn new(server: PathBuf, label: impl Into<String>, runs: PathBuf) -> OnAir {
        OnAir { server, label: label.into(), runs, extra: Vec::new() }
    }

    /// The newest release unpacked under `root` (`<root>/release/<version>/bin/onair-server`), with run folders under
    /// `<root>/run`.
    pub fn find(root: &Path) -> Option<OnAir> {
        let mut versions: Vec<(Vec<u32>, PathBuf, String)> = std::fs::read_dir(root.join("release"))
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let server = e.path().join("bin").join(if cfg!(windows) { "onair-server.exe" } else { "onair-server" });
                let key: Vec<u32> = name.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
                server.is_file().then_some((key, server, name))
            })
            .collect();
        versions.sort();
        let (_, server, v) = versions.pop()?;
        Some(OnAir::new(server, format!("OnAir {v}"), root.join("run")))
    }
}

impl WorkerFactory for OnAir {
    fn name(&self) -> String {
        self.label.clone()
    }

    fn modes(&self) -> Vec<WorkerMode> {
        MODES.iter().map(|&(id, name, rate)| WorkerMode { id: id.into(), name: name.into(), by: self.label.clone(), rate }).collect()
    }

    fn start(&self, spec: &WorkerSpec) -> Result<Box<dyn Worker>, String> {
        let name = MODES.iter().find(|m| m.0 == spec.mode).map_or(spec.mode.clone(), |m| m.1.to_string());
        let mut w = OnAirWorker {
            onair: self.clone(),
            spec: spec.clone(),
            name,
            dir: self.runs.join(format!("{}-{}-{}", spec.mode, std::process::id(), spec.iq.port())),
            child: None,
            port: 0,
            cursor: None,
            restarts: 0,
            backoff: BACKOFF_FIRST,
            retry_at: None,
            error: None,
            messages: 0,
        };
        w.launch()?;
        Ok(Box::new(w))
    }
}

pub struct OnAirWorker {
    onair: OnAir,
    spec: WorkerSpec,
    name: String,
    dir: PathBuf,
    child: Option<Child>,
    /// Its control API's port.
    port: u16,
    /// The events cursor (`None` until the first answer).
    cursor: Option<u64>,
    restarts: u32,
    backoff: Duration,
    retry_at: Option<Instant>,
    error: Option<String>,
    messages: u64,
}

impl OnAirWorker {
    /// Writes its settings, starts the process and waits for it to answer with the network radio selected.
    fn launch(&mut self) -> Result<(), String> {
        std::fs::create_dir_all(self.dir.join(".config")).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        self.port = free_port()?;
        let conf = self.dir.join("server.conf");
        let s = &self.spec;
        let settings = format!(
            "# Written by SpecTrawl's onair-rx: this worker reads SpecTrawl's feed, never a radio.\nport={}\nnet_radio=rtltcp,127.0.0.1,{}\ndevice=rtl_tcp\nmode={}\nfreq_mhz={:.6}\nrate_msps={}\nautostart=1\n",
            self.port,
            s.iq.port(),
            s.mode,
            s.center_hz / 1e6,
            s.rate / 1e6
        );
        let settings = settings + &self.onair.extra.iter().map(|l| format!("{l}\n")).collect::<String>();
        std::fs::write(&conf, settings).map_err(|e| format!("{}: {e}", conf.display()))?;
        let log = std::fs::File::create(self.dir.join("log.txt")).map_err(|e| e.to_string())?;
        let child = Command::new(&self.onair.server)
            .arg("--config")
            .arg(&conf)
            .env("HOME", &self.dir)
            .env("XDG_CONFIG_HOME", self.dir.join(".config"))
            .env("APPDATA", self.dir.join(".config"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("{}: {e}", self.onair.server.display()))?;
        self.child = Some(child);
        let t0 = Instant::now();
        loop {
            if let Some(code) = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                self.child = None;
                return Err(format!("{} exited at start ({code}); see {}", self.onair.label, self.dir.join("log.txt").display()));
            }
            if let Ok(st) = http::get_json(self.port, "/api/v1/status") {
                let device = st["device"].as_str().unwrap_or_default();
                if device.starts_with("rtl_tcp") {
                    log::info!(
                        "{} {} at {:.4} MHz: worker on port {}, fed from {}",
                        self.onair.label,
                        s.mode,
                        s.center_hz / 1e6,
                        self.port,
                        s.iq
                    );
                    self.error = None;
                    return Ok(());
                }
                if !device.is_empty() {
                    self.kill();
                    return Err(format!("{} chose the radio {device:?}, not SpecTrawl's feed: stopped", self.onair.label));
                }
            }
            if t0.elapsed() > START_TIMEOUT {
                self.kill();
                return Err(format!("{} did not answer within {START_TIMEOUT:?}", self.onair.label));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// The process has gone or stopped answering: try again later.
    fn failed(&mut self, why: String) {
        log::warn!("{} {}: {why}; restarting in {:?}", self.onair.label, self.spec.mode, self.backoff);
        self.kill();
        self.error = Some(why);
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    fn report(&self) -> WorkerReport {
        WorkerReport { error: self.error.clone(), restarts: self.restarts, ..Default::default() }
    }
}

impl Worker for OnAirWorker {
    fn poll(&mut self) -> WorkerReport {
        if self.child.is_none() {
            if self.retry_at.is_some_and(|t| Instant::now() < t) {
                return self.report();
            }
            self.restarts += 1;
            self.cursor = None;
            if let Err(e) = self.launch() {
                self.failed(e);
                return self.report();
            }
            self.backoff = BACKOFF_FIRST;
        }
        if let Some(code) = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            self.child = None;
            self.failed(format!("exited ({code})"));
            return self.report();
        }
        let tel = match http::get_json(self.port, "/api/v1/telemetry") {
            Ok(t) => t,
            Err(e) => {
                self.failed(format!("telemetry: {e}"));
                return self.report();
            }
        };
        // From 0 at first: the process is this worker's own, so every message in it is ours (without `since` OnAir
        // answers from now, and a message decoded before the first poll would be lost).
        let path = format!("/api/v1/events?since={}&max={EVENTS_PER_POLL}", self.cursor.unwrap_or(0));
        let mut messages = Vec::new();
        if let Ok(ev) = http::get_json(self.port, &path) {
            if let Some(n) = ev["next"].as_u64() {
                self.cursor = Some(n);
            }
            for e in ev["events"].as_array().into_iter().flatten() {
                messages.push(message(e, self.spec.center_hz));
            }
        }
        self.messages += messages.len() as u64;
        let (lamp, ok, bad) = lamp(&tel);
        let at = format!("{} {:.4} MHz", self.name, self.spec.center_hz / 1e6);
        let summary = match lamp {
            Lamp::Locked => format!("{at}: locked, {ok} blocks ok, {bad} bad, {} messages", self.messages),
            Lamp::Syncing => format!("{at}: syncing, {} messages", self.messages),
            Lamp::Searching => format!("{at}: searching, {} messages", self.messages),
        };
        WorkerReport { lamp: Some(lamp), summary, blocks_ok: ok, blocks_bad: bad, messages, error: None, restarts: self.restarts }
    }
}

impl Drop for OnAirWorker {
    fn drop(&mut self) {
        self.kill();
    }
}

/// OnAir's common telemetry block as a lamp and counters: `state` 0 searching, 1 converging, 2 locked; `locked` too.
pub fn lamp(t: &Value) -> (Lamp, u64, u64) {
    let (ok, bad) = (t["blocksOk"].as_u64().unwrap_or(0), t["blocksBad"].as_u64().unwrap_or(0));
    let lamp = if t["locked"].as_bool() == Some(true) || t["state"].as_u64() == Some(2) {
        Lamp::Locked
    } else if t["state"].as_u64() == Some(1) {
        Lamp::Syncing
    } else {
        Lamp::Searching
    };
    (lamp, ok, bad)
}

/// One of OnAir's decoded-message lines (`{"v":1,"type":…,"time":…,"mode":…,"freqHz":…,"fields":{…}}`) as a message:
/// its text is `fields.text` (or `message`, `summary`), else the fields as `key=value`; its frequency `freqHz`, else the
/// worker's centre.
pub fn message(e: &Value, center_hz: f64) -> WorkerMessage {
    let fields = &e["fields"];
    let text = ["text", "message", "summary"].iter().find_map(|k| fields[*k].as_str().map(String::from)).unwrap_or_else(|| {
        fields
            .as_object()
            .map(|o| o.iter().map(|(k, v)| format!("{k}={}", v.as_str().map_or(v.to_string(), String::from))).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    });
    let time = e["time"].as_f64().map_or(SystemTime::UNIX_EPOCH, |t| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(t.max(0.0)));
    WorkerMessage {
        time,
        freq_hz: e["freqHz"].as_f64().filter(|f| *f > 0.0).unwrap_or(center_hz),
        kind: e["type"].as_str().unwrap_or("message").to_string(),
        text,
    }
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn telemetry_reads_as_a_lamp() {
        assert_eq!(lamp(&json!({"state":0,"locked":false,"blocksOk":0,"blocksBad":0})), (Lamp::Searching, 0, 0));
        assert_eq!(
            lamp(&json!({"state":1,"locked":true,"blocksOk":5367,"blocksBad":969})),
            (Lamp::Locked, 5367, 969),
            "FLEX, as OnAir 3.6.0 sent it"
        );
        assert_eq!(lamp(&json!({"state":1,"locked":false})).0, Lamp::Syncing);
    }

    #[test]
    fn an_event_line_reads_as_a_message() {
        let e = json!({"v":1,"type":"pager.message","time":1760000000.5,"mode":"POCSAG","freqHz":153350000,"fields":{"text":"hello","address":1234}});
        let m = message(&e, 1e6);
        assert_eq!((m.kind.as_str(), m.text.as_str(), m.freq_hz), ("pager.message", "hello", 153.35e6));
        assert_eq!(m.time, SystemTime::UNIX_EPOCH + Duration::from_secs_f64(1760000000.5));
        let m = message(&json!({"type":"x","fields":{"a":1,"b":"two"}}), 5e6);
        assert_eq!((m.text.as_str(), m.freq_hz), ("a=1 b=two", 5e6));
    }
}
