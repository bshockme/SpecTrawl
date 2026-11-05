//! Synthetic SDR: generates a real-time IQ stream of noise plus FSK/FM
//! emitters at fixed absolute frequencies, so tuning behaves like hardware.
//! Used for UI work without a device and as the base of the test suite.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use num_complex::Complex;
use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, IfArchitecture, NativeSweep, Result, SampleFormat, Samples,
    StreamEvent, StreamSink,
};

/// A simulated board sweep's block (an RFOne's: 8176 real samples at 20 MS/s, as IQ at 10 MS/s).
pub const NATIVE_BLOCK_SAMPLES: usize = 4088;
/// Wall time per simulated board hop: lock wait plus a block, ~3 blocks of 409.6 µs as on an RFOne.
const NATIVE_HOP_S: f64 = 0.00123;

#[derive(Debug, Clone, Copy)]
pub enum Modulation {
    /// Unmodulated carrier.
    Cw,
    /// M-level FSK with random symbols; `levels` = 2 or 4. `sync` inserts a
    /// fixed symbol pattern periodically so protocol detection can be tested.
    Fsk { levels: u8, sym_rate: f64, dev_hz: f64, sync: Option<SyncInsert> },
    /// FM with a single audio tone.
    FmTone { tone_hz: f64, dev_hz: f64 },
    /// AM (double sideband, full carrier) with a single audio tone: the envelope is `1 + depth·sin(2π·tone·t)`.
    AmTone { tone_hz: f64, depth: f64 },
}

/// A sync word sent every `period` symbols. 4-level patterns are dibits
/// (2 bits per symbol, oldest first: 01=+3, 00=+1, 10=−1, 11=−3); 2-level
/// patterns are bits (1 = negative deviation).
#[derive(Debug, Clone, Copy)]
pub struct SyncInsert {
    pub bits: u64,
    pub symbols: u8,
    pub period: u64,
    /// Symbol offset of the sync within each period.
    pub offset: u64,
}

impl SyncInsert {
    /// Symbol `i` (0 = oldest) as a fraction of full deviation.
    fn level(&self, i: u64, levels: u8) -> f64 {
        if levels == 2 {
            let b = self.bits >> (self.symbols as u64 - 1 - i) & 1;
            if b == 1 { -1.0 } else { 1.0 }
        } else {
            match self.bits >> (2 * (self.symbols as u64 - 1 - i)) & 3 {
                0b01 => 1.0,
                0b00 => 1.0 / 3.0,
                0b10 => -1.0 / 3.0,
                _ => -1.0,
            }
        }
    }
}

/// P25 Phase 1 frame sync (48 bits = 24 dibits), every 864 symbols.
pub const P25_SYNC: SyncInsert = SyncInsert { bits: 0x5575_F5FF_77FF, symbols: 24, period: 864, offset: 0 };
/// DMR BS-sourced voice sync, once per 30 ms burst (144 symbols at 4800),
/// in the middle of the burst after 54 payload symbols as in real DMR.
pub const DMR_BS_VOICE: SyncInsert = SyncInsert { bits: 0x755F_D7DF_75F7, symbols: 24, period: 144, offset: 54 };
/// POCSAG batch sync codeword, every 544 bits.
pub const POCSAG_SYNC: SyncInsert = SyncInsert { bits: 0x7CD2_15D8, symbols: 32, period: 544, offset: 0 };

/// NXDN frame sync + LICH with the sign pattern 0x2B0C8:
/// outer symbols except the two ±1 positions the table masks (bits 9, 16).
pub fn nxdn_sync() -> SyncInsert {
    let sign = 0x2B0C8u64;
    let mut bits = 0u64;
    for b in (0..18).rev() {
        let neg = sign >> b & 1 == 1;
        let inner = b == 9 || b == 16;
        let dibit = match (neg, inner) {
            (false, false) => 0b01,
            (false, true) => 0b00,
            (true, true) => 0b10,
            (true, false) => 0b11,
        };
        bits = bits << 2 | dibit;
    }
    // 192 symbols = one 80 ms NXDN48 frame.
    SyncInsert { bits, symbols: 18, period: 192, offset: 0 }
}

