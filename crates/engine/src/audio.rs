//! The audio queue (spec §4.7): replays fill buffers of 48 kHz mono audio, a [`Mixer`] sums the voices playing them
//! with their gains, and one output pulls the mix: the default sound device, a paced discard for headless runs, or
//! nothing (a test pulls [`Mixer::render`] itself).
//!
//! The output's callback only copies samples under short locks; nothing in it waits on the engine.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use api::PlayId;
use parking_lot::Mutex;

/// Samples per second of every buffer and of the mix.
pub const RATE: f64 = dsp::demod::AUDIO_RATE;
/// A voice starts (and restarts after running dry) once this much is ready, or its buffer is complete.
pub const JITTER: Duration = Duration::from_millis(100);
/// The longest replay: 120 s (about 23 MB).
pub const MAX_SECS: f64 = 120.0;

/// `<Documents>/SpecTrawl/audio`: where a replay's WAV goes when its path is relative.
pub fn default_dir() -> std::path::PathBuf {
    crate::store::default_path().with_file_name("audio")
}

fn jitter_samples() -> usize {
    (JITTER.as_secs_f64() * RATE) as usize
}

/// A replay's demodulated audio, appended to by its producer and read by the mixer (or written out once complete).
#[derive(Default)]
pub struct AudioBuf {
    inner: Mutex<BufInner>,
}

#[derive(Default)]
struct BufInner {
    samples: Vec<f32>,
    complete: bool,
}

impl AudioBuf {
    pub fn new() -> Arc<AudioBuf> {
        Arc::new(AudioBuf::default())
    }

    /// Appends; returns `false` (and completes the buffer) once it holds [`MAX_SECS`].
    pub fn push(&self, x: &[f32]) -> bool {
        let max = (MAX_SECS * RATE) as usize;
        let mut b = self.inner.lock();
        if b.complete {
            return false;
        }
        let room = max.saturating_sub(b.samples.len());
        b.samples.extend_from_slice(&x[..x.len().min(room)]);
        if x.len() >= room {
            b.complete = true;
            return false;
        }
        true
    }

    /// No more audio will come.
    pub fn finish(&self) {
        self.inner.lock().complete = true;
    }

    pub fn len(&self) -> usize {
        self.inner.lock().samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_complete(&self) -> bool {
        self.inner.lock().complete
    }

    /// A copy of everything so far.
    pub fn to_vec(&self) -> Vec<f32> {
        self.inner.lock().samples.clone()
    }
}

struct Voice {
    id: PlayId,
    buf: Arc<AudioBuf>,
    pos: usize,
    gain: f32,
    /// Waiting for the jitter buffer to fill (at the start, or after running dry).
    waiting: bool,
    started: bool,
}

#[derive(Default)]
struct MixState {
    voices: Vec<Voice>,
    underruns: u64,
    rendered: u64,
}

/// Where a voice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoicePos {
    pub played: usize,
    pub started: bool,
    /// Its buffer is complete and every sample of it has been played.
    pub done: bool,
}

/// Sums the playing voices.
#[derive(Default)]
pub struct Mixer {
    state: Mutex<MixState>,
}

impl Mixer {
    pub fn new() -> Arc<Mixer> {
        Arc::new(Mixer::default())
    }

    pub fn add(&self, id: PlayId, buf: Arc<AudioBuf>, gain: f32) {
        self.state.lock().voices.push(Voice { id, buf, pos: 0, gain, waiting: true, started: false });
    }

    pub fn remove(&self, id: PlayId) {
        self.state.lock().voices.retain(|v| v.id != id);
    }

    pub fn set_gain(&self, id: PlayId, gain: f32) {
        if let Some(v) = self.state.lock().voices.iter_mut().find(|v| v.id == id) {
            v.gain = gain;
        }
    }

    pub fn position(&self, id: PlayId) -> Option<VoicePos> {
        let s = self.state.lock();
        let v = s.voices.iter().find(|v| v.id == id)?;
        let done = v.buf.is_complete() && v.pos >= v.buf.len();
        Some(VoicePos { played: v.pos, started: v.started, done })
    }

