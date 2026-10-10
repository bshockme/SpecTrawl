//! Live P25 Phase 1 voice: channel IQ in, speech out. Keeps the last
//! second of the channel and rescans it every fifth of a second with
//! `p25_voice_frames`, decoding each LDU once, in order. A second of
//! history lets an LDU whose sync was missed be recovered from its
//! neighbours (the flywheel) before it is played; the cost is that much
//! latency.

use num_complex::Complex32;

use crate::decode::{self, Call};
use crate::imbe;
use crate::p25_lc;

/// History kept and rescanned.
const WINDOW_S: f64 = 1.0;
/// How often to rescan.
const STEP_S: f64 = 0.2;
/// One LDU: 864 symbols at 4800 baud.
const LDU_S: f64 = 0.18;
/// A gap this long ends a transmission.
const END_S: f64 = 1.0;

/// What a push produced.
#[derive(Debug, Default)]
pub struct VoiceOut {
    /// Speech at 8 kHz, ±1 full scale (the vocoder's 16-bit scale).
    pub audio: Vec<f32>,
    /// LDUs decoded.
    pub ldus: usize,
    /// A new transmission started (after a gap) before this audio.
    pub started: bool,
}

pub struct VoiceStream {
    fs: f64,
    buf: Vec<Complex32>,
    /// Absolute sample index of `buf[0]`.
    buf_start: u64,
    /// Samples since the last scan.
    pending: usize,
    /// Absolute sample where the last decoded LDU's sync starts.
    last_ldu: Option<u64>,
    dec: imbe::Decoder,
    /// The current transmission: who (LDU1 link control) and whether it's
    /// encrypted (LDU1 service options, LDU2 algorithm).
    pub call: Option<Call>,
    pub encryption: Option<p25_lc::EncryptionSync>,
    encrypted: Option<bool>,
}

impl VoiceStream {
    /// For channel IQ at `fs` (≥ 24 kHz; 48 kHz is typical).
    pub fn new(fs: f64) -> Self {
        VoiceStream {
            fs,
            buf: Vec::new(),
            buf_start: 0,
            pending: 0,
            last_ldu: None,
            dec: Default::default(),
            call: None,
            encryption: None,
            encrypted: None,
        }
    }

    /// The current transmission is known to be encrypted.
    pub fn encrypted(&self) -> bool {
        self.encrypted == Some(true)
    }

    /// Seconds since the last LDU, as of the newest sample pushed.
    pub fn idle_s(&self) -> Option<f64> {
        self.last_ldu.map(|l| (self.buf_start + self.buf.len() as u64).saturating_sub(l) as f64 / self.fs)
    }

