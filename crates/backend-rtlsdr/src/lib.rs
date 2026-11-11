//! RTL-SDR backend (RTL2832U dongles: R820T/R820T2/R828D, E4000, FC001x)
//! over librtlsdr, loaded at run time so a missing library just hides the
//! backend. The RTL-SDR Blog V4 needs that project's build of librtlsdr; the
//! osmocom one doesn't know its tuner.
//!
//! librtlsdr's `read_async` blocks, so each open device streams on a thread of
//! its own, which owns the callback context and is joined on `stop`. Samples
//! arrive as offset-binary u8 pairs and are delivered as `Cf32` after a slow
//! DC estimate is taken out (the dongle is zero-IF, with a spike at the
//! centre). Retunes are reported in band at the sample they are estimated to
//! land on, a `TimeAnchor` follows at least once a second, `Overload` marks
//! clipping blocks, and samples the USB link loses are reported as a `Gap`
//! counted against the clock (librtlsdr reports no drops itself).
//!
//! Environment:
//! - `SPECTRAWL_RTLSDR_LIB`: path to `librtlsdr.so` / `rtlsdr.dll`.
//! - `SPECTRAWL_RTLSDR_RATE`: sample rate, in Hz or MS/s (default 2.4 MS/s).
//! - `SPECTRAWL_RTLSDR_PPM`: frequency correction in ppm (default 0).
//! - `SPECTRAWL_RTLSDR_BIAS_TEE`: `1` powers the antenna input (where the
//!   library and dongle support it).

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use libloading::Library;
use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, GainStage, IfArchitecture, Result, SampleFormat, Samples,
    StreamEvent, StreamSink,
};

#[repr(C)]
struct RtlDev {
    _p: [u8; 0],
}

type ReadCb = unsafe extern "C" fn(*mut c_uchar, u32, *mut c_void);

/// Bytes per USB transfer (a multiple of 512): 16384 samples, ~6.8 ms at 2.4 MS/s.
const BUF_LEN: u32 = 32_768;
/// Transfers queued in libusb (~110 ms at 2.4 MS/s).
const BUF_NUM: u32 = 16;
const TRANSFER_SAMPLES: usize = BUF_LEN as usize / 2;

/// The rate a device opens at when `SPECTRAWL_RTLSDR_RATE` doesn't say: the
/// highest most dongles hold without dropping samples.
const DEFAULT_RATE: f64 = 2.4e6;
/// Rates RTL2832U dongles take reliably (the chip also takes 225–300 kS/s).
const RATES: [f64; 9] = [1.024e6, 1.2e6, 1.44e6, 1.8e6, 2.048e6, 2.4e6, 2.56e6, 2.88e6, 3.2e6];

/// How long after `set_center_freq` returns the tuner's output actually
/// changes (the PLL settling plus the RTL2832's own filters).
const LANDS_AFTER: Duration = Duration::from_millis(5);

/// Fraction of clipped samples in a block that counts as overload.
const OVERLOAD_FRACTION: f32 = 0.01;

/// Per-block weight of the DC estimate (time constant ~20 transfers, ~140 ms at 2.4 MS/s).
const DC_ALPHA: f32 = 0.05;

/// Tuner gain steps the engine's manual control spans (its stages are set 0–15).
const MANUAL_STEPS: u8 = 15;

/// Process-wide epoch for passing `Instant`s through an `AtomicU64`.
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

fn instant_to_nanos(t: Instant) -> u64 {
    u64::try_from(t.saturating_duration_since(epoch()).as_nanos()).unwrap_or(u64::MAX)
}

fn nanos_to_instant(n: u64) -> Instant {
    epoch() + Duration::from_nanos(n)
}

