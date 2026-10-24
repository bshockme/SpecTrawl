//! Shared helpers for the engine's integration tests.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimControl, SimDevice};
use engine::{BudgetConfig, BusConfig, Consumer, Delivery, EndReason, Flow, GapReason, Keeper, RingBudget, RingConfig, SourceConfig};
use sdr_core::{Complex32, Device};

/// A simulated device with no emitters (noise only) at `rate`. It never
/// reports a lag gap (a paced sim that falls behind on a loaded runner just
/// restarts its clock), so only injected gaps reach the tests.
pub fn sim(rate: f64, paced: bool) -> (Box<dyn Device>, SimControl) {
    let (d, c) = SimDevice::with_config(SimConfig { emitters: vec![], rate, paced, report_lag_gaps: false, ..Default::default() });
    (Box::new(d), c)
}

/// A paced simulated device at `center_hz` 855.25 MHz with one continuous
/// CW carrier at `freq_hz` and `level_dbfs`. Like [`sim`], it never reports a
/// lag gap.
pub fn cw_sim(rate: f64, freq_hz: f64, level_dbfs: f64) -> (Box<dyn Device>, SimControl) {
    cw_emitters_sim(rate, &[freq_hz], level_dbfs)
}

/// Like [`cw_sim`] with two continuous CW carriers at `f1` and `f2`, both at `level_dbfs`.
pub fn two_tone_sim(rate: f64, f1: f64, f2: f64, level_dbfs: f64) -> (Box<dyn Device>, SimControl) {
    cw_emitters_sim(rate, &[f1, f2], level_dbfs)
}

fn cw_emitters_sim(rate: f64, freqs: &[f64], level_dbfs: f64) -> (Box<dyn Device>, SimControl) {
    let emitters =
        freqs.iter().map(|&freq_hz| Emitter { freq_hz, level_dbfs, modulation: Modulation::Cw, keying: Keying::Continuous }).collect();
    let (d, c) = SimDevice::with_config(SimConfig {
        emitters,
        rate,
        center_hz: 855.25e6,
        paced: true,
        report_lag_gaps: false,
        ..Default::default()
    });
    (Box::new(d), c)
}

/// A keeper holding at least 1 s, at most 256 MiB, swept every 0.2 s.
pub fn keeper() -> Arc<Keeper> {
    Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }))
}

/// 1 MiB blocks (262,144 samples), weight 1.0.
/// The bus configuration the load tests run with: the default, except that
/// `SPECTRAWL_TEST_WORKERS` overrides the pool size so a one-worker CI runner
/// can be reproduced on a big machine.
pub fn load_bus_config() -> BusConfig {
    let mut bus = BusConfig::default();
    if let Some(w) = std::env::var("SPECTRAWL_TEST_WORKERS").ok().and_then(|s| s.parse::<usize>().ok()) {
        bus.workers = w.max(1);
    }
    bus
}

/// How many channels the load tests subscribe: the spec's 200 on a pool of
/// 14 workers (a 16-CPU machine), scaled down with the pool so a CI runner
/// with one worker still exercises the same paths with 14, never fewer.
pub fn load_channels(workers: usize) -> usize {
    (200 * workers / 14).clamp(14, 200)
}

pub fn cfg() -> SourceConfig {
    SourceConfig { ring: RingConfig { block_bytes: 1 << 20 }, weight: 1.0 }
}

/// Poll `f` every 5 ms; panic if it is not true within 20 s.
pub fn wait_until(f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "condition not met within 20 s");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A `Samples` or `Gap` delivery, in the order they arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Samples { first_sample: u64, len: usize },
    Gap { from_sample: u64, to_sample: u64, reason: GapReason },
}

/// A cloneable [`Consumer`] that records what the bus delivers to it.
#[derive(Clone)]
pub struct Collector(Arc<Mutex<Collected>>);

