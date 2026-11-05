//! The record store: every signal record, every classification the engine
//! makes (with the features and the window it was made on), the emitters
//! as they stand, and the explorer's verdicts, in one SQLite file
//! (DESIGN.md §6). It is the audit trail for the classifier: a wrong label
//! in the UI can be traced to the classification jobs behind it, with their
//! SNR, features and sync counts, and to the records and emitters they came
//! from.
//!
//! Writes go through a channel to one writer thread, which batches them
//! into transactions, so the engine's threads never wait on the disk. The
//! handle is cheap to clone; dropping the last one flushes and closes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, params};

use crate::{Emitter, SignalRecord};

/// Schema version, stored in `meta`. Bump when a table changes shape; old
/// files are left alone and a new one is started beside them.
const SCHEMA: i64 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS sessions(
  id INTEGER PRIMARY KEY, started REAL NOT NULL, note TEXT);
CREATE TABLE IF NOT EXISTS records(
  session INTEGER NOT NULL, serial TEXT NOT NULL, record_id INTEGER NOT NULL,
  emitter_id INTEGER, t_start REAL, t_end REAL, f_lo_hz REAL, f_hi_hz REAL, bin_hz REAL,
  peak_dbfs REAL, mean_snr_db REAL, max_snr_db REAL, rows INTEGER, tuner_hz REAL,
  PRIMARY KEY(session, serial, record_id));
CREATE INDEX IF NOT EXISTS records_emitter ON records(session, serial, emitter_id);
CREATE TABLE IF NOT EXISTS classifications(
  id INTEGER PRIMARY KEY, session INTEGER NOT NULL, serial TEXT NOT NULL, time REAL NOT NULL,
  kind TEXT NOT NULL, record_id INTEGER, emitter_id INTEGER,
  center_hz REAL, bandwidth_hz REAL, tuner_hz REAL, window_start INTEGER, window_end INTEGER,
  analysed_s REAL, snr_db REAL, label TEXT, protocol TEXT, confidence REAL,
  sync_count INTEGER, sync_baud REAL, envelope_cv REAL, deviation_hz REAL, offset_hz REAL,
  fsk_levels INTEGER, symbol_rate REAL, symbol_rate_raw REAL,
  decoded INTEGER, tried INTEGER, field_id TEXT, activity TEXT, talk TEXT, kinds TEXT, messages TEXT,
  ms REAL);
CREATE INDEX IF NOT EXISTS classifications_emitter ON classifications(session, serial, emitter_id);
CREATE INDEX IF NOT EXISTS classifications_record ON classifications(session, serial, record_id);
CREATE TABLE IF NOT EXISTS emitters(
  session INTEGER NOT NULL, serial TEXT NOT NULL, emitter_id INTEGER NOT NULL, updated REAL,
  first_seen REAL, last_seen REAL, center_hz REAL, bandwidth_hz REAL, bin_hz REAL,
  records INTEGER, on_time_s REAL, confirmed INTEGER, peak_snr_db REAL,
  label TEXT, protocol_label TEXT, id_label TEXT, tally TEXT, protocols TEXT,
  PRIMARY KEY(session, serial, emitter_id));
CREATE TABLE IF NOT EXISTS verdicts(
  id INTEGER PRIMARY KEY, session INTEGER NOT NULL, time REAL NOT NULL, receiver TEXT,
  emitter_id INTEGER, center_hz REAL, bandwidth_hz REAL, kind TEXT, label TEXT, ident TEXT,
  dwell_s REAL, in_place INTEGER, capture TEXT, evidence TEXT);
CREATE TABLE IF NOT EXISTS notes(
  id INTEGER PRIMARY KEY, session INTEGER NOT NULL, time REAL NOT NULL, serial TEXT, text TEXT);
"#;

/// A classification job, as the store sees it.
pub struct ClassJob<'a> {
    /// "record", "group" (an emitter's multi-burst window) or "follow"
    /// (a granted call's voice channel).
    pub kind: &'static str,
    pub record_id: Option<u64>,
    pub emitter_id: Option<u64>,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub tuner_hz: f64,
    pub window: (u64, u64),
    pub class: &'a detect::Classification,
    /// Wall time the job took.
    pub ms: f64,
}

/// An explorer verdict, as the store sees it.
pub struct VerdictRow<'a> {
    pub receiver: char,
    pub emitter_id: u64,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub kind: &'a str,
    pub label: &'a str,
    pub ident: &'a str,
    pub dwell_s: f64,
    pub in_place: bool,
    pub capture: Option<&'a str>,
    /// What the verdict rested on (the focus emitter or occupant that
    /// supplied the label, with its tally), for tracing a wrong one.
    pub evidence: &'a str,
}