/// The sample index inside `[block_first, block_first + block_len]` where a
/// retune requested at `set_at` took effect, assuming the block's last sample
/// arrived at `block_end` and the hardware applies a tune `lands_after` after
/// the request. Clamped to the block.
fn landing_sample(block_first: u64, block_len: usize, rate: f64, block_end: Instant, set_at: Instant, lands_after: Duration) -> u64 {
    let block_time = Duration::from_secs_f64(block_len as f64 / rate);
    let block_start = block_end.checked_sub(block_time).unwrap_or(block_end);
    let landing = set_at + lands_after;
    let offset = (landing.saturating_duration_since(block_start).as_secs_f64() * rate) as u64;
    block_first + offset.min(block_len as u64)
}

/// Offset-binary u8 IQ pairs to `Complex32` in ±1, appended to `out`.
/// Returns how many samples have I or Q at either rail.
fn convert(bytes: &[u8], out: &mut Vec<Complex32>) -> usize {
    let mut clipped = 0usize;
    out.reserve(bytes.len() / 2);
    for p in bytes.as_chunks::<2>().0 {
        clipped += usize::from(matches!(p[0], 0 | 255) | matches!(p[1], 0 | 255));
        out.push(Complex32::new((f32::from(p[0]) - 127.5) / 128.0, (f32::from(p[1]) - 127.5) / 128.0));
    }
    clipped
}

/// The index into a tuner's gain table (`n` entries, ascending) for a 0–1 setting.
fn gain_index(v: f32, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    ((v.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize).min(n - 1)
}

/// Device ids from each index's USB serial: the serial itself when it is
/// present and unique, else `serial#index` (cheap dongles often all say
/// "00000001"; write a distinct one with `rtl_eeprom -s` to keep a stable id).
fn device_ids(serials: &[Option<String>]) -> Vec<String> {
    serials
        .iter()
        .enumerate()
        .map(|(i, s)| match s.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) if serials.iter().filter(|o| o.as_deref().map(str::trim) == Some(s)).count() == 1 => s.to_string(),
            Some(s) => format!("{s}#{i}"),
            None => format!("#{i}"),
        })
        .collect()
}

/// The device index a `device_ids` id names, given the serials now attached.
fn index_of(id: &str, serials: &[Option<String>]) -> Option<u32> {
    if let Some((_, i)) = id.rsplit_once('#') {
        let i: usize = i.parse().ok()?;
        return (i < serials.len()).then_some(i as u32);
    }
    serials.iter().position(|s| s.as_deref().map(str::trim) == Some(id)).map(|i| i as u32)
}

/// What each tuner covers (librtlsdr's `rtlsdr_tuner` enum).
fn tuner(kind: c_int) -> (&'static str, Vec<(f64, f64)>) {
    match kind {
        1 => ("E4000", vec![(52e6, 1.1e9), (1.25e9, 2.2e9)]),
        2 => ("FC0012", vec![(22e6, 948.6e6)]),
        3 => ("FC0013", vec![(22e6, 1.1e9)]),
        4 => ("FC2580", vec![(146e6, 308e6), (438e6, 924e6)]),
        5 => ("R820T", vec![(24e6, 1.766e9)]),
        6 => ("R828D", vec![(24e6, 1.766e9)]),
        _ => ("unknown tuner", vec![(24e6, 1.766e9)]),
    }
}

fn env_rate() -> f64 {
    let Some(v) = std::env::var("SPECTRAWL_RTLSDR_RATE").ok().and_then(|s| s.trim().parse::<f64>().ok()) else {
        return DEFAULT_RATE;
    };
    let hz = if v < 1000.0 { v * 1e6 } else { v };
    if (225_001.0..=300_000.0).contains(&hz) || (900_001.0..=3_200_000.0).contains(&hz) {
        hz
    } else {
        log::warn!("SPECTRAWL_RTLSDR_RATE={v}: an RTL2832U takes 0.225–0.3 or 0.9–3.2 MS/s; using {DEFAULT_RATE}");
        DEFAULT_RATE
    }
}