#[derive(Debug, Clone, Copy)]
pub enum Keying {
    Continuous,
    /// On for `duty × period`, starting at `phase × period`.
    Bursts {
        period_s: f64,
        duty: f64,
        phase: f64,
    },
    /// TDMA-style: bursts gated further by slots of `slot_s` on/off.
    Slotted {
        period_s: f64,
        duty: f64,
        phase: f64,
        slot_s: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Emitter {
    pub freq_hz: f64,
    pub level_dbfs: f64,
    pub modulation: Modulation,
    pub keying: Keying,
}

/// A scenario resembling a 10 MHz slice of an 800 MHz public-safety band.
pub fn default_scenario() -> Vec<Emitter> {
    use Keying::*;
    use Modulation::*;
    let c4fm = Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync: None };
    let p25 = Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0, sync: Some(P25_SYNC) };
    let dmr = Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1944.0, sync: Some(DMR_BS_VOICE) };
    let mut v = vec![
        Emitter { freq_hz: 851.0125e6, level_dbfs: -38.0, modulation: p25, keying: Continuous },
        Emitter { freq_hz: 851.2625e6, level_dbfs: -44.0, modulation: p25, keying: Bursts { period_s: 9.0, duty: 0.45, phase: 0.0 } },
        Emitter { freq_hz: 851.5375e6, level_dbfs: -50.0, modulation: c4fm, keying: Bursts { period_s: 13.0, duty: 0.3, phase: 0.5 } },
        Emitter {
            freq_hz: 853.6500e6,
            level_dbfs: -46.0,
            modulation: Fsk { levels: 2, sym_rate: 1200.0, dev_hz: 4500.0, sync: Some(POCSAG_SYNC) },
            keying: Bursts { period_s: 11.0, duty: 0.2, phase: 0.3 },
        },
        Emitter { freq_hz: 855.0000e6, level_dbfs: -62.0, modulation: Cw, keying: Continuous },
        Emitter {
            freq_hz: 856.2125e6,
            level_dbfs: -42.0,
            modulation: FmTone { tone_hz: 1000.0, dev_hz: 3000.0 },
            keying: Bursts { period_s: 7.0, duty: 0.35, phase: 0.6 },
        },
        Emitter {
            freq_hz: 857.2375e6,
            level_dbfs: -45.0,
            modulation: dmr,
            keying: Slotted { period_s: 8.0, duty: 0.55, phase: 0.1, slot_s: 0.030 },
        },
        Emitter {
            freq_hz: 858.8125e6,
            level_dbfs: -48.0,
            modulation: Fsk { levels: 4, sym_rate: 2400.0, dev_hz: 800.0, sync: Some(nxdn_sync()) },
            keying: Bursts { period_s: 6.0, duty: 0.3, phase: 0.2 },
        },
        Emitter {
            freq_hz: 859.4625e6,
            level_dbfs: -47.0,
            modulation: Fsk { levels: 4, sym_rate: 9600.0, dev_hz: 2900.0, sync: None },
            keying: Bursts { period_s: 2.0, duty: 0.1, phase: 0.0 },
        },
    ];
    // Background traffic.
    let mut rng = XorShift(0x5eed_1234_abcd_0001);
    for _ in 0..14 {
        let ch = (rng.uniform() * 760.0) as i64; // 12.5 kHz channels across ~9.5 MHz
        v.push(Emitter {
            freq_hz: 851.3e6 + ch as f64 * 12_500.0,
            level_dbfs: -64.0 + rng.uniform() * 18.0,
            modulation: c4fm,
            keying: Bursts { period_s: 4.0 + rng.uniform() * 14.0, duty: 0.1 + rng.uniform() * 0.4, phase: rng.uniform() },
        });
    }
    v
}

pub struct SimBackend;

impl Backend for SimBackend {
    fn name(&self) -> &str {
        "sim"
    }

    fn enumerate(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![info()])
    }

    fn open(&self, serial: &str) -> Result<Box<dyn Device>> {
        if serial != info().serial {
            return Err(Error::NotFound(serial.into()));
        }
        Ok(Box::new(SimDevice::new(default_scenario())))
    }
}

fn info() -> DeviceInfo {
    info_for("SIM-0001")
}

fn info_for(serial: &str) -> DeviceInfo {
    DeviceInfo { backend: "sim".into(), serial: serial.into(), label: "Simulated SDR".into() }
}

/// Everything needed to build a simulated device.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub emitters: Vec<Emitter>,
    pub rate: f64,
    pub center_hz: f64,
    /// Pace generation to real time. When false the generator runs as fast as
    /// the CPU allows and never reports a `Gap` for falling behind.
    pub paced: bool,
    /// When paced and more than 250 ms behind, report the skipped span as a
    /// `Gap` (true, like a device dropping transfers) or just restart the
    /// pacing clock without losing samples (false, for tests that must not
    /// depend on the machine's scheduling).
    pub report_lag_gaps: bool,
    pub serial: String,
    /// Offer a board sweep (`Capabilities::native_sweep`), simulated the way an RFOne's firmware sends one.
    pub native_sweep: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        // The centre keeps every default-scenario emitter inside the 90% usable band.
        Self {
            emitters: default_scenario(),
            rate: 10e6,
            center_hz: 855.25e6,
            paced: true,
            report_lag_gaps: true,
            serial: "SIM-0001".into(),
            native_sweep: false,
        }
    }
}

