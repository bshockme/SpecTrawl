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

use crate::store::ClassJob;
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

pub(crate) fn run(shared: Arc<Shared>, cfg: SourceConfig, stop: Arc<AtomicBool>, rate: f64, caps: sdr_core::Capabilities) {
    let radio = Radio {
        spur_grid_hz: caps.spur_grid_hz,
        transfer_tone_hz: caps.transfer_samples.map(|n| rate / n as f64),
        dc_spike: caps.if_arch == sdr_core::IfArchitecture::ZeroIf,
    };
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
        let store = shared.store.lock().clone();
        if let Some(w) = follow_window(&shared, rate) {
            // Trunk following: a granted call's voice channel comes first.
            let class = classify_window(&shared, rate, &radio, &w, &mut chunk, &stop);
            log_call(&shared, w.center_hz, &class);
            if let Some(db) = &store {
                db.classification(&ClassJob {
                    kind: "follow",
                    record_id: None,
                    emitter_id: None,
                    center_hz: w.center_hz,
                    bandwidth_hz: w.bandwidth_hz,
                    tuner_hz: w.tuner_center_hz,
                    window: (w.start, w.end),
                    class: &class,
                    ms: t0.elapsed().as_secs_f64() * 1e3,
                });
            }
        } else if let Some(rec) = pick(&shared) {
            let (class, window) = classify_record(&shared, &cfg, rate, &radio, &rec, &mut chunk, &stop);
            let class = Arc::new(class);
            log::debug!(
                "classify: record {} at {:.4} MHz, {:.1} kHz: {} ({:.0} ms, {:.2} s analysed)",
                rec.id,
                rec.center_hz() / 1e6,
                rec.bandwidth_hz() / 1e3,
                class.label,
                t0.elapsed().as_secs_f64() * 1e3,
                class.analysed_s
            );
            log_call(&shared, rec.center_hz(), &class);
            let mut st = shared.records.lock();
            st.emitters.tally(rec.id, &class);
            let emitter = st.emitters.by_record.get(&rec.id).copied();
            st.classes.insert(rec.id, (class.clone(), Instant::now(), rec.active));
            if let Some(db) = &store {
                db.classification(&ClassJob {
                    kind: "record",
                    record_id: Some(rec.id),
                    emitter_id: emitter,
                    center_hz: rec.center_hz(),
                    bandwidth_hz: rec.bandwidth_hz(),
                    tuner_hz: rec.tuner_center_hz,
                    window,
                    class: &class,
                    ms: t0.elapsed().as_secs_f64() * 1e3,
                });
                if let Some(e) = emitter.and_then(|id| st.emitters.get(id)) {
                    db.emitter(e);
                }
            }
        } else if let Some(job) = pick_emitter(&shared, rate) {
            let class = Arc::new(classify_window(&shared, rate, &radio, &job, &mut chunk, &stop));
            log::debug!(
                "classify: emitter {} group window at {:.4} MHz: {} ({:.0} ms)",
                job.emitter,
                job.center_hz / 1e6,
                class.label,
                t0.elapsed().as_secs_f64() * 1e3
            );
            log_call(&shared, job.center_hz, &class);
            let mut st = shared.records.lock();
            st.emitters.set_group_class(job.emitter, class.clone());
            if let Some(db) = &store {
                db.classification(&ClassJob {
                    kind: "group",
                    record_id: None,
                    emitter_id: Some(job.emitter),
                    center_hz: job.center_hz,
                    bandwidth_hz: job.bandwidth_hz,
                    tuner_hz: job.tuner_center_hz,
                    window: (job.start, job.end),
                    class: &class,
                    ms: t0.elapsed().as_secs_f64() * 1e3,
                });
                if let Some(e) = st.emitters.get(job.emitter) {
                    db.emitter(e);
                }
            }
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

/// How much of a followed voice channel to look at: enough for P25 link
/// control (an LDU1 every 360 ms) or a Phase 2 superframe (360 ms).
const FOLLOW_WINDOW_S: f64 = 1.0;

/// The newest second of a granted call's voice channel, if one is due
/// (see `CallLog::follow`) and inside the tuned span.
fn follow_window(shared: &Shared, rate: f64) -> Option<Window> {
    let tuner = shared.state.lock().center_hz;
    let half = rate * 0.45;
    let f = shared.calls.lock().follow(std::time::SystemTime::now(), tuner - half, tuner + half)?;
    let end = shared.ring.lock().head();
    Some(Window {
        emitter: 0,
        start: end.saturating_sub((FOLLOW_WINDOW_S * rate) as u64),
        end,
        center_hz: f,
        bandwidth_hz: 12_500.0,
        tuner_center_hz: tuner,
    })
}

/// A call named in a classification goes to the call log.
fn log_call(shared: &Shared, center_hz: f64, c: &Classification) {
    if let Some(call) = c.fields.as_ref().and_then(|f| f.call) {
        shared.calls.lock().heard(center_hz, &c.label, call, std::time::SystemTime::now());
    }
}

fn pick(shared: &Shared) -> Option<SignalRecord> {
    let st = shared.records.lock();
    // A record of an emitter already fixed by position (a spur, an image,
    // a comb line) three times over doesn't need another look: those
    // flicker in and out all day and would take most of the classifier's
    // time.
    let settled = |r: &SignalRecord| {
        st.emitters.by_record.get(&r.id).and_then(|id| st.emitters.get(*id)).is_some_and(|e| {
            e.tally.iter().any(|(l, n)| crate::emitters::is_sticky(l) && *n >= SETTLED_STICKY)
        })
    };
    // A closed record needs a (final) look if it was never classified, or
    // only while open, when it may have held too little signal.
    let fresh = |r: &SignalRecord| st.classes.get(&r.id).is_none_or(|c| c.2);
    if let Some(r) = st.closed.iter().rev().take(200).find(|r| fresh(r) && !settled(r)) {
        return Some(r.clone());
    }
    st.open
        .iter()
        // Let an open record build up ~0.25 s of signal before a first look.
        .filter(|r| r.rows >= 12)
        .filter(|r| !settled(r))
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
/// (P25 frames are 180 ms apart, NXDN48 80 ms, DMR 30-360 ms)...
const GROUP_WINDOW_S: f64 = 0.75;
/// ...unless the emitter's records are short bursts (one TDMA slot, a
/// beacon every few hundred ms: mean record under this), when the window
/// reaches back over the newest `GROUP_BURSTS` of them, up to this long, so
/// it holds the two syncs one burst can't (measured with
/// `detect/examples/burst_windows`: on the Phase 2 TDMA site capture a 0.25
/// s window names the protocol in 30% of windows and a 1.5 s one in all
/// that have signal; a single DMR burst has one sync).
const GROUP_WINDOW_MAX_S: f64 = 3.0;
const GROUP_BURSTS: usize = 4;
const BURSTY_RECORD_S: f64 = 0.15;
/// Records shorter than this can't hold two syncs; their emitter's group
/// window covers them instead of a DDC pass each.
const MIN_RECORD_S: f64 = 0.06;
/// Position-fixed labels on an emitter before its records stop being
/// classified (see `pick`).
const SETTLED_STICKY: u32 = 3;

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
            start: group_window_start(e, rate),
            end: e.last_end_sample,
            center_hz: e.center_hz,
            bandwidth_hz: e.bandwidth_hz,
            tuner_center_hz: e.tuner_center_hz,
        })
}