/// Resolved librtlsdr entry points. The `Library` is kept alive alongside.
struct Api {
    _lib: Library,
    get_device_count: unsafe extern "C" fn() -> u32,
    get_device_name: unsafe extern "C" fn(u32) -> *const c_char,
    get_device_usb_strings: unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int,
    open: unsafe extern "C" fn(*mut *mut RtlDev, u32) -> c_int,
    close: unsafe extern "C" fn(*mut RtlDev) -> c_int,
    set_center_freq: unsafe extern "C" fn(*mut RtlDev, u32) -> c_int,
    set_freq_correction: unsafe extern "C" fn(*mut RtlDev, c_int) -> c_int,
    get_tuner_type: unsafe extern "C" fn(*mut RtlDev) -> c_int,
    get_tuner_gains: unsafe extern "C" fn(*mut RtlDev, *mut c_int) -> c_int,
    set_tuner_gain: unsafe extern "C" fn(*mut RtlDev, c_int) -> c_int,
    set_tuner_gain_mode: unsafe extern "C" fn(*mut RtlDev, c_int) -> c_int,
    set_sample_rate: unsafe extern "C" fn(*mut RtlDev, u32) -> c_int,
    set_agc_mode: unsafe extern "C" fn(*mut RtlDev, c_int) -> c_int,
    reset_buffer: unsafe extern "C" fn(*mut RtlDev) -> c_int,
    read_async: unsafe extern "C" fn(*mut RtlDev, ReadCb, *mut c_void, u32, u32) -> c_int,
    cancel_async: unsafe extern "C" fn(*mut RtlDev) -> c_int,
    /// Newer librtlsdr builds only (0.6+, the RTL-SDR Blog fork).
    set_bias_tee: Option<unsafe extern "C" fn(*mut RtlDev, c_int) -> c_int>,
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
        Err(Error::LibraryMissing(format!("librtlsdr not found (tried {})", tried.join("; "))))
    }

    unsafe fn resolve(lib: Library) -> std::result::Result<Self, libloading::Error> {
        unsafe {
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!("rtlsdr_", $name, "\0").as_bytes())?
                };
            }
            Ok(Self {
                get_device_count: sym!("get_device_count"),
                get_device_name: sym!("get_device_name"),
                get_device_usb_strings: sym!("get_device_usb_strings"),
                open: sym!("open"),
                close: sym!("close"),
                set_center_freq: sym!("set_center_freq"),
                set_freq_correction: sym!("set_freq_correction"),
                get_tuner_type: sym!("get_tuner_type"),
                get_tuner_gains: sym!("get_tuner_gains"),
                set_tuner_gain: sym!("set_tuner_gain"),
                set_tuner_gain_mode: sym!("set_tuner_gain_mode"),
                set_sample_rate: sym!("set_sample_rate"),
                set_agc_mode: sym!("set_agc_mode"),
                reset_buffer: sym!("reset_buffer"),
                read_async: sym!("read_async"),
                cancel_async: sym!("cancel_async"),
                set_bias_tee: lib.get(b"rtlsdr_set_bias_tee\0").ok().map(|s| *s),
                _lib: lib,
            })
        }
    }

    fn check(what: &str, rc: c_int) -> Result<()> {
        if rc == 0 { Ok(()) } else { Err(Error::Driver(format!("rtlsdr_{what} failed ({rc})"))) }
    }

    /// Each attached dongle's USB serial string, by index (`None` when it
    /// can't be read).
    fn serials(&self) -> Vec<Option<String>> {
        let n = unsafe { (self.get_device_count)() };
        (0..n)
            .map(|i| {
                let (mut m, mut p, mut s) = ([0 as c_char; 256], [0 as c_char; 256], [0 as c_char; 256]);
                let rc = unsafe { (self.get_device_usb_strings)(i, m.as_mut_ptr(), p.as_mut_ptr(), s.as_mut_ptr()) };
                (rc == 0).then(|| unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().into_owned())
            })
            .collect()
    }

    fn name(&self, index: u32) -> String {
        let p = unsafe { (self.get_device_name)(index) };
        let name = if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string() };
        if name.is_empty() { "RTL-SDR".into() } else { name }
    }

    /// The tuner's gain table in tenths of a dB, ascending.
    fn gains(&self, dev: *mut RtlDev) -> Vec<c_int> {
        let n = unsafe { (self.get_tuner_gains)(dev, std::ptr::null_mut()) };
        if n <= 0 {
            return Vec::new();
        }
        let mut v = vec![0 as c_int; n as usize];
        let got = unsafe { (self.get_tuner_gains)(dev, v.as_mut_ptr()) };
        v.truncate(got.max(0) as usize);
        v.sort_unstable();
        v
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("SPECTRAWL_RTLSDR_LIB") {
        v.push(PathBuf::from(p));
    }
    if cfg!(windows) {
        v.push("rtlsdr.dll".into());
        v.push("librtlsdr.dll".into());
        v.push(r"C:\Program Files\PothosSDR\bin\rtlsdr.dll".into());
    } else if cfg!(target_os = "macos") {
        v.push("librtlsdr.dylib".into());
        v.push("librtlsdr.0.dylib".into());
        v.push("/opt/homebrew/lib/librtlsdr.dylib".into());
        v.push("/usr/local/lib/librtlsdr.dylib".into());
    } else {
        v.push("librtlsdr.so.0".into());
        v.push("librtlsdr.so.2".into());
        v.push("librtlsdr.so".into());
    }
    v
}

