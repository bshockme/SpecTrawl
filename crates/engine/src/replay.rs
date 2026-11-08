//! Replay (spec §4.7): a record, or a region of a source, demodulated into an [`AudioBuf`] that the mixer plays or that
//! is written out as a WAV.
//!
//! - **From the ring**, while it holds the start: a bus subscription back-filled from the pre-roll, demodulated on the
//!   bus worker, ending at the record's end plus the channel filter's delay (following the live edge while the record
//!   is still open).
//! - **From the record's slice** (§4.8) when the ring no longer has the start or the source is gone: the slice file is
//!   read and demodulated on the player's `replay` thread, which also writes WAVs.
//! - Otherwise the play is refused, with the reason.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use api::{AudioMode, AudioView, IqClip, PlayFrom, PlayId, PlayState, PlayTarget, PlayTo, PlayView, RecordId, SignalRecord};
use dsp::demod::{Demod, Mode as DemodMode};
use parking_lot::Mutex;

use crate::audio::{AudioBuf, AudioConfig, AudioOutput, Mixer, RATE, write_wav};
use crate::channels::{BusError, ChannelBus, ChannelId, ChannelSpec, Consumer, Delivery, EndReason, Flow, GapReason, Priority, Start};
use crate::records::RecordBook;
use crate::source::Source;

/// A replay starts this long before its record and ends this long after it, as its slice does
/// ([`crate::recorder::SliceConfig::new`]).
pub const PRE_ROLL: Duration = crate::classify::LEAD;
pub const POST_ROLL: Duration = crate::classify::LEAD;
/// Ended plays stay in the view this many views (20 Hz snapshots: 10 s).
const KEEP_ENDED_VIEWS: u64 = 200;

/// The engine's stand-in for the detector bank (step 6): AM on the air bands, WFM on broadcast FM, NFM elsewhere.
pub fn default_mode(center_hz: f64) -> AudioMode {
    let mhz = center_hz / 1e6;
    if (108.0..137.0).contains(&mhz) || (225.0..400.0).contains(&mhz) {
        AudioMode::Am
    } else if (88.0..108.0).contains(&mhz) {
        AudioMode::Wfm
    } else {
        AudioMode::Nfm
    }
}

fn demod_mode(m: AudioMode) -> DemodMode {
    match m {
        AudioMode::Am => DemodMode::Am,
        AudioMode::Nfm => DemodMode::Nfm,
        AudioMode::Wfm => DemodMode::Wfm,
    }
}

/// The channel a replay is cut at: the record's width with its skirts (× 1.25), at least the mode's own and at most
/// twice it (the demodulator low-passes the rest away anyway).
pub fn channel_width(mode: AudioMode, record_bw_hz: f64) -> f64 {
    let own = 2.0 * demod_mode(mode).channel().0;
    (1.25 * record_bw_hz).clamp(own, 2.0 * own)
}

/// A demodulator with its squelch open: a replay plays the whole record, noise and all.
fn demod(mode: AudioMode, fs: f64) -> Demod {
    let mut d = Demod::new(demod_mode(mode), fs);
    d.squelch = 0.0;
    d
}

/// What a play's producer reports back.
#[derive(Default)]
struct Shared {
    /// The last input sample to play (`u64::MAX` while the record is open).
    end: AtomicU64,
    cancel: AtomicBool,
    /// Why it stopped short.
    early: Mutex<Option<String>>,
    /// The WAV is written (or failed: `early` says why).
    written: AtomicBool,
}

impl Shared {
    fn end_early(&self, why: impl Into<String>) {
        self.early.lock().get_or_insert_with(|| why.into());
    }
}

struct Play {
    id: PlayId,
    target: PlayTarget,
    mode: AudioMode,
    guessed: bool,
    from: PlayFrom,
    to: PlayTo,
    buf: Arc<AudioBuf>,
    shared: Arc<Shared>,
    channel: Option<ChannelId>,
    /// The record is still open: its end (and the post-roll, in source samples) is passed on when it closes.
    open_record: Option<RecordId>,
    post: u64,
    on_speaker: bool,
    wav_queued: bool,
    stopped: bool,
    /// The view count at which it ended.
    ended_at: Option<u64>,
}

enum Job {
    Slice { clip: IqClip, mode: AudioMode, buf: Arc<AudioBuf>, shared: Arc<Shared> },
    Wav { path: PathBuf, buf: Arc<AudioBuf>, shared: Arc<Shared> },
}

/// A play as asked for, once the engine has found what it targets.
pub enum Resolved {
    /// A record and, when the ring may still hold it, its source.
    Record {
        record: Arc<SignalRecord>,
        source: Option<Arc<Source>>,
    },
    Region {
        source: Arc<Source>,
        center_hz: f64,
        bandwidth_hz: f64,
        from_sample: u64,
        to_sample: u64,
    },
}