/// State shared between a `SimDevice`, its generator thread and its `SimControl`s.
#[derive(Default)]
pub struct SimShared {
    pending_gap: AtomicU64,
    pending_overload: AtomicBool,
    pending_drop: AtomicBool,
    pending_stall: AtomicBool,
    starts: AtomicU32,
    samples_sent: AtomicU64,
    /// True from `start` until the generator thread exits.
    alive: AtomicBool,
    /// The board sweep running, if any.
    native: Mutex<Option<NativeSweep>>,
}

/// Test handle for a `SimDevice`: injects faults and observes the stream.
/// Injections are consumed by the generator at its next block boundary, in the
/// order gap, overload, samples, device loss.
#[derive(Clone)]
pub struct SimControl(Arc<SimShared>);

impl SimControl {
    /// Next block: `Gap { at_sample: n, lost_samples }`, then `n += lost_samples`.
    pub fn inject_gap(&self, lost_samples: u64) {
        self.0.pending_gap.fetch_add(lost_samples, Ordering::Relaxed);
    }

    /// Next block: `Overload { at_sample: n }`.
    pub fn inject_overload(&self) {
        self.0.pending_overload.store(true, Ordering::Relaxed);
    }

    /// Next block: `DeviceLost`, then the generator exits and `is_streaming()` becomes false.
    pub fn drop_device(&self) {
        self.0.pending_drop.store(true, Ordering::Relaxed);
    }

    /// Next block: the generator exits without sending `DeviceLost`, so
    /// `is_streaming()` turns false and samples stop. Simulates a driver that
    /// dies silently, for watchdog tests.
    pub fn stall(&self) {
        self.0.pending_stall.store(true, Ordering::Relaxed);
    }

    /// How many times `Device::start` ran.
    pub fn starts(&self) -> u32 {
        self.0.starts.load(Ordering::Relaxed)
    }

    /// Samples delivered through `on_samples` (gaps excluded).
    pub fn samples_sent(&self) -> u64 {
        self.0.samples_sent.load(Ordering::Relaxed)
    }
}

/// Clears `alive` when the generator thread ends, however it ends.
struct AliveGuard(Arc<SimShared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Relaxed);
    }
}

pub struct SimDevice {
    info: DeviceInfo,
    caps: Capabilities,
    emitters: Arc<Vec<Emitter>>,
    center: Arc<AtomicU64>, // f64 bits
    rate: f64,
    noise_dbfs: f64,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    paced: bool,
    report_lag_gaps: bool,
    shared: Arc<SimShared>,
}

impl SimDevice {
    pub fn new(emitters: Vec<Emitter>) -> Self {
        Self::with_config(SimConfig { emitters, ..Default::default() }).0
    }

    pub fn with_config(cfg: SimConfig) -> (SimDevice, SimControl) {
        let shared = Arc::new(SimShared::default());
        let dev = Self {
            info: info_for(&cfg.serial),
            caps: Capabilities {
                freq_ranges_hz: vec![(24e6, 1.8e9)],
                tuning_step_hz: 1.0,
                retune_latency_s: 0.0,
                sample_rates: vec![2.5e6, 6e6, 10e6],
                usable_bandwidth_fraction: 0.9,
                native_format: SampleFormat::Ci16,
                adc_bits: 12,
                if_arch: IfArchitecture::RealIf,
                gain_stages: vec![],
                has_linearity_sensitivity_presets: false,
                native_sweep: cfg.native_sweep,
                channels: 1,
                ext_clock: false,
                bias_tee: false,
                hw_timestamps: false,
                spur_grid_hz: None,
                transfer_samples: None,
            },
            emitters: Arc::new(cfg.emitters),
            center: Arc::new(AtomicU64::new(cfg.center_hz.to_bits())),
            rate: cfg.rate,
            noise_dbfs: -62.0,
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
            paced: cfg.paced,
            report_lag_gaps: cfg.report_lag_gaps,
            shared: shared.clone(),
        };
        (dev, SimControl(shared))
    }
}

impl Device for SimDevice {
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
        self.center.store(hz.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        if self.is_streaming() {
            return Err(Error::Unsupported("change rate while streaming".into()));
        }
        if !self.caps.sample_rates.contains(&rate) {
            return Err(Error::Unsupported(format!("rate {rate}")));
        }
        self.rate = rate;
        Ok(())
    }

    fn set_gain(&mut self, _gain: GainSetting) -> Result<()> {
        Ok(())
    }

    fn start_native_sweep(&mut self, plan: NativeSweep) -> Result<NativeSweep> {
        if !self.caps.native_sweep {
            return Err(Error::Unsupported("native sweep".into()));
        }
        if !self.is_streaming() || plan.steps == 0 || plan.dwell_blocks == 0 {
            return Err(Error::Unsupported("native sweep: not streaming, or an empty grid".into()));
        }
        let granted = NativeSweep { block_samples: NATIVE_BLOCK_SAMPLES, ..plan };
        *self.shared.native.lock().unwrap() = Some(granted);
        Ok(granted)
    }

