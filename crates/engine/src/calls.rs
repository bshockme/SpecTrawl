//! The call log: one entry per call, merged from what a control channel
//! granted and what the voice channel's own signalling said (P25 link
//! control, Phase 2 MAC, DMR call headers).
//!
//! Voice channels are only sampled by the classifier now and then, so the
//! times are approximate: a call's span is from the first to the last time
//! anything saw it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use detect::decode::Call;
use detect::trunk::SystemKey;

/// Sightings of the same call (same frequency and target, compatible
/// source) less than this apart are one call.
const SAME_CALL: Duration = Duration::from_secs(8);
/// Frequencies this close are the same channel.
const SAME_FREQ_HZ: f64 = 3_000.0;
const MAX_CALLS: usize = 5000;
/// Follow a call's voice channel while its grant is this fresh...
const FOLLOW_FRESH: Duration = Duration::from_secs(3);
/// ...sampling it this often.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct CallEvent {
    pub first: SystemTime,
    pub last: SystemTime,
    /// The voice channel, when known (a grant names it; a sighting is on it).
    pub center_hz: Option<f64>,
    /// The TDMA timeslot (logical channel 0 or 1) a Phase 2 grant named.
    pub slot: Option<u8>,
    /// "P25 Phase 1", "P25 Phase 2", "DMR …": the voice channel's label, or
    /// the control channel's system for a grant not yet heard.
    pub protocol: String,
    /// The trunked system that granted it, when a control channel did.
    pub system: Option<SystemKey>,
    pub call: Call,
    /// A control channel granted it.
    pub granted: bool,
    /// Its voice channel was heard carrying it.
    pub heard: bool,
    /// When a control channel last (re)announced it.
    pub granted_at: Option<SystemTime>,
    /// When the classifier last looked at its voice channel for it.
    pub(crate) followed_at: Option<SystemTime>,
    /// Its audio, when it was listened to (one file per transmission).
    pub recordings: Vec<PathBuf>,
}

#[derive(Default)]
pub(crate) struct CallLog {
    pub calls: VecDeque<CallEvent>,
}

impl CallLog {
    /// A control channel granted `call` on `freq`.
    pub fn granted(&mut self, freq: Option<f64>, slot: Option<u8>, system: &str, key: Option<SystemKey>, call: Call, now: SystemTime) {
        self.merge(freq, call, now, system, |e| {
            e.slot = slot.or(e.slot);
            // A key learned later (WACN after the first grants) is fuller.
            if key.is_some() {
                e.system = key;
            }
            e.granted = true;
            e.granted_at = Some(now);
            // The grant's channel is exact; a voice sighting's is measured.
            e.center_hz = freq.or(e.center_hz);
        });
    }

    /// A voice channel at `freq` was heard carrying `call`.
    pub fn heard(&mut self, freq: f64, label: &str, call: Call, now: SystemTime) {
        self.merge(Some(freq), call, now, label, |e| {
            e.heard = true;
            // The voice channel's label beats the control channel's.
            e.protocol = label.to_string();
        });
    }

    /// A transmission of `call` on `freq` was recorded to `path`.
    pub fn recorded(&mut self, freq: f64, call: Call, path: PathBuf) {
        self.merge(Some(freq), call, SystemTime::now(), "P25 Phase 1", |e| e.recordings.push(path));
    }

    fn merge(&mut self, freq: Option<f64>, call: Call, now: SystemTime, label: &str, mark: impl FnOnce(&mut CallEvent)) {
        let same = self.calls.iter_mut().rev().take(256).find(|e| {
            e.call.group == call.group
                && e.call.target == call.target
                && (e.call.source.is_none() || call.source.is_none() || e.call.source == call.source)
                && match (e.center_hz, freq) {
                    (Some(a), Some(b)) => (a - b).abs() < SAME_FREQ_HZ,
                    _ => true,
                }
                && now.duration_since(e.last).map(|d| d < SAME_CALL).unwrap_or(true)
        });
        match same {
            Some(e) => {
                e.last = e.last.max(now);
                e.center_hz = e.center_hz.or(freq);
                e.call.source = e.call.source.or(call.source);
                e.call.encrypted |= call.encrypted;
                e.call.emergency |= call.emergency;
                mark(e);
            }
            None => {
                if self.calls.len() >= MAX_CALLS {
                    self.calls.pop_front();
                }
                let mut e =
                    CallEvent {
                        first: now,
                        last: now,
                        center_hz: freq,
                        slot: None,
                        protocol: label.to_string(),
                        system: None,
                        call,
                        granted: false,
                        heard: false,
                        granted_at: None,
                        followed_at: None,
                        recordings: Vec::new(),
                    };
                mark(&mut e);
                self.calls.push_back(e);
            }
        }
    }
}