enum Msg {
    Record { serial: Arc<str>, r: SignalRecord, emitter: Option<u64> },
    Class(Box<ClassRow>),
    Emitter { serial: Arc<str>, e: Emitter },
    Verdict(Box<VerdictOwned>),
    Note { serial: Option<Arc<str>>, text: String },
    Flush(SyncSender<()>),
}

struct ClassRow {
    serial: Arc<str>,
    time: f64,
    kind: &'static str,
    record_id: Option<u64>,
    emitter_id: Option<u64>,
    center_hz: f64,
    bandwidth_hz: f64,
    tuner_hz: f64,
    window: (u64, u64),
    analysed_s: f64,
    snr_db: Option<f32>,
    label: String,
    protocol: Option<String>,
    confidence: f32,
    sync: Option<(usize, f64)>,
    envelope_cv: f32,
    deviation_hz: f32,
    offset_hz: f32,
    fsk_levels: u8,
    symbol_rate: Option<f64>,
    symbol_rate_raw: Option<f64>,
    tone_hz: Option<f32>,
    decoded: Option<(usize, usize)>,
    field_id: Option<String>,
    activity: Option<String>,
    talk: Option<String>,
    kinds: Option<String>,
    messages: Option<String>,
    ms: f64,
}

struct VerdictOwned {
    time: f64,
    receiver: String,
    emitter_id: u64,
    center_hz: f64,
    bandwidth_hz: f64,
    kind: String,
    label: String,
    ident: String,
    dwell_s: f64,
    in_place: bool,
    capture: Option<String>,
    evidence: String,
}

/// A handle on the store. Clone it for every source and the app.
#[derive(Clone)]
pub struct Store {
    tx: Sender<Msg>,
    path: Arc<PathBuf>,
    session: i64,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({}, session {})", self.path.display(), self.session)
    }
}

fn unix(t: SystemTime) -> f64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn now() -> f64 {
    unix(SystemTime::now())
}

/// The default file: `Documents/SpecTrawl/spectrawl.db`, beside the
/// settings and the captures.
pub fn default_path() -> PathBuf {
    crate::sigmf::default_dir().parent().map(|p| p.join("spectrawl.db")).unwrap_or_else(|| "spectrawl.db".into())
}

impl Store {
    /// Opens (creating if needed) the store at `path` and starts a session.
    /// `note` says what the session is (the app, an example and its
    /// arguments).
    pub fn open(path: &Path, note: &str) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA_SQL).map_err(|e| e.to_string())?;
        // Columns added since the schema was first written (older files
        // gain them in place; the error for an existing column is ignored).
        let _ = conn.execute("ALTER TABLE classifications ADD COLUMN tone_hz REAL", []);
        let have: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get(0)).ok();
        match have.and_then(|v| v.parse::<i64>().ok()) {
            None => {
                conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema', ?1)", params![SCHEMA.to_string()])
                    .map_err(|e| e.to_string())?;
            }
            Some(v) if v == SCHEMA => {}
            Some(v) => return Err(format!("{}: schema {v}, this build writes {SCHEMA}; move the file aside", path.display())),
        }
        conn.execute("INSERT INTO sessions(started, note) VALUES(?1, ?2)", params![now(), note]).map_err(|e| e.to_string())?;
        let session = conn.last_insert_rowid();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("store".into()).spawn(move || writer(conn, rx, session)).map_err(|e| e.to_string())?;
        log::info!("store: {} (session {session})", path.display());
        Ok(Store { tx, path: Arc::new(path.to_path_buf()), session })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session(&self) -> i64 {
        self.session
    }

    /// The store's view of one source.
    pub fn for_source(&self, serial: &str) -> SourceStore {
        SourceStore { store: self.clone(), serial: Arc::from(serial) }
    }

    pub fn verdict(&self, v: &VerdictRow) {
        let _ = self.tx.send(Msg::Verdict(Box::new(VerdictOwned {
            time: now(),
            receiver: v.receiver.to_string(),
            emitter_id: v.emitter_id,
            center_hz: v.center_hz,
            bandwidth_hz: v.bandwidth_hz,
            kind: v.kind.to_string(),
            label: v.label.to_string(),
            ident: v.ident.to_string(),
            dwell_s: v.dwell_s,
            in_place: v.in_place,
            capture: v.capture.map(str::to_string),
            evidence: v.evidence.to_string(),
        })));
    }

    /// A free-text note for the session (a retune, a mode change, what the
    /// operator heard).
    pub fn note(&self, text: &str) {
        let _ = self.tx.send(Msg::Note { serial: None, text: text.to_string() });
    }

    /// Waits until everything sent so far is on disk.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::sync_channel(0);
        if self.tx.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }
}

/// A `Store` bound to one source's serial.
#[derive(Clone, Debug)]
pub struct SourceStore {
    store: Store,
    serial: Arc<str>,
}

