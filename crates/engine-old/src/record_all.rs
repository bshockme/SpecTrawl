//! "Record every signal": one SigMF channel capture per detected signal.
//!
//! While on, a worker follows the detector's records. A record that has
//! lasted `min_duration_s` gets a file: its IQ is down-converted out of the
//! look-back ring (like a channel export) from a little before its first row,
//! and appended as the record grows, so a transmission longer than the ring
//! is still captured whole. The metadata is written when the record closes
//! (or at `max_duration_s`, after which the rest is dropped), and the file
//! is named by its start time, frequency and the best label known then.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use detect::Ddc;
use sdr_core::Complex32;

use crate::{Shared, SignalRecord, SourceConfig};

#[derive(Debug, Clone)]
pub struct RecordAllConfig {
    /// Records shorter than this never get a file.
    pub min_duration_s: f64,
    /// A file stops growing here even if the record goes on.
    pub max_duration_s: f64,
    pub min_bandwidth_hz: f64,
    /// Wider signals (TV, LTE, FM broadcast) are skipped.
    pub max_bandwidth_hz: f64,
    pub dir: PathBuf,
    /// Also record signals inside known wideband occupants (FM, TV, LTE).
    pub include_squelched: bool,
}

impl Default for RecordAllConfig {
    fn default() -> Self {
        Self {
            min_duration_s: 0.3,
            max_duration_s: 120.0,
            min_bandwidth_hz: 0.0,
            max_bandwidth_hz: 1.0e6,
            dir: crate::sigmf::default_dir().join("signals"),
            include_squelched: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecordAllStatus {
    /// Finished files (data + meta).
    pub files: u64,
    pub bytes: u64,
    /// Records whose IQ had left the ring before they could be written.
    pub skipped_expired: u64,
    /// Records outside the duration/bandwidth filters or squelched.
    pub skipped_filter: u64,
    /// Files still being appended to.
    pub open: usize,
    pub last_path: Option<PathBuf>,
    pub last_error: Option<String>,
}

/// A file being written for one record.
struct Job {
    record: SignalRecord,
    ddc: Ddc,
    writer: BufWriter<File>,
    data_path: PathBuf,
    stem: String,
    /// Ring sample the file is written up to.
    pos: u64,
    start_sample: u64,
    start_time: SystemTime,
    out_samples: u64,
    bytes: u64,
    started: Instant,
    /// Reached `max_duration_s`: no more appends, finish at once.
    capped: bool,
}

/// How often the records are polled.
const POLL: Duration = Duration::from_millis(200);
/// Margin before the first row and after the last, seconds.
const MARGIN_S: f64 = 0.05;
const CHUNK: usize = 1 << 17;

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64, cfg: SourceConfig, hw: String) {
    let mut jobs: HashMap<u64, Job> = HashMap::new();
    // Records already given a file or decided against, so closed ones aren't
    // handled twice. Pruned by record id below the oldest closed record.
    let mut handled: HashSet<u64> = HashSet::new();
    let mut chunk = vec![Complex32::default(); CHUNK];
    let per_row = (rate / cfg.rows_per_sec) as u64;
    let margin = (MARGIN_S * rate) as u64;
    let mut was_on = false;
    while !stop.load(Ordering::Relaxed) {
        let Some(c) = shared.record_all.lock().clone() else {
            if was_on {
                // Switched off: close what is open.
                for (_, j) in jobs.drain() {
                    finish(&shared, &hw, j);
                }
                was_on = false;
                shared.record_all_status.lock().open = 0;
            }
            std::thread::sleep(POLL);
            continue;
        };
        was_on = true;
        let (oldest, head) = {
            let r = shared.ring.lock();
            (r.oldest(), r.head())
        };
        // Snapshot the records (one lock at a time, copied out).
        let (open, closed): (Vec<SignalRecord>, Vec<SignalRecord>) = {
            let st = shared.records.lock();
            let open: Vec<SignalRecord> = st.open.iter().map(|r| st.with_class(r)).collect();
            // Closed records not yet handled: the detector closes a record a
            // hangover after its last row, so recent ones are at the back.
            let closed: Vec<SignalRecord> = st
                .closed
                .iter()
                .rev()
                .take(500)
                .filter(|r| !handled.contains(&r.id) && !jobs.contains_key(&r.id))
                .map(|r| st.with_class(r))
                .collect();
            if let Some(oldest_id) = st.closed.front().map(|r| r.id) {
                handled.retain(|id| *id >= oldest_id);
            }
            (open, closed)
        };
        let squelched = |r: &SignalRecord| {
            !c.include_squelched && {
                let f = r.center_hz();
                shared.occupancy.lock().known.iter().any(|k| (k.occupant.lo_hz..=k.occupant.hi_hz).contains(&f))
            }
        };
        let passes = |r: &SignalRecord| {
            let bw = r.bandwidth_hz();
            bw >= c.min_bandwidth_hz && bw <= c.max_bandwidth_hz && !squelched(r)
        };
        // Open records that have lasted long enough start a file.
        for r in &open {
            if jobs.contains_key(&r.id) || handled.contains(&r.id) || r.duration_s() < c.min_duration_s {
                continue;
            }
            if !passes(r) {
                handled.insert(r.id);
                shared.record_all_status.lock().skipped_filter += 1;
                continue;
            }
            match start_job(&shared, &c, r, rate, per_row, margin, oldest) {
                Ok(j) => {
                    jobs.insert(r.id, j);
                }
                Err(expired) => {
                    handled.insert(r.id);
                    let mut s = shared.record_all_status.lock();
                    if expired {
                        s.skipped_expired += 1;
                    } else {
                        s.last_error = Some("could not create the capture file".into());
                    }
                }
            }
        }
        // Closed records: finish their jobs, or write the whole thing for
        // ones that closed between polls.
        for r in &closed {
            handled.insert(r.id);
            if r.duration_s() < c.min_duration_s {
                continue;
            }
            if !passes(r) {
                shared.record_all_status.lock().skipped_filter += 1;
                continue;
            }
            match start_job(&shared, &c, r, rate, per_row, margin, oldest) {
                Ok(mut j) => {
                    j.record = r.clone();
                    append(&shared, &mut j, (r.last_row_end_sample + margin).min(head), &mut chunk);
                    finish(&shared, &hw, j);
                }
                Err(expired) => {
                    let mut s = shared.record_all_status.lock();
                    if expired {
                        s.skipped_expired += 1;
                    }
                }
            }
        }
        // Grow the open jobs; finish the ones whose record has closed.
        let open_ids: HashSet<u64> = open.iter().map(|r| r.id).collect();
        let mut done: Vec<u64> = Vec::new();
        for (id, j) in jobs.iter_mut() {
            if let Some(r) = open.iter().find(|r| r.id == *id) {
                j.record = r.clone();
            }
            if j.capped {
                continue;
            }
            let still_open = open_ids.contains(id);
            let target = if still_open { j.record.last_row_end_sample } else { (j.record.last_row_end_sample + margin).min(head) };
            // The cap, in ring samples from the start.
            let cap_end = j.start_sample + (c.max_duration_s * rate) as u64;
            let target = target.min(cap_end);
            if target > j.pos {
                append(&shared, j, target, &mut chunk);
            }
            if target >= cap_end {
                j.capped = true;
                done.push(*id);
            } else if !still_open {
                // Closed: the record store has its final shape by now.
                if let Some(r) = shared.records.lock().closed.iter().rev().take(500).find(|r| r.id == *id).cloned() {
                    j.record = r;
                }
                done.push(*id);
            }
        }
        for id in done {
            if let Some(j) = jobs.remove(&id) {
                handled.insert(id);
                finish(&shared, &hw, j);
            }
        }
        shared.record_all_status.lock().open = jobs.len();
        std::thread::sleep(POLL);
    }
    for (_, j) in jobs.drain() {
        finish(&shared, &hw, j);
    }
}

/// Best name for a record right now.
fn label_of(r: &SignalRecord) -> String {
    r.display_class().map(|c| c.label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| "signal".into())
}

fn stem_for(r: &SignalRecord, t: SystemTime) -> String {
    format!("{}_{:.4}MHz_{}_r{}", crate::sigmf::stamp(t), r.center_hz() / 1e6, crate::sigmf::sanitize(&label_of(r)), r.id)
}

/// Opens the data file for a record. Err(true) when its IQ has left the ring.
fn start_job(
    shared: &Shared,
    c: &RecordAllConfig,
    r: &SignalRecord,
    rate: f64,
    per_row: u64,
    margin: u64,
    oldest: u64,
) -> Result<Job, bool> {
    let want = r.first_row_end_sample.saturating_sub(per_row + margin);
    // Allow a little loss at the front, but not a record that is mostly gone.
    let start = want.max(oldest);
    if start >= r.last_row_end_sample || start > want + (0.5 * rate) as u64 {
        return Err(true);
    }
    let start_time = shared.state.lock().time_of(start, rate);
    let stem = stem_for(r, start_time);
    if let Err(e) = std::fs::create_dir_all(&c.dir) {
        log::warn!("record all: {}: {e}", c.dir.display());
        return Err(false);
    }
    let data_path = c.dir.join(format!("{stem}.sigmf-data"));
    let file = File::create(&data_path).map_err(|e| {
        log::warn!("record all: {}: {e}", data_path.display());
        false
    })?;
    let bw = r.bandwidth_hz().max(5e3);
    let ddc = Ddc::new(rate, r.center_hz() - r.tuner_center_hz, bw * 0.65 + 1e3, (bw * 2.5).max(48e3));
    log::debug!("record all: {} starts at ring {start}", data_path.display());
    Ok(Job {
        record: r.clone(),
        ddc,
        writer: BufWriter::with_capacity(1 << 20, file),
        data_path,
        stem,
        pos: start,
        start_sample: start,
        start_time,
        out_samples: 0,
        bytes: 0,
        started: Instant::now(),
        capped: false,
    })
}

/// Writes ring samples `job.pos..to` through the DDC into the file.
fn append(shared: &Shared, j: &mut Job, to: u64, chunk: &mut [Complex32]) {
    let oldest = shared.ring.lock().oldest();
    if j.pos < oldest {
        // Fell behind the ring (disk or CPU too slow): skip the lost stretch.
        log::warn!("record all: {} lost {} samples to the ring", j.stem, oldest - j.pos);
        j.pos = oldest;
    }
    let mut out: Vec<Complex32> = Vec::with_capacity(CHUNK / 8);
    while j.pos < to {
        let n = ((to - j.pos) as usize).min(chunk.len());
        if !shared.ring.lock().read(j.pos, &mut chunk[..n]) {
            break;
        }
        out.clear();
        j.ddc.process(&chunk[..n], &mut out);
        for s in &out {
            if j.writer.write_all(&s.re.to_le_bytes()).and_then(|_| j.writer.write_all(&s.im.to_le_bytes())).is_err() {
                shared.record_all_status.lock().last_error = Some(format!("write {}", j.data_path.display()));
                j.capped = true;
                return;
            }
        }
        j.out_samples += out.len() as u64;
        j.bytes += out.len() as u64 * 8;
        j.pos += n as u64;
    }
}

/// Flushes the data, renames it after the final label, writes the metadata.
fn finish(shared: &Shared, hw: &str, mut j: Job) {
    let _ = j.writer.flush();
    drop(j.writer);
    let r = &j.record;
    let stem = stem_for(r, j.start_time);
    let mut data_path = j.data_path.clone();
    if stem != j.stem {
        let renamed = data_path.with_file_name(format!("{stem}.sigmf-data"));
        if std::fs::rename(&data_path, &renamed).is_ok() {
            data_path = renamed;
        }
    }
    let meta_path = data_path.with_extension("sigmf-meta");
    let label = label_of(r);
    let seconds = j.out_samples as f64 / j.ddc.fs_out;
    let meta = serde_json::json!({
        "global": {
            "core:version": "1.2.0",
            "core:datatype": "cf32_le",
            "core:sample_rate": j.ddc.fs_out,
            "core:hw": hw,
            "core:recorder": "SpecTrawl",
            "core:description": format!(
                "SpecTrawl record-all capture: {label}, {:.4} MHz, {:.1} kHz wide, record {}{}, peak {:.1} dBFS, SNR {:.1} dB, {:.1} s",
                r.center_hz() / 1e6,
                r.bandwidth_hz() / 1e3,
                r.id,
                r.emitter.map(|e| format!(", emitter {e}")).unwrap_or_default(),
                r.peak_dbfs,
                r.max_snr_db,
                seconds
            ),
            "spectrawl:label": label,
            "spectrawl:record": r.id,
            "spectrawl:emitter": r.emitter,
            "spectrawl:bandwidth_hz": r.bandwidth_hz(),
            "spectrawl:peak_dbfs": r.peak_dbfs,
            "spectrawl:max_snr_db": r.max_snr_db,
            "spectrawl:capped": j.capped,
        },
        "captures": [{
            "core:sample_start": 0,
            "core:frequency": r.center_hz(),
            "core:datetime": crate::sigmf::rfc3339(j.start_time),
        }],
        "annotations": [{
            "core:sample_start": 0,
            "core:sample_count": j.out_samples.max(1),
            "core:freq_lower_edge": r.f_lo_hz,
            "core:freq_upper_edge": r.f_hi_hz,
            "core:label": label,
        }],
    });
    let mut s = shared.record_all_status.lock();
    match std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()) {
        Ok(()) => {
            s.files += 1;
            s.bytes += j.bytes;
            s.last_path = Some(data_path.clone());
            log::info!(
                "record all: {} ({seconds:.1} s, {:.1} MB, {:.1} s to write)",
                data_path.display(),
                j.bytes as f64 / 1e6,
                j.started.elapsed().as_secs_f64()
            );
        }
        Err(e) => s.last_error = Some(format!("{}: {e}", meta_path.display())),
    }
}
