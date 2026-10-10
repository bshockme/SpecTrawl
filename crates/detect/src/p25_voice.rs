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