impl SourceStore {
    pub fn record(&self, r: &SignalRecord, emitter: Option<u64>) {
        let _ = self.store.tx.send(Msg::Record { serial: self.serial.clone(), r: r.clone(), emitter });
    }

    pub fn emitter(&self, e: &Emitter) {
        let _ = self.store.tx.send(Msg::Emitter { serial: self.serial.clone(), e: e.clone() });
    }

    pub fn classification(&self, j: &ClassJob) {
        let c = j.class;
        let f = &c.features;
        let fields = c.fields.as_ref();
        let row = ClassRow {
            serial: self.serial.clone(),
            time: now(),
            kind: j.kind,
            record_id: j.record_id,
            emitter_id: j.emitter_id,
            center_hz: j.center_hz,
            bandwidth_hz: j.bandwidth_hz,
            tuner_hz: j.tuner_hz,
            window: j.window,
            analysed_s: c.analysed_s,
            snr_db: (!f.snr_db.is_nan()).then_some(f.snr_db),
            label: c.label.clone(),
            protocol: c.protocol.clone(),
            confidence: c.confidence,
            sync: c.sync.as_ref().map(|s| (s.count, s.baud)),
            envelope_cv: f.envelope_cv,
            deviation_hz: f.deviation_hz,
            offset_hz: f.offset_hz,
            fsk_levels: f.fsk_levels,
            symbol_rate: f.symbol_rate,
            symbol_rate_raw: f.symbol_rate_raw,
            tone_hz: f.tone_hz,
            decoded: fields.map(|f| (f.decoded, f.tried)),
            field_id: fields.and_then(|f| f.id.clone()),
            activity: fields.and_then(|f| f.activity.map(|a| format!("{a:?}"))),
            talk: fields.and_then(|f| f.talk.clone()),
            kinds: fields
                .filter(|f| !f.kinds.is_empty())
                .map(|f| f.kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect::<Vec<_>>().join(", ")),
            messages: fields.filter(|f| !f.messages.is_empty()).map(|f| f.messages.join("\n")),
            ms: j.ms,
        };
        let _ = self.store.tx.send(Msg::Class(Box::new(row)));
    }

    pub fn note(&self, text: &str) {
        let _ = self.store.tx.send(Msg::Note { serial: Some(self.serial.clone()), text: text.to_string() });
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
}

fn tally_json(t: &std::collections::BTreeMap<String, u32>) -> String {
    serde_json::to_string(t).unwrap_or_default()
}

/// Batches messages into transactions: one every `BATCH` or every 250 ms.
fn writer(mut conn: Connection, rx: Receiver<Msg>, session: i64) {
    const BATCH: usize = 256;
    let mut pending: Vec<Msg> = Vec::with_capacity(BATCH);
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(m) = first {
            pending.push(m);
            while pending.len() < BATCH {
                match rx.try_recv() {
                    Ok(m) => pending.push(m),
                    Err(_) => break,
                }
            }
        }
        if pending.is_empty() {
            continue;
        }
        let mut flushes = Vec::new();
        match conn.transaction() {
            Ok(tx) => {
                for m in pending.drain(..) {
                    if let Err(e) = write_one(&tx, session, m, &mut flushes) {
                        log::warn!("store: {e}");
                    }
                }
                if let Err(e) = tx.commit() {
                    log::warn!("store: commit: {e}");
                }
            }
            Err(e) => {
                log::warn!("store: transaction: {e}");
                pending.clear();
            }
        }
        for f in flushes {
            let _ = f.send(());
        }
    }
    // Anything left when the last handle dropped.
    if let Ok(tx) = conn.transaction() {
        let mut flushes = Vec::new();
        for m in pending.drain(..) {
            let _ = write_one(&tx, session, m, &mut flushes);
        }
        let _ = tx.commit();
    }
}