    pub fn underruns(&self) -> u64 {
        self.state.lock().underruns
    }

    /// Samples rendered since the mixer was made.
    pub fn rendered(&self) -> u64 {
        self.state.lock().rendered
    }

    /// Overwrites `out` with the next `out.len()` samples of the mix.
    pub fn render(&self, out: &mut [f32]) {
        out.iter_mut().for_each(|v| *v = 0.0);
        let mut s = self.state.lock();
        s.rendered += out.len() as u64;
        let mut underruns = 0;
        for v in s.voices.iter_mut() {
            let b = v.buf.inner.lock();
            let avail = b.samples.len().saturating_sub(v.pos);
            if v.waiting {
                if avail < jitter_samples() && !b.complete {
                    continue;
                }
                v.waiting = false;
            }
            let n = avail.min(out.len());
            for (o, x) in out.iter_mut().zip(&b.samples[v.pos..v.pos + n]) {
                *o += x * v.gain;
            }
            v.pos += n;
            v.started |= n > 0;
            if n < out.len() && !b.complete {
                underruns += 1;
                v.waiting = true;
            }
        }
        s.underruns += underruns;
    }
}

/// Which output pulls the mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioConfig {
    /// The default sound device.
    #[default]
    Device,
    /// No sound: the mix is pulled at real time and dropped (headless runs, where plays must still advance).
    Paced,
    /// Nothing pulls; a test calls [`Mixer::render`].
    Manual,
}

/// The running output: owns its thread (which, for a device, owns the stream) and stops it on drop.
pub struct AudioOutput {
    status: Arc<Mutex<Result<String, String>>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioOutput {
    pub fn start(cfg: AudioConfig, mixer: Arc<Mixer>) -> AudioOutput {
        let status = Arc::new(Mutex::new(Err("starting".to_string())));
        let (tx, rx) = mpsc::channel::<()>();
        let thread = match cfg {
            AudioConfig::Manual => {
                *status.lock() = Ok("manual (pulled by the caller)".into());
                None
            }
            AudioConfig::Paced => {
                *status.lock() = Ok("paced (no sound)".into());
                Some(
                    std::thread::Builder::new()
                        .name("audio-paced".into())
                        .spawn(move || paced(mixer, rx))
                        .expect("spawn the paced audio output"),
                )
            }
            AudioConfig::Device => {
                let st = status.clone();
                Some(
                    std::thread::Builder::new()
                        .name("audio-out".into())
                        .spawn(move || device(mixer, rx, st))
                        .expect("spawn the audio output"),
                )
            }
        };
        AudioOutput { status, stop: Some(tx), thread }
    }

    /// The device's name, or why there is no sound.
    pub fn status(&self) -> Result<String, String> {
        self.status.lock().clone()
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            log::error!("the audio output thread panicked");
        }
    }
}

