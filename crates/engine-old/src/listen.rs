//! Listening: one channel of a live source demodulated to audio. Follows
//! the live edge of the IQ ring, down-converts the chosen channel and
//! demodulates it into a queue the app's audio output drains.
//!
//! P25 Phase 1 channels are decoded to speech (`detect::p25_voice`), each
//! transmission saved as a WAV. Following picks the channel itself: the
//! newest call a control channel is granting, held until it goes quiet.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use detect::Ddc;
use detect::decode::Call;
use detect::demod::{AUDIO_RATE, Demod, Mode};
use detect::p25_voice::{LiveVoice, Phase2Stream, VoiceStream};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::Shared;

/// What to listen to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListenSpec {
    pub center_hz: f64,
    pub mode: Mode,
    /// Follow granted calls (P25): the engine chooses the channel, and
    /// `center_hz` is only where it starts.
    pub follow: bool,
    /// P25 Phase 2 (TDMA): which logical channel (timeslot) to play, None
    /// for whichever starts talking. Phase 1 when `tdma` is false.
    pub tdma: bool,
    pub slot: Option<u8>,
}

/// Which talkgroups following prefers or ignores.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FollowPolicy {
    /// Never followed.
    pub lockout: std::collections::BTreeSet<u32>,
    /// Followed first, interrupting a call on any other talkgroup.
    pub priority: std::collections::BTreeSet<u32>,
}

/// The digital voice channel being played, for the UI.
#[derive(Debug, Clone, Default)]
pub struct VoiceStatus {
    pub center_hz: Option<f64>,
    /// Phase 2: the timeslot (logical channel) being played.
    pub slot: Option<u8>,
    /// Who is talking (from the voice channel's link control).
    pub call: Option<Call>,
    /// The cipher, when the transmission is encrypted (and so silent).
    pub encrypted: Option<&'static str>,
    /// Voice frames are arriving.
    pub active: bool,
    /// The current transmission is being recorded.
    pub recording: bool,
}

/// Audio waiting to be played, 48 kHz mono. Bounded: if the player stops
/// draining, the oldest audio is dropped so latency stays low.
#[derive(Default)]
pub struct AudioQueue {
    samples: Mutex<VecDeque<f32>>,
    /// (most samples held, samples to gather before playing after the
    /// queue ran dry): digital voice arrives in bursts and needs both.
    limits: Mutex<(usize, usize)>,
    /// Waiting to gather `limits.1` before playing again.
    starved: AtomicBool,
    /// The squelch is open (NFM), voice is playing (P25); always true for
    /// AM/WFM.
    pub open: AtomicBool,
}

/// Most audio buffered before the oldest is dropped.
const MAX_QUEUED_S: f64 = 0.4;
/// Where listening starts relative to the live edge.
const START_BEHIND_S: f64 = 0.05;
/// Digital voice is decoded a fifth of a second at a time, LDU by LDU, so
/// it gets a deeper queue and a jitter buffer.
const P25_MAX_QUEUED_S: f64 = 2.0;
const P25_PREBUFFER_S: f64 = 0.4;
/// Following starts a little in the past so a call's first words, spoken
/// before the switch, are heard.
const P25_START_BEHIND_S: f64 = 0.5;

impl AudioQueue {
    fn set_limits(&self, max_s: f64, prebuffer_s: f64) {
        *self.limits.lock() = ((max_s * AUDIO_RATE) as usize, (prebuffer_s * AUDIO_RATE) as usize);
    }

    fn push(&self, a: &[f32]) {
        let mut q = self.samples.lock();
        q.extend(a);
        let max = match self.limits.lock().0 {
            0 => (MAX_QUEUED_S * AUDIO_RATE) as usize,
            m => m,
        };
        if q.len() > max {
            let extra = q.len() - max;
            q.drain(..extra);
        }
    }

