//! Classifier thread for a live source: pulls each record's channel out of
//! the look-back ring, down-converts it, and runs `detect::classify`.
//!
//! Priorities: newly closed records first (their IQ will age out of the
//! ring), then open records that have no class yet or one older than
//! `REFRESH`. Records whose IQ has already left the ring are marked so they
//! aren't retried.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use detect::{Classification, Ddc};
use sdr_core::Complex32;

use crate::{Shared, SignalRecord, SourceConfig};

/// Re-classify an open record this often (less often once identified).
const REFRESH: Duration = Duration::from_secs(8);
const REFRESH_IDENTIFIED: Duration = Duration::from_secs(30);
const REFRESH_SHORT: Duration = Duration::from_secs(2);
/// At most this much signal is analysed per record.
const MAX_WINDOW_S: f64 = 1.5;
/// Records wider than this get a label from bandwidth alone.
const MAX_DDC_BW_HZ: f64 = 1.0e6;

/// Fraction of one core the classifier may use. Under heavy traffic it
/// rests between jobs instead of pinning a core; results just arrive later.
const BUDGET: f64 = 0.6;

#[derive(Debug, Clone, Copy, Default)]
pub struct ClassifierLoad {
    pub jobs_per_sec: f64,
    pub avg_job_ms: f64,
    /// Fraction of wall time spent working (≤ BUDGET when saturated).
    pub busy: f64,
}

pub(crate) fn run(shared: Arc<Shared>, cfg: SourceConfig, stop: Arc<AtomicBool>, rate: f64) {
    let mut chunk = vec![Complex32::default(); 1 << 18];
    let (mut win_start, mut jobs, mut work) = (Instant::now(), 0u32, Duration::ZERO);
    while !stop.load(Ordering::Relaxed) {
        let el = win_start.elapsed();
        if el >= Duration::from_secs(2) {
            *shared.classifier_load.lock() = ClassifierLoad {
                jobs_per_sec: jobs as f64 / el.as_secs_f64(),
                avg_job_ms: if jobs > 0 { work.as_secs_f64() * 1e3 / jobs as f64 } else { 0.0 },
                busy: work.as_secs_f64() / el.as_secs_f64(),
            };
            (win_start, jobs, work) = (Instant::now(), 0, Duration::ZERO);
        }
        let t0 = Instant::now();
        if let Some(rec) = pick(&shared) {
            let class = Arc::new(classify_record(&shared, &cfg, rate, &rec, &mut chunk, &stop));
            let mut st = shared.records.lock();
            st.emitters.tally(rec.id, &class);
            st.classes.insert(rec.id, (class, Instant::now(), rec.active));
        } else if let Some(job) = pick_emitter(&shared, rate) {
            let class = Arc::new(classify_window(&shared, rate, &job, &mut chunk, &stop));
            shared.records.lock().emitters.set_group_class(job.emitter, class);
        } else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let spent = t0.elapsed();
        jobs += 1;
        work += spent;
        // Rest so that work / (work + rest) stays within the budget.
        std::thread::sleep(spent.mul_f64(1.0 / BUDGET - 1.0));
    }
}

fn pick(shared: &Shared) -> Option<SignalRecord> {
    let st = shared.records.lock();
    // A closed record needs a (final) look if it was never classified, or
    // only while open, when it may have held too little signal.
    let fresh = |r: &SignalRecord| st.classes.get(&r.id).is_none_or(|c| c.2);
    if let Some(r) = st.closed.iter().rev().take(200).find(|r| fresh(r)) {
        return Some(r.clone());
    }
    st.open
        .iter()
        // Let an open record build up ~0.25 s of signal before a first look.
        .filter(|r| r.rows >= 12)
        // Identified records need re-checking far less often.
        .filter(|r| {
            st.classes.get(&r.id).is_none_or(|c| {
                // A first look made on a short window is redone soon, on a full one.
                let wait = if c.0.analysed_s < MAX_WINDOW_S * 0.9 {
                    REFRESH_SHORT
                } else if c.0.protocol.is_some() {
                    REFRESH_IDENTIFIED
                } else {
                    REFRESH
                };
                c.1.elapsed() > wait
            })
        })
        // Unclassified first, then the stalest.
        .min_by_key(|r| st.classes.get(&r.id).map(|c| c.1))
        .cloned()
}

/// A window of IQ to classify, independent of record boundaries.
struct Window {
    emitter: u64,
    start: u64,
    end: u64,
    center_hz: f64,
    bandwidth_hz: f64,
    tuner_center_hz: f64,
}