impl CallLog {
    /// Trunk following: the voice channel to look at next, if any. A
    /// granted call whose grant is still being announced, on a frequency
    /// inside `lo..hi`, not looked at within `FOLLOW_EVERY`; the one waiting
    /// longest goes first. Marks it as looked at.
    pub fn follow(&mut self, now: SystemTime, lo: f64, hi: f64) -> Option<f64> {
        let age = |t: Option<SystemTime>| t.and_then(|t| now.duration_since(t).ok());
        let e = self
            .calls
            .iter_mut()
            .rev()
            .take(256)
            .filter(|e| e.center_hz.is_some_and(|f| (lo..hi).contains(&f)))
            .filter(|e| age(e.granted_at).is_some_and(|a| a < FOLLOW_FRESH))
            .filter(|e| age(e.followed_at).is_none_or(|a| a >= FOLLOW_EVERY))
            .max_by_key(|e| age(e.followed_at).unwrap_or(Duration::MAX))?;
        e.followed_at = Some(now);
        e.center_hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tg: u32, src: Option<u32>) -> Call {
        Call { group: true, target: tg, source: src, encrypted: false, emergency: false }
    }

    #[test]
    fn grant_and_voice_merge_into_one_call() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let mut log = CallLog::default();
        log.granted(Some(770.31875e6), None, "NAC 0x2A5", None, call(10025, Some(2121592)), t0);
        // The voice channel's link control, heard 2 s later and 1 kHz off
        // (measured centre), first without the source.
        log.heard(770.3198e6, "P25 Phase 1", call(10025, None), t0 + Duration::from_secs(2));
        log.heard(770.3198e6, "P25 Phase 1", call(10025, Some(2121592)), t0 + Duration::from_secs(5));
        assert_eq!(log.calls.len(), 1);
        let e = &log.calls[0];
        assert!(e.granted && e.heard);
        assert_eq!((e.protocol.as_str(), e.call.source), ("P25 Phase 1", Some(2121592)));
        assert_eq!(e.last.duration_since(e.first).unwrap(), Duration::from_secs(5));
        // A different talker on the same talkgroup is a new call; so is the
        // same one after a long gap.
        log.heard(770.3198e6, "P25 Phase 1", call(10025, Some(2121604)), t0 + Duration::from_secs(6));
        log.heard(770.3198e6, "P25 Phase 1", call(10025, Some(2121592)), t0 + Duration::from_secs(30));
        assert_eq!(log.calls.len(), 3);
    }

    #[test]
    fn follows_fresh_grants_in_span() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let mut log = CallLog::default();
        log.granted(Some(770.31875e6), None, "NAC 0x2A5", None, call(10025, Some(1)), t0);
        log.granted(Some(860.0e6), None, "NAC 0x2A5", None, call(10044, Some(2)), t0);
        let (lo, hi) = (766e6, 776e6);
        assert_eq!(log.follow(t0, lo, hi), Some(770.31875e6));
        // Just looked at; out of span; so nothing until FOLLOW_EVERY.
        assert_eq!(log.follow(t0 + Duration::from_secs(1), lo, hi), None);
        // The grant is re-announced, so it's followed again...
        log.granted(Some(770.31875e6), None, "NAC 0x2A5", None, call(10025, Some(1)), t0 + Duration::from_secs(2));
        assert_eq!(log.follow(t0 + Duration::from_secs(2), lo, hi), Some(770.31875e6));
        // ...until the grants stop.
        assert_eq!(log.follow(t0 + Duration::from_secs(6), lo, hi), None);
    }
}
