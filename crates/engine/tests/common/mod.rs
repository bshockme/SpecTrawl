//! Shared helpers for the engine's integration tests.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimControl, SimDevice};
use engine::{RingConfig, SourceConfig};
use sdr_core::Device;

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
    let emitters = vec![Emitter { freq_hz, level_dbfs, modulation: Modulation::Cw, keying: Keying::Continuous }];
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

/// 1 MiB blocks (262,144 samples), weight 1.0.
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
