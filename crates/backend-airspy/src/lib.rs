//! Native Airspy backend (R2, Mini; R0/R1 too) over libairspy, loaded at
//! runtime so a missing library just hides the backend.
//!
//! Samples are requested as FLOAT32_IQ (libairspy scales to ±1.0) and stored
//! by the engine as i16. Retunes are reported in-band at the sample count the
//! callback has reached when it notices the change, which is within one USB
//! transfer of the true switch.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use libloading::Library;
use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, GainStage, IfArchitecture, Result, SampleFormat, Samples,
    StreamEvent, StreamSink,
};

#[repr(C)]
struct AirspyDev {
    _p: [u8; 0],
}

#[repr(C)]
struct Transfer {
    device: *mut AirspyDev,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: c_int,
    dropped_samples: u64,
    sample_type: c_int,
}

type RxCb = unsafe extern "C" fn(*mut Transfer) -> c_int;

const SAMPLE_FLOAT32_IQ: c_int = 0;
const SUCCESS: c_int = 0;
const TRUE: c_int = 1;

/// Resolved libairspy entry points. The `Library` is kept alive alongside.
struct Api {
    _lib: Library,
    list_devices: unsafe extern "C" fn(*mut u64, c_int) -> c_int,
    open_sn: unsafe extern "C" fn(*mut *mut AirspyDev, u64) -> c_int,
    close: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    get_samplerates: unsafe extern "C" fn(*mut AirspyDev, *mut u32, u32) -> c_int,
    set_samplerate: unsafe extern "C" fn(*mut AirspyDev, u32) -> c_int,
    set_sample_type: unsafe extern "C" fn(*mut AirspyDev, c_int) -> c_int,
    start_rx: unsafe extern "C" fn(*mut AirspyDev, RxCb, *mut c_void) -> c_int,
    stop_rx: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    is_streaming: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    set_freq: unsafe extern "C" fn(*mut AirspyDev, u32) -> c_int,
    set_linearity_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_sensitivity_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_lna_agc: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_mixer_agc: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    r820t_read: unsafe extern "C" fn(*mut AirspyDev, u8, *mut u8) -> c_int,
    error_name: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Api {
    fn load() -> Result<Self> {
        let mut tried = Vec::new();
        for cand in candidates() {
            match open_library(&cand) {
                Ok(lib) => return unsafe { Self::resolve(lib) }.map_err(|e| Error::LibraryMissing(format!("{}: {e}", cand.display()))),
                Err(e) => tried.push(format!("{}: {e}", cand.display())),
            }
        }
        Err(Error::LibraryMissing(format!("libairspy not found (tried {})", tried.join("; "))))
    }

    unsafe fn resolve(lib: Library) -> std::result::Result<Self, libloading::Error> {
        unsafe {
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(Self {
                list_devices: sym!("airspy_list_devices"),
                open_sn: sym!("airspy_open_sn"),
                close: sym!("airspy_close"),
                get_samplerates: sym!("airspy_get_samplerates"),
                set_samplerate: sym!("airspy_set_samplerate"),
                set_sample_type: sym!("airspy_set_sample_type"),
                start_rx: sym!("airspy_start_rx"),
                stop_rx: sym!("airspy_stop_rx"),
                is_streaming: sym!("airspy_is_streaming"),
                set_freq: sym!("airspy_set_freq"),
                set_linearity_gain: sym!("airspy_set_linearity_gain"),
                set_sensitivity_gain: sym!("airspy_set_sensitivity_gain"),
                set_lna_agc: sym!("airspy_set_lna_agc"),
                set_mixer_agc: sym!("airspy_set_mixer_agc"),
                r820t_read: sym!("airspy_r820t_read"),
                error_name: sym!("airspy_error_name"),
                _lib: lib,
            })
        }
    }

    fn check(&self, what: &str, rc: c_int) -> Result<()> {
        if rc == SUCCESS {
            return Ok(());
        }
        let name = unsafe {
            let p = (self.error_name)(rc);
            if p.is_null() { "?".into() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        Err(Error::Driver(format!("{what}: {name} ({rc})")))
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("SPECTRAWL_AIRSPY_LIB") {
        v.push(PathBuf::from(p));
    }
    if cfg!(windows) {
        v.push("airspy.dll".into());
        v.push(r"C:\Program Files\PothosSDR\bin\airspy.dll".into());
    } else if cfg!(target_os = "macos") {
        v.push("libairspy.dylib".into());
        v.push("/opt/homebrew/lib/libairspy.dylib".into());
        v.push("/usr/local/lib/libairspy.dylib".into());
    } else {
        v.push("libairspy.so.0".into());
        v.push("libairspy.so".into());
    }
    v
}

fn open_library(path: &PathBuf) -> std::result::Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        // For a full path, let Windows find the DLL's own dependencies
        // (libusb, pthreads) next to it.
        if path.is_absolute() {
            const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
            return unsafe { libloading::os::windows::Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }.map(Into::into);
        }
    }
    unsafe { Library::new(path) }
}

pub struct AirspyBackend {
    api: std::result::Result<Arc<Api>, String>,
}

impl AirspyBackend {
    pub fn new() -> Self {
        Self { api: Api::load().map(Arc::new).map_err(|e| e.to_string()) }
    }

    fn api(&self) -> Result<Arc<Api>> {
        self.api.clone().map_err(Error::LibraryMissing)
    }
}

impl Default for AirspyBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn serial_str(sn: u64) -> String {
    format!("{sn:016X}")
}

/// Opens, reads the sample-rate list, and identifies the model by it.
fn probe(api: &Api, sn: u64) -> Result<(*mut AirspyDev, Vec<f64>, &'static str)> {
    let mut dev = std::ptr::null_mut();
    api.check("open", unsafe { (api.open_sn)(&mut dev, sn) })?;
    let mut n = 0u32;
    let rates = unsafe {
        if (api.get_samplerates)(dev, &mut n, 0) == SUCCESS && n > 0 {
            let mut buf = vec![0u32; n as usize];
            (api.get_samplerates)(dev, buf.as_mut_ptr(), n);
            buf.into_iter().map(|r| r as f64).collect()
        } else {
            vec![]
        }
    };
    // Mini firmware reports 6/3 MS/s; R2 reports 10/2.5 MS/s.
    let model = if rates.iter().any(|&r| (r - 6e6).abs() < 1.0 || (r - 3e6).abs() < 1.0) { "Airspy Mini" } else { "Airspy R2" };
    Ok((dev, rates, model))
}

impl Backend for AirspyBackend {
    fn name(&self) -> &str {
        "airspy"
    }

    fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        let mut sns = [0u64; 16];
        let n = unsafe { (api.list_devices)(sns.as_mut_ptr(), sns.len() as c_int) };
        if n < 0 {
            api.check("list_devices", n)?;
        }
        let mut out = Vec::new();
        for &sn in &sns[..(n.max(0) as usize).min(sns.len())] {
            let label = match probe(&api, sn) {
                Ok((dev, _, model)) => {
                    unsafe { (api.close)(dev) };
                    model.to_string()
                }
                Err(_) => "Airspy (in use)".to_string(),
            };
            out.push(DeviceInfo { backend: "airspy".into(), serial: serial_str(sn), label });
        }
        Ok(out)
    }

    fn open(&self, serial: &str) -> Result<Box<dyn Device>> {
        Ok(Box::new(self.open_device(serial)?))
    }
}

impl AirspyBackend {
    /// `open`, but keeps the concrete type for Airspy-only calls.
    pub fn open_device(&self, serial: &str) -> Result<AirspyDevice> {
        let api = self.api()?;
        let sn = u64::from_str_radix(serial, 16).map_err(|_| Error::NotFound(serial.into()))?;
        let (dev, rates, model) = probe(&api, sn)?;
        let rate = rates.iter().copied().fold(0.0, f64::max);
        let mut d = AirspyDevice {
            api,
            dev,
            info: DeviceInfo { backend: "airspy".into(), serial: serial.into(), label: model.into() },
            caps: Capabilities {
                freq_ranges_hz: vec![(24e6, 1.8e9)],
                tuning_step_hz: 1.0,
                // Measured on an R2 (engine/examples/chunk_probe): set_freq issued as a
                // 65536-sample transfer arrives shows up 1.4-2.4 ms into the next one.
                retune_latency_s: 0.003,
                sample_rates: rates,
                usable_bandwidth_fraction: 0.9,
                native_format: SampleFormat::Cf32,
                adc_bits: 12,
                if_arch: IfArchitecture::RealIf,
                gain_stages: vec![
                    GainStage { name: "LNA".into(), min: 0.0, max: 15.0, step: 1.0 },
                    GainStage { name: "Mixer".into(), min: 0.0, max: 15.0, step: 1.0 },
                    GainStage { name: "VGA".into(), min: 0.0, max: 15.0, step: 1.0 },
                ],
                has_linearity_sensitivity_presets: true,
                native_sweep: false,
                channels: 1,
                ext_clock: false,
                bias_tee: true,
                hw_timestamps: false,
                spur_grid_hz: Some(5e6),
                transfer_samples: Some(65536),
            },
            center: Arc::new(AtomicU64::new(100e6f64.to_bits())),
            rate,
            ctx: std::ptr::null_mut(),
        };
        d.api.check("set_sample_type", unsafe { (d.api.set_sample_type)(dev, SAMPLE_FLOAT32_IQ) })?;
        d.api.check("set_samplerate", unsafe { (d.api.set_samplerate)(dev, rate as u32) })?;
        d.set_center(100e6)?;
        d.set_gain(GainSetting::Linearity(0.6))?;
        Ok(d)
    }
}

impl AirspyDevice {
    /// Reads one R820T tuner register (through the Airspy's firmware).
    pub fn r820t_read(&self, reg: u8) -> Result<u8> {
        let mut v = 0u8;
        self.api.check("r820t_read", unsafe { (self.api.r820t_read)(self.dev, reg, &mut v) })?;
        Ok(v)
    }
}

/// State owned by the libairspy callback thread.
struct CbCtx {
    sink: Box<dyn StreamSink>,
    n: u64,
    center: Arc<AtomicU64>,
    reported_center: u64,
    rate: f64,
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // Never unwind into C.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let t = &*t;
        let ctx = &mut *(t.ctx as *mut CbCtx);
        if t.dropped_samples > 0 {
            ctx.sink.on_event(StreamEvent::Gap { at_sample: ctx.n, lost_samples: t.dropped_samples });
            ctx.n += t.dropped_samples;
        }
        let c = ctx.center.load(Ordering::Relaxed);
        if c != ctx.reported_center {
            ctx.reported_center = c;
            ctx.sink.on_event(StreamEvent::Retune { at_sample: ctx.n, center_hz: f64::from_bits(c), sample_rate: ctx.rate });
        }
        let s = std::slice::from_raw_parts(t.samples as *const Complex32, t.sample_count.max(0) as usize);
        ctx.sink.on_samples(ctx.n, Samples::Cf32(s));
        ctx.n += s.len() as u64;
    }));
    if r.is_err() { -1 } else { 0 }
}