fn open_library(path: &Path) -> std::result::Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        // For a full path, let Windows find the DLL's own dependencies
        // (libusb-1.0.dll, pthreads) next to it.
        if path.is_absolute() {
            const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
            return unsafe { libloading::os::windows::Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }.map(Into::into);
        }
    }
    unsafe { Library::new(path) }
}

pub struct RtlSdrBackend {
    api: std::result::Result<Arc<Api>, String>,
}

impl RtlSdrBackend {
    pub fn new() -> Self {
        let api = Api::load().map(Arc::new).map_err(|e| match e {
            Error::LibraryMissing(m) => m,
            e => e.to_string(),
        });
        Self { api }
    }

    fn api(&self) -> Result<Arc<Api>> {
        self.api.clone().map_err(Error::LibraryMissing)
    }
}

impl Default for RtlSdrBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for RtlSdrBackend {
    fn name(&self) -> &str {
        "rtlsdr"
    }

    fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        let serials = api.serials();
        Ok(device_ids(&serials)
            .into_iter()
            .enumerate()
            .map(|(i, serial)| {
                let mut label = api.name(i as u32);
                if serials[i].is_none() {
                    label.push_str(if cfg!(windows) { " (in use, or no WinUSB driver)" } else { " (in use)" });
                }
                DeviceInfo { backend: "rtlsdr".into(), serial, label }
            })
            .collect())
    }

    fn open(&self, serial: &str) -> Result<Box<dyn Device>> {
        Ok(Box::new(self.open_device(serial)?))
    }
}