    /// Fills `out` with queued audio, zeros when there's none.
    pub fn pop_into(&self, out: &mut [f32]) {
        let mut q = self.samples.lock();
        let prebuffer = self.limits.lock().1;
        if self.starved.load(Ordering::Relaxed) && q.len() < prebuffer {
            out.iter_mut().for_each(|o| *o = 0.0);
            return;
        }
        self.starved.store(false, Ordering::Relaxed);
        for o in out.iter_mut() {
            *o = q.pop_front().unwrap_or(0.0);
        }
        if q.is_empty() {
            self.starved.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        self.samples.lock().clear();
    }

    /// Seconds of audio waiting, and whether playback is paused to gather
    /// the jitter buffer.
    pub fn depth(&self) -> (f64, bool) {
        (self.samples.lock().len() as f64 / AUDIO_RATE, self.starved.load(Ordering::Relaxed))
    }
}

/// Follow: a granted call stays a candidate this long after its last
/// announcement...
const GRANT_FRESH_S: f64 = 3.0;
/// ...and the followed channel is held while voice arrived this recently
/// (or, after switching, while waiting this long for the first).
const HOLD_S: f64 = 1.5;
/// Voice counts as playing while an LDU arrived this recently (they come
/// in batches every fifth of a second, a second behind).
const VOICE_ACTIVE_S: f64 = 1.0;
/// A talkgroup heard encrypted is skipped this long (its grants look clear
/// until the voice channel says otherwise).
const ENCRYPTED_LOCKOUT_S: f64 = 600.0;

/// The P25 chain: decoder, 8 → 48 kHz, level control, recorder.
struct P25Chain {
    voice: Box<dyn LiveVoice>,
    /// Phase 2: the decoder, for its timeslot (`voice` is the same object).
    tdma: bool,
    agc: Agc,
    /// Last 8 kHz sample, for interpolation.
    last: f32,
    rec: Recorder,
    /// When the chain was built (follow waits for the first voice).
    since: SystemTime,
}

impl P25Chain {
    fn new(fs: f64, tdma: bool, slot: Option<u8>) -> Self {
        let voice: Box<dyn LiveVoice> = if tdma { Box::new(Phase2Stream::new(fs, slot)) } else { Box::new(VoiceStream::new(fs)) };
        P25Chain { voice, tdma, agc: Agc::default(), last: 0.0, rec: Recorder::default(), since: SystemTime::now() }
    }
}

enum Chain {
    Analog(Box<Demod>),
    P25(Box<P25Chain>),
}

/// Speech level control for digital voice: the vocoder's output is quiet
/// (speech peaks well under a tenth of full scale). Gain follows the level
/// of frames with speech in them, rising slowly and falling fast, with a
/// soft limiter after it.
struct Agc {
    level: f32,
    gain: f32,
}

impl Default for Agc {
    fn default() -> Self {
        Agc { level: 0.02, gain: 5.0 }
    }
}

impl Agc {
    const TARGET: f32 = 0.15;

    fn process(&mut self, x: &mut [f32]) {
        for frame in x.chunks_mut(160) {
            let rms = (frame.iter().map(|v| v * v).sum::<f32>() / frame.len() as f32).sqrt();
            if rms > 0.003 {
                let k = if rms > self.level { 0.3 } else { 0.05 };
                self.level += k * (rms - self.level);
            }
            let want = (Self::TARGET / self.level.max(1e-4)).clamp(1.0, 40.0);
            for v in frame.iter_mut() {
                self.gain += 0.002 * (want - self.gain);
                *v = (*v * self.gain).tanh();
            }
        }
    }
}

/// A transmission's audio, written as a WAV when it ends:
/// `<recordings>/<UTC start>_<MHz>[_TG<n>[_<radio>]].wav`.
#[derive(Default)]
struct Recorder {
    samples: Vec<i16>,
    /// (start, channel) of the transmission being recorded.
    open: Option<(SystemTime, f64)>,
}

impl Recorder {
    fn start(&mut self, center_hz: f64) {
        self.open = Some((SystemTime::now(), center_hz));
        self.samples.clear();
    }

    fn push(&mut self, audio: &[f32]) {
        if self.open.is_some() {
            self.samples.extend(audio.iter().map(|v| (v * 32767.0).clamp(-32767.0, 32767.0) as i16));
        }
    }

