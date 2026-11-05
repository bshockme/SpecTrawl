//! The record store: SQLite, written by one thread off a bounded queue (spec §4.8).
//!
//! The engine never waits for it: the record book hands its lasting changes to [`Store`] (its [`RecordSink`]), which
//! queues them; a full queue drops the change and counts it. The "store" thread drains the queue into one transaction
//! per batch, answers history queries, and on close writes what is queued before it stops. At open the newest records,
//! the recently seen emitters and the id counters are loaded for the book, so records and their ids survive a restart.

mod schema;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use api::{EmitterId, Note, QueryResult, RecordId, RecordQuery, SignalRecord, StoreView};
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::records::{EmitterState, Loaded, RecordSink};

pub use schema::{QUERY_MAX, SCHEMA_VERSION};

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub path: PathBuf,
    /// Operations queued at most; beyond that they are dropped and counted.
    pub queue: usize,
    /// The writer commits at least this often while operations arrive.
    pub batch_every: Duration,
    /// How long closing waits for the queue to be written.
    pub shutdown: Duration,
    /// Closed records loaded at open.
    pub window: usize,
    /// Emitters loaded at open: those seen within this of the newest.
    pub emitter_window: Duration,
    /// Test only: each batch takes this much longer.
    #[doc(hidden)]
    pub write_delay: Duration,
}

/// `<Documents>/SpecTrawl/records.db` (the old app's `spectrawl.db` beside it has another schema).
pub fn default_path() -> PathBuf {
    let home =
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("SpecTrawl").join("records.db")
}

impl StoreConfig {
    pub fn new(path: impl Into<PathBuf>) -> StoreConfig {
        StoreConfig {
            path: path.into(),
            queue: 4096,
            batch_every: Duration::from_millis(250),
            shutdown: Duration::from_secs(2),
            window: crate::records::WINDOW,
            emitter_window: crate::records::EMITTER_WINDOW,
            write_delay: Duration::ZERO,
        }
    }
}

enum Op {
    Record(Box<SignalRecord>),
    Emitter(Box<EmitterState>),
    Folded(EmitterId, EmitterId),
    Tag(RecordId, String, bool),
    Note(RecordId, Note),
    NextIds(u64, u64),
    Query(u64, RecordQuery),
    /// Write what is queued, then signal.
    Flush(mpsc::Sender<()>),
}

struct Shared {
    written: AtomicU64,
    queued: AtomicUsize,
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
    query: Mutex<Option<Arc<QueryResult>>>,
}

pub struct Store {
    path: PathBuf,
    tx: Mutex<Option<SyncSender<Op>>>,
    shared: Arc<Shared>,
    thread: Mutex<Option<(JoinHandle<()>, mpsc::Receiver<()>)>>,
    shutdown: Duration,
}

impl Store {
    /// Opens the file (creating it and its directory), loads what the book starts from, and starts the "store" thread.
    pub fn open(cfg: StoreConfig) -> Result<(Arc<Store>, Loaded), String> {
        let (conn, session) = schema::open(&cfg.path)?;
        let loaded = schema::load(&conn, cfg.window, cfg.emitter_window).map_err(|e| format!("{}: {e}", cfg.path.display()))?;
        let (tx, rx) = mpsc::sync_channel(cfg.queue.max(1));
        let shared = Arc::new(Shared {
            written: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            error: Mutex::new(None),
            query: Mutex::new(None),
        });
        let (done_tx, done_rx) = mpsc::channel();
        let writer = Writer { conn, session, shared: shared.clone(), batch_every: cfg.batch_every, write_delay: cfg.write_delay };
        let thread = std::thread::Builder::new()
            .name("store".into())
            .spawn(move || {
                let _done = done_tx;
                writer.run(rx);
            })
            .map_err(|e| format!("start the store thread: {e}"))?;
        let store = Arc::new(Store {
            path: cfg.path,
            tx: Mutex::new(Some(tx)),
            shared,
            thread: Mutex::new(Some((thread, done_rx))),
            shutdown: cfg.shutdown,
        });
        Ok((store, loaded))
    }

    pub fn view(&self) -> StoreView {
        StoreView {
            path: self.path.clone(),
            written: self.shared.written.load(Ordering::Relaxed),
            queued: self.shared.queued.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            error: self.shared.error.lock().clone(),
        }
    }

    /// The latest query's answer.
    pub fn query_result(&self) -> Option<Arc<QueryResult>> {
        self.shared.query.lock().clone()
    }

    /// Queues a history search, answered by [`Store::query_result`] with `seq`.
    pub fn query(&self, seq: u64, q: RecordQuery) -> Result<(), String> {
        self.send(Op::Query(seq, q)).then_some(()).ok_or_else(|| "the store's queue is full".to_string())
    }

