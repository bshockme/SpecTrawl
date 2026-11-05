//! Native Airspy backend (R2, Mini; R0/R1 too) over libairspy, loaded at
//! runtime so a missing library just hides the backend. The HydraSDR RFOne,
//! an R2-compatible board, is driven through its own libhydrasdr when that is
//! installed (the same API under a `hydrasdr_` prefix), else through
//! libairspy, which speaks to it too but can't list it.
//!
//! Samples are requested as FLOAT32_IQ (libairspy scales to ±1.0) and stored
//! by the engine as i16. Retunes are reported in-band at the sample where the
//! tune is estimated to have landed: the request time plus `LANDS_AFTER`,
//! placed inside the transfer that first shows the new centre. A `TimeAnchor`
//! follows at least once a second and `Overload` marks clipping blocks.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use libloading::Library;
use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, GainStage, IfArchitecture, NativeSweep, Result, SampleFormat,
    Samples, StreamEvent, StreamSink,
};

pub mod native;

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

#[repr(C)]
#[derive(Default)]
struct PartIdSerial {
    part_id: [u32; 2],
    serial_no: [u32; 4],
}

/// libairspy tunes in u32 Hz, libhydrasdr in u64.
enum SetFreq {
    U32(unsafe extern "C" fn(*mut AirspyDev, u32) -> c_int),
    U64(unsafe extern "C" fn(*mut AirspyDev, u64) -> c_int),
}

type RxCb = unsafe extern "C" fn(*mut Transfer) -> c_int;

const SAMPLE_FLOAT32_IQ: c_int = 0;
/// libhydrasdr: the ADC's words untouched (the firmware sweep's tagged blocks).
const SAMPLE_RAW: c_int = 5;
const SUCCESS: c_int = 0;
const TRUE: c_int = 1;

/// How long after `set_freq` the tuner's output actually changes (midpoint of
/// the measured 1.4-2.4 ms).
const LANDS_AFTER: Duration = Duration::from_millis(2);

/// Fraction of clipped samples in a block that counts as overload.
const OVERLOAD_FRACTION: f32 = 0.01;

/// Process-wide epoch for passing `Instant`s through an `AtomicU64`.
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds since the epoch (saturating at `u64::MAX`, ~584 years).
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
pub(crate) fn landing_sample(
    block_first: u64,
    block_len: usize,
    rate: f64,
    block_end: Instant,
    set_at: Instant,
    lands_after: Duration,
) -> u64 {
    let block_time = Duration::from_secs_f64(block_len as f64 / rate);
    let block_start = block_end.checked_sub(block_time).unwrap_or(block_end);
    let landing = set_at + lands_after;
    let offset = (landing.saturating_duration_since(block_start).as_secs_f64() * rate) as u64;
    block_first + offset.min(block_len as u64)
}