impl RtlSdrBackend {
    /// `open`, but keeps the concrete type.
    pub fn open_device(&self, serial: &str) -> Result<RtlSdrDevice> {
        let api = self.api()?;
        let index = index_of(serial, &api.serials()).ok_or_else(|| Error::NotFound(serial.into()))?;
        let mut dev = std::ptr::null_mut();
        Api::check("open", unsafe { (api.open)(&mut dev, index) })?;
        if dev.is_null() {
            return Err(Error::Driver("rtlsdr_open returned no device".into()));
        }
        // Nothing below fails before `d` owns the handle (and closes it on an error).
        let (tuner_name, ranges) = tuner(unsafe { (api.get_tuner_type)(dev) });
        let gains = api.gains(dev);
        let label = format!("{} ({tuner_name})", api.name(index));
        let (lo_db, hi_db) = (gains.first().map_or(0.0, |g| *g as f32 / 10.0), gains.last().map_or(0.0, |g| *g as f32 / 10.0));
        let rate = env_rate();
        let mut sample_rates = RATES.to_vec();
        if !sample_rates.contains(&rate) {
            sample_rates.push(rate);
            sample_rates.sort_by(f64::total_cmp);
        }
        let bias_tee = api.set_bias_tee.is_some();
        let mut d = RtlSdrDevice {
            api,
            dev,
            info: DeviceInfo { backend: "rtlsdr".into(), serial: serial.into(), label },
            caps: Capabilities {
                freq_ranges_hz: ranges,
                tuning_step_hz: 1.0,
                retune_latency_s: 0.012,
                sample_rates,
                // The RTL2832's decimation filter rolls off in the outer ~10% each side.
                usable_bandwidth_fraction: 0.8,
                native_format: SampleFormat::Cf32,
                adc_bits: 8,
                if_arch: IfArchitecture::ZeroIf,
                gain_stages: if gains.is_empty() {
                    vec![]
                } else {
                    vec![GainStage { name: format!("Tuner gain (0–15 = {lo_db:.1}–{hi_db:.1} dB)"), min: 0.0, max: 15.0, step: 1.0 }]
                },
                has_linearity_sensitivity_presets: false,
                native_sweep: false,
                channels: 1,
                ext_clock: false,
                bias_tee,
                hw_timestamps: false,
                // Harmonics of the 28.8 MHz reference crystal.
                spur_grid_hz: Some(28.8e6),
                transfer_samples: Some(TRANSFER_SAMPLES),
            },
            gains,
            center: Arc::new(AtomicU64::new(100e6f64.to_bits())),
            center_set_at: Arc::new(AtomicU64::new(0)),
            rate,
            stream: None,
        };
        Api::check("set_sample_rate", unsafe { (d.api.set_sample_rate)(dev, rate as u32) })?;
        // The RTL2832's own digital AGC only adds distortion; gain is the tuner's.
        Api::check("set_agc_mode", unsafe { (d.api.set_agc_mode)(dev, 0) })?;
        if let Some(ppm) = std::env::var("SPECTRAWL_RTLSDR_PPM").ok().and_then(|s| s.trim().parse::<c_int>().ok()).filter(|p| *p != 0) {
            Api::check("set_freq_correction", unsafe { (d.api.set_freq_correction)(dev, ppm) })?;
        }
        if let Some(f) = d.api.set_bias_tee {
            let on = std::env::var("SPECTRAWL_RTLSDR_BIAS_TEE").is_ok_and(|v| v.trim() == "1");
            Api::check("set_bias_tee", unsafe { f(dev, c_int::from(on)) })?;
        }
        d.set_center(100e6)?;
        d.set_gain(GainSetting::Linearity(0.6))?;
        log::info!("{}: {} at {:.3} MS/s, {} gain steps", d.info.serial, d.info.label, rate / 1e6, d.gains.len());
        Ok(d)
    }
}

/// State owned by the streaming thread (the `read_async` callback runs on it).
struct CbCtx {
    sink: Box<dyn StreamSink>,
    n: u64,
    center: Arc<AtomicU64>,
    center_set_at: Arc<AtomicU64>,
    reported_center: u64,
    rate: f64,
    last_anchor: u64,
    last_overload: Option<u64>,
    serial: String,
    buf: Vec<Complex32>,
    /// The running DC estimate, `None` until the first block.
    dc: Option<Complex32>,
    /// Start of the current delivery-rate window (time, sample count), and
    /// whether it is the first (start-up buffering, not judged).
    window: Option<(Instant, u64, bool)>,
    warned: Option<Instant>,
}

/// Delivery is judged over windows this long.
const RATE_WINDOW: Duration = Duration::from_secs(2);
/// A window short by more than this share of its samples lost some.
const RATE_SHORT: f64 = 0.05;