/// Where an emitter's group window starts (its end is the newest record's
/// end): `GROUP_WINDOW_S` back, or for bursty emitters the start of the
/// `GROUP_BURSTS`-th newest record, within `GROUP_WINDOW_MAX_S`.
fn group_window_start(e: &crate::Emitter, rate: f64) -> u64 {
    let end = e.last_end_sample;
    let plain = end.saturating_sub((GROUP_WINDOW_S * rate) as u64);
    let mean_s = if e.records > 0 { e.on_time_s / e.records as f64 } else { 0.0 };
    if mean_s >= BURSTY_RECORD_S || e.recent.len() < 2 {
        return plain;
    }
    let mut starts: Vec<u64> = e.recent.iter().map(|(s, _)| *s).collect();
    starts.sort_unstable_by(|a, b| b.cmp(a));
    let nth = starts[GROUP_BURSTS.min(starts.len()) - 1];
    let floor = end.saturating_sub((GROUP_WINDOW_MAX_S * rate) as u64);
    nth.max(floor).min(plain)
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

/// What the radio is known to do to its own spectrum (see `detect::context`).
struct Radio {
    spur_grid_hz: Option<f64>,
    transfer_tone_hz: Option<f64>,
    /// Zero-IF: a DC spike sits at the tuner centre.
    dc_spike: bool,
}

fn classify_record(
    shared: &Shared,
    cfg: &SourceConfig,
    rate: f64,
    radio: &Radio,
    r: &SignalRecord,
    chunk: &mut [Complex32],
    stop: &AtomicBool,
) -> (Classification, (u64, u64)) {
    if r.duration_s() < MIN_RECORD_S && !r.active {
        return (label_only("too short"), (0, 0));
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
    let window = (w.start, w.end);
    (classify_window(shared, rate, radio, &w, chunk, stop), window)
}

/// Blocks of wideband IQ averaged for the window's SNR, spread over it.
const SNR_FFT: usize = 16384;
const SNR_BLOCKS: u64 = 24;

/// The window's signal-to-noise ratio from the wideband spectrum: the mean
/// power density over the record's bandwidth against the 20th percentile
/// of the bins beside it (from 0.75 to 3 bandwidths out, plus a little,
/// either side), in dB. Up to `SNR_BLOCKS` FFTs spread over `start..end`.
/// None when the window is shorter than one block or the channel sits
/// outside the row.
fn window_snr(shared: &Shared, rate: f64, start: u64, end: u64, offset_hz: f64, bandwidth_hz: f64, buf: &mut Vec<Complex32>) -> Option<f32> {
    let n = SNR_FFT as u64;
    if end < start + n {
        return None;
    }
    let bin_hz = rate / SNR_FFT as f64;
    let centre = ((offset_hz + rate / 2.0) / bin_hz).round();
    let half = (bandwidth_hz / 2.0 / bin_hz).ceil().max(1.0);
    let near = half * 1.5 + 2.0;
    let far = (half * 6.0 + 10e3 / bin_hz).max(near + 8.0);
    if centre - far < 0.0 || centre + far >= SNR_FFT as f64 {
        return None;
    }
    let mut sa = dsp::SpectrumAnalyzer::new(SNR_FFT);
    buf.resize(SNR_FFT, Complex32::default());
    let blocks = ((end - start) / n).min(SNR_BLOCKS).max(1);
    let stride = (end - start - n) / blocks.max(1);
    for b in 0..blocks {
        let at = start + b * stride;
        if shared.ring.lock().read(at, &mut buf[..]) {
            sa.push(&buf[..]);
        }
    }
    let mut power = Vec::with_capacity(SNR_FFT);
    if !sa.take_power(&mut power) || power.len() != SNR_FFT {
        return None;
    }
    let (c, h, nr, fr) = (centre as usize, half as usize, near as usize, far as usize);
    let band = &power[c - h..=c + h];
    let density = band.iter().map(|v| *v as f64).sum::<f64>() / band.len() as f64;
    let mut beside: Vec<f32> = power[c - fr..c - nr].iter().chain(&power[c + nr + 1..=c + fr]).copied().collect();
    if beside.len() < 8 {
        return None;
    }
    let k = beside.len() / 5;
    let floor = *beside.select_nth_unstable_by(k, f32::total_cmp).1 as f64;
    Some((10.0 * (density / floor.max(1e-30)).log10()) as f32)
}

fn classify_window(shared: &Shared, rate: f64, radio: &Radio, w: &Window, chunk: &mut [Complex32], stop: &AtomicBool) -> Classification {
    let bw = w.bandwidth_hz.max(5e3);
    if bw > MAX_DDC_BW_HZ {
        return label_only("Wideband");
    }
    let (start, end) = (w.start, w.end);
    let oldest = shared.ring.lock().oldest();
    // Only IQ of one tuning, clear of retunes and losses, and mixed with
    // the tuner centre that IQ was actually taken at.
    let (from, tuner_hz) = {
        let st = shared.state.lock();
        (st.continuous_from(end), st.epoch_at(end.saturating_sub(1)).map_or(w.tuner_center_hz, |e| e.center_hz))
    };
    let start = start.max(oldest).max(from);
    if end <= start + (0.02 * rate) as u64 {
        return label_only(if end <= oldest { "IQ expired" } else { "too short" });
    }

    let offset = w.center_hz - tuner_hz;
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
    let mut snr_buf = Vec::new();
    let snr_db = window_snr(shared, rate, start, end, offset, w.bandwidth_hz, &mut snr_buf);
    let mut c = detect::classify_with(&chan, ddc.fs_out, w.bandwidth_hz, detect::Hints { snr_db });
    let ctx = detect::context::Context {
        center_hz: w.center_hz,
        tuner_hz,
        bandwidth_hz: w.bandwidth_hz,
        spur_grid_hz: radio.spur_grid_hz,
        transfer_tone_hz: radio.transfer_tone_hz,
        dc_spike: radio.dc_spike,
    };
    detect::context::refine(&mut c, &ctx, &chan, ddc.fs_out);
    // A line's company: its image about the tuner centre, or a comb it
    // belongs to, says it is the radio's (or the host's) own whatever its
    // modulation measured as. Protocols and already-placed labels stand.
    if c.protocol.is_none() && !crate::emitters::is_sticky(&c.label) && !matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") {
        let ctx = shared.records.lock().emitters.spur_context(w.center_hz, w.bandwidth_hz, tuner_hz);
        if let Some(sc) = ctx {
            c.label = sc.label();
            c.confidence = 0.8;
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn emitter(records: u32, on_time_s: f64, last_end: u64, recent: &[(u64, u64)]) -> crate::Emitter {
        crate::Emitter {
            id: 1,
            center_hz: 856.3375e6,
            bandwidth_hz: 12_500.0,
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
            records,
            on_time_s,
            active: false,
            confirmed: true,
            peak_snr_db: 20.0,
            tally: Default::default(),
            protocols: Default::default(),
            ids: Default::default(),
            activity: Default::default(),
            talk: Default::default(),
            last_talk: None,
            capplus_repeater: None,
            strong: Default::default(),
            class: None,
            best: None,
            class_at: None,
            group_misses: 0,
            last_end_sample: last_end,
            recent: recent.iter().copied().collect(),
            tuner_center_hz: 856e6,
            bin_hz: 610.0,
            solid_records: records,
        }
    }

    #[test]
    fn group_window_reaches_back_over_short_bursts_only() {
        let rate = 10e6;
        // Sample index of a time in milliseconds (10 000 samples per ms).
        let s = |ms: u64| ms * 10_000;
        // Long records (a 2 s transmission): the plain 0.75 s window.
        let long = emitter(3, 6.0, s(20_000), &[(s(10_000), s(12_000)), (s(14_000), s(16_000)), (s(18_000), s(20_000))]);
        assert_eq!(group_window_start(&long, rate), s(19_250));
        // Short bursts 0.8 s apart: back to the start of the fourth-newest.
        let bursts: Vec<(u64, u64)> = (0..6).map(|k| (s(14_000 + 800 * k), s(14_100 + 800 * k))).collect();
        let bursty = emitter(6, 0.6, s(18_100), &bursts);
        assert_eq!(group_window_start(&bursty, rate), s(15_600));
        // Bursts further apart than the cap allows: no further back than 3 s.
        let sparse: Vec<(u64, u64)> = (0..6).map(|k| (s(2_000 * k), s(2_000 * k + 100))).collect();
        let sparse_e = emitter(6, 0.6, s(10_100), &sparse);
        assert_eq!(group_window_start(&sparse_e, rate), s(7_100));
        // One record so far: nothing to reach back to.
        let one = emitter(1, 0.05, s(5_050), &[(s(5_000), s(5_050))]);
        assert_eq!(group_window_start(&one, rate), s(4_300));
    }
}