pub struct AirspyDevice {
    api: Arc<Api>,
    dev: *mut AirspyDev,
    info: DeviceInfo,
    caps: Capabilities,
    center: Arc<AtomicU64>,
    rate: f64,
    ctx: *mut CbCtx,
}

// The raw handle is only used through libairspy, which is thread-safe for the
// calls we make; the callback context is touched only by the callback thread
// between start_rx and stop_rx.
unsafe impl Send for AirspyDevice {}

impl Device for AirspyDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn center_hz(&self) -> f64 {
        f64::from_bits(self.center.load(Ordering::Relaxed))
    }

    fn sample_rate(&self) -> f64 {
        self.rate
    }

    fn set_center(&mut self, hz: f64) -> Result<()> {
        let (lo, hi) = self.caps.freq_ranges_hz[0];
        if !(lo..=hi).contains(&hz) {
            return Err(Error::Unsupported(format!("{:.3} MHz out of range", hz / 1e6)));
        }
        self.api.check("set_freq", unsafe { (self.api.set_freq)(self.dev, hz.round() as u32) })?;
        self.center.store(hz.round().to_bits(), Ordering::Relaxed);
        Ok(())
    }

    fn tuner_locked(&mut self) -> Option<bool> {
        // R820T status register 2: bit 6 = PLL locked (bits 5:0 are the VCO
        // code, which counts through its range during a slow lock).
        self.r820t_read(2).ok().map(|v| v & 0x40 != 0)
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        if self.is_streaming() {
            return Err(Error::Unsupported("change rate while streaming".into()));
        }
        self.api.check("set_samplerate", unsafe { (self.api.set_samplerate)(self.dev, rate as u32) })?;
        self.rate = rate;
        Ok(())
    }

    fn set_gain(&mut self, gain: GainSetting) -> Result<()> {
        let a = &self.api;
        let step = |v: f32| (v.clamp(0.0, 1.0) * 21.0).round() as u8;
        match gain {
            GainSetting::Linearity(v) => a.check("set_linearity_gain", unsafe { (a.set_linearity_gain)(self.dev, step(v)) }),
            GainSetting::Sensitivity(v) => a.check("set_sensitivity_gain", unsafe { (a.set_sensitivity_gain)(self.dev, step(v)) }),
            GainSetting::Auto => {
                a.check("set_lna_agc", unsafe { (a.set_lna_agc)(self.dev, 1) })?;
                a.check("set_mixer_agc", unsafe { (a.set_mixer_agc)(self.dev, 1) })
            }
        }
    }

    fn start(&mut self, mut sink: Box<dyn StreamSink>) -> Result<()> {
        if !self.ctx.is_null() {
            return Ok(());
        }
        let center = self.center.load(Ordering::Relaxed);
        sink.on_event(StreamEvent::Retune { at_sample: 0, center_hz: f64::from_bits(center), sample_rate: self.rate });
        sink.on_event(StreamEvent::TimeAnchor { at_sample: 0, host_time: SystemTime::now() });
        let ctx = Box::into_raw(Box::new(CbCtx { sink, n: 0, center: self.center.clone(), reported_center: center, rate: self.rate }));
        let rc = unsafe { (self.api.start_rx)(self.dev, rx_callback, ctx as *mut c_void) };
        if let Err(e) = self.api.check("start_rx", rc) {
            drop(unsafe { Box::from_raw(ctx) });
            return Err(e);
        }
        self.ctx = ctx;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if self.ctx.is_null() {
            return Ok(());
        }
        let rc = unsafe { (self.api.stop_rx)(self.dev) };
        // stop_rx joins libairspy's threads, so the callback can no longer run.
        drop(unsafe { Box::from_raw(self.ctx) });
        self.ctx = std::ptr::null_mut();
        self.api.check("stop_rx", rc)
    }

    fn is_streaming(&self) -> bool {
        !self.ctx.is_null() && unsafe { (self.api.is_streaming)(self.dev) } == TRUE
    }
}

impl Drop for AirspyDevice {
    fn drop(&mut self) {
        let _ = self.stop();
        unsafe { (self.api.close)(self.dev) };
    }
}