    /// Writes the WAV, if there was at least half a second of speech (not
    /// an encrypted transmission or a one-LDU blip). Returns its path.
    fn finish(&mut self, call: Option<Call>) -> Option<PathBuf> {
        let (start, hz) = self.open.take()?;
        let pcm = std::mem::take(&mut self.samples);
        let speech = pcm.chunks(160).filter(|f| f.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / 160.0 > 300.0f64.powi(2)).count();
        if speech < 25 {
            return None;
        }
        let who = match call {
            Some(c) if c.group => format!("_TG{}{}", c.target, c.source.map(|s| format!("_{s}")).unwrap_or_default()),
            Some(c) => format!("_{}-to-{}", c.source.unwrap_or(0), c.target),
            None => String::new(),
        };
        let dir = recordings_dir();
        let path = dir.join(format!("{}_{:.5}MHz{who}.wav", crate::sigmf::stamp(start), hz / 1e6));
        let _ = std::fs::create_dir_all(&dir);
        match std::fs::write(&path, wav(&pcm)) {
            Ok(()) => Some(path),
            Err(e) => {
                log::warn!("recording {}: {e}", path.display());
                None
            }
        }
    }
}

/// Where call recordings go: next to the captures.
pub fn recordings_dir() -> PathBuf {
    crate::sigmf::default_dir().parent().map(|p| p.join("recordings")).unwrap_or_else(|| "recordings".into())
}

/// 8 kHz mono 16-bit WAV.
fn wav(pcm: &[i16]) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend(b"RIFF");
    w.extend((36 + data).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(8000u32.to_le_bytes());
    w.extend(16000u32.to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend(data.to_le_bytes());
    pcm.iter().for_each(|s| w.extend(s.to_le_bytes()));
    w
}

/// Following: the channel to be on. Stays on `current` while it carries
/// voice (or hasn't had time to); otherwise the call most recently
/// (re)granted inside the span, skipping Phase 2 (TDMA, not decoded yet)
/// and calls known to be encrypted.
fn follow_target(
    shared: &Shared,
    current: Option<(f64, Option<u8>, &P25Chain)>,
    lo: f64,
    hi: f64,
    locked: &HashMap<(bool, u32), SystemTime>,
) -> Option<(f64, Option<u8>)> {
    let now = SystemTime::now();
    let age = |t: SystemTime| now.duration_since(t).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let policy = shared.follow_policy.lock().clone();
    let group_in = |set: &std::collections::BTreeSet<u32>, c: Option<Call>| c.is_some_and(|c| c.group && set.contains(&c.target));
    let calls = shared.calls.lock();
    // (channel, slot, granted at, priority) of each call worth following.
    let candidates: Vec<((f64, Option<u8>), SystemTime, bool)> = calls
        .calls
        .iter()
        .rev()
        .take(256)
        .filter(|e| e.granted_at.is_some_and(|t| age(t) < GRANT_FRESH_S))
        .filter(|e| !e.call.encrypted && !group_in(&policy.lockout, Some(e.call)))
        .filter(|e| locked.get(&(e.call.group, e.call.target)).is_none_or(|&t| age(t) > ENCRYPTED_LOCKOUT_S))
        .filter_map(|e| {
            let f = e.center_hz.filter(|f| (lo..hi).contains(f))?;
            Some(((f, e.slot), e.granted_at?, group_in(&policy.priority, Some(e.call))))
        })
        .filter(|(t, _, _)| current.is_none_or(|(hz, slot, _)| (hz - t.0).abs() > 1.0 || slot != t.1))
        .collect();
    drop(calls);
    let best = candidates.iter().max_by_key(|(_, t, p)| (*p, *t)).copied();
    if let Some((hz, slot, c)) = current {
        let busy = match c.voice.idle_s() {
            Some(idle) => idle < HOLD_S && c.voice.cipher().is_none(),
            None => age(c.since) < HOLD_S,
        };
        let talking = c.voice.call();
        let current_priority = group_in(&policy.priority, talking);
        // A priority talkgroup preempts anything else; a locked-out one
        // (locked while it was playing) is left.
        let preempt = best.is_some_and(|(_, _, p)| p) && !current_priority;
        if busy && !preempt && !group_in(&policy.lockout, talking) {
            return Some((hz, slot));
        }
    }
    let keep = current.filter(|(_, _, c)| !group_in(&policy.lockout, c.voice.call())).map(|(hz, slot, _)| (hz, slot));
    best.map(|(t, _, _)| t).or(keep)
}

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64) {
    let mut chunk = vec![Complex32::default(); 1 << 16];
    let mut chan = Vec::new();
    let mut audio = Vec::new();
    // (spec, tuner centre) the chain was built for.
    let mut active: Option<(ListenSpec, f64, Ddc, Chain, u64)> = None;
    // Talkgroups (group, target) last heard encrypted, and when.
    let mut locked: HashMap<(bool, u32), SystemTime> = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        let asked = *shared.listen.lock();
        let tuner = shared.state.lock().center_hz;
        let Some(mut spec) = asked else {
            if let Some((s, _, _, Chain::P25(mut c), _)) = active.take() {
                finish_recording(&shared, s.center_hz, &mut c);
            }
            shared.audio.set_limits(MAX_QUEUED_S, 0.0);
            shared.audio.clear();
            *shared.voice.lock() = VoiceStatus::default();
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if spec.follow {
            spec.mode = Mode::P25;
            let half = rate * 0.45;
            let current = active.as_ref().and_then(|(s, _, _, c, _)| match c {
                Chain::P25(p) if s.follow => Some((s.center_hz, s.tdma.then_some(s.slot).flatten(), p.as_ref())),
                _ => None,
            });
            match follow_target(&shared, current, tuner - half, tuner + half, &locked) {
                Some((hz, slot)) => {
                    spec.center_hz = hz;
                    spec.tdma = slot.is_some();
                    spec.slot = slot;
                }
                None => {
                    // Nothing granted yet: wait.
                    *shared.voice.lock() = VoiceStatus::default();
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        if active.as_ref().is_none_or(|a| a.0 != spec || a.1 != tuner) {
            if let Some((s, _, _, Chain::P25(mut c), _)) = active.take() {
                finish_recording(&shared, s.center_hz, &mut c);
            }
            let (cutoff, fs) = spec.mode.channel();
            let ddc = Ddc::new(rate, spec.center_hz - tuner, cutoff, fs);
            let chain = match spec.mode {
                Mode::P25 => Chain::P25(Box::new(P25Chain::new(ddc.fs_out, spec.tdma, spec.slot))),
                m => Chain::Analog(Box::new(Demod::new(m, ddc.fs_out))),
            };
            let behind = match chain {
                Chain::P25(_) => {
                    shared.audio.set_limits(P25_MAX_QUEUED_S, P25_PREBUFFER_S);
                    P25_START_BEHIND_S
                }
                Chain::Analog(_) => {
                    shared.audio.set_limits(MAX_QUEUED_S, 0.0);
                    START_BEHIND_S
                }
            };
            let head = shared.ring.lock().head();
            active = Some((spec, tuner, ddc, chain, head.saturating_sub((behind * rate) as u64)));
            shared.audio.clear();
        }
        let Some((spec, _, ddc, chain, pos)) = active.as_mut() else { continue };
        let (oldest, head) = {
            let r = shared.ring.lock();
            (r.oldest(), r.head())
        };
        if *pos < oldest {
            *pos = head.saturating_sub((START_BEHIND_S * rate) as u64);
        }
        let mut worked = false;
        while *pos < head {
            let n = ((head - *pos) as usize).min(chunk.len());
            if !shared.ring.lock().read(*pos, &mut chunk[..n]) {
                break;
            }
            chan.clear();
            ddc.process(&chunk[..n], &mut chan);
            audio.clear();
            match chain {
                Chain::Analog(demod) => demod.process(&chan, &mut audio),
                Chain::P25(c) => {
                    p25_audio(&shared, spec.center_hz, c, &chan, &mut audio);
                    if let Some(call) = c.voice.call().filter(|_| c.voice.cipher().is_some()) {
                        locked.insert((call.group, call.target), SystemTime::now());
                    }
                }
            }
            shared.audio.push(&audio);
            *pos += n as u64;
            worked = true;
        }
        let open = match chain {
            Chain::Analog(demod) => demod.is_open(),
            Chain::P25(c) => c.voice.idle_s().is_some_and(|s| s < VOICE_ACTIVE_S),
        };
        shared.audio.open.store(open, Ordering::Relaxed);
        if !worked {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if let Some((s, _, _, Chain::P25(mut c), _)) = active.take() {
        finish_recording(&shared, s.center_hz, &mut c);
    }
}

/// Decodes channel IQ to 48 kHz speech, records it, and tells the call log
/// and the UI who is talking.
fn p25_audio(shared: &Shared, center_hz: f64, c: &mut P25Chain, chan: &[Complex32], out: &mut Vec<f32>) {
    let call_before = c.voice.call();
    let v = c.voice.push(chan);
    if v.started {
        // A previous transmission on this channel ended.
        if let Some(path) = c.rec.finish(call_before) {
            attach(shared, center_hz, call_before, path);
        }
        c.rec.start(center_hz);
    }
    let mut speech = v.audio;
    c.agc.process(&mut speech);
    c.rec.push(&speech);
    // 8 → 48 kHz, linear interpolation.
    for &s in &speech {
        for k in 1..=6 {
            out.push(c.last + (s - c.last) * k as f32 / 6.0);
        }
        c.last = s;
    }
    let label = if c.tdma { "P25 Phase 2" } else { "P25 Phase 1" };
    if let Some(mut call) = c.voice.call().filter(|_| v.ldus > 0) {
        call.encrypted |= c.voice.cipher().is_some();
        shared.calls.lock().heard(center_hz, label, call, SystemTime::now());
    }
    *shared.voice.lock() = VoiceStatus {
        center_hz: Some(center_hz),
        slot: slot_of(c),
        call: c.voice.call(),
        encrypted: c.voice.cipher(),
        active: c.voice.idle_s().is_some_and(|s| s < VOICE_ACTIVE_S),
        recording: c.rec.open.is_some(),
    };
    // Quiet for a second: the transmission is over.
    if c.voice.idle_s().is_some_and(|s| s > 1.0) && c.rec.open.is_some() {
        finish_recording(shared, center_hz, c);
    }
}

/// The Phase 2 timeslot a chain is playing.
fn slot_of(c: &P25Chain) -> Option<u8> {
    c.voice.slot()
}

fn finish_recording(shared: &Shared, center_hz: f64, c: &mut P25Chain) {
    let call = c.voice.call();
    if let Some(path) = c.rec.finish(call) {
        attach(shared, center_hz, call, path);
    }
}

/// Files a recording with its call in the call log.
fn attach(shared: &Shared, center_hz: f64, call: Option<Call>, path: PathBuf) {
    log::info!("recorded {}", path.display());
    if let Some(call) = call {
        shared.calls.lock().recorded(center_hz, call, path);
    }
}

/// A recording being played: a WAV (16-bit PCM, any rate, mono or the
/// first channel) fed at real time into its own audio queue.
pub struct Playback {
    pub path: PathBuf,
    pub queue: Arc<AudioQueue>,
    pub seconds: f64,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Playback {
    pub fn start(path: &std::path::Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let (rate, pcm) = read_wav(&bytes).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "not a 16-bit PCM WAV"))?;
        let queue = Arc::new(AudioQueue::default());
        queue.set_limits(1.0, 0.0);
        let (stop, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let seconds = pcm.len() as f64 / rate;
        let (q, st, dn) = (queue.clone(), stop.clone(), done.clone());
        std::thread::Builder::new().name("playback".into()).spawn(move || {
            // Linear interpolation to the output rate, 20 ms at a time.
            let step = rate / AUDIO_RATE;
            let out_len = (pcm.len() as f64 / step) as usize;
            let chunk = (0.02 * AUDIO_RATE) as usize;
            let t0 = std::time::Instant::now();
            let mut i = 0;
            while i < out_len && !st.load(Ordering::Relaxed) {
                let block: Vec<f32> = (i..(i + chunk).min(out_len))
                    .map(|k| {
                        let x = k as f64 * step;
                        let (j, f) = (x as usize, (x.fract()) as f32);
                        let a = pcm[j] as f32;
                        let b = *pcm.get(j + 1).unwrap_or(&pcm[j]) as f32;
                        (a + (b - a) * f) / 32768.0
                    })
                    .collect();
                q.push(&block);
                i += block.len();
                // Stay about 0.1 s ahead of the player.
                let ahead = i as f64 / AUDIO_RATE - t0.elapsed().as_secs_f64();
                if ahead > 0.1 {
                    std::thread::sleep(Duration::from_secs_f64(ahead - 0.1));
                }
            }
            if !st.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(150));
            }
            dn.store(true, Ordering::Relaxed);
        })?;
        Ok(Playback { path: path.to_path_buf(), queue, seconds, stop, done })
    }

    /// Played to the end (or stopped).
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.queue.clear();
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// (sample rate, samples of the first channel) of a 16-bit PCM WAV.
fn read_wav(b: &[u8]) -> Option<(f64, Vec<i16>)> {
    if b.get(0..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut at, mut fmt) = (12, None);
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        let body = b.get(at + 8..(at + 8 + len).min(b.len()))?;
        if id == b"fmt " && body.len() >= 16 {
            let channels = u16::from_le_bytes([body[2], body[3]]) as usize;
            let rate = u32::from_le_bytes(body[4..8].try_into().ok()?) as f64;
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if u16::from_le_bytes([body[0], body[1]]) != 1 || bits != 16 || channels == 0 {
                return None;
            }
            fmt = Some((rate, channels));
        } else if id == b"data" {
            let (rate, channels) = fmt?;
            let pcm = body.chunks_exact(2 * channels).map(|s| i16::from_le_bytes([s[0], s[1]])).collect();
            return Some((rate, pcm));
        }
        at += 8 + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trip() {
        let pcm: Vec<i16> = (0..400).map(|i| (i * 37 % 2000) as i16 - 1000).collect();
        let (rate, back) = read_wav(&wav(&pcm)).unwrap();
        assert_eq!((rate, back), (8000.0, pcm));
    }
}