    fn stop_native_sweep(&mut self) -> Result<()> {
        self.shared.native.lock().unwrap().take();
        Ok(())
    }

    fn start(&mut self, sink: Box<dyn StreamSink>) -> Result<()> {
        if self.is_streaming() {
            return Ok(());
        }
        // Reap a generator that already exited (after `drop_device`).
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        self.running.store(true, Ordering::Relaxed);
        self.shared.alive.store(true, Ordering::Relaxed);
        self.shared.starts.fetch_add(1, Ordering::Relaxed);
        let generator = Generator {
            emitters: self.emitters.clone(),
            center: self.center.clone(),
            rate: self.rate,
            noise_amp: 10f64.powf(self.noise_dbfs / 20.0) as f32,
            running: self.running.clone(),
            paced: self.paced,
            report_lag_gaps: self.report_lag_gaps,
            shared: self.shared.clone(),
        };
        self.worker = Some(std::thread::Builder::new().name("sim-sdr".into()).spawn(move || generator.run(sink)).unwrap());
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        Ok(())
    }

    fn is_streaming(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }
}

impl Drop for SimDevice {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Generator {
    emitters: Arc<Vec<Emitter>>,
    center: Arc<AtomicU64>,
    rate: f64,
    noise_amp: f32,
    running: Arc<AtomicBool>,
    paced: bool,
    report_lag_gaps: bool,
    shared: Arc<SimShared>,
}

struct EmState {
    z: Complex32,
    step: Complex32,
    amp: f32,
    sym_left: f64,
    tone_phase: f64,
    sym_index: u64,
}

impl Generator {
    fn run(self, mut sink: Box<dyn StreamSink>) {
        const BLOCK: usize = 65_536;
        let _alive = AliveGuard(self.shared.clone());
        let fs = self.rate;
        let mut center = f64::from_bits(self.center.load(Ordering::Relaxed));
        sink.on_event(StreamEvent::Retune { at_sample: 0, center_hz: center, sample_rate: fs });
        let mut st: Vec<EmState> = self
            .emitters
            .iter()
            .map(|e| EmState {
                z: Complex32::new(1.0, 0.0),
                step: Complex32::new(1.0, 0.0),
                amp: 10f64.powf(e.level_dbfs / 20.0) as f32,
                sym_left: 0.0,
                tone_phase: 0.0,
                sym_index: 0,
            })
            .collect();
        let mut rng = XorShift(0x1234_5678_9abc_def1);
        let mut acc = vec![Complex32::default(); BLOCK];
        let mut out = vec![[0i16; 2]; BLOCK];
        let mut n: u64 = 0;
        let mut t0 = Instant::now();
        let mut last_anchor: Option<u64> = None;
        // Stream time (s): samples / rate live, hops × NATIVE_HOP_S in a board sweep.
        let mut virt = 0.0f64;
        let mut last_anchor_t = 0.0f64;
        let mut board: Option<(NativeSweep, u32)> = None;

        while self.running.load(Ordering::Relaxed) {
            if self.shared.pending_stall.swap(false, Ordering::Relaxed) {
                // Silent death: no event; `_alive` clears `alive` on return.
                return;
            }
            // Pending injections, in order: gap, overload, (samples), device loss.
            let lost = self.shared.pending_gap.swap(0, Ordering::Relaxed);
            if lost > 0 {
                sink.on_event(StreamEvent::Gap { at_sample: n, lost_samples: lost });
                n += lost;
                // The lost span did not take real time; keep the pacing clock in step.
                let skipped = Duration::from_secs_f64(lost as f64 / fs);
                t0 = t0.checked_sub(skipped).unwrap_or(t0);
                virt += lost as f64 / fs;
            }
            if self.shared.pending_overload.swap(false, Ordering::Relaxed) {
                sink.on_event(StreamEvent::Overload { at_sample: n });
            }
            let plan = *self.shared.native.lock().unwrap();
            if plan != board.map(|b| b.0) {
                board = plan.map(|p| (p, 0));
                if board.is_none() {
                    // Back to the live tuning.
                    center = f64::NAN;
                }
            }
            let len = board.map_or(BLOCK, |(p, _)| p.block_samples * usize::from(p.dwell_blocks)).min(BLOCK);
            if board.is_some() {
                if virt - last_anchor_t >= 0.025 || last_anchor.is_none() {
                    sink.on_event(StreamEvent::TimeAnchor { at_sample: n, host_time: SystemTime::now() });
                    (last_anchor, last_anchor_t) = (Some(n), virt);
                }
            } else if last_anchor.is_none_or(|a| (n - a) as f64 >= fs) {
                sink.on_event(StreamEvent::TimeAnchor { at_sample: n, host_time: SystemTime::now() });
                (last_anchor, last_anchor_t) = (Some(n), virt);
            }
            let c = match board.as_mut() {
                Some((p, step)) => {
                    let c = p.first_center_hz + p.step_hz * f64::from(*step);
                    *step = (*step + 1) % p.steps;
                    // Every hop is a segment of its own, even one repeating the last centre.
                    center = f64::NAN;
                    c
                }
                None => f64::from_bits(self.center.load(Ordering::Relaxed)),
            };
            if c != center {
                center = c;
                // Every emitter's step was computed against the old centre.
                for s in st.iter_mut() {
                    s.sym_left = 0.0;
                }
                sink.on_event(StreamEvent::Retune { at_sample: n, center_hz: center, sample_rate: fs });
            }
            let (acc, out) = (&mut acc[..len], &mut out[..len]);
            for a in acc.iter_mut() {
                *a = Complex32::new(rng.gauss() * self.noise_amp, rng.gauss() * self.noise_amp);
            }
            let t_block = virt;
            for (e, s) in self.emitters.iter().zip(st.iter_mut()) {
                let off = e.freq_hz - center;
                if off.abs() > fs * 0.5 {
                    continue;
                }
                // Gate at block granularity (6.5 ms at 10 MS/s) except TDMA slots.
                let (on, slot_s) = match e.keying {
                    Keying::Continuous => (true, None),
                    Keying::Bursts { period_s, duty, phase } => (gate(t_block, period_s, duty, phase), None),
                    Keying::Slotted { period_s, duty, phase, slot_s } => (gate(t_block, period_s, duty, phase), Some(slot_s)),
                };
                if !on {
                    continue;
                }
                let mut i = 0;
                while i < acc.len() {
                    if s.sym_left <= 0.0 {
                        let dev = match e.modulation {
                            Modulation::Fsk { levels, sym_rate, dev_hz, sync } => {
                                s.sym_left += fs / sym_rate;
                                let i = s.sym_index;
                                s.sym_index += 1;
                                match sync {
                                    Some(sy) if (i % sy.period).wrapping_sub(sy.offset) < sy.symbols as u64 => {
                                        sy.level((i % sy.period) - sy.offset, levels) * dev_hz
                                    }
                                    _ => {
                                        let k = (rng.next() % levels as u64) as f64;
                                        // Map 0..levels-1 onto ±1 (2FSK) or ±1/3, ±1 (4FSK).
                                        (2.0 * k / (levels as f64 - 1.0) - 1.0) * dev_hz
                                    }
                                }
                            }
                            Modulation::Cw | Modulation::AmTone { .. } => 0.0,
                            Modulation::FmTone { tone_hz, dev_hz } => {
                                // Update the tone ~32 times per cycle.
                                let per = fs / tone_hz / 32.0;
                                s.sym_left += per;
                                s.tone_phase += std::f64::consts::TAU / 32.0;
                                s.tone_phase.sin() * dev_hz
                            }
                        };
                        if matches!(e.modulation, Modulation::Cw | Modulation::AmTone { .. }) {
                            s.sym_left = f64::INFINITY;
                        }
                        let w = std::f64::consts::TAU * (off + dev) / fs;
                        s.step = Complex32::new(w.cos() as f32, w.sin() as f32);
                    }
                    // The samples up to the next symbol: the check above fires once `sym_left` has been decremented
                    // to zero or below, ceil(sym_left) samples on (the decrements by 1.0 are exact).
                    let left = acc.len() - i;
                    let run = if s.sym_left.is_finite() { (s.sym_left.ceil() as usize).clamp(1, left) } else { left };
                    if s.sym_left.is_finite() {
                        s.sym_left -= run as f64;
                    }
                    let span = &mut acc[i..i + run];
                    match (e.modulation, slot_s) {
                        (Modulation::AmTone { tone_hz, depth }, None) => {
                            // The tone as a phasor too: one sin/cos per run, not per sample.
                            let dt = std::f64::consts::TAU * tone_hz / fs;
                            let (mut t, t_step) = (Complex::<f64>::from_polar(1.0, s.tone_phase), Complex::<f64>::from_polar(1.0, dt));
                            for a in span.iter_mut() {
                                s.z *= s.step;
                                t *= t_step;
                                *a += s.z * (s.amp * (1.0 + depth * t.im) as f32);
                            }
                            s.tone_phase += dt * run as f64;
                        }
                        (_, None) => rotate_into(span, &mut s.z, s.step, s.amp),
                        (m, Some(sl)) => {
                            for (j, a) in span.iter_mut().enumerate() {
                                s.z *= s.step;
                                let env = match m {
                                    Modulation::AmTone { tone_hz, depth } => {
                                        s.tone_phase += std::f64::consts::TAU * tone_hz / fs;
                                        (1.0 + depth * s.tone_phase.sin()) as f32
                                    }
                                    _ => 1.0,
                                };
                                if (((t_block + (i + j) as f64 / fs) / sl) as u64).is_multiple_of(2) {
                                    *a += s.z * (s.amp * env);
                                }
                            }
                        }
                    }
                    i += run;
                }
                // Keep the phasor on the unit circle, and the tone's phase small.
                s.z /= s.z.norm();
                s.tone_phase %= std::f64::consts::TAU;
            }
            for (o, a) in out.iter_mut().zip(acc.iter()) {
                *o = [(a.re * 32767.0).clamp(-32768.0, 32767.0) as i16, (a.im * 32767.0).clamp(-32768.0, 32767.0) as i16];
            }
            sink.on_samples(n, Samples::Ci16(out));
            n += len as u64;
            virt += if board.is_some() { NATIVE_HOP_S } else { len as f64 / fs };
            self.shared.samples_sent.fetch_add(len as u64, Ordering::Relaxed);

            if self.shared.pending_drop.swap(false, Ordering::Relaxed) {
                sink.on_event(StreamEvent::DeviceLost);
                return;
            }
            if !self.paced {
                continue;
            }

            // Pace to real time; if we fall behind, report the skipped span as
            // a gap, or (without lag gaps) restart the clock from now.
            let due = t0 + Duration::from_secs_f64(virt);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > Duration::from_millis(250) {
                if self.report_lag_gaps && board.is_none() {
                    let lost = ((now - due).as_secs_f64() * fs) as u64;
                    sink.on_event(StreamEvent::Gap { at_sample: n, lost_samples: lost });
                    n += lost;
                    virt += lost as f64 / fs;
                } else {
                    t0 += now - due;
                }
            }
        }
    }
}

/// Adds `amp·z·stepᵏ` (k = 1, 2, …) to each of `out`, leaving `z` at the last. Four phasors a step apart advance by
/// step⁴, so the multiplies don't wait on each other: one chain held the simulator to ~30 MS/s.
fn rotate_into(out: &mut [Complex32], z: &mut Complex32, step: Complex32, amp: f32) {
    let s2 = step * step;
    let s4 = s2 * s2;
    let mut zs = [*z * step, *z * s2, *z * s2 * step, *z * s4];
    let mut last = *z;
    let (quads, rest) = out.as_chunks_mut::<4>();
    for q in quads {
        for (a, zk) in q.iter_mut().zip(&zs) {
            *a += *zk * amp;
        }
        last = zs[3];
        for zk in &mut zs {
            *zk *= s4;
        }
    }
    for (a, zk) in rest.iter_mut().zip(zs) {
        *a += zk * amp;
        last = zk;
    }
    *z = last;
}

fn gate(t: f64, period: f64, duty: f64, phase: f64) -> bool {
    let p = (t / period + phase).fract();
    p < duty
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Cheap approximately-Gaussian sample (sum of 4 uniforms), unit variance.
    fn gauss(&mut self) -> f32 {
        let x = self.next();
        let s = (x & 0xffff) + ((x >> 16) & 0xffff) + ((x >> 32) & 0xffff) + (x >> 48);
        ((s as f32 / 65535.0) - 2.0) * 1.732
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn four_phasors_add_what_one_chain_adds() {
        // Against the exact f64 rotation, over one block of the stage's size: no further off than one chain.
        let (w, w0, amp) = (0.013_7f64, 0.4f64, 0.25f32);
        let step = Complex32::new(w.cos() as f32, w.sin() as f32);
        for len in [0, 1, 3, 4, 5, 7, 8, 4097, 65_536] {
            let exact = |k: usize| Complex32::new(0.1, -0.2) + Complex32::from_polar(amp, (w0 + w * k as f64) as f32);
            let z0 = Complex32::new(w0.cos() as f32, w0.sin() as f32);
            let (mut chain, mut zc) = (vec![Complex32::new(0.1, -0.2); len], z0);
            for a in &mut chain {
                zc *= step;
                *a += zc * amp;
            }
            let (mut got, mut zg) = (vec![Complex32::new(0.1, -0.2); len], z0);
            rotate_into(&mut got, &mut zg, step, amp);
            let worst = |v: &[Complex32]| v.iter().enumerate().map(|(k, a)| (a - exact(k + 1)).norm()).fold(0.0f32, f32::max);
            let (e_chain, e_got) = (worst(&chain), worst(&got));
            assert!(e_got <= 2.0 * e_chain + 1e-5, "len {len}: off by {e_got:e}, one chain by {e_chain:e}");
            let z_exact = Complex32::from_polar(1.0, (w0 + w * len as f64) as f32);
            assert!((zg - z_exact).norm() <= 2.0 * (zc - z_exact).norm() + 1e-5, "len {len}: z {zg}, one chain {zc}, exact {z_exact}");
        }
    }

    #[derive(Default)]
    struct Log {
        blocks: Vec<(u64, usize)>,
        events: Vec<StreamEvent>,
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Log>>);

    impl Recorder {
        fn new() -> Self {
            Self::default()
        }

        fn samples(&self) -> u64 {
            self.0.lock().unwrap().blocks.iter().map(|b| b.1 as u64).sum()
        }

        fn events(&self, pred: impl Fn(&StreamEvent) -> bool) -> Vec<StreamEvent> {
            self.0.lock().unwrap().events.iter().filter(|e| pred(e)).cloned().collect()
        }

        fn has(&self, pred: impl Fn(&StreamEvent) -> bool) -> bool {
            self.0.lock().unwrap().events.iter().any(pred)
        }

        /// `first_sample` of the first block whose `first_sample >= at`.
        fn first_sample_after(&self, at: u64) -> u64 {
            self.0.lock().unwrap().blocks.iter().map(|b| b.0).find(|&f| f >= at).expect("no block at or after sample")
        }
    }

    impl StreamSink for Recorder {
        fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
            self.0.lock().unwrap().blocks.push((first_sample, samples.len()));
        }

        fn on_event(&mut self, event: StreamEvent) {
            self.0.lock().unwrap().events.push(event);
        }
    }

    fn at_sample(e: &StreamEvent) -> u64 {
        match e {
            StreamEvent::Retune { at_sample, .. }
            | StreamEvent::Gap { at_sample, .. }
            | StreamEvent::TimeAnchor { at_sample, .. }
            | StreamEvent::GainChange { at_sample, .. }
            | StreamEvent::Overload { at_sample } => *at_sample,
            StreamEvent::DeviceLost => panic!("DeviceLost has no sample position"),
        }
    }

    fn wait_until(pred: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred() {
            assert!(Instant::now() < deadline, "wait_until timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn anchors_once_per_second_and_retune_at_block_start() {
        let (mut d, _c) = SimDevice::with_config(SimConfig { emitters: vec![], paced: false, ..Default::default() });
        let log = Recorder::new();
        d.start(Box::new(log.clone())).unwrap();
        wait_until(|| log.samples() >= 25_000_000);
        d.set_center(860e6).unwrap();
        wait_until(|| log.samples() >= 30_000_000);
        d.stop().unwrap();
        let at: Vec<u64> = log.events(|e| matches!(e, StreamEvent::TimeAnchor { .. })).iter().map(at_sample).collect();
        assert!(at.len() >= 3, "{at:?}");
        assert_eq!(at[0], 0);
        assert!(at.windows(2).all(|w| w[1] - w[0] >= 10_000_000 && w[1] - w[0] < 10_000_000 + 65_536), "{at:?}");
        let retunes = log.events(|e| matches!(e, StreamEvent::Retune { center_hz, .. } if *center_hz == 860e6));
        assert_eq!(retunes.len(), 1);
        assert_eq!(at_sample(&retunes[0]) % 65_536, 0);
    }

    /// Keeps every block's samples, and the events, for spectral checks.
    #[derive(Default)]
    struct Captured {
        blocks: Vec<(u64, Vec<[i16; 2]>)>,
        events: Vec<StreamEvent>,
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Captured>>);

    impl StreamSink for Capture {
        fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
            let Samples::Ci16(s) = samples else { panic!("sim is Ci16") };
            self.0.lock().unwrap().blocks.push((first_sample, s.to_vec()));
        }

        fn on_event(&mut self, event: StreamEvent) {
            self.0.lock().unwrap().events.push(event);
        }
    }

    #[test]
    fn a_cw_emitter_stays_at_its_absolute_frequency_across_a_retune() {
        let cw = Emitter { freq_hz: 855.0e6, level_dbfs: -20.0, modulation: Modulation::Cw, keying: Keying::Continuous };
        let (mut d, _c) = SimDevice::with_config(SimConfig { emitters: vec![cw], center_hz: 855.25e6, paced: false, ..Default::default() });
        let cap = Capture::default();
        d.start(Box::new(cap.clone())).unwrap();
        wait_until(|| cap.0.lock().unwrap().blocks.len() >= 3);
        // 859 MHz, not 860: a -5 MHz offset is exactly Nyquist (step = -1), where the phase-difference estimator is ambiguous.
        d.set_center(859e6).unwrap();
        let retune_at = || {
            cap.0.lock().unwrap().events.iter().find_map(|e| match e {
                StreamEvent::Retune { at_sample, center_hz, .. } if *center_hz == 859e6 => Some(*at_sample),
                _ => None,
            })
        };
        wait_until(|| retune_at().is_some_and(|at| cap.0.lock().unwrap().blocks.iter().filter(|b| b.0 >= at).count() >= 2));
        d.stop().unwrap();
        let at = retune_at().unwrap();
        let g = cap.0.lock().unwrap();
        let x: Vec<Complex32> =
            g.blocks.iter().filter(|b| b.0 >= at).flat_map(|b| b.1.iter().map(|v| Complex32::new(v[0] as f32, v[1] as f32))).collect();
        let fs = 10e6;
        let sum: f64 = x.windows(2).map(|w| (w[1] * w[0].conj()).arg() as f64).sum();
        let hz = sum / (x.len() - 1) as f64 * fs / std::f64::consts::TAU;
        assert!((hz - -4.0e6).abs() < 100.0, "tone at {hz} Hz, expected -4 MHz");
    }

    #[test]
    fn an_am_emitter_carries_its_tone_on_the_envelope_at_its_depth() {
        let am = Emitter {
            freq_hz: 855.25e6,
            level_dbfs: -20.0,
            modulation: Modulation::AmTone { tone_hz: 1000.0, depth: 0.5 },
            keying: Keying::Continuous,
        };
        let (mut d, _c) = SimDevice::with_config(SimConfig { emitters: vec![am], center_hz: 855.25e6, paced: false, ..Default::default() });
        let cap = Capture::default();
        d.start(Box::new(cap.clone())).unwrap();
        wait_until(|| cap.0.lock().unwrap().blocks.len() >= 2);
        d.stop().unwrap();
        let g = cap.0.lock().unwrap();
        // Six whole cycles of the tone (10 000 samples each at 10 MS/s).
        let env: Vec<f64> = g.blocks[1].1[..60_000].iter().map(|v| f64::from(v[0]).hypot(f64::from(v[1]))).collect();
        let mean = env.iter().sum::<f64>() / env.len() as f64;
        let at = |hz: f64| {
            let w = std::f64::consts::TAU * hz / 10e6;
            let (re, im) =
                env.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &v)| (re + v * (w * n as f64).cos(), im - v * (w * n as f64).sin()));
            2.0 * re.hypot(im) / env.len() as f64
        };
        let depth = at(1000.0) / mean;
        assert!((depth - 0.5).abs() < 0.02, "depth {depth}");
        assert!(at(1500.0) / mean < 0.02, "the envelope has energy off the tone");
    }