    pub fn push(&mut self, iq: &[Complex32]) -> VoiceOut {
        let mut out = VoiceOut::default();
        self.buf.extend_from_slice(iq);
        self.pending += iq.len();
        if (self.pending as f64) < STEP_S * self.fs {
            return out;
        }
        self.pending = 0;
        let keep = (WINDOW_S * self.fs) as usize;
        if self.buf.len() > keep {
            let drop = self.buf.len() - keep;
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
        let ldu_len = (LDU_S * self.fs) as u64;
        for f in crate::p25_voice_frames(&self.buf, self.fs) {
            let at = self.buf_start + (f.at_symbol as f64 * self.fs / 4800.0) as u64;
            if self.last_ldu.is_some_and(|l| at < l + ldu_len / 2) {
                continue;
            }
            let Some(frames) = imbe::ldu_frames(&f) else { continue };
            // Missed LDUs inside a transmission keep their time as silence;
            // a long gap starts a new transmission.
            match self.last_ldu {
                Some(l) if at - l > (END_S * self.fs) as u64 => {
                    self.new_transmission();
                    out.started = true;
                }
                Some(l) => {
                    let missing = ((at - l) as f64 / ldu_len as f64).round() as usize;
                    out.audio.extend(std::iter::repeat_n(0.0, missing.saturating_sub(1) * 9 * 160));
                }
                None => out.started = true,
            }
            self.last_ldu = Some(at);
            out.ldus += 1;
            match decode::p25_nid(&f).map(|n| n.duid) {
                Some(0x5) => {
                    if let Some(c) = p25_lc::ldu1(&f).and_then(|lc| lc.call()) {
                        self.encrypted = Some(c.encrypted);
                        self.call = Some(c);
                    }
                }
                _ => {
                    // An algorithm ID that's neither "clear" nor a known
                    // cipher is a miscorrected word: ignored.
                    if let Some(e) = p25_lc::ldu2(&f).filter(|e| e.algorithm() != "other") {
                        self.encrypted = Some(e.encrypted());
                        self.encryption = Some(e);
                    }
                }
            }
            if self.encrypted() {
                continue;
            }
            for c in &frames {
                let (s, _) = self.dec.decode(c);
                out.audio.extend(s.iter().map(|&v| (v / 32768.0) as f32));
            }
        }
        out
    }

    fn new_transmission(&mut self) {
        self.dec = Default::default();
        self.call = None;
        self.encryption = None;
        self.encrypted = None;
    }
}

/// A live digital voice decoder, whichever protocol.
pub trait LiveVoice: Send {
    /// Channel IQ in; speech, when any, out.
    fn push(&mut self, iq: &[Complex32]) -> VoiceOut;
    /// Who is talking in the current transmission.
    fn call(&self) -> Option<Call>;
    /// The current transmission's cipher, when it's encrypted.
    fn cipher(&self) -> Option<&'static str>;
    /// Seconds since voice last arrived.
    fn idle_s(&self) -> Option<f64>;
    /// Phase 2: the timeslot (logical channel) being played.
    fn slot(&self) -> Option<u8> {
        None
    }
}

impl LiveVoice for VoiceStream {
    fn push(&mut self, iq: &[Complex32]) -> VoiceOut {
        VoiceStream::push(self, iq)
    }
    fn call(&self) -> Option<Call> {
        self.call
    }
    fn cipher(&self) -> Option<&'static str> {
        self.encrypted().then(|| self.encryption.map(|e| e.algorithm()).unwrap_or("encrypted"))
    }
    fn idle_s(&self) -> Option<f64> {
        VoiceStream::idle_s(self)
    }
}

/// Phase 2 symbols per second.
const P2_BAUD: f64 = 6000.0;
/// Phase 2 history kept and rescanned: a fragment needs ~0.15 s around
/// its sync.
const P2_WINDOW_S: f64 = 0.8;

/// Live P25 Phase 2 voice on one logical channel (TDMA timeslot) of a
/// channel: rescans the last 0.8 s every fifth of a second, decoding each
/// voice burst once, in order. Scrambling uses the sequences of the
/// systems registered with `p25p2`.
pub struct Phase2Stream {
    fs: f64,
    /// The logical channel asked for (None: whichever starts talking).
    want: Option<u8>,
    /// The one being played.
    channel: Option<u8>,
    buf: Vec<Complex32>,
    buf_start: u64,
    pending: usize,
    /// Absolute symbol of the last decoded voice burst.
    last_burst: Option<u64>,
    dec: crate::ambe::Decoder,
    pub call: Option<Call>,
    /// The current transmission's algorithm ID (MAC_PTT), 0x80 = clear.
    pub algid: Option<u8>,
}

impl Phase2Stream {
    /// `channel`: logical channel 0 or 1, or None to take whichever
    /// starts a transmission first (until it ends).
    pub fn new(fs: f64, channel: Option<u8>) -> Self {
        Phase2Stream {
            fs,
            want: channel,
            channel,
            buf: Vec::new(),
            buf_start: 0,
            pending: 0,
            last_burst: None,
            dec: Default::default(),
            call: None,
            algid: None,
        }
    }

    fn symbol_now(&self) -> u64 {
        ((self.buf_start + self.buf.len() as u64) as f64 * P2_BAUD / self.fs) as u64
    }

