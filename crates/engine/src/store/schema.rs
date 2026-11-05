//! The store's SQLite schema (version 1) and the mapping between its rows and the record types.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use api::{Claim, EmitterId, IqClip, Note, RecordId, RecordQuery, Role, SegmentId, SignalRecord, SourceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::records::{EmitterState, Loaded};

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS records(
  id INTEGER PRIMARY KEY, session INTEGER NOT NULL, source INTEGER NOT NULL, serial TEXT NOT NULL, role TEXT NOT NULL,
  segment INTEGER NOT NULL, tuner_hz REAL NOT NULL, first_sample INTEGER NOT NULL, end_sample INTEGER NOT NULL,
  t_start REAL NOT NULL, t_end REAL NOT NULL, f_lo_hz REAL NOT NULL, f_hi_hz REAL NOT NULL, bin_hz REAL NOT NULL,
  peak_dbfs REAL NOT NULL, mean_dbfs REAL NOT NULL, mean_snr_db REAL NOT NULL, max_snr_db REAL NOT NULL, rows INTEGER NOT NULL,
  emitter INTEGER, clip_path TEXT, clip_from INTEGER, clip_samples INTEGER, clip_rate REAL, clip_center_hz REAL,
  slice_evicted INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS records_t_end ON records(t_end);
CREATE INDEX IF NOT EXISTS records_emitter ON records(emitter);
CREATE INDEX IF NOT EXISTS records_freq ON records(f_lo_hz, f_hi_hz);
CREATE TABLE IF NOT EXISTS claims(
  record INTEGER NOT NULL, ord INTEGER NOT NULL, label TEXT NOT NULL, confidence REAL NOT NULL, kind TEXT NOT NULL,
  evidence TEXT NOT NULL, PRIMARY KEY(record, ord));
CREATE TABLE IF NOT EXISTS emitters(
  id INTEGER PRIMARY KEY, serial TEXT NOT NULL, center_hz REAL NOT NULL, bandwidth_hz REAL NOT NULL, bin_hz REAL NOT NULL,
  first_seen REAL NOT NULL, last_seen REAL NOT NULL, records INTEGER NOT NULL, solid_records INTEGER NOT NULL,
  on_time_s REAL NOT NULL, confirmed INTEGER NOT NULL, peak_snr_db REAL NOT NULL, station INTEGER NOT NULL, headline TEXT);
CREATE INDEX IF NOT EXISTS emitters_last_seen ON emitters(last_seen);
CREATE TABLE IF NOT EXISTS record_tags(record INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(record, tag));
CREATE TABLE IF NOT EXISTS record_notes(id INTEGER PRIMARY KEY, record INTEGER NOT NULL, time REAL NOT NULL, text TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS record_notes_record ON record_notes(record);
"#;

pub fn secs(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

pub fn time(s: f64) -> SystemTime {
    if s >= 0.0 { UNIX_EPOCH + Duration::from_secs_f64(s) } else { UNIX_EPOCH - Duration::from_secs_f64(-s) }
}

/// Opens (creating if needed) the file, refuses a newer schema, and starts a session; returns its id.
pub fn open(path: &std::path::Path) -> Result<(Connection, i64), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    let have: Option<String> =
        conn.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
    match have.as_deref().map(str::parse::<i64>) {
        None => {
            conn.execute("INSERT INTO meta(key, value) VALUES ('schema', ?1)", [SCHEMA_VERSION.to_string()]).map_err(|e| e.to_string())?;
        }
        Some(Ok(v)) if v > SCHEMA_VERSION => {
            return Err(format!("{}: schema {v} is newer than this build's {SCHEMA_VERSION}", path.display()));
        }
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(format!("{}: bad schema version: {e}", path.display())),
    }
    conn.execute("INSERT INTO sessions DEFAULT VALUES", []).map_err(|e| e.to_string())?;
    let session = conn.last_insert_rowid();
    Ok((conn, session))
}

fn meta_u64(conn: &Connection, key: &str) -> rusqlite::Result<u64> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?;
    Ok(v.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// The newest `window` records (with claims, tags and notes), the emitters seen within `emitter_window` of the newest
/// one, and the next ids (never below any id stored).
pub fn load(conn: &Connection, window: usize, emitter_window: Duration) -> rusqlite::Result<Loaded> {
    let ids: Vec<i64> = {
        let mut q = conn.prepare("SELECT id FROM records ORDER BY t_end DESC, id DESC LIMIT ?1")?;
        q.query_map([window as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let records = records_by_id(conn, &ids)?;
    let newest: Option<f64> = conn.query_row("SELECT MAX(last_seen) FROM emitters", [], |r| r.get(0))?;
    let emitters = match newest {
        Some(n) => {
            let mut q = conn.prepare(
                "SELECT id, serial, center_hz, bandwidth_hz, bin_hz, first_seen, last_seen, records, solid_records, on_time_s,
                        confirmed, peak_snr_db, station, headline FROM emitters WHERE last_seen >= ?1",
            )?;
            q.query_map([n - emitter_window.as_secs_f64()], |r| {
                let headline: Option<String> = r.get(13)?;
                Ok(EmitterState {
                    id: EmitterId(r.get::<_, i64>(0)? as u64),
                    serial: r.get::<_, String>(1)?.into(),
                    center_hz: r.get(2)?,
                    bandwidth_hz: r.get(3)?,
                    bin_hz: r.get(4)?,
                    first_seen: time(r.get(5)?),
                    last_seen: time(r.get(6)?),
                    records: r.get::<_, i64>(7)? as u32,
                    solid_records: r.get::<_, i64>(8)? as u32,
                    on_time_s: r.get(9)?,
                    confirmed: r.get::<_, i64>(10)? != 0,
                    peak_snr_db: r.get::<_, f64>(11)? as f32,
                    station: r.get::<_, i64>(12)? != 0,
                    headline: headline.and_then(|h| serde_json::from_str::<Claim>(&h).ok()),
                })
            })?
            .collect::<rusqlite::Result<_>>()?
        }
        None => Vec::new(),
    };
    let max_rec: Option<i64> = conn.query_row("SELECT MAX(id) FROM records", [], |r| r.get(0))?;
    let max_em: Option<i64> = conn.query_row("SELECT MAX(id) FROM emitters", [], |r| r.get(0))?;
    Ok(Loaded {
        records,
        emitters,
        next_record_id: meta_u64(conn, "next_record_id")?.max(max_rec.map_or(1, |m| m as u64 + 1)),
        next_emitter_id: meta_u64(conn, "next_emitter_id")?.max(max_em.map_or(1, |m| m as u64 + 1)),
    })
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Live => "live",
        Role::Survey => "survey",
    }
}

fn evidence_kind(c: &Claim) -> &'static str {
    match c.by {
        api::Evidence::SyncBank { .. } => "sync_bank",
        api::Evidence::Features { .. } => "features",
        api::Evidence::Decoder { .. } => "decoder",
        api::Evidence::BandPlan { .. } => "band_plan",
        api::Evidence::Neighbour { .. } => "neighbour",
        api::Evidence::Position(_) => "position",
    }
}

/// Writes (or rewrites) a record and its claims; its tags and notes live in their own tables.
pub fn upsert_record(tx: &rusqlite::Transaction<'_>, session: i64, r: &SignalRecord) -> rusqlite::Result<()> {
    let clip = r.iq_clip.as_ref();
    tx.execute(
        "INSERT OR REPLACE INTO records(id, session, source, serial, role, segment, tuner_hz, first_sample, end_sample, t_start,
           t_end, f_lo_hz, f_hi_hz, bin_hz, peak_dbfs, mean_dbfs, mean_snr_db, max_snr_db, rows, emitter, clip_path, clip_from,
           clip_samples, clip_rate, clip_center_hz, slice_evicted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        params![
            r.id.0 as i64,
            session,
            r.source.0,
            &*r.serial,
            role_name(r.role),
            r.segment.0 as i64,
            r.tuner_hz,
            r.first_sample as i64,
            r.end_sample as i64,
            secs(r.t_start),
            secs(r.t_end),
            r.f_lo_hz,
            r.f_hi_hz,
            r.bin_hz,
            r.peak_dbfs as f64,
            r.mean_dbfs as f64,
            r.mean_snr_db as f64,
            r.max_snr_db as f64,
            r.rows,
            r.emitter.map(|e| e.0 as i64),
            clip.map(|c| c.path.to_string_lossy().into_owned()),
            clip.map(|c| c.from_sample as i64),
            clip.map(|c| c.samples as i64),
            clip.map(|c| c.rate),
            clip.map(|c| c.center_hz),
            r.slice_evicted,
        ],
    )?;
    tx.execute("DELETE FROM claims WHERE record = ?1", [r.id.0 as i64])?;
    for (k, c) in r.claims.iter().enumerate() {
        let evidence = serde_json::to_string(&c.by).expect("evidence serializes");
        tx.execute(
            "INSERT INTO claims(record, ord, label, confidence, kind, evidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![r.id.0 as i64, k as i64, c.label, c.confidence as f64, evidence_kind(c), evidence],
        )?;
    }
    Ok(())
}

pub fn upsert_emitter(tx: &rusqlite::Transaction<'_>, e: &EmitterState) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO emitters(id, serial, center_hz, bandwidth_hz, bin_hz, first_seen, last_seen, records, solid_records,
           on_time_s, confirmed, peak_snr_db, station, headline)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            e.id.0 as i64,
            &*e.serial,
            e.center_hz,
            e.bandwidth_hz,
            e.bin_hz,
            secs(e.first_seen),
            secs(e.last_seen),
            e.records,
            e.solid_records,
            e.on_time_s,
            e.confirmed,
            e.peak_snr_db as f64,
            e.station,
            e.headline.as_ref().map(|h| serde_json::to_string(h).expect("a claim serializes")),
        ],
    )?;
    Ok(())
}

pub fn fold_emitter(tx: &rusqlite::Transaction<'_>, gone: EmitterId, into: EmitterId) -> rusqlite::Result<()> {
    tx.execute("UPDATE records SET emitter = ?2 WHERE emitter = ?1", params![gone.0 as i64, into.0 as i64])?;
    tx.execute("DELETE FROM emitters WHERE id = ?1", [gone.0 as i64])?;
    Ok(())
}

pub fn tag(tx: &rusqlite::Transaction<'_>, record: RecordId, tag: &str, on: bool) -> rusqlite::Result<()> {
    if on {
        tx.execute("INSERT OR IGNORE INTO record_tags(record, tag) VALUES (?1, ?2)", params![record.0 as i64, tag])?;
    } else {
        tx.execute("DELETE FROM record_tags WHERE record = ?1 AND tag = ?2", params![record.0 as i64, tag])?;
    }
    Ok(())
}

pub fn note(tx: &rusqlite::Transaction<'_>, record: RecordId, n: &Note) -> rusqlite::Result<()> {
    tx.execute("INSERT INTO record_notes(record, time, text) VALUES (?1, ?2, ?3)", params![record.0 as i64, secs(n.time), n.text])?;
    Ok(())
}

pub fn next_ids(tx: &rusqlite::Transaction<'_>, record: u64, emitter: u64) -> rusqlite::Result<()> {
    for (k, v) in [("next_record_id", record), ("next_emitter_id", emitter)] {
        // Never lower a counter: a late batch from before a restart can't hand ids out twice.
        let have = meta_u64(tx, k)?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", params![k, have.max(v).to_string()])?;
    }
    Ok(())
}

/// Records by id with their claims, tags and notes, in the order given.
pub fn records_by_id(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<SignalRecord>> {
    let mut rq = conn.prepare(
        "SELECT id, source, serial, role, segment, tuner_hz, first_sample, end_sample, t_start, t_end, f_lo_hz, f_hi_hz, bin_hz,
                peak_dbfs, mean_dbfs, mean_snr_db, max_snr_db, rows, emitter, clip_path, clip_from, clip_samples, clip_rate,
                clip_center_hz, slice_evicted FROM records WHERE id = ?1",
    )?;
    let mut cq = conn.prepare("SELECT label, confidence, evidence FROM claims WHERE record = ?1 ORDER BY ord")?;
    let mut tq = conn.prepare("SELECT tag FROM record_tags WHERE record = ?1 ORDER BY rowid")?;
    let mut nq = conn.prepare("SELECT time, text FROM record_notes WHERE record = ?1 ORDER BY id")?;
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        let Some(mut r) = rq
            .query_row([id], |r| {
                let clip_path: Option<String> = r.get(19)?;
                let iq_clip = match clip_path {
                    Some(p) => Some(IqClip {
                        path: PathBuf::from(p),
                        from_sample: r.get::<_, i64>(20)? as u64,
                        samples: r.get::<_, i64>(21)? as u64,
                        rate: r.get(22)?,
                        center_hz: r.get(23)?,
                    }),
                    None => None,
                };
                Ok(SignalRecord {
                    id: RecordId(r.get::<_, i64>(0)? as u64),
                    source: SourceId(r.get(1)?),
                    serial: Arc::from(r.get::<_, String>(2)?),
                    role: if r.get::<_, String>(3)? == "survey" { Role::Survey } else { Role::Live },
                    segment: SegmentId(r.get::<_, i64>(4)? as u64),
                    tuner_hz: r.get(5)?,
                    first_sample: r.get::<_, i64>(6)? as u64,
                    end_sample: r.get::<_, i64>(7)? as u64,
                    t_start: time(r.get(8)?),
                    t_end: time(r.get(9)?),
                    f_lo_hz: r.get(10)?,
                    f_hi_hz: r.get(11)?,
                    bin_hz: r.get(12)?,
                    peak_dbfs: r.get::<_, f64>(13)? as f32,
                    mean_dbfs: r.get::<_, f64>(14)? as f32,
                    mean_snr_db: r.get::<_, f64>(15)? as f32,
                    max_snr_db: r.get::<_, f64>(16)? as f32,
                    rows: r.get(17)?,
                    open: false,
                    claims: vec![],
                    emitter: r.get::<_, Option<i64>>(18)?.map(|e| EmitterId(e as u64)),
                    tags: vec![],
                    notes: vec![],
                    iq_clip,
                    slice_evicted: r.get(24)?,
                })
            })
            .optional()?
        else {
            continue;
        };
        r.claims = cq
            .query_map([id], |c| {
                let evidence: String = c.get(2)?;
                Ok((c.get::<_, String>(0)?, c.get::<_, f64>(1)?, evidence))
            })?
            .filter_map(|c| c.ok())
            .filter_map(|(label, confidence, evidence)| {
                serde_json::from_str(&evidence).ok().map(|by| Claim { label, confidence: confidence as f32, by })
            })
            .collect();
        r.tags = tq.query_map([id], |t| t.get(0))?.collect::<rusqlite::Result<_>>()?;
        r.notes = nq.query_map([id], |n| Ok(Note { time: time(n.get(0)?), text: n.get(1)? }))?.collect::<rusqlite::Result<_>>()?;
        out.push(r);
    }
    Ok(out)
}

/// Most a query returns.
pub const QUERY_MAX: usize = 5000;

/// The records matching `q`, newest first, and whether more matched than its limit.
pub fn query(conn: &Connection, q: &RecordQuery) -> rusqlite::Result<(Vec<SignalRecord>, bool)> {
    let mut sql = String::from("SELECT id FROM records r WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut arg = |sql: &mut String, cond: &str, v: rusqlite::types::Value| {
        args.push(v);
        sql.push_str(&cond.replace("?", &format!("?{}", args.len())));
    };
    use rusqlite::types::Value as V;
    if let Some(s) = &q.serial {
        arg(&mut sql, " AND r.serial = ?", V::Text(s.clone()));
    }
    if let Some(t) = q.from {
        arg(&mut sql, " AND r.t_end >= ?", V::Real(secs(t)));
    }
    if let Some(t) = q.to {
        arg(&mut sql, " AND r.t_start <= ?", V::Real(secs(t)));
    }
    if let Some(f) = q.f_lo_hz {
        arg(&mut sql, " AND r.f_hi_hz >= ?", V::Real(f));
    }
    if let Some(f) = q.f_hi_hz {
        arg(&mut sql, " AND r.f_lo_hz <= ?", V::Real(f));
    }
    if let Some(t) = &q.tag {
        arg(&mut sql, " AND EXISTS (SELECT 1 FROM record_tags t WHERE t.record = r.id AND t.tag = ?)", V::Text(t.clone()));
    }
    for word in q.text.iter().flat_map(|t| t.split_whitespace()) {
        let like = format!("%{}%", word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        args.push(V::Text(like));
        let n = args.len();
        sql.push_str(&format!(
            " AND (EXISTS (SELECT 1 FROM claims c WHERE c.record = r.id AND c.label LIKE ?{n} ESCAPE '\\')
               OR EXISTS (SELECT 1 FROM record_tags t WHERE t.record = r.id AND t.tag LIKE ?{n} ESCAPE '\\')
               OR EXISTS (SELECT 1 FROM record_notes o WHERE o.record = r.id AND o.text LIKE ?{n} ESCAPE '\\'))"
        ));
    }
    let limit = q.limit.clamp(1, QUERY_MAX);
    sql.push_str(&format!(" ORDER BY r.t_end DESC, r.id DESC LIMIT {}", limit + 1));
    let mut stmt = conn.prepare(&sql)?;
    let mut ids: Vec<i64> = stmt.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let truncated = ids.len() > limit;
    ids.truncate(limit);
    Ok((records_by_id(conn, &ids)?, truncated))
}