    /// A recorder that blocks for 400 ms on its third block, like a descheduled runner.
    struct Stalling(Recorder, usize);

    impl StreamSink for Stalling {
        fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
            self.1 += 1;
            if self.1 == 3 {
                std::thread::sleep(Duration::from_millis(400));
            }
            self.0.on_samples(first_sample, samples);
        }

        fn on_event(&mut self, event: StreamEvent) {
            self.0.on_event(event);
        }
    }

    #[test]
    fn falling_behind_reports_a_gap_only_when_lag_gaps_are_on() {
        for report_lag_gaps in [true, false] {
            let (mut d, _c) = SimDevice::with_config(SimConfig { emitters: vec![], rate: 2.5e6, report_lag_gaps, ..Default::default() });
            let log = Recorder::new();
            d.start(Box::new(Stalling(log.clone(), 0))).unwrap();
            wait_until(|| log.samples() >= 10 * 65_536);
            d.stop().unwrap();
            let gaps = log.events(|e| matches!(e, StreamEvent::Gap { .. }));
            assert_eq!(gaps.is_empty(), !report_lag_gaps, "report_lag_gaps = {report_lag_gaps}: {gaps:?}");
            if !report_lag_gaps {
                let blocks = log.0.lock().unwrap().blocks.clone();
                assert!(blocks.windows(2).all(|w| w[1].0 == w[0].0 + w[0].1 as u64), "samples are continuous");
            }
        }
    }

    #[test]
    fn injected_gap_overload_and_loss_arrive_in_order() {
        let (mut d, c) = SimDevice::with_config(SimConfig { emitters: vec![], paced: false, ..Default::default() });
        let log = Recorder::new();
        d.start(Box::new(log.clone())).unwrap();
        wait_until(|| log.samples() >= 1_000_000);
        c.inject_gap(123_456);
        c.inject_overload();
        wait_until(|| log.has(|e| matches!(e, StreamEvent::Overload { .. })));
        c.drop_device();
        wait_until(|| !d.is_streaming());
        let StreamEvent::Gap { at_sample, lost_samples } = log.events(|e| matches!(e, StreamEvent::Gap { .. }))[0].clone() else {
            unreachable!()
        };
        assert_eq!(lost_samples, 123_456);
        assert_eq!(log.first_sample_after(at_sample), at_sample + 123_456);
        assert_eq!(log.events(|e| matches!(e, StreamEvent::DeviceLost)).len(), 1);
        assert_eq!(c.starts(), 1);
    }
}