impl CbCtx {
    fn on_block(&mut self, bytes: &[u8], block_end: Instant) {
        self.buf.clear();
        let clipped = convert(bytes, &mut self.buf);
        if self.buf.is_empty() {
            return;
        }
        let mean = self.buf.iter().sum::<Complex32>() / self.buf.len() as f32;
        let dc = match self.dc {
            Some(dc) => dc + (mean - dc) * DC_ALPHA,
            None => mean,
        };
        self.dc = Some(dc);
        for c in &mut self.buf {
            *c -= dc;
        }
        // Centre first, then its request time: `set_center` stores them in the
        // opposite order with Release, so a new centre comes with its time.
        let c = self.center.load(Ordering::Acquire);
        if c != self.reported_center {
            self.reported_center = c;
            let set_at = nanos_to_instant(self.center_set_at.load(Ordering::Acquire));
            let at_sample = landing_sample(self.n, self.buf.len(), self.rate, block_end, set_at, LANDS_AFTER);
            self.sink.on_event(StreamEvent::Retune { at_sample, center_hz: f64::from_bits(c), sample_rate: self.rate });
        }
        if clipped as f32 >= OVERLOAD_FRACTION * self.buf.len() as f32 && self.last_overload.is_none_or(|o| self.n - o >= self.rate as u64)
        {
            self.last_overload = Some(self.n);
            self.sink.on_event(StreamEvent::Overload { at_sample: self.n });
        }
        self.sink.on_samples(self.n, Samples::Cf32(&self.buf));
        self.n += self.buf.len() as u64;
        self.check_delivery();
        if self.n - self.last_anchor >= self.rate as u64 {
            self.last_anchor = self.n;
            self.sink.on_event(StreamEvent::TimeAnchor { at_sample: self.n, host_time: SystemTime::now() });
        }
    }

    /// librtlsdr drops samples silently when the host doesn't keep up (a busy
    /// USB controller, a rate the dongle can't hold). Counts what arrived
    /// against the clock and reports the shortfall as a gap, so the timeline
    /// stays true to wall time.
    fn check_delivery(&mut self) {
        let now = Instant::now();
        let Some((t0, n0, first)) = self.window else {
            self.window = Some((now, self.n, true));
            return;
        };
        let dt = now - t0;
        if dt < RATE_WINDOW {
            return;
        }
        let expected = dt.as_secs_f64() * self.rate;
        let short = expected - (self.n - n0) as f64;
        if !first && short > RATE_SHORT * expected {
            let lost = short as u64;
            self.sink.on_event(StreamEvent::Gap { at_sample: self.n, lost_samples: lost });
            self.n += lost;
            if self.warned.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
                self.warned = Some(now);
                log::warn!(
                    "{}: delivering {:.2} of {:.2} MS/s, the rest lost on the USB link (try a lower SPECTRAWL_RTLSDR_RATE)",
                    self.serial,
                    (expected - short) / dt.as_secs_f64() / 1e6,
                    self.rate / 1e6
                );
            }
        }
        self.window = Some((now, self.n, false));
    }
}

unsafe extern "C" fn rx_callback(buf: *mut c_uchar, len: u32, ctx: *mut c_void) {
    // The transfer just completed, so "now" is when its last sample arrived.
    let block_end = Instant::now();
    // Never unwind into C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if buf.is_null() || ctx.is_null() {
            return;
        }
        let ctx = &mut *(ctx as *mut CbCtx);
        ctx.on_block(std::slice::from_raw_parts(buf, len as usize), block_end);
    }));
}

/// The device handle, carried to the streaming thread.
struct DevPtr(*mut RtlDev);

// librtlsdr is used from two threads the way rtl_tcp uses it: `read_async`
// on one, tuning, gain and `cancel_async` on the other.
unsafe impl Send for DevPtr {}

impl DevPtr {
    /// Through a method, so a closure captures the whole (Send) wrapper.
    fn get(&self) -> *mut RtlDev {
        self.0
    }
}