    fn new_transmission(&mut self) {
        self.dec = Default::default();
        self.call = None;
        self.algid = None;
        self.channel = self.want;
    }

    /// The logical channel being played.
    pub fn channel(&self) -> Option<u8> {
        self.channel
    }
}

impl LiveVoice for Phase2Stream {
    fn push(&mut self, iq: &[Complex32]) -> VoiceOut {
        let mut out = VoiceOut::default();
        self.buf.extend_from_slice(iq);
        self.pending += iq.len();
        if (self.pending as f64) < STEP_S * self.fs {
            return out;
        }
        self.pending = 0;
        let keep = (P2_WINDOW_S * self.fs) as usize;
        if self.buf.len() > keep {
            let drop = self.buf.len() - keep;
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
        let seqs = crate::p25p2::candidate_sequences();
        let seqs: Vec<&[u8]> = seqs.iter().map(|s| s.as_slice()).collect();
        let base = (self.buf_start as f64 * P2_BAUD / self.fs) as u64;
        // Every fragment once (each has two syncs), in time order.
        let mut frags: Vec<(u64, crate::p25p2::FragmentInfo)> = Vec::new();
        for f in crate::frames(&self.buf, self.fs, 12_500.0, 560, 380).iter().filter(|f| f.protocol == "P25" && f.detail == "Phase 2") {
            let Some(info) = crate::p25p2::fragment(f, &seqs) else { continue };
            let start = (base + f.at_symbol as u64 + info.start as u64).saturating_sub(f.sync_start as u64);
            if !frags.iter().any(|(s, _)| s.abs_diff(start) < 5) {
                frags.push((start, info));
            }
        }
        frags.sort_by_key(|(s, _)| *s);
        // A transmission that went quiet ends here, so an automatic choice
        // of channel is free again.
        if self.last_burst.is_some_and(|l| self.symbol_now().saturating_sub(l) > (END_S * P2_BAUD) as u64) && self.want.is_none() {
            self.channel = None;
        }
        for (start, info) in frags {
            if self.channel.is_none() {
                self.channel = info.voice.first().map(|v| v.channel);
            }
            let Some(ch) = self.channel else { continue };
            for s in info.signals.iter().filter(|s| s.channel == ch) {
                match s.mac {
                    crate::p25p2::Mac::Ptt { end: false, algid, .. } => {
                        self.algid = Some(algid);
                        self.call = s.mac.call().or(self.call);
                    }
                    _ => {
                        if let Some(c) = s.mac.call() {
                            self.call = Some(c);
                        }
                    }
                }
            }
            for v in info.voice.iter().filter(|v| v.channel == ch) {
                let at = start + v.slot as u64 * 180;
                if self.last_burst.is_some_and(|l| at < l + 90) {
                    continue;
                }
                match self.last_burst {
                    Some(l) if at - l > (END_S * P2_BAUD) as u64 => {
                        self.new_transmission();
                        out.started = true;
                    }
                    None => out.started = true,
                    _ => {}
                }
                self.last_burst = Some(at);
                out.ldus += 1;
                if self.cipher().is_some() {
                    continue;
                }
                for f in &v.frames {
                    let (s, _) = self.dec.decode(&crate::ambe::decode(f));
                    out.audio.extend(s.iter().map(|&v| (v / 32768.0) as f32));
                }
            }
        }
        out
    }

    fn call(&self) -> Option<Call> {
        self.call
    }

    fn cipher(&self) -> Option<&'static str> {
        match self.algid {
            Some(a) if a != 0x80 && p25_lc::algorithm_name(a) != "other" => Some(p25_lc::algorithm_name(a)),
            _ => self.call.filter(|c| c.encrypted).map(|_| "encrypted"),
        }
    }

    fn idle_s(&self) -> Option<f64> {
        self.last_burst.map(|l| self.symbol_now().saturating_sub(l) as f64 / P2_BAUD)
    }

    fn slot(&self) -> Option<u8> {
        self.channel
    }
}
