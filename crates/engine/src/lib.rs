//! Per-source pipeline: driver → IQ ring (look-back) → spectrum frames.
//! See DESIGN.md §5.

mod ring;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use dsp::SpectrumAnalyzer;
use parking_lot::Mutex;
use sdr_core::{Complex32, Device, DeviceInfo, Samples, StreamEvent, StreamSink};

pub use ring::IqRing;

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub fft_size: usize,
    pub rows_per_sec: f64,
    pub ring_seconds: f64,
    /// Spectrum rows kept for scrolling back the waterfall.
    pub row_history: usize,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self { fft_size: 16384, rows_per_sec: 50.0, ring_seconds: 10.0, row_history: 3000 }
    }
}

/// Tuning in effect for a stretch of the stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub center_hz: f64,
    pub sample_rate: f64,
}

/// One waterfall row: averaged power spectrum in dBFS, DC-centred.
#[derive(Debug)]
pub struct SpectrumRow {
    pub seq: u64,
    pub tuning: Tuning,
    /// Sample index of the newest sample that went into this row.
    pub end_sample: u64,
    pub time: SystemTime,
    pub db: Vec<f32>,
}

#[derive(Default)]
struct StreamState {
    tuning: Option<Tuning>,
    gaps: u64,
    lost_samples: u64,
    overloads: u64,
    lost_device: bool,
}

struct Shared {
    ring: Mutex<IqRing>,
    state: Mutex<StreamState>,
    rows: Mutex<VecDeque<Arc<SpectrumRow>>>,
    stop: AtomicBool,
}

struct RingSink {
    shared: Arc<Shared>,
}

