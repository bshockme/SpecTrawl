//! Synthetic SDR: generates a real-time IQ stream of noise plus FSK/FM
//! emitters at fixed absolute frequencies, so tuning behaves like hardware.
//! Used for UI work without a device and as the base of the test suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, IfArchitecture, Result, SampleFormat, Samples, StreamEvent,
    StreamSink,
};

#[derive(Debug, Clone, Copy)]
pub enum Modulation {
    /// Unmodulated carrier.
    Cw,
    /// M-level FSK with random symbols; `levels` = 2 or 4. `sync` inserts a
    /// fixed symbol pattern periodically so protocol detection can be tested.
    Fsk { levels: u8, sym_rate: f64, dev_hz: f64, sync: Option<SyncInsert> },
    /// FM with a single audio tone.
    FmTone { tone_hz: f64, dev_hz: f64 },
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
    pub serial: String,
}

impl Default for SimConfig {
    fn default() -> Self {
        // The centre keeps every default-scenario emitter inside the 90% usable band.
        Self { emitters: default_scenario(), rate: 10e6, center_hz: 855.25e6, paced: true, serial: "SIM-0001".into() }
    }
}

/// State shared between a `SimDevice`, its generator thread and its `SimControl`s.
#[derive(Default)]
pub struct SimShared {
    pending_gap: AtomicU64,
    pending_overload: AtomicBool,
    pending_drop: AtomicBool,
    starts: AtomicU32,
    samples_sent: AtomicU64,
    /// True from `start` until the generator thread exits.
    alive: AtomicBool,
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
                native_sweep: false,
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

        while self.running.load(Ordering::Relaxed) {
            // Pending injections, in order: gap, overload, (samples), device loss.
            let lost = self.shared.pending_gap.swap(0, Ordering::Relaxed);
            if lost > 0 {
                sink.on_event(StreamEvent::Gap { at_sample: n, lost_samples: lost });
                n += lost;
                // The lost span did not take real time; keep the pacing clock in step.
                let skipped = Duration::from_secs_f64(lost as f64 / fs);
                t0 = t0.checked_sub(skipped).unwrap_or(t0);
            }
            if self.shared.pending_overload.swap(false, Ordering::Relaxed) {
                sink.on_event(StreamEvent::Overload { at_sample: n });
            }
            if last_anchor.is_none_or(|a| (n - a) as f64 >= fs) {
                sink.on_event(StreamEvent::TimeAnchor { at_sample: n, host_time: SystemTime::now() });
                last_anchor = Some(n);
            }
            let c = f64::from_bits(self.center.load(Ordering::Relaxed));
            if c != center {
                center = c;
                sink.on_event(StreamEvent::Retune { at_sample: n, center_hz: center, sample_rate: fs });
            }
            for a in acc.iter_mut() {
                *a = Complex32::new(rng.gauss() * self.noise_amp, rng.gauss() * self.noise_amp);
            }
            let t_block = n as f64 / fs;
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
                for (i, a) in acc.iter_mut().enumerate() {
                    if s.sym_left <= 0.0 {
                        let dev = match e.modulation {
                            Modulation::Cw => 0.0,
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
                            Modulation::FmTone { tone_hz, dev_hz } => {
                                // Update the tone ~32 times per cycle.
                                let per = fs / tone_hz / 32.0;
                                s.sym_left += per;
                                s.tone_phase += std::f64::consts::TAU / 32.0;
                                s.tone_phase.sin() * dev_hz
                            }
                        };
                        if matches!(e.modulation, Modulation::Cw) {
                            s.sym_left = f64::INFINITY;
                        }
                        let w = std::f64::consts::TAU * (off + dev) / fs;
                        s.step = Complex32::new(w.cos() as f32, w.sin() as f32);
                    }
                    s.sym_left -= 1.0;
                    s.z *= s.step;
                    let slot_on = match slot_s {
                        Some(sl) => (((t_block + i as f64 / fs) / sl) as u64).is_multiple_of(2),
                        None => true,
                    };
                    if slot_on {
                        *a += s.z * s.amp;
                    }
                }
                // Keep the phasor on the unit circle.
                s.z /= s.z.norm();
            }
            for (o, a) in out.iter_mut().zip(&acc) {
                *o = [(a.re * 32767.0).clamp(-32768.0, 32767.0) as i16, (a.im * 32767.0).clamp(-32768.0, 32767.0) as i16];
            }
            sink.on_samples(n, Samples::Ci16(&out));
            n += BLOCK as u64;
            self.shared.samples_sent.fetch_add(BLOCK as u64, Ordering::Relaxed);

            if self.shared.pending_drop.swap(false, Ordering::Relaxed) {
                sink.on_event(StreamEvent::DeviceLost);
                return;
            }
            if !self.paced {
                continue;
            }

            // Pace to real time; if we fall behind, report the skipped span as a gap.
            let due = t0 + Duration::from_secs_f64(n as f64 / fs);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > Duration::from_millis(250) {
                let lost = ((now - due).as_secs_f64() * fs) as u64;
                sink.on_event(StreamEvent::Gap { at_sample: n, lost_samples: lost });
                n += lost;
            }
        }
    }
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
