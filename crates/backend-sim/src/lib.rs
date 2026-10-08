//! Synthetic SDR: generates a real-time IQ stream of noise plus FSK/FM
//! emitters at fixed absolute frequencies, so tuning behaves like hardware.
//! Used for UI work without a device and as the base of the test suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use sdr_core::{
    Backend, Capabilities, Complex32, Device, DeviceInfo, Error, GainSetting, IfArchitecture, Result, SampleFormat,
    Samples, StreamEvent, StreamSink,
};

#[derive(Debug, Clone, Copy)]
pub enum Modulation {
    /// Unmodulated carrier.
    Cw,
    /// M-level FSK with random symbols; `levels` = 2 or 4.
    Fsk { levels: u8, sym_rate: f64, dev_hz: f64 },
    /// FM with a single audio tone.
    FmTone { tone_hz: f64, dev_hz: f64 },
}

#[derive(Debug, Clone, Copy)]
pub enum Keying {
    Continuous,
    /// On for `duty × period`, starting at `phase × period`.
    Bursts { period_s: f64, duty: f64, phase: f64 },
    /// TDMA-style: bursts gated further by slots of `slot_s` on/off.
    Slotted { period_s: f64, duty: f64, phase: f64, slot_s: f64 },
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
    let c4fm = Fsk { levels: 4, sym_rate: 4800.0, dev_hz: 1800.0 };
    let mut v = vec![
        Emitter { freq_hz: 851.0125e6, level_dbfs: -38.0, modulation: c4fm, keying: Continuous },
        Emitter { freq_hz: 851.2625e6, level_dbfs: -44.0, modulation: c4fm, keying: Bursts { period_s: 9.0, duty: 0.45, phase: 0.0 } },
        Emitter { freq_hz: 851.5375e6, level_dbfs: -50.0, modulation: c4fm, keying: Bursts { period_s: 13.0, duty: 0.3, phase: 0.5 } },
        Emitter { freq_hz: 853.6500e6, level_dbfs: -46.0, modulation: Fsk { levels: 2, sym_rate: 1200.0, dev_hz: 4500.0 }, keying: Bursts { period_s: 11.0, duty: 0.2, phase: 0.3 } },
        Emitter { freq_hz: 855.0000e6, level_dbfs: -62.0, modulation: Cw, keying: Continuous },
        Emitter { freq_hz: 856.2125e6, level_dbfs: -42.0, modulation: FmTone { tone_hz: 1000.0, dev_hz: 3000.0 }, keying: Bursts { period_s: 7.0, duty: 0.35, phase: 0.6 } },
        Emitter { freq_hz: 857.2375e6, level_dbfs: -45.0, modulation: c4fm, keying: Slotted { period_s: 8.0, duty: 0.55, phase: 0.1, slot_s: 0.030 } },
        Emitter { freq_hz: 858.8125e6, level_dbfs: -48.0, modulation: Fsk { levels: 4, sym_rate: 2400.0, dev_hz: 800.0 }, keying: Bursts { period_s: 6.0, duty: 0.3, phase: 0.2 } },
        Emitter { freq_hz: 859.4625e6, level_dbfs: -47.0, modulation: Fsk { levels: 4, sym_rate: 9600.0, dev_hz: 2900.0 }, keying: Bursts { period_s: 2.0, duty: 0.1, phase: 0.0 } },
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
    DeviceInfo { backend: "sim".into(), serial: "SIM-0001".into(), label: "Simulated SDR".into() }
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
}

impl SimDevice {
    pub fn new(emitters: Vec<Emitter>) -> Self {
        Self {
            info: info(),
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
            },
            emitters: Arc::new(emitters),
            center: Arc::new(AtomicU64::new(856e6f64.to_bits())),
            rate: 10e6,
            noise_dbfs: -62.0,
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
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
        self.running.store(true, Ordering::Relaxed);
        let generator = Generator {
            emitters: self.emitters.clone(),
            center: self.center.clone(),
            rate: self.rate,
            noise_amp: 10f64.powf(self.noise_dbfs / 20.0) as f32,
            running: self.running.clone(),
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
        self.worker.is_some()
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
}

struct EmState {
    z: Complex32,
    step: Complex32,
    amp: f32,
    sym_left: f64,
    tone_phase: f64,
}

impl Generator {
    fn run(self, mut sink: Box<dyn StreamSink>) {
        const BLOCK: usize = 65_536;
        let fs = self.rate;
        let mut center = f64::from_bits(self.center.load(Ordering::Relaxed));
        sink.on_event(StreamEvent::Retune { at_sample: 0, center_hz: center, sample_rate: fs });
        sink.on_event(StreamEvent::TimeAnchor { at_sample: 0, host_time: SystemTime::now() });
        let mut st: Vec<EmState> = self
            .emitters
            .iter()
            .map(|e| EmState {
                z: Complex32::new(1.0, 0.0),
                step: Complex32::new(1.0, 0.0),
                amp: 10f64.powf(e.level_dbfs / 20.0) as f32,
                sym_left: 0.0,
                tone_phase: 0.0,
            })
            .collect();
        let mut rng = XorShift(0x1234_5678_9abc_def1);
        let mut acc = vec![Complex32::default(); BLOCK];
        let mut out = vec![[0i16; 2]; BLOCK];
        let mut n: u64 = 0;
        let t0 = Instant::now();

        while self.running.load(Ordering::Relaxed) {
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
                            Modulation::Fsk { levels, sym_rate, dev_hz } => {
                                s.sym_left += fs / sym_rate;
                                let k = (rng.next() % levels as u64) as f64;
                                // Map 0..levels-1 onto ±1 (2FSK) or ±1/3, ±1 (4FSK).
                                (2.0 * k / (levels as f64 - 1.0) - 1.0) * dev_hz
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
                        Some(sl) => ((t_block + i as f64 / fs) / sl) as u64 % 2 == 0,
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
