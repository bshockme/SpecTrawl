//! Replay and the audio queue (spec §4.7): what an interface asks to hear, and what it is told about it.

use std::path::PathBuf;

use crate::SourceId;
use crate::records::RecordId;

/// One replay, from `Command::Play` until it is stopped or finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayId(pub u64);

/// How a channel is turned into sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Am,
    Nfm,
    Wfm,
}

impl AudioMode {
    pub fn name(self) -> &'static str {
        match self {
            AudioMode::Am => "AM",
            AudioMode::Nfm => "NFM",
            AudioMode::Wfm => "WFM",
        }
    }
}

/// What to replay.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    /// A record: from the ring while it holds the record's start, else from the record's slice.
    Record(RecordId),
    /// A stretch of a source's ring (a waterfall selection): `from_sample..to_sample` around `center_hz`.
    Region { source: SourceId, center_hz: f64, bandwidth_hz: f64, from_sample: u64, to_sample: u64 },
}

/// Where the sound goes.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTo {
    /// The audio output, mixed with anything else playing.
    Speaker,
    /// A 16-bit mono WAV at 48 kHz, written once the replay has been demodulated.
    Wav(PathBuf),
}

/// Where a replay's samples come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayFrom {
    Ring,
    Slice,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayState {
    /// Filling the buffer, not yet playing (less than the jitter buffer is ready).
    Preparing,
    Playing,
    /// Everything was played (or written).
    Finished,
    /// Stopped by a `StopPlay`.
    Stopped,
    /// Ended before its end, with the reason: the source went away, the pool shed it, a read failed.
    EndedEarly(String),
}

impl PlayState {
    pub fn is_over(&self) -> bool {
        matches!(self, PlayState::Finished | PlayState::Stopped | PlayState::EndedEarly(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayView {
    pub id: PlayId,
    pub target: PlayTarget,
    pub mode: AudioMode,
    /// The mode was chosen by the engine's frequency rule, not asked for.
    pub mode_guessed: bool,
    pub from: PlayFrom,
    pub to: PlayTo,
    pub state: PlayState,
    /// Seconds played (or, for a WAV, demodulated).
    pub position_s: f64,
    /// Seconds of audio demodulated so far.
    pub length_s: f64,
    /// All of the replay's audio has been demodulated: `length_s` is final.
    pub complete: bool,
}

/// The audio output and every replay that has not been gone long.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioView {
    /// The output device's name, or why there is none.
    pub output: Result<String, String>,
    /// Running and recently ended replays, oldest first.
    pub plays: Vec<PlayView>,
    /// Times a playing replay ran out of demodulated audio and waited.
    pub underruns: u64,
}

impl Default for AudioView {
    fn default() -> Self {
        AudioView { output: Err("no audio output".into()), plays: Vec::new(), underruns: 0 }
    }
}