/// First retry interval for an unidentified emitter's group window; doubles
/// after each attempt that finds nothing, up to `GROUP_REFRESH_MAX`.
const GROUP_REFRESH: Duration = Duration::from_secs(10);
const GROUP_REFRESH_MAX: Duration = Duration::from_secs(160);
/// Group windows: long enough for two syncs of every supported protocol
/// (P25 frames are 180 ms apart, NXDN48 80 ms, DMR 30-360 ms).
const GROUP_WINDOW_S: f64 = 0.75;
/// Records shorter than this can't hold two syncs; their emitter's group
/// window covers them instead of a DDC pass each.
const MIN_RECORD_S: f64 = 0.06;

/// Emitters still unidentified despite recent activity: their records were
/// individually too short (TDMA slots, short bursts), so classify a window
/// spanning several of them.
fn pick_emitter(shared: &Shared, rate: f64) -> Option<Window> {
    let st = shared.records.lock();
    let now = std::time::SystemTime::now();
    st.emitters
        .map
        .values()
        .filter(|e| !e.has_protocol() && e.records >= 3)
        .filter(|e| now.duration_since(e.last_seen).map(|d| d.as_secs_f64() < 3.0).unwrap_or(true))
        .filter(|e| e.class_at.is_none_or(|t| t.elapsed() > e.group_backoff(GROUP_REFRESH, GROUP_REFRESH_MAX)))
        .min_by_key(|e| e.class_at)
        .map(|e| Window {
            emitter: e.id,
            start: e.last_end_sample.saturating_sub((GROUP_WINDOW_S * rate) as u64),
            end: e.last_end_sample,
            center_hz: e.center_hz,
            bandwidth_hz: e.bandwidth_hz,
            tuner_center_hz: e.tuner_center_hz,
        })
}

fn label_only(label: &str) -> Classification {
    Classification {
        label: label.into(),
        protocol: None,
        confidence: 0.0,
        features: Default::default(),
        sync: None,
        fields: None,
        analysed_s: 0.0,
    }
}

fn classify_record(
    shared: &Shared,
    cfg: &SourceConfig,
    rate: f64,
    r: &SignalRecord,
    chunk: &mut [Complex32],
    stop: &AtomicBool,
) -> Classification {
    if r.duration_s() < MIN_RECORD_S && !r.active {
        return label_only("too short");
    }
    // The first row's FFTs started roughly one row before its end sample.
    let per_row = (rate / cfg.rows_per_sec) as u64;
    let end = r.last_row_end_sample;
    let w = Window {
        emitter: 0,
        start: r.first_row_end_sample.saturating_sub(per_row).max(end.saturating_sub((MAX_WINDOW_S * rate) as u64)),
        end,
        center_hz: r.center_hz(),
        bandwidth_hz: r.bandwidth_hz(),
        tuner_center_hz: r.tuner_center_hz,
    };
    classify_window(shared, rate, &w, chunk, stop)
}

fn classify_window(shared: &Shared, rate: f64, w: &Window, chunk: &mut [Complex32], stop: &AtomicBool) -> Classification {
    let bw = w.bandwidth_hz.max(5e3);
    if bw > MAX_DDC_BW_HZ {
        return label_only("Wideband");
    }
    let (start, end) = (w.start, w.end);
    let oldest = shared.ring.lock().oldest();
    let start = start.max(oldest);
    if end <= start + (0.02 * rate) as u64 {
        return label_only(if end <= oldest { "IQ expired" } else { "too short" });
    }

    let offset = w.center_hz - w.tuner_center_hz;
    let mut ddc = Ddc::new(rate, offset, bw * 0.65 + 1e3, (bw * 2.5).max(48e3));
    let mut chan = Vec::with_capacity(((end - start) as f64 / rate * ddc.fs_out) as usize + 16);
    let mut pos = start;
    while pos < end {
        if stop.load(Ordering::Relaxed) {
            return label_only("interrupted");
        }
        let n = ((end - pos) as usize).min(chunk.len());
        // One lock at a time; copy out and release before processing.
        let ok = shared.ring.lock().read(pos, &mut chunk[..n]);
        if !ok {
            break;
        }
        ddc.process(&chunk[..n], &mut chan);
        pos += n as u64;
    }
    if chan.is_empty() {
        return label_only("IQ expired");
    }
    detect::classify(&chan, ddc.fs_out, w.bandwidth_hz)
}
