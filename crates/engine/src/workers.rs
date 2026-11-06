//! Decoder workers (spec §4.6): separate programs that read rtl_tcp (rtl_433, AIS-catcher, OnAir's `onair-server`),
//! fed a channel of a source by [`crate::iqserve`] and polled for what they decode (`docs/decoder-worker.md`). The
//! engine knows workers only through [`WorkerFactory`] and [`Worker`]; a worker's own crate implements them, and the app
//! hands its factories to the engine ([`crate::Engine::add_worker_factory`]), so the engine never depends on a worker's
//! crate.
//!
//! A manager thread polls every worker each [`POLL`]. Its view goes into the snapshot. Each message it decoded becomes
//! a `Decoder` claim, filed under `worker:<mode>`, on the record nearest the message's frequency: an open one, or one
//! closed within [`MESSAGE_RECORD`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use api::{Claim, Evidence, Lamp, RecordId, WorkerId, WorkerMessage, WorkerMode, WorkerView};
use parking_lot::Mutex;

use crate::budget::SourceId;
use crate::iqserve::{IqServeConfig, IqServer};
use crate::records::RecordBook;
use crate::source::Source;

/// How often the manager polls each worker.
pub const POLL: Duration = Duration::from_millis(500);
/// A message lands on a record that was open then, or closed at most this long before it.
pub const MESSAGE_RECORD: Duration = Duration::from_secs(10);
/// Messages kept per worker for the view.
const MESSAGES_KEPT: usize = 50;
/// How sure a worker's decoded message makes us of what the record is.
pub const MESSAGE_CONFIDENCE: f32 = 0.95;

/// What a worker is asked to do.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub mode: String,
    pub center_hz: f64,
    pub rate: f64,
    /// Where its rtl_tcp feed listens.
    pub iq: std::net::SocketAddr,
}

/// What a worker says when polled.
#[derive(Debug, Clone, Default)]
pub struct WorkerReport {
    pub lamp: Option<Lamp>,
    pub summary: String,
    pub blocks_ok: u64,
    pub blocks_bad: u64,
    /// New since the last poll.
    pub messages: Vec<WorkerMessage>,
    /// Why it is not running, if it is not.
    pub error: Option<String>,
    pub restarts: u32,
}

/// One running worker. `poll` must return promptly (a local request or two).
pub trait Worker: Send {
    fn poll(&mut self) -> WorkerReport;
}

/// A program that offers modes, and starts a worker for one.
pub trait WorkerFactory: Send + Sync {
    /// "OnAir 3.6.0".
    fn name(&self) -> String;
    fn modes(&self) -> Vec<WorkerMode>;
    fn start(&self, spec: &WorkerSpec) -> Result<Box<dyn Worker>, String>;
}

/// The largest rate at most `want` that the channelizer gives exactly at `fs` (`fs·K/N`, `K` a power of two from 64 to
/// `N/4`), or `None` if even the smallest is more.
pub fn exact_rate(fs: f64, want: f64) -> Option<f64> {
    let n = dsp::channelizer::FFT_SIZE;
    (6..=n.trailing_zeros() - 2).rev().map(|b| fs * (1usize << b) as f64 / n as f64).find(|r| *r <= want * (1.0 + 1e-9))
}

struct Running {
    id: WorkerId,
    source: Arc<Source>,
    mode: String,
    /// The mode's name ("FLEX") and the program serving it.
    name: String,
    by: String,
    /// Declared before the feed, so the worker is dropped (and its process stopped) before the feed it reads.
    worker: Box<dyn Worker>,
    _iq: IqServer,
    view: WorkerView,
    /// Messages this worker put on each record.
    counts: HashMap<RecordId, u32>,
}

#[derive(Default)]
struct Inner {
    factories: Vec<Arc<dyn WorkerFactory>>,
    /// Each behind its own lock, so the manager polls one without holding the map, and a worker stopped meanwhile is
    /// dropped (its process stopped) when the poll lets go of it.
    running: BTreeMap<WorkerId, Arc<Mutex<Running>>>,
    next: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
}

pub struct Workers {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Workers {
    pub(crate) fn start(book: Arc<RecordBook>) -> Workers {
        let shared = Arc::new(Shared { inner: Mutex::new(Inner::default()), stop: AtomicBool::new(false) });
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("workers".into()).spawn(move || manage(s, book)).expect("spawn");
        Workers { shared, thread: Some(thread) }
    }

    pub fn add_factory(&self, f: Arc<dyn WorkerFactory>) {
        self.shared.inner.lock().factories.push(f);
    }

    /// Every mode the factories offer.
    pub fn modes(&self) -> Vec<WorkerMode> {
        self.shared.inner.lock().factories.iter().flat_map(|f| f.modes()).collect()
    }