/// Fraction of samples with `|re| >= 0.999 || |im| >= 0.999`.
pub(crate) fn clipped_fraction(s: &[Complex32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let mut clipped = 0usize;
    for c in s {
        clipped += usize::from((c.re.abs() >= 0.999) | (c.im.abs() >= 0.999));
    }
    clipped as f32 / s.len() as f32
}

/// Resolved libairspy entry points. The `Library` is kept alive alongside.
struct Api {
    _lib: Library,
    /// libhydrasdr rather than libairspy.
    hydra: bool,
    list_devices: unsafe extern "C" fn(*mut u64, c_int) -> c_int,
    open_sn: unsafe extern "C" fn(*mut *mut AirspyDev, u64) -> c_int,
    open_any: unsafe extern "C" fn(*mut *mut AirspyDev) -> c_int,
    partid_serialno: unsafe extern "C" fn(*mut AirspyDev, *mut PartIdSerial) -> c_int,
    version_string: unsafe extern "C" fn(*mut AirspyDev, *mut c_char, u8) -> c_int,
    close: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    get_samplerates: unsafe extern "C" fn(*mut AirspyDev, *mut u32, u32) -> c_int,
    set_samplerate: unsafe extern "C" fn(*mut AirspyDev, u32) -> c_int,
    set_sample_type: unsafe extern "C" fn(*mut AirspyDev, c_int) -> c_int,
    set_packing: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    /// libhydrasdr only: the RFOne's input (0 = ANT, 1/2 = CABLE1/CABLE2).
    set_rf_port: Option<unsafe extern "C" fn(*mut AirspyDev, c_int) -> c_int>,
    start_rx: unsafe extern "C" fn(*mut AirspyDev, RxCb, *mut c_void) -> c_int,
    stop_rx: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    is_streaming: unsafe extern "C" fn(*mut AirspyDev) -> c_int,
    set_freq: SetFreq,
    set_linearity_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_sensitivity_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_lna_agc: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_mixer_agc: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_lna_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_mixer_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    set_vga_gain: unsafe extern "C" fn(*mut AirspyDev, u8) -> c_int,
    r820t_read: unsafe extern "C" fn(*mut AirspyDev, u8, *mut u8) -> c_int,
    error_name: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Api {
    fn load(hydra: bool) -> Result<Self> {
        let name = if hydra { "libhydrasdr" } else { "libairspy" };
        let mut tried = Vec::new();
        for cand in candidates(hydra) {
            match open_library(&cand) {
                Ok(lib) => {
                    return unsafe { Self::resolve(lib, hydra) }.map_err(|e| Error::LibraryMissing(format!("{}: {e}", cand.display())));
                }
                Err(e) => tried.push(format!("{}: {e}", cand.display())),
            }
        }
        Err(Error::LibraryMissing(format!("{name} not found (tried {})", tried.join("; "))))
    }

    unsafe fn resolve(lib: Library, hydra: bool) -> std::result::Result<Self, libloading::Error> {
        let prefix = if hydra { "hydrasdr_" } else { "airspy_" };
        unsafe {
            macro_rules! sym {
                ($name:expr) => {
                    *lib.get(format!("{prefix}{}\0", $name).as_bytes())?
                };
            }
            Ok(Self {
                hydra,
                list_devices: sym!("list_devices"),
                open_sn: sym!("open_sn"),
                open_any: sym!("open"),
                partid_serialno: sym!("board_partid_serialno_read"),
                version_string: sym!("version_string_read"),
                close: sym!("close"),
                get_samplerates: sym!("get_samplerates"),
                set_samplerate: sym!("set_samplerate"),
                set_sample_type: sym!("set_sample_type"),
                set_packing: sym!("set_packing"),
                set_rf_port: if hydra { Some(sym!("set_rf_port")) } else { None },
                start_rx: sym!("start_rx"),
                stop_rx: sym!("stop_rx"),
                is_streaming: sym!("is_streaming"),
                set_freq: if hydra { SetFreq::U64(sym!("set_freq")) } else { SetFreq::U32(sym!("set_freq")) },
                set_linearity_gain: sym!("set_linearity_gain"),
                set_sensitivity_gain: sym!("set_sensitivity_gain"),
                set_lna_agc: sym!("set_lna_agc"),
                set_mixer_agc: sym!("set_mixer_agc"),
                set_lna_gain: sym!("set_lna_gain"),
                set_mixer_gain: sym!("set_mixer_gain"),
                set_vga_gain: sym!("set_vga_gain"),
                r820t_read: sym!(if hydra { "r82x_read" } else { "r820t_read" }),
                error_name: sym!("error_name"),
                _lib: lib,
            })
        }
    }

    fn set_freq(&self, dev: *mut AirspyDev, hz: f64) -> c_int {
        match self.set_freq {
            SetFreq::U32(f) => unsafe { f(dev, hz.round() as u32) },
            SetFreq::U64(f) => unsafe { f(dev, hz.round() as u64) },
        }
    }

    fn list(&self) -> Result<Vec<u64>> {
        let mut sns = [0u64; 16];
        let n = unsafe { (self.list_devices)(sns.as_mut_ptr(), sns.len() as c_int) };
        if n < 0 {
            self.check("list_devices", n)?;
        }
        Ok(sns[..(n.max(0) as usize).min(sns.len())].to_vec())
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

fn candidates(hydra: bool) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if hydra {
        if let Ok(p) = std::env::var("SPECTRAWL_HYDRASDR_LIB") {
            v.push(PathBuf::from(p));
        }
        if cfg!(windows) {
            v.push("hydrasdr.dll".into());
        } else if cfg!(target_os = "macos") {
            v.push("libhydrasdr.dylib".into());
            v.push("/opt/homebrew/lib/libhydrasdr.dylib".into());
            v.push("/usr/local/lib/libhydrasdr.dylib".into());
        } else {
            v.push("libhydrasdr.so.1".into());
            v.push("libhydrasdr.so".into());
        }
        return v;
    }
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
    hydra: Option<Arc<Api>>,
}

impl AirspyBackend {
    pub fn new() -> Self {
        let hydra = Api::load(true).map(Arc::new);
        if let Err(e) = &hydra {
            log::debug!("{e}");
        }
        Self { api: Api::load(false).map(Arc::new).map_err(|e| e.to_string()), hydra: hydra.ok() }
    }

    /// The libraries to try, libhydrasdr first: libairspy's lookups skip
    /// HydraSDR boards, libhydrasdr's skip Airspys.
    fn apis(&self) -> Result<Vec<Arc<Api>>> {
        let mut v: Vec<Arc<Api>> = self.hydra.iter().cloned().collect();
        match &self.api {
            Ok(a) => v.push(a.clone()),
            Err(e) if v.is_empty() => return Err(Error::LibraryMissing(e.clone())),
            Err(_) => {}
        }
        Ok(v)
    }

    /// Whether to look for HydraSDR boards through libairspy (when
    /// libhydrasdr is missing).
    fn scan_unlisted(&self) -> bool {
        self.hydra.is_none()
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

/// The USB bus (one host controller's USB 2 side) the Airspy with this
/// serial is on, from Linux sysfs; `None` elsewhere or when not found.
/// Two Airspys streaming on one bus corrupt each other's samples in
/// bursts (seen on eddie 2026-11-03: no P25 or FLEX decoded at all, strong
/// signals smeared 13–26 dB, a +22 dB line at the other's centre); on
/// separate buses they don't.
pub fn usb_bus(serial: &str) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let want = serial.trim().to_ascii_uppercase();
        for e in std::fs::read_dir("/sys/bus/usb/devices").ok()?.flatten() {
            let Ok(s) = std::fs::read_to_string(e.path().join("serial")) else { continue };
            let Some((_, sn)) = s.trim().split_once("SN:") else { continue };
            if sn.trim().to_ascii_uppercase() == want {
                return std::fs::read_to_string(e.path().join("busnum")).ok()?.trim().parse().ok();
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = serial;
        None
    }
}

/// The serial number the board itself reports (the same one libairspy
/// lists and `airspy_info` prints).
fn board_serial(api: &Api, dev: *mut AirspyDev) -> Option<u64> {
    let mut r = PartIdSerial::default();
    (unsafe { (api.partid_serialno)(dev, &mut r) } == SUCCESS).then(|| (u64::from(r.serial_no[2]) << 32) | u64::from(r.serial_no[3]))
}

fn firmware(api: &Api, dev: *mut AirspyDev) -> String {
    let mut buf = [0 as c_char; 128];
    if unsafe { (api.version_string)(dev, buf.as_mut_ptr(), buf.len() as u8 - 1) } != SUCCESS {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// Opens every Airspy-protocol board nobody holds yet, with the serial each
/// reports. libairspy's `list_devices` and `open_sn` only see boards whose USB
/// serial string starts "AIRSPY SN:", so clones that speak the same protocol
/// under their own name (the HydraSDR RFOne says "HYDRASDR SN:") are found
/// this way. The caller closes what it doesn't keep.
fn open_unlisted(api: &Api) -> Vec<(*mut AirspyDev, u64)> {
    let mut out = Vec::new();
    for _ in 0..16 {
        let mut dev = std::ptr::null_mut();
        if unsafe { (api.open_any)(&mut dev) } != SUCCESS || dev.is_null() {
            break;
        }
        match board_serial(api, dev) {
            Some(sn) => out.push((dev, sn)),
            None => {
                unsafe { (api.close)(dev) };
                break;
            }
        }
    }
    out
}

/// Opens by serial through the first library that finds the board (the
/// last, libairspy, may also open unlisted boards and keep the one with this
/// serial). Another process enumerating holds each board for a moment, so a
/// miss is retried.
fn open_serial(apis: &[Arc<Api>], unlisted: bool, sn: u64) -> Result<(Arc<Api>, *mut AirspyDev)> {
    let mut rc = SUCCESS;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        for api in apis {
            let mut dev = std::ptr::null_mut();
            rc = unsafe { (api.open_sn)(&mut dev, sn) };
            if rc == SUCCESS {
                return Ok((api.clone(), dev));
            }
        }
        let Some(api) = apis.last().filter(|_| unlisted) else { continue };
        let mut found = None;
        for (d, s) in open_unlisted(api) {
            if s == sn && found.is_none() {
                found = Some(d);
            } else {
                unsafe { (api.close)(d) };
            }
        }
        if let Some(d) = found {
            return Ok((api.clone(), d));
        }
    }
    let api = apis.last().expect("at least one library");
    Err(api.check("open", rc).expect_err("open_sn failed"))
}

/// Opens, reads the sample-rate list, and identifies the model by it.
fn probe(apis: &[Arc<Api>], unlisted: bool, sn: u64) -> Result<(Arc<Api>, *mut AirspyDev, Vec<f64>, &'static str)> {
    let (api, dev) = open_serial(apis, unlisted, sn)?;
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
    // Mini firmware reports 6/3 MS/s; R2 reports 10/2.5 MS/s; the HydraSDR
    // RFOne (an R2-compatible board) 10/5/2.5 and names itself.
    let model = if api.hydra || firmware(&api, dev).contains("HydraSDR") {
        "HydraSDR RFOne"
    } else if rates.iter().any(|&r| (r - 6e6).abs() < 1.0 || (r - 3e6).abs() < 1.0) {
        "Airspy Mini"
    } else {
        "Airspy R2"
    };
    Ok((api, dev, rates, model))
}

impl Backend for AirspyBackend {
    fn name(&self) -> &str {
        "airspy"
    }

    fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        let apis = self.apis()?;
        let mut all = Vec::new();
        for api in &apis {
            for sn in api.list()? {
                if !all.contains(&sn) {
                    all.push(sn);
                }
            }
        }
        if self.scan_unlisted() {
            let api = apis.last().expect("at least one library");
            for (dev, sn) in open_unlisted(api) {
                unsafe { (api.close)(dev) };
                if !all.contains(&sn) {
                    all.push(sn);
                }
            }
        }
        let mut out = Vec::new();
        for &sn in &all {
            let label = match probe(&apis, self.scan_unlisted(), sn) {
                Ok((api, dev, _, model)) => {
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
        let apis = self.apis()?;
        let sn = u64::from_str_radix(serial, 16).map_err(|_| Error::NotFound(serial.into()))?;
        let (api, dev, rates, model) = probe(&apis, self.scan_unlisted(), sn)?;
        let rate = rates.iter().copied().fold(0.0, f64::max);
        // Custom RFOne firmware with an on-board sweep ("v1.0.2-sweep2"); stock
        // firmware STALLs its requests. It needs libhydrasdr's raw stream.
        let fw = firmware(&api, dev);
        let native_sweep = api.hydra && fw.contains("sweep");
        if native_sweep {
            log::info!("{serial}: firmware {fw} sweeps on the board");
        }
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
                    // libairspy clamps the LNA above 14; libhydrasdr reports 0-14.
                    GainStage { name: "LNA".into(), min: 0.0, max: 14.0, step: 1.0 },
                    GainStage { name: "Mixer".into(), min: 0.0, max: 15.0, step: 1.0 },
                    GainStage { name: "VGA".into(), min: 0.0, max: 15.0, step: 1.0 },
                ],
                has_linearity_sensitivity_presets: true,
                native_sweep,
                channels: 1,
                ext_clock: false,
                bias_tee: true,
                hw_timestamps: false,
                spur_grid_hz: Some(5e6),
                transfer_samples: Some(65536),
            },
            center: Arc::new(AtomicU64::new(100e6f64.to_bits())),
            center_set_at: Arc::new(AtomicU64::new(0)),
            rate,
            ctx: std::ptr::null_mut(),
            packing: false,
            native: None,
        };
        d.api.check("set_sample_type", unsafe { (d.api.set_sample_type)(dev, SAMPLE_FLOAT32_IQ) })?;
        d.api.check("set_samplerate", unsafe { (d.api.set_samplerate)(dev, rate as u32) })?;
        // The RFOne keeps its input selection between opens: another program
        // that left it on CABLE1 (trunk-recorder-pro's device test, eddie
        // 2026-11-04) made every later session here hear nothing. Always ANT.
        if let Some(f) = d.api.set_rf_port {
            d.api.check("set_rf_port", unsafe { f(dev, 0) })?;
        }
        d.set_center(100e6)?;
        d.set_gain(GainSetting::Linearity(0.6))?;
        Ok(d)
    }
}

impl AirspyDevice {
    /// 12-bit packed USB transfers (15 rather than 20 MB/s at 10 MS/s); the
    /// samples are the same. Takes effect at the next start.
    pub fn set_packing(&mut self, on: bool) -> Result<()> {
        self.api.check("set_packing", unsafe { (self.api.set_packing)(self.dev, u8::from(on)) })?;
        self.packing = on;
        Ok(())
    }

    /// Reads one R820T tuner register (through the Airspy's firmware).
    pub fn r820t_read(&self, reg: u8) -> Result<u8> {
        let mut v = 0u8;
        self.api.check("r820t_read", unsafe { (self.api.r820t_read)(self.dev, reg, &mut v) })?;
        Ok(v)
    }

    /// The callback's sweep state. Safe to touch only while the callback is
    /// stopped, or (as `start_native_sweep` does) for a plain field it only reads.
    unsafe fn raw_mut(&mut self) -> Option<&mut RawRx> {
        if self.ctx.is_null() { None } else { unsafe { (*self.ctx).raw.as_mut() } }
    }

    fn granted(&self, first: u32, step: u32, plan: NativeSweep) -> NativeSweep {
        NativeSweep {
            first_center_hz: f64::from(first),
            step_hz: f64::from(step),
            steps: plan.steps,
            dwell_blocks: plan.dwell_blocks,
            block_samples: native::BLOCK_SAMPLES,
        }
    }

    /// Stops the stream and starts it again raw for a firmware sweep (`Some`),
    /// or back as IQ at the live centre (`None`). The callback's sample count
    /// carries on; a time anchor and a `Retune` follow the restart.
    fn restart_stream(&mut self, raw: Option<RawRx>) -> Result<()> {
        let a = self.api.clone();
        a.check("stop_rx", unsafe { (a.stop_rx)(self.dev) })?;
        // stop_rx joined the library's threads: the context is ours until start_rx.
        let ctx = unsafe { &mut *self.ctx };
        let sweeping = raw.is_some();
        ctx.raw = raw;
        ctx.dropped_total = None;
        ctx.window = None;
        ctx.last_anchor = 0;
        if sweeping {
            a.check("set_packing", unsafe { (a.set_packing)(self.dev, 0) })?;
            a.check("set_sample_type", unsafe { (a.set_sample_type)(self.dev, SAMPLE_RAW) })?;
        } else {
            a.check("set_sample_type", unsafe { (a.set_sample_type)(self.dev, SAMPLE_FLOAT32_IQ) })?;
            a.check("set_packing", unsafe { (a.set_packing)(self.dev, u8::from(self.packing)) })?;
            // The board left the tuner on its last step: back to the live centre,
            // reported at the first sample after the restart.
            let c = self.center.load(Ordering::Relaxed);
            a.check("set_freq", a.set_freq(self.dev, f64::from_bits(c)))?;
            ctx.reported_center = 0;
            ctx.center_set_at.store(instant_to_nanos(Instant::now()), Ordering::Release);
        }
        a.check("start_rx", unsafe { (a.start_rx)(self.dev, rx_callback, self.ctx as *mut c_void) })
    }

    fn apply_gain(&mut self, gain: GainSetting) -> Result<()> {
        let a = &self.api;
        let step = |v: f32| (v.clamp(0.0, 1.0) * 21.0).round() as u8;
        match gain {
            GainSetting::Linearity(v) => a.check("set_linearity_gain", unsafe { (a.set_linearity_gain)(self.dev, step(v)) }),
            GainSetting::Sensitivity(v) => a.check("set_sensitivity_gain", unsafe { (a.set_sensitivity_gain)(self.dev, step(v)) }),
            GainSetting::Auto => {
                a.check("set_lna_agc", unsafe { (a.set_lna_agc)(self.dev, 1) })?;
                a.check("set_mixer_agc", unsafe { (a.set_mixer_agc)(self.dev, 1) })
            }
            GainSetting::Manual { lna, mixer, vga } => {
                a.check("set_lna_agc", unsafe { (a.set_lna_agc)(self.dev, 0) })?;
                a.check("set_mixer_agc", unsafe { (a.set_mixer_agc)(self.dev, 0) })?;
                a.check("set_lna_gain", unsafe { (a.set_lna_gain)(self.dev, lna.min(14)) })?;
                a.check("set_mixer_gain", unsafe { (a.set_mixer_gain)(self.dev, mixer.min(15)) })?;
                a.check("set_vga_gain", unsafe { (a.set_vga_gain)(self.dev, vga.min(15)) })
            }
        }
    }
}

/// State owned by the libairspy callback thread.
struct CbCtx {
    sink: Box<dyn StreamSink>,
    n: u64,
    center: Arc<AtomicU64>,
    /// When the centre was last requested, nanoseconds since `EPOCH`.
    center_set_at: Arc<AtomicU64>,
    reported_center: u64,
    rate: f64,
    last_anchor: u64,
    last_overload: u64,
    serial: String,
    /// Start of the current delivery-rate window (time, sample count), and
    /// whether it is the first (start-up buffering, not judged).
    window: Option<(Instant, u64, bool)>,
    warned: Option<Instant>,
    hydra: bool,
    /// libhydrasdr reports dropped samples as a running total (libairspy per
    /// transfer): the total seen so far, `None` until the first transfer.
    dropped_total: Option<u64>,
    /// Running the firmware sweep: the stream is its raw tagged blocks.
    raw: Option<RawRx>,
}

/// The callback's side of a firmware sweep.
struct RawRx {
    conv: native::RawToIq,
    buf: Vec<Complex32>,
    /// Steps in the grid (for spotting skipped steps).
    steps: u32,
    /// The hop being delivered, and whether its blocks are being left out.
    key: Option<(u32, u16)>,
    skip: bool,
    /// The device clock's origin on the host clock, and the last anchor's tick.
    clock: Option<(u32, SystemTime)>,
    last_anchor: u32,
    last_overload: Option<u32>,
    stats: RawStats,
}

#[derive(Debug, Default, Clone, Copy)]
struct RawStats {
    hops: u64,
    torn: u64,
    untagged: u64,
    lock_timeouts: u64,
    skipped_steps: u64,
    dropped_blocks: u64,
}

/// A time anchor at least this often on the device's clock (~25 ms), so a
/// hop's time is known though the stream is not continuous.
const ANCHOR_TICKS: u32 = 61;

impl RawRx {
    fn new(steps: u32) -> Self {
        Self {
            conv: native::RawToIq::new(),
            buf: Vec::with_capacity(native::BLOCK_SAMPLES),
            steps,
            key: None,
            skip: false,
            clock: None,
            last_anchor: 0,
            last_overload: None,
            stats: RawStats::default(),
        }
    }
}

/// Delivery is judged over windows this long.
const RATE_WINDOW: Duration = Duration::from_secs(2);
/// A window short by more than this share of its samples lost some. USB
/// scheduling jitter is a few transfers (~10 ms) at a window's ends.
const RATE_SHORT: f64 = 0.05;

impl CbCtx {
    /// Samples lost since the last transfer, from the library's count.
    fn newly_dropped(&mut self, reported: u64, hydra: bool) -> u64 {
        if !hydra {
            return reported;
        }
        let lost = self.dropped_total.map_or(0, |seen| reported.saturating_sub(seen));
        self.dropped_total = Some(reported);
        lost
    }

    /// One transfer of the firmware sweep: whole tagged blocks, each becoming
    /// one block of IQ; a new hop starts with its `Retune`.
    fn on_raw(&mut self, raw: &mut RawRx, words: &[u16]) {
        for w in words.as_chunks::<{ native::BLOCK_WORDS }>().0 {
            let t = match native::parse(w) {
                native::Block::Tagged(t) => t,
                native::Block::Torn => {
                    raw.stats.torn += 1;
                    continue;
                }
                native::Block::Untagged => {
                    raw.stats.untagged += 1;
                    continue;
                }
            };
            let key = (t.sweep, t.step);
            if raw.key != Some(key) {
                if let Some((sweep, step)) = raw.key {
                    let next = if u32::from(step) + 1 >= raw.steps { (sweep.wrapping_add(1), 0) } else { (sweep, step + 1) };
                    if key != next && t.step != 0 {
                        raw.stats.skipped_steps += 1;
                    }
                }
                raw.key = Some(key);
                // A hop is delivered whole or not at all: from its first block, locked.
                raw.skip = t.lock_timed_out || t.block != 0;
                if t.lock_timed_out {
                    raw.stats.lock_timeouts += 1;
                }
                if raw.skip {
                    continue;
                }
                let (tick0, host0) = *raw.clock.get_or_insert((t.dma, SystemTime::now()));
                let since = t.dma.wrapping_sub(tick0);
                if raw.stats.hops == 0 || t.dma.wrapping_sub(raw.last_anchor) >= ANCHOR_TICKS {
                    raw.last_anchor = t.dma;
                    let host_time = host0 + Duration::from_secs_f64(f64::from(since) * native::TICK_S);
                    self.sink.on_event(StreamEvent::TimeAnchor { at_sample: self.n, host_time });
                }
                self.sink.on_event(StreamEvent::Retune { at_sample: self.n, center_hz: f64::from(t.center_hz), sample_rate: self.rate });
                raw.stats.hops += 1;
            } else if raw.skip {
                continue;
            }
            // At most one overload a second of the device's clock.
            if native::clipped_fraction(w) >= OVERLOAD_FRACTION
                && raw.last_overload.is_none_or(|o| f64::from(t.dma.wrapping_sub(o)) * native::TICK_S >= 1.0)
            {
                raw.last_overload = Some(t.dma);
                self.sink.on_event(StreamEvent::Overload { at_sample: self.n });
            }
            raw.conv.process(&w[8..native::BLOCK_WORDS - 8], &mut raw.buf);
            self.sink.on_samples(self.n, Samples::Cf32(&raw.buf));
            self.n += raw.buf.len() as u64;
        }
    }

    /// The board's FIFO overflows when the host can't drain it fast enough
    /// (two radios at 10 MS/s through one USB 2 hub: each got 5.6 MS/s on
    /// eddie, 2026-11-04) and the samples vanish without either library
    /// noticing. Counts what arrived against the clock and reports the
    /// shortfall as a gap, so the timeline stays true to wall time and the
    /// loss shows in the source's stats.
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
                    "{}: delivering {:.1} of {:.1} MS/s, the rest lost on the USB link (another radio on the same USB controller?)",
                    self.serial,
                    (expected - short) / dt.as_secs_f64() / 1e6,
                    self.rate / 1e6
                );
            }
        }
        self.window = Some((now, self.n, false));
    }
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // Never unwind into C.
    // The transfer just completed, so "now" is when its last sample arrived.
    let block_end = Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let t = &*t;
        let ctx = &mut *(t.ctx as *mut CbCtx);
        if let Some(mut raw) = ctx.raw.take() {
            // A dropped raw buffer is lost hops, not a hole in a continuous stream.
            let lost = ctx.newly_dropped(t.dropped_samples, true);
            raw.stats.dropped_blocks += lost / native::BLOCK_WORDS as u64;
            let words = std::slice::from_raw_parts(t.samples as *const u16, t.sample_count.max(0) as usize);
            ctx.on_raw(&mut raw, words);
            ctx.raw = Some(raw);
            return;
        }
        let lost = ctx.newly_dropped(t.dropped_samples, ctx.hydra);
        if lost > 0 {
            ctx.sink.on_event(StreamEvent::Gap { at_sample: ctx.n, lost_samples: lost });
            ctx.n += lost;
        }
        let s = std::slice::from_raw_parts(t.samples as *const Complex32, t.sample_count.max(0) as usize);
        // Centre first, then its request time: `set_center` stores them in the
        // opposite order with Release, so a new centre comes with its time.
        let c = ctx.center.load(Ordering::Acquire);
        if c != ctx.reported_center {
            ctx.reported_center = c;
            let set_at = nanos_to_instant(ctx.center_set_at.load(Ordering::Acquire));
            let at_sample = landing_sample(ctx.n, s.len(), ctx.rate, block_end, set_at, LANDS_AFTER);
            ctx.sink.on_event(StreamEvent::Retune { at_sample, center_hz: f64::from_bits(c), sample_rate: ctx.rate });
        }
        if clipped_fraction(s) >= OVERLOAD_FRACTION && ctx.n - ctx.last_overload >= ctx.rate as u64 {
            ctx.last_overload = ctx.n;
            ctx.sink.on_event(StreamEvent::Overload { at_sample: ctx.n });
        }
        ctx.sink.on_samples(ctx.n, Samples::Cf32(s));
        ctx.n += s.len() as u64;
        ctx.check_delivery();
        if ctx.n - ctx.last_anchor >= ctx.rate as u64 {
            ctx.last_anchor = ctx.n;
            ctx.sink.on_event(StreamEvent::TimeAnchor { at_sample: ctx.n, host_time: SystemTime::now() });
        }
    }));
    if r.is_err() { -1 } else { 0 }
}