struct Stream {
    thread: JoinHandle<()>,
    /// Set by `stop`, so the thread knows `read_async` returned on request.
    cancelled: Arc<AtomicBool>,
}

pub struct RtlSdrDevice {
    api: Arc<Api>,
    dev: *mut RtlDev,
    info: DeviceInfo,
    caps: Capabilities,
    /// The tuner's gain table, tenths of a dB, ascending.
    gains: Vec<c_int>,
    center: Arc<AtomicU64>,
    center_set_at: Arc<AtomicU64>,
    rate: f64,
    stream: Option<Stream>,
}

// The handle is used only through librtlsdr (see `DevPtr`).
unsafe impl Send for RtlSdrDevice {}

impl RtlSdrDevice {
    fn set_gain_index(&self, i: usize) -> Result<()> {
        let Some(&g) = self.gains.get(i) else { return Ok(()) };
        Api::check("set_tuner_gain_mode", unsafe { (self.api.set_tuner_gain_mode)(self.dev, 1) })?;
        Api::check("set_tuner_gain", unsafe { (self.api.set_tuner_gain)(self.dev, g) })
    }
}

impl Device for RtlSdrDevice {
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
        if !self.caps.freq_ranges_hz.iter().any(|&(lo, hi)| (lo..=hi).contains(&hz)) {
            return Err(Error::Unsupported(format!("{:.3} MHz out of range", hz / 1e6)));
        }
        Api::check("set_center_freq", unsafe { (self.api.set_center_freq)(self.dev, hz.round() as u32) })?;
        // Time first, centre second (Release): the callback reads them in the
        // opposite order, so a new centre is never paired with a stale time.
        self.center_set_at.store(instant_to_nanos(Instant::now()), Ordering::Release);
        self.center.store(hz.round().to_bits(), Ordering::Release);
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        if self.is_streaming() {
            return Err(Error::Unsupported("change rate while streaming".into()));
        }
        Api::check("set_sample_rate", unsafe { (self.api.set_sample_rate)(self.dev, rate as u32) })?;
        self.rate = rate;
        Ok(())
    }

    fn set_gain(&mut self, gain: GainSetting) -> Result<()> {
        let n = self.gains.len();
        match gain {
            GainSetting::Auto => Api::check("set_tuner_gain_mode", unsafe { (self.api.set_tuner_gain_mode)(self.dev, 0) }),
            GainSetting::Linearity(v) | GainSetting::Sensitivity(v) => self.set_gain_index(gain_index(v, n)),
            GainSetting::Manual { lna, .. } => {
                self.set_gain_index(gain_index(f32::from(lna.min(MANUAL_STEPS)) / f32::from(MANUAL_STEPS), n))
            }
        }
    }

    fn start(&mut self, mut sink: Box<dyn StreamSink>) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        Api::check("reset_buffer", unsafe { (self.api.reset_buffer)(self.dev) })?;
        let center = self.center.load(Ordering::Relaxed);
        sink.on_event(StreamEvent::Retune { at_sample: 0, center_hz: f64::from_bits(center), sample_rate: self.rate });
        sink.on_event(StreamEvent::TimeAnchor { at_sample: 0, host_time: SystemTime::now() });
        let mut ctx = Box::new(CbCtx {
            sink,
            n: 0,
            center: self.center.clone(),
            center_set_at: self.center_set_at.clone(),
            reported_center: center,
            rate: self.rate,
            last_anchor: 0,
            last_overload: None,
            serial: self.info.serial.clone(),
            buf: Vec::with_capacity(TRANSFER_SAMPLES),
            dc: None,
            window: None,
            warned: None,
        });
        let cancelled = Arc::new(AtomicBool::new(false));
        let (api, dev, stopped) = (self.api.clone(), DevPtr(self.dev), cancelled.clone());
        let serial = self.info.serial.clone();
        let thread = std::thread::Builder::new()
            .name(format!("rtlsdr {serial}"))
            .spawn(move || {
                let p = &mut *ctx as *mut CbCtx as *mut c_void;
                let rc = unsafe { (api.read_async)(dev.get(), rx_callback, p, BUF_NUM, BUF_LEN) };
                if !stopped.load(Ordering::Acquire) {
                    log::warn!("{serial}: stream ended ({rc}); device lost");
                    ctx.sink.on_event(StreamEvent::DeviceLost);
                }
            })
            .map_err(|e| Error::Driver(format!("spawn the stream thread: {e}")))?;
        self.stream = Some(Stream { thread, cancelled });
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        let Some(s) = self.stream.take() else { return Ok(()) };
        s.cancelled.store(true, Ordering::Release);
        // `cancel_async` is refused until `read_async` has got going, so keep
        // asking until the thread has finished (bounded, in case the library hangs).
        let t0 = Instant::now();
        while !s.thread.is_finished() {
            unsafe { (self.api.cancel_async)(self.dev) };
            if t0.elapsed() > Duration::from_secs(3) {
                log::warn!("{}: the stream did not stop; leaving its thread", self.info.serial);
                return Err(Error::Driver("rtlsdr_cancel_async: the stream did not stop".into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = s.thread.join();
        Ok(())
    }

    fn is_streaming(&self) -> bool {
        self.stream.as_ref().is_some_and(|s| !s.thread.is_finished())
    }
}

impl Drop for RtlSdrDevice {
    fn drop(&mut self) {
        // Closing under a running stream would free what its thread is using.
        if self.stop().is_ok() {
            unsafe { (self.api.close)(self.dev) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_centres_and_scales_and_counts_rails() {
        let mut out = Vec::new();
        let clipped = convert(&[128, 127, 255, 0, 200, 50, 7], &mut out);
        assert_eq!(out.len(), 3, "a trailing odd byte is dropped");
        assert!((out[0].re - 0.5 / 128.0).abs() < 1e-6 && (out[0].im + 0.5 / 128.0).abs() < 1e-6);
        assert!(out[1].re > 0.99 && out[1].im < -0.99);
        assert_eq!(clipped, 1);
    }

    #[test]
    fn gain_index_spans_the_table() {
        assert_eq!(gain_index(0.0, 29), 0);
        assert_eq!(gain_index(1.0, 29), 28);
        assert_eq!(gain_index(0.5, 29), 14);
        assert_eq!(gain_index(7.0, 29), 28);
        assert_eq!(gain_index(0.5, 0), 0);
    }

    #[test]
    fn duplicate_or_missing_serials_get_their_index() {
        let s = |v: &str| Some(v.to_string());
        let serials = vec![s("00000001"), s("00000001"), s("SCANNER"), None, s("  ")];
        let ids = device_ids(&serials);
        assert_eq!(ids, ["00000001#0", "00000001#1", "SCANNER", "#3", "#4"]);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(index_of(id, &serials), Some(i as u32), "{id}");
        }
        assert_eq!(index_of("#9", &serials), None);
        assert_eq!(index_of("OTHER", &serials), None);
    }

    #[test]
    fn landing_sample_is_inside_the_block() {
        let t0 = Instant::now();
        let rate = 2.4e6;
        let block_end = t0 + Duration::from_micros(6827); // 16384 samples
        let s = landing_sample(1000, 16_384, rate, block_end, t0, Duration::from_millis(5));
        assert!((11_500..12_500).contains(&(s - 1000)), "{s}");
        assert_eq!(landing_sample(1000, 16_384, rate, block_end, block_end, LANDS_AFTER), 1000 + 16_384);
    }

    #[test]
    fn tuner_ranges_cover_the_usual_dongles() {
        assert_eq!(tuner(5).0, "R820T");
        assert!(tuner(5).1.iter().any(|&(lo, hi)| (lo..=hi).contains(&855e6)));
        assert!(tuner(1).1.iter().all(|&(lo, hi)| !(lo..=hi).contains(&1.2e9)), "the E4000's gap");
    }
}