#[derive(Default)]
struct Collected {
    /// Keep the samples (false: only count them).
    store: bool,
    /// Return `Flow::Stop` once this many samples have arrived.
    limit: Option<usize>,
    /// Sleep this long in each delivery.
    sleep: Option<Duration>,
    /// Sleep this long in the first delivery only.
    sleep_once: Option<Duration>,
    /// Deliveries finished.
    deliveries: u64,
    stored: Vec<Complex32>,
    samples: usize,
    first_samples: Vec<u64>,
    events: Vec<Event>,
    gaps: u64,
    sheds: u64,
    ends: u64,
    ended: Option<EndReason>,
}

impl Collector {
    fn with(c: Collected) -> Collector {
        Collector(Arc::new(Mutex::new(c)))
    }

    /// Stores every sample.
    pub fn new() -> Collector {
        Collector::with(Collected { store: true, ..Default::default() })
    }

    /// Counts samples without storing them.
    pub fn counting_only() -> Collector {
        Collector::with(Collected::default())
    }

    /// Stores samples and returns `Flow::Stop` once `n` have arrived.
    pub fn with_limit(n: usize) -> Collector {
        Collector::with(Collected { store: true, limit: Some(n), ..Default::default() })
    }

    /// Stores samples and sleeps `d` in every delivery.
    pub fn sleeping(d: Duration) -> Collector {
        Collector::with(Collected { store: true, sleep: Some(d), ..Default::default() })
    }

    /// Counts samples and sleeps `d` in its first delivery only.
    pub fn sleeping_once(d: Duration) -> Collector {
        Collector::with(Collected { sleep_once: Some(d), ..Default::default() })
    }

    /// Deliveries that have returned.
    pub fn deliveries(&self) -> u64 {
        self.0.lock().unwrap().deliveries
    }

    /// Samples received.
    pub fn samples(&self) -> usize {
        self.0.lock().unwrap().samples
    }

    pub fn gaps(&self) -> u64 {
        self.0.lock().unwrap().gaps
    }

    pub fn shed_deliveries(&self) -> u64 {
        self.0.lock().unwrap().sheds
    }

    pub fn end_deliveries(&self) -> u64 {
        self.0.lock().unwrap().ends
    }

    pub fn ended(&self) -> Option<EndReason> {
        self.0.lock().unwrap().ended
    }

    /// `first_sample` of the first `Samples` delivery; panics if there was none.
    pub fn first_delivery_first_sample(&self) -> u64 {
        *self.0.lock().unwrap().first_samples.first().expect("no Samples delivery yet")
    }

    /// Every `Samples` delivery starts `hop` input samples after the one before it.
    pub fn first_samples_are_consecutive_by(&self, hop: u64) -> bool {
        self.0.lock().unwrap().first_samples.windows(2).all(|w| w[1] == w[0] + hop)
    }

    /// Every `Samples` and `Gap` delivery so far, in order.
    pub fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().events.clone()
    }

    /// The last `n` stored samples (fewer if fewer were stored).
    pub fn tail(&self, n: usize) -> Vec<Complex32> {
        let c = self.0.lock().unwrap();
        c.stored[c.stored.len().saturating_sub(n)..].to_vec()
    }
}

impl Consumer for Collector {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        let sleep = {
            let mut c = self.0.lock().unwrap();
            c.sleep.or(c.sleep_once.take())
        };
        if let Some(t) = sleep {
            std::thread::sleep(t); // outside the lock, so the test can read meanwhile
        }
        let mut c = self.0.lock().unwrap();
        c.deliveries += 1;
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                c.first_samples.push(first_sample);
                c.events.push(Event::Samples { first_sample, len: samples.len() });
                c.samples += samples.len();
                if c.store {
                    c.stored.extend_from_slice(samples);
                }
            }
            Delivery::Gap { from_sample, to_sample, reason } => {
                c.gaps += 1;
                c.events.push(Event::Gap { from_sample, to_sample, reason });
            }
            Delivery::Shed => c.sheds += 1,
            Delivery::Ended { reason } => {
                c.ends += 1;
                c.ended = Some(reason);
            }
        }
        if c.limit.is_some_and(|l| c.samples >= l) { Flow::Stop } else { Flow::Continue }
    }
}