impl StreamSink for RingSink {
    fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
        self.shared.ring.lock().write(first_sample, samples);
    }

    fn on_event(&mut self, event: StreamEvent) {
        let mut st = self.shared.state.lock();
        match event {
            StreamEvent::Retune { center_hz, sample_rate, .. } => st.tuning = Some(Tuning { center_hz, sample_rate }),
            StreamEvent::Gap { lost_samples, .. } => {
                st.gaps += 1;
                st.lost_samples += lost_samples;
            }
            StreamEvent::Overload { .. } => st.overloads += 1,
            StreamEvent::DeviceLost => st.lost_device = true,
            StreamEvent::TimeAnchor { .. } | StreamEvent::GainChange { .. } => {}
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourceStats {
    pub gaps: u64,
    pub lost_samples: u64,
    pub overloads: u64,
    pub device_lost: bool,
    pub ring_samples: u64,
    pub ring_capacity: usize,
    pub total_samples: u64,
    pub ffts_per_sec: f64,
}

/// A running source: owns the device, its IQ ring and the spectrum thread.
pub struct Source {
    device: Box<dyn Device>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    cfg: SourceConfig,
    ffts_per_sec: Arc<Mutex<f64>>,
}

impl Source {
    pub fn start(mut device: Box<dyn Device>, cfg: SourceConfig) -> sdr_core::Result<Self> {
        let rate = device.sample_rate();
        let cap = (rate * cfg.ring_seconds).ceil() as usize;
        let shared = Arc::new(Shared {
            ring: Mutex::new(IqRing::new(cap)),
            state: Mutex::new(StreamState {
                tuning: Some(Tuning { center_hz: device.center_hz(), sample_rate: rate }),
                ..Default::default()
            }),
            rows: Mutex::new(VecDeque::with_capacity(cfg.row_history)),
            stop: AtomicBool::new(false),
        });
        device.start(Box::new(RingSink { shared: shared.clone() }))?;
        let ffts_per_sec = Arc::new(Mutex::new(0.0));
        let worker = {
            let shared = shared.clone();
            let cfg = cfg.clone();
            let fps = ffts_per_sec.clone();
            std::thread::Builder::new()
                .name("spectrum".into())
                .spawn(move || spectrum_worker(shared, cfg, fps))
                .expect("spawn spectrum thread")
        };
        Ok(Self { device, shared, worker: Some(worker), cfg, ffts_per_sec })
    }

    pub fn info(&self) -> &DeviceInfo {
        self.device.info()
    }

    pub fn device(&self) -> &dyn Device {
        self.device.as_ref()
    }

    pub fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    pub fn set_center(&mut self, hz: f64) -> sdr_core::Result<()> {
        self.device.set_center(hz)
    }

    /// Rows with `seq > after`, oldest first.
    pub fn rows_after(&self, after: Option<u64>) -> Vec<Arc<SpectrumRow>> {
        let rows = self.shared.rows.lock();
        match after {
            None => rows.iter().cloned().collect(),
            Some(a) => rows.iter().filter(|r| r.seq > a).cloned().collect(),
        }
    }

    pub fn latest_row(&self) -> Option<Arc<SpectrumRow>> {
        self.shared.rows.lock().back().cloned()
    }

    pub fn stats(&self) -> SourceStats {
        let st = self.shared.state.lock();
        let ring = self.shared.ring.lock();
        SourceStats {
            gaps: st.gaps,
            lost_samples: st.lost_samples,
            overloads: st.overloads,
            device_lost: st.lost_device,
            ring_samples: ring.available(),
            ring_capacity: ring.capacity(),
            total_samples: ring.head(),
            ffts_per_sec: *self.ffts_per_sec.lock(),
        }
    }

    /// Copies `out.len()` samples starting at `start` out of the look-back ring.
    pub fn read_iq(&self, start: u64, out: &mut [Complex32]) -> bool {
        self.shared.ring.lock().read(start, out)
    }

    pub fn ring_head(&self) -> u64 {
        self.shared.ring.lock().head()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.device.stop();
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn spectrum_worker(shared: Arc<Shared>, cfg: SourceConfig, fps_out: Arc<Mutex<f64>>) {
    let n = cfg.fft_size;
    let mut sa = SpectrumAnalyzer::new(n);
    let mut block = vec![Complex32::default(); n];
    let mut cursor: Option<u64> = None;
    let mut seq = 0u64;
    let row_interval = Duration::from_secs_f64(1.0 / cfg.rows_per_sec);
    let mut next_row = Instant::now() + row_interval;
    let mut fft_count = 0u64;
    let mut fft_window = Instant::now();
    let mut db = Vec::with_capacity(n);

    while !shared.stop.load(Ordering::Relaxed) {
        let (head, oldest) = {
            let r = shared.ring.lock();
            (r.head(), r.oldest())
        };
        let cur = cursor.get_or_insert(head.saturating_sub(n as u64));
        // Fell behind the ring's tail: skip forward rather than read overwritten data.
        if *cur < oldest {
            *cur = head.saturating_sub(n as u64);
        }
        let mut did_work = false;
        // Cap the work per pass so rows keep flowing even if the CPU can't keep up.
        let mut budget = 64;
        while *cur + n as u64 <= head && budget > 0 {
            if shared.ring.lock().read(*cur, &mut block) {
                sa.push(&block);
                fft_count += 1;
            }
            *cur += n as u64;
            budget -= 1;
            did_work = true;
        }
        if budget == 0 {
            // Can't keep up: drop to the live edge.
            *cur = head.saturating_sub(n as u64);
        }

        let now = Instant::now();
        if now >= next_row {
            next_row += row_interval;
            if next_row < now {
                next_row = now + row_interval;
            }
            if sa.take_db(&mut db) {
                let tuning = shared.state.lock().tuning.unwrap_or(Tuning { center_hz: 0.0, sample_rate: 1.0 });
                seq += 1;
                let row = Arc::new(SpectrumRow {
                    seq,
                    tuning,
                    end_sample: *cur,
                    time: SystemTime::now(),
                    db: std::mem::replace(&mut db, Vec::with_capacity(n)),
                });
                let mut rows = shared.rows.lock();
                if rows.len() >= cfg.row_history {
                    rows.pop_front();
                }
                rows.push_back(row);
            }
        }
        let el = fft_window.elapsed();
        if el >= Duration::from_secs(1) {
            *fps_out.lock() = fft_count as f64 / el.as_secs_f64();
            fft_count = 0;
            fft_window = Instant::now();
        }
        if !did_work {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
