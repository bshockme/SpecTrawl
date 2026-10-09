//! Hardware abstraction shared by every SDR backend: the `Backend`/`Device`
//! traits, the `Capabilities` record roles are matched against, and the stream
//! contract (sample blocks numbered by a running sample counter, interleaved
//! with in-band events). See DESIGN.md §4.

use std::time::SystemTime;

pub use num_complex::Complex32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backend library not available: {0}")]
    LibraryMissing(String),
    #[error("device not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("driver error: {0}")]
    Driver(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Sample formats a device can deliver natively. The IQ ring stores the native
/// format; conversion to `Complex32` happens only when samples are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Ci8,
    Ci16,
    Cf32,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::Ci8 => 2,
            SampleFormat::Ci16 => 4,
            SampleFormat::Cf32 => 8,
        }
    }
}

/// A borrowed run of samples in a device's native format.
#[derive(Debug, Clone, Copy)]
pub enum Samples<'a> {
    Ci8(&'a [[i8; 2]]),
    Ci16(&'a [[i16; 2]]),
    Cf32(&'a [Complex32]),
}

impl Samples<'_> {
    pub fn len(&self) -> usize {
        match self {
            Samples::Ci8(s) => s.len(),
            Samples::Ci16(s) => s.len(),
            Samples::Cf32(s) => s.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfArchitecture {
    /// Real IF sampling (e.g. Airspy): no DC spike at the centre.
    RealIf,
    /// Zero-IF / direct conversion (HackRF, RTL-SDR): DC spike, offset-tune.
    ZeroIf,
}

#[derive(Debug, Clone)]
pub struct GainStage {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub step: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GainSetting {
    /// 0.0 = minimum, 1.0 = maximum, mapped by the backend onto its stages.
    Linearity(f32),
    Sensitivity(f32),
    Auto,
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub freq_ranges_hz: Vec<(f64, f64)>,
    pub tuning_step_hz: f64,
    /// A retune issued as a block of samples arrives shows up within this
    /// long after the start of the next block (fast steps; see
    /// `Device::tuner_locked` for slow ones).
    pub retune_latency_s: f64,
    pub sample_rates: Vec<f64>,
    /// Fraction of the sample rate that is alias/roll-off free (0..1).
    pub usable_bandwidth_fraction: f64,
    pub native_format: SampleFormat,
    pub adc_bits: u8,
    pub if_arch: IfArchitecture,
    pub gain_stages: Vec<GainStage>,
    pub has_linearity_sensitivity_presets: bool,
    pub native_sweep: bool,
    pub channels: u8,
    pub ext_clock: bool,
    pub bias_tee: bool,
    pub hw_timestamps: bool,
    /// The radio's own clock harmonics land on multiples of this: narrow
    /// spurs that aren't signals (Airspy R2: 20 and 25 MHz, so 5 MHz).
    pub spur_grid_hz: Option<f64>,
    /// Samples per USB transfer. Internal spurs are often modulated at
    /// sample rate / this (152.6 Hz for an Airspy at 10 MS/s).
    pub transfer_samples: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub backend: String,
    /// Stable identity used for per-device config and calibration.
    pub serial: String,
    pub label: String,
}

/// In-band events, positioned on the sample counter of the stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Retune { at_sample: u64, center_hz: f64, sample_rate: f64 },
    Gap { at_sample: u64, lost_samples: u64 },
    TimeAnchor { at_sample: u64, host_time: SystemTime },
    GainChange { at_sample: u64, gain: GainSetting },
    Overload { at_sample: u64 },
    DeviceLost,
}

/// Receives a device's stream. Called from the driver's thread: implementations
/// must copy and return quickly (no blocking, no DSP).
pub trait StreamSink: Send {
    fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>);
    fn on_event(&mut self, event: StreamEvent);
}

pub trait Device: Send {
    fn info(&self) -> &DeviceInfo;
    fn capabilities(&self) -> &Capabilities;
    fn center_hz(&self) -> f64;
    fn sample_rate(&self) -> f64;
    fn set_center(&mut self, hz: f64) -> Result<()>;
    /// Whether the tuner's PLL reports lock after a retune. None if the
    /// device can't tell. Some tuners take far longer on some steps than
    /// others (an R820T searching its VCO range: ~13 ms instead of ~1).
    fn tuner_locked(&mut self) -> Option<bool> {
        None
    }
    fn set_sample_rate(&mut self, rate: f64) -> Result<()>;
    fn set_gain(&mut self, gain: GainSetting) -> Result<()>;
    fn start(&mut self, sink: Box<dyn StreamSink>) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn is_streaming(&self) -> bool;
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    /// Ok(empty) when the library loads but no device is attached;
    /// Err(LibraryMissing) when the backend is unavailable on this machine.
    fn enumerate(&self) -> Result<Vec<DeviceInfo>>;
    fn open(&self, serial: &str) -> Result<Box<dyn Device>>;
}