fn write_one(tx: &rusqlite::Transaction, session: i64, m: Msg, flushes: &mut Vec<SyncSender<()>>) -> rusqlite::Result<()> {
    match m {
        Msg::Record { serial, r, emitter } => {
            tx.execute(
                "INSERT OR REPLACE INTO records(session, serial, record_id, emitter_id, t_start, t_end, f_lo_hz, f_hi_hz, bin_hz,
                   peak_dbfs, mean_snr_db, max_snr_db, rows, tuner_hz)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    session,
                    &*serial,
                    r.id as i64,
                    emitter.map(|e| e as i64),
                    unix(r.t_start),
                    unix(r.t_end),
                    r.f_lo_hz,
                    r.f_hi_hz,
                    r.bin_hz,
                    r.peak_dbfs,
                    r.mean_snr_db,
                    r.max_snr_db,
                    r.rows,
                    r.tuner_center_hz
                ],
            )?;
        }
        Msg::Class(c) => {
            tx.execute(
                "INSERT INTO classifications(session, serial, time, kind, record_id, emitter_id, center_hz, bandwidth_hz, tuner_hz,
                   window_start, window_end, analysed_s, snr_db, label, protocol, confidence, sync_count, sync_baud,
                   envelope_cv, deviation_hz, offset_hz, fsk_levels, symbol_rate, symbol_rate_raw,
                   decoded, tried, field_id, activity, talk, kinds, messages, ms, tone_hz)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                   ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)",
                params![
                    session,
                    &*c.serial,
                    c.time,
                    c.kind,
                    c.record_id.map(|v| v as i64),
                    c.emitter_id.map(|v| v as i64),
                    c.center_hz,
                    c.bandwidth_hz,
                    c.tuner_hz,
                    c.window.0 as i64,
                    c.window.1 as i64,
                    c.analysed_s,
                    c.snr_db,
                    c.label,
                    c.protocol,
                    c.confidence,
                    c.sync.map(|s| s.0 as i64),
                    c.sync.map(|s| s.1),
                    c.envelope_cv,
                    c.deviation_hz,
                    c.offset_hz,
                    c.fsk_levels,
                    c.symbol_rate,
                    c.symbol_rate_raw,
                    c.decoded.map(|d| d.0 as i64),
                    c.decoded.map(|d| d.1 as i64),
                    c.field_id,
                    c.activity,
                    c.talk,
                    c.kinds,
                    c.messages,
                    c.ms,
                    c.tone_hz
                ],
            )?;
        }
        Msg::Emitter { serial, e } => {
            tx.execute(
                "INSERT OR REPLACE INTO emitters(session, serial, emitter_id, updated, first_seen, last_seen, center_hz, bandwidth_hz,
                   bin_hz, records, on_time_s, confirmed, peak_snr_db, label, protocol_label, id_label, tally, protocols)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    session,
                    &*serial,
                    e.id as i64,
                    now(),
                    unix(e.first_seen),
                    unix(e.last_seen),
                    e.center_hz,
                    e.bandwidth_hz,
                    e.bin_hz,
                    e.records,
                    e.on_time_s,
                    e.confirmed,
                    e.peak_snr_db,
                    e.label(),
                    e.protocol_label(),
                    e.id_label(),
                    tally_json(&e.tally),
                    tally_json(&e.protocols)
                ],
            )?;
        }
        Msg::Verdict(v) => {
            tx.execute(
                "INSERT INTO verdicts(session, time, receiver, emitter_id, center_hz, bandwidth_hz, kind, label, ident, dwell_s,
                   in_place, capture, evidence)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    session,
                    v.time,
                    v.receiver,
                    v.emitter_id as i64,
                    v.center_hz,
                    v.bandwidth_hz,
                    v.kind,
                    v.label,
                    v.ident,
                    v.dwell_s,
                    v.in_place,
                    v.capture,
                    v.evidence
                ],
            )?;
        }
        Msg::Note { serial, text } => {
            tx.execute(
                "INSERT INTO notes(session, time, serial, text) VALUES(?1, ?2, ?3, ?4)",
                params![session, now(), serial.as_deref(), text],
            )?;
        }
        Msg::Flush(s) => flushes.push(s),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_rows_and_reopens() {
        let dir = std::env::temp_dir().join(format!("spectrawl-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("t.db");
        let st = Store::open(&path, "test").unwrap();
        let src = st.for_source("SERIAL1");
        let class = detect::Classification {
            label: "AM / OOK".into(),
            protocol: None,
            confidence: 0.5,
            features: detect::Features { snr_db: 3.5, envelope_cv: 0.52, ..Default::default() },
            sync: None,
            fields: None,
            analysed_s: 1.5,
        };
        src.classification(&ClassJob {
            kind: "record",
            record_id: Some(7),
            emitter_id: Some(2),
            center_hz: 850e6,
            bandwidth_hz: 12.5e3,
            tuner_hz: 851e6,
            window: (100, 200),
            class: &class,
            ms: 12.0,
        });
        st.verdict(&VerdictRow {
            receiver: 'A',
            emitter_id: 9,
            center_hz: 850e6,
            bandwidth_hz: 54e3,
            kind: "classified",
            label: "AM / OOK",
            ident: "",
            dwell_s: 0.0,
            in_place: true,
            capture: None,
            evidence: "focus emitter 2",
        });
        st.flush();
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM classifications WHERE snr_db < 5", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let label: String = conn.query_row("SELECT label FROM verdicts", [], |r| r.get(0)).unwrap();
        assert_eq!(label, "AM / OOK");
        drop(st);
        // A second session in the same file.
        let st2 = Store::open(&path, "test 2").unwrap();
        assert_eq!(st2.session(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