    /// Waits until everything queued so far is written (tests, and closing).
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        let Some(sender) = self.tx.lock().clone() else { return false };
        // A blocking send: a flush must not be dropped by a full queue.
        if sender.send(Op::Flush(tx)).is_err() {
            return false;
        }
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        rx.recv_timeout(timeout).is_ok()
    }

    /// Writes what is queued (waiting up to the configured shutdown time) and stops the thread. Idempotent; later
    /// changes are dropped and counted.
    pub fn close(&self) {
        let Some(tx) = self.tx.lock().take() else { return };
        drop(tx);
        if let Some((thread, done)) = self.thread.lock().take() {
            match done.recv_timeout(self.shutdown) {
                Err(RecvTimeoutError::Timeout) => log::warn!("store: still writing after {:?}; left to finish on its own", self.shutdown),
                _ => {
                    if thread.join().is_err() {
                        log::error!("the store thread panicked");
                    }
                }
            }
        }
    }

    fn send(&self, op: Op) -> bool {
        let tx = self.tx.lock();
        let Some(tx) = tx.as_ref() else {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        match tx.try_send(op) {
            Ok(()) => {
                self.shared.queued.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.close();
    }
}

impl RecordSink for Store {
    fn record(&self, r: &SignalRecord) {
        self.send(Op::Record(Box::new(r.clone())));
    }
    fn emitter(&self, e: &EmitterState) {
        self.send(Op::Emitter(Box::new(e.clone())));
    }
    fn emitter_folded(&self, gone: EmitterId, into: EmitterId) {
        self.send(Op::Folded(gone, into));
    }
    fn tag(&self, record: RecordId, tag: &str, on: bool) {
        self.send(Op::Tag(record, tag.to_string(), on));
    }
    fn note(&self, record: RecordId, note: &Note) {
        self.send(Op::Note(record, note.clone()));
    }
    fn next_ids(&self, record: u64, emitter: u64) {
        self.send(Op::NextIds(record, emitter));
    }
}

struct Writer {
    conn: Connection,
    session: i64,
    shared: Arc<Shared>,
    batch_every: Duration,
    write_delay: Duration,
}

impl Writer {
    /// Until every sender is gone: wait for an operation, gather what else arrives within `batch_every`, write it in one
    /// transaction. A query or a flush is answered after the batch it arrived in is written.
    fn run(mut self, rx: mpsc::Receiver<Op>) {
        let mut batch: Vec<Op> = Vec::new();
        let mut ids: Option<(u64, u64)> = None;
        'run: loop {
            match rx.recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
            let until = Instant::now() + self.batch_every;
            loop {
                let flush = batch.iter().any(|o| matches!(o, Op::Flush(_) | Op::Query(..)));
                let left = until.saturating_duration_since(Instant::now());
                if flush || left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(op) => batch.push(op),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.write(&mut batch, &mut ids);
                        break 'run;
                    }
                }
            }
            self.write(&mut batch, &mut ids);
        }
        self.write(&mut batch, &mut ids);
    }

    fn write(&mut self, batch: &mut Vec<Op>, ids: &mut Option<(u64, u64)>) {
        if batch.is_empty() {
            return;
        }
        if !self.write_delay.is_zero() {
            std::thread::sleep(self.write_delay);
        }
        let n = batch.len();
        let mut answers: Vec<Op> = Vec::new();
        let result = (|| -> rusqlite::Result<()> {
            let tx = self.conn.transaction()?;
            for op in batch.drain(..) {
                match op {
                    Op::Record(r) => schema::upsert_record(&tx, self.session, &r)?,
                    Op::Emitter(e) => schema::upsert_emitter(&tx, &e)?,
                    Op::Folded(g, i) => schema::fold_emitter(&tx, g, i)?,
                    Op::Tag(r, t, on) => schema::tag(&tx, r, &t, on)?,
                    Op::Note(r, note) => schema::note(&tx, r, &note)?,
                    Op::NextIds(r, e) => *ids = Some(ids.map_or((r, e), |(a, b)| (a.max(r), b.max(e)))),
                    other @ (Op::Query(..) | Op::Flush(_)) => answers.push(other),
                }
            }
            if let Some((r, e)) = ids.take() {
                schema::next_ids(&tx, r, e)?;
            }
            tx.commit()
        })();
        let _ = self.shared.queued.try_update(Ordering::Relaxed, Ordering::Relaxed, |q| Some(q.saturating_sub(n)));
        match result {
            Ok(()) => {
                self.shared.written.fetch_add(n as u64, Ordering::Relaxed);
                *self.shared.error.lock() = None;
            }
            Err(e) => {
                log::error!("store: a batch of {n} failed: {e}");
                *self.shared.error.lock() = Some(e.to_string());
            }
        }
        for a in answers {
            match a {
                Op::Query(seq, q) => {
                    let result = match schema::query(&self.conn, &q) {
                        Ok((records, truncated)) => QueryResult { seq, records: records.into_iter().map(Arc::new).collect(), truncated },
                        Err(e) => {
                            log::error!("store: query {seq} failed: {e}");
                            QueryResult { seq, records: vec![], truncated: false }
                        }
                    };
                    *self.shared.query.lock() = Some(Arc::new(result));
                }
                Op::Flush(done) => {
                    let _ = done.send(());
                }
                _ => unreachable!("only queries and flushes are answers"),
            }
        }
    }
}