pub struct AirspyDevice {
    api: Arc<Api>,
    dev: *mut AirspyDev,
    info: DeviceInfo,
    caps: Capabilities,
    center: Arc<AtomicU64>,
    center_set_at: Arc<AtomicU64>,
    rate: f64,
    ctx: *mut CbCtx,
    /// Set by `set_packing`, restored after a firmware sweep (which needs it off).
    packing: bool,
    native: Option<NativeState>,
}

/// A running firmware sweep.
struct NativeState {
    handle: native::VendorHandle,
    params: [u8; 16],
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
        if self.native.is_some() {
            return Err(Error::Unsupported("tune while the board sweeps".into()));
        }
        self.api.check("set_freq", self.api.set_freq(self.dev, hz))?;
        // Time first, centre second (Release): the callback reads them in the
        // opposite order, so a new centre is never paired with a stale time.
        self.center_set_at.store(instant_to_nanos(Instant::now()), Ordering::Release);
        self.center.store(hz.round().to_bits(), Ordering::Release);
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
        // The board refuses gain requests while it sweeps: pause it around them.
        if let Some(n) = &self.native {
            n.handle.request(native::SWEEP_STOP, &[])?;
            let r = self.apply_gain(gain);
            let n = self.native.as_ref().expect("still sweeping");
            n.handle.request(native::SWEEP_START, &n.params)?;
            return r;
        }
        self.apply_gain(gain)
    }

    fn start_native_sweep(&mut self, plan: NativeSweep) -> Result<NativeSweep> {
        if !self.caps.native_sweep {
            return Err(Error::Unsupported("native sweep (needs RFOne sweep firmware and libhydrasdr)".into()));
        }
        if self.ctx.is_null() {
            return Err(Error::Unsupported("native sweep before the stream starts".into()));
        }
        let (lo, hi) = self.caps.freq_ranges_hz[0];
        let last = plan.first_center_hz + plan.step_hz * f64::from(plan.steps.max(1) - 1);
        if plan.steps == 0 || plan.steps > u32::from(u16::MAX) || plan.dwell_blocks == 0 {
            return Err(Error::Unsupported(format!("native sweep of {} steps, {} blocks each", plan.steps, plan.dwell_blocks)));
        }
        // The flat part of the IF is ~±4.65 MHz.
        if !(1e6..=9.3e6).contains(&plan.step_hz) || plan.first_center_hz < lo || last > hi {
            return Err(Error::Unsupported(format!(
                "native sweep {:.3}–{:.3} MHz in {:.2} MHz steps",
                plan.first_center_hz / 1e6,
                last / 1e6,
                plan.step_hz / 1e6
            )));
        }
        let (first, step) = (plan.first_center_hz.round() as u32, plan.step_hz.round() as u32);
        let params = native::start_params(first, step, plan.steps as u16, plan.dwell_blocks);
        match self.native.take() {
            Some(mut n) => {
                let r = n.handle.request(native::SWEEP_STOP, &[]).and_then(|()| {
                    // The callback is running; it only reads the step count, to spot skipped steps.
                    if let Some(r) = unsafe { self.raw_mut() } {
                        r.steps = plan.steps;
                    }
                    n.handle.request(native::SWEEP_START, &params)
                });
                n.params = params;
                self.native = Some(n);
                r?;
            }
            None => {
                let handle = native::VendorHandle::open(&self.info.serial)?;
                self.restart_stream(Some(RawRx::new(plan.steps)))?;
                // As the reference client does: let the stream get going first.
                std::thread::sleep(Duration::from_millis(50));
                if let Err(e) = handle.request(native::SWEEP_START, &params) {
                    let _ = self.restart_stream(None);
                    return Err(e);
                }
                log::info!(
                    "{}: board sweep {:.3}–{:.3} MHz, {} steps of {:.2} MHz, {} block(s) each",
                    self.info.serial,
                    f64::from(first) / 1e6,
                    last / 1e6,
                    plan.steps,
                    f64::from(step) / 1e6,
                    plan.dwell_blocks
                );
                self.native = Some(NativeState { handle, params });
                return Ok(self.granted(first, step, plan));
            }
        }
        Ok(self.granted(first, step, plan))
    }

    fn stop_native_sweep(&mut self) -> Result<()> {
        let Some(n) = self.native.take() else { return Ok(()) };
        let stopped = n.handle.request(native::SWEEP_STOP, &[]);
        drop(n);
        if let Some(r) = unsafe { self.raw_mut() } {
            let s = r.stats;
            log::info!(
                "{}: board sweep ended: {} hops; {} torn, {} untagged, {} lock timeouts, {} skipped steps, {} dropped blocks",
                self.info.serial,
                s.hops,
                s.torn,
                s.untagged,
                s.lock_timeouts,
                s.skipped_steps,
                s.dropped_blocks
            );
        }
        self.restart_stream(None)?;
        stopped
    }

    fn start(&mut self, mut sink: Box<dyn StreamSink>) -> Result<()> {
        if !self.ctx.is_null() {
            return Ok(());
        }
        let center = self.center.load(Ordering::Relaxed);
        sink.on_event(StreamEvent::Retune { at_sample: 0, center_hz: f64::from_bits(center), sample_rate: self.rate });
        sink.on_event(StreamEvent::TimeAnchor { at_sample: 0, host_time: SystemTime::now() });
        let ctx = Box::into_raw(Box::new(CbCtx {
            sink,
            n: 0,
            center: self.center.clone(),
            center_set_at: self.center_set_at.clone(),
            reported_center: center,
            rate: self.rate,
            last_anchor: 0,
            last_overload: 0,
            serial: self.info.serial.clone(),
            window: None,
            warned: None,
            hydra: self.api.hydra,
            dropped_total: None,
            raw: None,
        }));
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
        if let Some(n) = self.native.take() {
            let _ = n.handle.request(native::SWEEP_STOP, &[]);
            let _ = self.api.check("set_sample_type", unsafe { (self.api.set_sample_type)(self.dev, SAMPLE_FLOAT32_IQ) });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_sample_is_inside_the_block_and_follows_the_request_time() {
        let t0 = Instant::now();
        let rate = 10e6;
        let block_end = t0 + Duration::from_micros(6554); // 65536 samples at 10 MS/s
        // Requested 1 ms into the block: lands 3 ms in = sample 30000.
        let s = landing_sample(1_000_000, 65_536, rate, block_end, t0 + Duration::from_millis(1), Duration::from_millis(2));
        assert!((29_000..31_000).contains(&(s - 1_000_000)), "{s}");
        // Requested long before the block: clamps to its first sample.
        assert_eq!(landing_sample(1_000_000, 65_536, rate, block_end, t0 - Duration::from_secs(1), Duration::from_millis(2)), 1_000_000);
        // Requested after the block ended: clamps to its last sample.
        assert_eq!(landing_sample(1_000_000, 65_536, rate, block_end, block_end, Duration::from_millis(2)), 1_000_000 + 65_536);
    }

    #[test]
    fn clipped_fraction_counts_full_scale_samples() {
        let mut v = vec![Complex32::new(0.1, 0.1); 100];
        v[3] = Complex32::new(1.0, 0.0);
        v[7] = Complex32::new(0.0, -0.9995);
        assert!((clipped_fraction(&v) - 0.02).abs() < 1e-6);
    }
}