    /// Serves the channel and starts the factory that offers `mode` on it.
    pub(crate) fn launch(
        &self,
        bus: Arc<crate::channels::ChannelBus>,
        source: Arc<Source>,
        mode: &str,
        center_hz: f64,
    ) -> Result<WorkerId, String> {
        let (factory, m) = {
            let inner = self.shared.inner.lock();
            inner
                .factories
                .iter()
                .find_map(|f| f.modes().into_iter().find(|m| m.id == mode).map(|m| (f.clone(), m)))
                .ok_or_else(|| format!("no installed worker offers {mode:?}"))?
        };
        let fs = source.current_segment().sample_rate;
        let rate =
            exact_rate(fs, m.rate).ok_or_else(|| format!("{mode} wants {:.0} S/s, under the narrowest channel at {fs:.0} S/s", m.rate))?;
        let iq =
            IqServer::start(bus, source.clone(), IqServeConfig { center_hz, rate, priority: crate::channels::Priority::EmitterDecoder })?;
        let spec = WorkerSpec { mode: mode.to_string(), center_hz, rate, iq: iq.addr() };
        let worker = factory.start(&spec)?;
        let mut inner = self.shared.inner.lock();
        inner.next += 1;
        let id = WorkerId(inner.next);
        let view = WorkerView {
            id,
            source: source.id(),
            mode: mode.to_string(),
            by: m.by.clone(),
            center_hz,
            rate,
            lamp: Lamp::Searching,
            summary: format!("{} at {:.4} MHz: starting", m.name, center_hz / 1e6),
            blocks_ok: 0,
            blocks_bad: 0,
            messages: Vec::new(),
            error: None,
            restarts: 0,
        };
        log::info!("worker {} ({}) started at {:.4} MHz, {:.0} S/s", mode, m.by, center_hz / 1e6, rate);
        let r = Running { id, source, mode: mode.to_string(), name: m.name, by: m.by, worker, _iq: iq, view, counts: HashMap::new() };
        inner.running.insert(id, Arc::new(Mutex::new(r)));
        Ok(id)
    }

    /// Stops a worker (its process and its feed). False if there was none.
    pub fn stop_worker(&self, id: WorkerId) -> bool {
        let gone = self.shared.inner.lock().running.remove(&id);
        gone.is_some()
    }

    /// Stops every worker on `source` (it is going away).
    pub(crate) fn stop_source(&self, source: SourceId) {
        let gone: Vec<Arc<Mutex<Running>>> = {
            let mut inner = self.shared.inner.lock();
            let ids: Vec<WorkerId> = inner.running.values().map(|r| r.lock()).filter(|r| r.source.id() == source).map(|r| r.id).collect();
            ids.iter().filter_map(|id| inner.running.remove(id)).collect()
        };
        drop(gone);
    }

    pub fn views(&self) -> Vec<WorkerView> {
        let all: Vec<Arc<Mutex<Running>>> = self.shared.inner.lock().running.values().cloned().collect();
        all.iter().map(|r| r.lock().view.clone()).collect()
    }

    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let all = std::mem::take(&mut self.shared.inner.lock().running);
        drop(all);
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.stop();
    }
}

fn manage(shared: Arc<Shared>, book: Arc<RecordBook>) {
    while !shared.stop.load(Ordering::Relaxed) {
        // Poll outside the lock: a worker's poll is a local request, but the lock is the snapshot's too.
        let all: Vec<Arc<Mutex<Running>>> = shared.inner.lock().running.values().cloned().collect();
        for w in all {
            let mut guard = w.lock();
            let r = &mut *guard;
            let rep = r.worker.poll();
            for m in &rep.messages {
                claim_message(&book, r, m);
            }
            let v = &mut r.view;
            if let Some(l) = rep.lamp {
                v.lamp = l;
            }
            if !rep.summary.is_empty() {
                v.summary = rep.summary;
            }
            (v.blocks_ok, v.blocks_bad, v.error, v.restarts) = (rep.blocks_ok, rep.blocks_bad, rep.error, rep.restarts);
            v.messages.extend(rep.messages);
            let n = v.messages.len();
            if n > MESSAGES_KEPT {
                v.messages.drain(..n - MESSAGES_KEPT);
            }
        }
        // In slices, so a stop is seen promptly (the engine's drop joins this thread).
        let t0 = std::time::Instant::now();
        while t0.elapsed() < POLL && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A message as a claim on the record it most likely came from: the one nearest its frequency on the worker's source,
/// within the record's width (at least ±6.25 kHz), open or closed within [`MESSAGE_RECORD`] of the message. The claim
/// is the newest message, with how many the worker has put on that record.
fn claim_message(book: &RecordBook, r: &mut Running, m: &WorkerMessage) {
    let view = book.view();
    let near = |rec: &api::SignalRecord| (rec.center_hz() - m.freq_hz).abs() <= (rec.bandwidth_hz() / 2.0).max(6_250.0);
    let recent =
        |rec: &api::SignalRecord| rec.open || m.time.duration_since(rec.t_end).is_ok_and(|d| d <= MESSAGE_RECORD) || m.time <= rec.t_end;
    let Some(rec) = view
        .records
        .iter()
        .filter(|rec| rec.source == r.source.id() && near(rec) && recent(rec))
        .min_by(|a, b| (a.center_hz() - m.freq_hz).abs().total_cmp(&(b.center_hz() - m.freq_hz).abs()))
    else {
        return;
    };
    let count = r.counts.entry(rec.id).or_insert(0);
    *count += 1;
    let summary = format!("{} ({} message{} from {})", m.text, count, if *count == 1 { "" } else { "s" }, r.by);
    let claim = Claim::new(
        format!("{} ({})", r.name, r.by),
        MESSAGE_CONFIDENCE,
        Evidence::Decoder { mode: r.mode.clone(), locked: true, summary, identity: None },
    );
    book.set_claims(rec.id, &format!("worker:{}", r.mode), vec![claim]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_rates_are_the_channelizers() {
        assert_eq!(exact_rate(10e6, 2.5e6), Some(2.5e6));
        assert_eq!(exact_rate(10e6, 2.4e6), Some(1.25e6));
        assert_eq!(exact_rate(10e6, 1e6), Some(625e3));
        assert_eq!(exact_rate(2.5e6, 2.5e6), Some(625e3), "N/4 at 2.5 MS/s");
        assert_eq!(exact_rate(10e6, 5e3), None);
    }
}