/// Pulls the mix at real time, in 10 ms steps, until told to stop.
fn paced(mixer: Arc<Mixer>, stop: mpsc::Receiver<()>) {
    let t0 = Instant::now();
    let mut pulled = 0u64;
    let mut scratch = Vec::new();
    loop {
        match stop.recv_timeout(Duration::from_millis(10)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let due = (t0.elapsed().as_secs_f64() * RATE) as u64;
        // After a stall, catch up at most 200 ms (as a sound card would drop, not burst).
        let n = (due - pulled).min((0.2 * RATE) as u64) as usize;
        pulled = due;
        scratch.resize(n, 0.0);
        mixer.render(&mut scratch);
    }
}

/// Opens the default device and plays the mix until told to stop. The stream lives on this thread (cpal's streams
/// may not move between threads).
fn device(mixer: Arc<Mixer>, stop: mpsc::Receiver<()>, status: Arc<Mutex<Result<String, String>>>) {
    match open_device(mixer, status.clone()) {
        Ok((stream, name)) => {
            log::info!("audio: playing on {name}");
            *status.lock() = Ok(name);
            // Until the sender drops.
            let _ = stop.recv();
            drop(stream);
        }
        Err(e) => {
            log::warn!("audio: no output: {e}");
            *status.lock() = Err(e);
        }
    }
}

fn open_device(mixer: Arc<Mixer>, status: Arc<Mutex<Result<String, String>>>) -> Result<(cpal::Stream, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no audio output device")?;
    let name = dev.name().unwrap_or_else(|_| "default output".into());
    // 48 kHz f32 when the device offers it; else its default, resampled.
    let want = cpal::SampleRate(RATE as u32);
    let cfg = dev
        .supported_output_configs()
        .ok()
        .and_then(|mut it| {
            it.find(|c| c.sample_format() == cpal::SampleFormat::F32 && c.min_sample_rate() <= want && want <= c.max_sample_rate())
        })
        .map(|c| c.with_sample_rate(want))
        .map_or_else(|| dev.default_output_config().map_err(|e| e.to_string()), Ok)?;
    let channels = usize::from(cfg.channels());
    let mut rs = Resampler::new(f64::from(cfg.sample_rate().0));
    let on_error = move |e: cpal::StreamError| {
        log::warn!("audio: {e}");
        *status.lock() = Err(e.to_string());
    };
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            dev.build_output_stream(&cfg.config(), move |data: &mut [f32], _| rs.fill(&mixer, data, channels, |v| v), on_error, None)
        }
        cpal::SampleFormat::I16 => dev.build_output_stream(
            &cfg.config(),
            move |data: &mut [i16], _| rs.fill(&mixer, data, channels, |v| (v.clamp(-1.0, 1.0) * 32767.0) as i16),
            on_error,
            None,
        ),
        f => return Err(format!("unsupported audio sample format {f:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, name))
}

/// Mix samples to device frames, linearly interpolated when the device's rate is not 48 kHz.
struct Resampler {
    /// Mix samples per device frame.
    step: f64,
    pos: f64,
    prev: f32,
    cur: f32,
    mono: Vec<f32>,
}

impl Resampler {
    fn new(device_rate: f64) -> Resampler {
        Resampler { step: RATE / device_rate, pos: 0.0, prev: 0.0, cur: 0.0, mono: Vec::new() }
    }

    fn fill<T: Copy>(&mut self, mixer: &Mixer, data: &mut [T], channels: usize, conv: impl Fn(f32) -> T) {
        let frames = data.len() / channels.max(1);
        // How many mix samples these frames consume: one each time `pos` reaches 1.
        let mut p = self.pos;
        let mut need = 0;
        for _ in 0..frames {
            while p >= 1.0 {
                need += 1;
                p -= 1.0;
            }
            p += self.step;
        }
        self.mono.resize(need, 0.0);
        mixer.render(&mut self.mono);
        let mut i = 0;
        for frame in data.chunks_mut(channels.max(1)) {
            while self.pos >= 1.0 {
                self.prev = self.cur;
                self.cur = self.mono.get(i).copied().unwrap_or(0.0);
                i += 1;
                self.pos -= 1.0;
            }
            let v = conv(self.prev + (self.cur - self.prev) * self.pos as f32);
            frame.iter_mut().for_each(|o| *o = v);
            self.pos += self.step;
        }
    }
}

/// A 16-bit mono WAV.
pub fn write_wav(path: &Path, x: &[f32], rate: f64) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let data_len = (x.len() * 2) as u32;
    let rate = rate as u32;
    let mut b = Vec::with_capacity(44 + x.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for v in x {
        b.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    std::fs::write(path, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(n: usize, v: f32, complete: bool) -> Arc<AudioBuf> {
        let b = AudioBuf::new();
        b.push(&vec![v; n]);
        if complete {
            b.finish();
        }
        b
    }

    #[test]
    fn two_voices_mix_with_their_gains_and_a_finished_one_reports_done() {
        let m = Mixer::new();
        m.add(PlayId(1), filled(1000, 0.5, true), 1.0);
        m.add(PlayId(2), filled(400, 0.25, true), 2.0);
        let mut out = vec![0.0; 600];
        m.render(&mut out);
        assert!((out[0] - 1.0).abs() < 1e-6 && (out[399] - 1.0).abs() < 1e-6, "{} {}", out[0], out[399]);
        assert!((out[400] - 0.5).abs() < 1e-6);
        assert!(m.position(PlayId(2)).unwrap().done);
        assert!(!m.position(PlayId(1)).unwrap().done);
        m.render(&mut out);
        assert!(m.position(PlayId(1)).unwrap().done);
        assert_eq!(m.underruns(), 0, "a complete buffer running out is not an underrun");
    }

    #[test]
    fn the_jitter_buffer_waits_and_a_dry_buffer_counts_one_underrun() {
        let m = Mixer::new();
        let b = AudioBuf::new();
        m.add(PlayId(1), b.clone(), 1.0);
        b.push(&vec![0.1; 1000]);
        let mut out = vec![0.0; 480];
        m.render(&mut out);
        assert!(out.iter().all(|&v| v == 0.0), "started before 100 ms was ready");
        assert!(!m.position(PlayId(1)).unwrap().started);
        b.push(&vec![0.1; jitter_samples()]);
        m.render(&mut out);
        assert!(out.iter().all(|&v| (v - 0.1).abs() < 1e-6));
        // Drain what is there, then run dry: one underrun, then silence until 100 ms is ready again.
        let mut big = vec![0.0; 10_000];
        m.render(&mut big);
        assert_eq!(m.underruns(), 1);
        b.push(&vec![0.2; 100]);
        m.render(&mut out);
        assert!(out.iter().all(|&v| v == 0.0), "restarted before 100 ms was ready");
        assert_eq!(m.underruns(), 1, "waiting is not a new underrun");
    }

    #[test]
    fn a_buffer_stops_at_its_cap() {
        let b = AudioBuf::new();
        let max = (MAX_SECS * RATE) as usize;
        assert!(b.push(&vec![0.0; max - 10]));
        assert!(!b.push(&[0.0; 20]));
        assert_eq!(b.len(), max);
        assert!(b.is_complete());
    }

    #[test]
    fn the_paced_output_pulls_at_real_time() {
        let m = Mixer::new();
        let out = AudioOutput::start(AudioConfig::Paced, m.clone());
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(500));
        let secs = m.rendered() as f64 / RATE;
        let el = t0.elapsed().as_secs_f64();
        drop(out);
        assert!((secs - el).abs() < 0.1 * el, "{secs:.3} s rendered in {el:.3} s");
    }

    #[test]
    fn the_resampler_keeps_a_tone_at_its_frequency() {
        // A 1 kHz tone at 48 kHz played to a 44.1 kHz device.
        let m = Mixer::new();
        let tone: Vec<f32> = (0..48_000).map(|n| (std::f64::consts::TAU * 1000.0 * n as f64 / RATE).sin() as f32).collect();
        let b = AudioBuf::new();
        b.push(&tone);
        b.finish();
        m.add(PlayId(1), b, 1.0);
        let mut rs = Resampler::new(44_100.0);
        let mut data = vec![0.0f32; 2 * 22_050];
        rs.fill(&m, &mut data, 2, |v| v);
        let left: Vec<f32> = data.chunks(2).map(|f| f[0]).collect();
        assert!(data.chunks(2).all(|f| f[0] == f[1]), "mono goes to both channels");
        let crossings = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        // Half a second of a 1 kHz tone.
        assert!(crossings.abs_diff(500) <= 2, "{crossings}");
    }

    #[test]
    fn a_wav_has_the_header_and_samples() {
        let dir = std::env::temp_dir().join(format!("spectrawl-wav-{}", std::process::id()));
        let p = dir.join("a.wav");
        write_wav(&p, &[0.0, 0.5, -1.0], RATE).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 48_000);
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(i16::from_le_bytes([b[46], b[47]]), 16384);
        assert_eq!(i16::from_le_bytes([b[48], b[49]]), -32767);
        let _ = std::fs::remove_dir_all(dir);
    }
}