pub struct Player {
    mixer: Arc<Mixer>,
    output: AudioOutput,
    plays: Mutex<BTreeMap<PlayId, Play>>,
    next: AtomicU64,
    views: AtomicU64,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Player {
    /// Starts the output and the `replay` thread.
    pub fn new(audio: AudioConfig) -> Player {
        let mixer = Mixer::new();
        let output = AudioOutput::start(audio, mixer.clone());
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::Builder::new().name("replay".into()).spawn(move || work(rx)).expect("spawn the replay thread");
        Player {
            mixer,
            output,
            plays: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            views: AtomicU64::new(0),
            jobs: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn mixer(&self) -> &Arc<Mixer> {
        &self.mixer
    }

    /// Starts a replay; `Err` says why it can't be heard.
    pub fn play(
        &self,
        bus: &ChannelBus,
        what: Resolved,
        target: PlayTarget,
        mode: Option<AudioMode>,
        gain: f32,
        to: PlayTo,
    ) -> Result<PlayId, String> {
        let id = PlayId(self.next.fetch_add(1, Ordering::Relaxed));
        let buf = AudioBuf::new();
        let shared = Arc::new(Shared { end: AtomicU64::new(u64::MAX), ..Default::default() });
        let (center, bw) = match &what {
            Resolved::Record { record, .. } => (record.center_hz(), record.bandwidth_hz()),
            Resolved::Region { center_hz, bandwidth_hz, .. } => (*center_hz, *bandwidth_hz),
        };
        let guessed = mode.is_none();
        let mode = mode.unwrap_or_else(|| default_mode(center));
        let mut open_record = None;
        let mut post = 0;
        let (from, channel) = match what {
            Resolved::Region { source, from_sample, to_sample, .. } => {
                if to_sample <= from_sample {
                    return Err("an empty region".into());
                }
                shared.end.store(to_sample, Ordering::Relaxed);
                let ch = ring(bus, &source, center, channel_width(mode, bw), from_sample, mode, &buf, &shared)
                    .map_err(|e| format!("the ring no longer holds that region ({e})"))?;
                (PlayFrom::Ring, Some(ch))
            }
            Resolved::Record { record, source } => {
                let rate = source.as_ref().map(|s| s.segment_at(record.first_sample).sample_rate);
                let pre = rate.map_or(0, |r| (PRE_ROLL.as_secs_f64() * r) as u64);
                post = rate.map_or(0, |r| (POST_ROLL.as_secs_f64() * r) as u64);
                if record.open {
                    open_record = Some(record.id);
                } else {
                    shared.end.store(record.end_sample + post, Ordering::Relaxed);
                }
                let from_ring = source
                    .as_ref()
                    .map(|s| ring(bus, s, center, channel_width(mode, bw), record.first_sample.saturating_sub(pre), mode, &buf, &shared));
                match from_ring {
                    Some(Ok(ch)) => (PlayFrom::Ring, Some(ch)),
                    held => {
                        let why = match held {
                            Some(Err(e)) => e,
                            _ => "its source is gone".into(),
                        };
                        let Some(clip) = record.iq_clip.clone() else {
                            let slice = if record.slice_evicted {
                                "its slice was evicted to keep under the disk cap"
                            } else if record.open {
                                "its slice is written when it closes"
                            } else {
                                "it has no slice"
                            };
                            return Err(format!("record {} can't be heard: {why}, and {slice}", record.id.0));
                        };
                        self.send(Job::Slice { clip, mode, buf: buf.clone(), shared: shared.clone() })?;
                        (PlayFrom::Slice, None)
                    }
                }
            }
        };
        let on_speaker = to == PlayTo::Speaker;
        if on_speaker {
            self.mixer.add(id, buf.clone(), gain);
        }
        self.plays.lock().insert(
            id,
            Play {
                id,
                target,
                mode,
                guessed,
                from,
                to,
                buf,
                shared,
                channel,
                open_record,
                post,
                on_speaker,
                wav_queued: false,
                stopped: false,
                ended_at: None,
            },
        );
        Ok(id)
    }

    /// Stops a replay at once; `false` if there is none (or it already ended).
    pub fn stop(&self, bus: &ChannelBus, id: PlayId) -> bool {
        let mut plays = self.plays.lock();
        let Some(p) = plays.get_mut(&id).filter(|p| p.ended_at.is_none()) else { return false };
        p.shared.cancel.store(true, Ordering::Relaxed);
        if let Some(ch) = p.channel.take() {
            bus.unsubscribe(ch);
        }
        p.buf.finish();
        self.mixer.remove(id);
        p.stopped = true;
        true
    }

    /// The audio view; also passes open records' ends on, queues finished WAVs and forgets plays ended long ago.
    pub fn view(&self, book: &RecordBook) -> AudioView {
        let n = self.views.fetch_add(1, Ordering::Relaxed);
        let mut plays = self.plays.lock();
        let mut out = Vec::with_capacity(plays.len());
        for p in plays.values_mut() {
            if let Some(id) = p.open_record {
                match book.get(id) {
                    Some(r) if r.open => {}
                    Some(r) => {
                        p.shared.end.store(r.end_sample + p.post, Ordering::Relaxed);
                        p.open_record = None;
                    }
                    // Merged into another or gone: what was heard is all there will be.
                    None => {
                        p.shared.end_early(format!("record {} was merged away", id.0));
                        p.buf.finish();
                        p.open_record = None;
                    }
                }
            }
            if let PlayTo::Wav(path) = &p.to
                && !p.wav_queued
                && p.buf.is_complete()
                && !p.stopped
            {
                p.wav_queued = true;
                let job = Job::Wav { path: path.clone(), buf: p.buf.clone(), shared: p.shared.clone() };
                if let Some(tx) = self.jobs.lock().as_ref() {
                    let _ = tx.send(job);
                }
            }
            let state = self.state_of(p);
            if state.is_over() && p.ended_at.is_none() {
                p.ended_at = Some(n);
                if p.on_speaker {
                    self.mixer.remove(p.id);
                }
            }
            let played = if p.on_speaker { self.mixer.position(p.id).map_or(p.buf.len(), |v| v.played) } else { p.buf.len() };
            out.push(PlayView {
                id: p.id,
                target: p.target.clone(),
                mode: p.mode,
                mode_guessed: p.guessed,
                from: p.from,
                to: p.to.clone(),
                state,
                position_s: played as f64 / RATE,
                length_s: p.buf.len() as f64 / RATE,
                complete: p.buf.is_complete(),
            });
        }
        plays.retain(|_, p| p.ended_at.is_none_or(|at| n - at < KEEP_ENDED_VIEWS));
        AudioView { output: self.output.status(), plays: out, underruns: self.mixer.underruns() }
    }

    fn state_of(&self, p: &Play) -> PlayState {
        if p.stopped {
            return PlayState::Stopped;
        }
        let early = p.shared.early.lock().clone();
        let finished = match &p.to {
            PlayTo::Speaker => match self.mixer.position(p.id) {
                Some(v) if v.done => true,
                Some(v) if v.started => return PlayState::Playing,
                Some(_) => return PlayState::Preparing,
                // Taken out of the mixer: it had ended.
                None => true,
            },
            PlayTo::Wav(_) => p.shared.written.load(Ordering::Relaxed),
        };
        match (finished, early) {
            (true, Some(why)) => PlayState::EndedEarly(why),
            (true, None) => PlayState::Finished,
            (false, _) => PlayState::Preparing,
        }
    }

    fn send(&self, job: Job) -> Result<(), String> {
        let tx = self.jobs.lock();
        tx.as_ref().ok_or("the replay thread has stopped")?.send(job).map_err(|_| "the replay thread has stopped".to_string())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        for p in self.plays.lock().values() {
            p.shared.cancel.store(true, Ordering::Relaxed);
        }
        drop(self.jobs.lock().take());
        if let Some(t) = self.worker.lock().take()
            && t.join().is_err()
        {
            log::error!("the replay thread panicked");
        }
    }
}

/// Subscribes a ring replay from input sample `from`.
#[allow(clippy::too_many_arguments)]
fn ring(
    bus: &ChannelBus,
    source: &Arc<Source>,
    center_hz: f64,
    bandwidth_hz: f64,
    from: u64,
    mode: AudioMode,
    buf: &Arc<AudioBuf>,
    shared: &Arc<Shared>,
) -> Result<ChannelId, String> {
    let stage = bus.stage(source.id()).ok_or("its source is not on the bus")?;
    let consumer = RingReplay {
        mode,
        demod: None,
        buf: buf.clone(),
        shared: shared.clone(),
        group_delay: stage.fft_size() as u64 / 4,
        hop: stage.hop(),
        input_rate: source.segment_at(from).sample_rate,
        audio: Vec::new(),
    };
    let spec = ChannelSpec { source: source.id(), center_hz, bandwidth_hz, min_rate: bandwidth_hz, parent: None };
    match bus.subscribe(spec, Start::At(from), Priority::Listen, Box::new(consumer)) {
        Ok(info) => Ok(info.id),
        Err(BusError::NotHeld { .. }) => Err("its start has left the ring".into()),
        Err(e) => Err(format!("{e:?}")),
    }
}

struct RingReplay {
    mode: AudioMode,
    demod: Option<Demod>,
    buf: Arc<AudioBuf>,
    shared: Arc<Shared>,
    /// The channel filter's delay in input samples: a block's samples show the signal of that much earlier.
    group_delay: u64,
    hop: u64,
    input_rate: f64,
    audio: Vec<f32>,
}

impl RingReplay {
    fn stop(&mut self, early: Option<String>) -> Flow {
        if let Some(why) = early {
            self.shared.end_early(why);
        }
        self.buf.finish();
        Flow::Stop
    }
}

impl Consumer for RingReplay {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        if self.shared.cancel.load(Ordering::Relaxed) {
            return self.stop(None);
        }
        match d {
            Delivery::Samples { first_sample, samples, .. } => {
                if first_sample.saturating_sub(self.group_delay) >= self.shared.end.load(Ordering::Relaxed) {
                    return self.stop(None);
                }
                let (mode, rate) = (self.mode, self.input_rate * samples.len() as f64 / self.hop as f64);
                let dm = self.demod.get_or_insert_with(|| demod(mode, rate));
                self.audio.clear();
                dm.process(samples, &mut self.audio);
                if !self.buf.push(&self.audio) {
                    return self.stop(None);
                }
                Flow::Continue
            }
            Delivery::Gap { from_sample, reason, .. } => {
                if from_sample >= self.shared.end.load(Ordering::Relaxed) {
                    return self.stop(None);
                }
                let why = match reason {
                    GapReason::OutOfSpan => "the receiver retuned away".to_string(),
                    GapReason::Unreadable(e) => format!("the ring lost samples ({e:?})"),
                };
                self.stop(Some(why))
            }
            Delivery::Shed => self.stop(Some("shed by a saturated worker pool".into())),
            Delivery::Ended { reason } => self.stop(Some(
                match reason {
                    EndReason::SourceDetached => "the source went away",
                    EndReason::RateChanged => "the sample rate changed",
                    EndReason::Panicked => "the replay panicked",
                }
                .into(),
            )),
        }
    }
}

/// The `replay` thread: slices demodulated, WAVs written, one at a time.
fn work(rx: mpsc::Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Slice { clip, mode, buf, shared } => {
                if let Err(e) = slice(&clip, mode, &buf, &shared) {
                    shared.end_early(format!("reading its slice failed: {e}"));
                }
                buf.finish();
            }
            Job::Wav { path, buf, shared } => {
                if let Err(e) = write_wav(&path, &buf.to_vec(), RATE) {
                    shared.end_early(format!("writing {} failed: {e}", path.display()));
                }
                shared.written.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Demodulates a slice into `buf`, 0.1 s at a time. Slices are cf32; those written before 2026-11-07 are ci16, and
/// the metadata says which.
fn slice(clip: &IqClip, mode: AudioMode, buf: &AudioBuf, shared: &Shared) -> std::io::Result<()> {
    let format = crate::sigmf::Format::of_meta(&clip.path.with_extension("sigmf-meta"))?;
    let mut f = std::fs::File::open(clip.path.with_extension("sigmf-data"))?;
    let mut dm = demod(mode, clip.rate);
    let chunk = ((clip.rate * 0.1) as usize).max(1);
    let size = format.bytes();
    let mut raw = vec![0u8; chunk * size];
    let (mut x, mut audio) = (Vec::with_capacity(chunk), Vec::new());
    let mut left = clip.samples;
    while left > 0 && !shared.cancel.load(Ordering::Relaxed) {
        let n = (left as usize).min(chunk);
        f.read_exact(&mut raw[..n * size])?;
        x.clear();
        format.decode(&raw[..n * size], &mut x);
        audio.clear();
        dm.process(&x, &mut audio);
        if !buf.push(&audio) {
            break;
        }
        left -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_follows_the_band() {
        assert_eq!(default_mode(124.9e6), AudioMode::Am);
        assert_eq!(default_mode(282.8e6), AudioMode::Am);
        assert_eq!(default_mode(97.7e6), AudioMode::Wfm);
        assert_eq!(default_mode(855.6375e6), AudioMode::Nfm);
        assert_eq!(default_mode(137.5e6), AudioMode::Nfm);
    }

    #[test]
    fn the_channel_is_the_records_with_skirts_within_the_modes_bounds() {
        assert_eq!(channel_width(AudioMode::Am, 6e3), 10e3);
        assert_eq!(channel_width(AudioMode::Am, 12e3), 15e3);
        assert_eq!(channel_width(AudioMode::Am, 1e6), 20e3);
        assert_eq!(channel_width(AudioMode::Nfm, 12.5e3), 15.625e3);
        assert_eq!(channel_width(AudioMode::Wfm, 150e3), 200e3);
    }
}
