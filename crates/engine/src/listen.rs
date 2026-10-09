//! Listening: one channel of a live source demodulated to audio. Follows
//! the live edge of the IQ ring, down-converts the chosen channel and
//! demodulates it into a queue the app's audio output drains.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use detect::Ddc;
use detect::demod::{AUDIO_RATE, Demod, Mode};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::Shared;

/// What to listen to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListenSpec {
    pub center_hz: f64,
    pub mode: Mode,
}

/// Audio waiting to be played, 48 kHz mono. Bounded: if the player stops
/// draining, the oldest audio is dropped so latency stays low.
#[derive(Default)]
pub struct AudioQueue {
    samples: Mutex<VecDeque<f32>>,
    /// The squelch is open (NFM); always true for AM/WFM.
    pub open: AtomicBool,
}

/// Most audio buffered before the oldest is dropped.
const MAX_QUEUED_S: f64 = 0.4;
/// Where listening starts relative to the live edge.
const START_BEHIND_S: f64 = 0.05;

impl AudioQueue {
    fn push(&self, a: &[f32]) {
        let mut q = self.samples.lock();
        q.extend(a);
        let max = (MAX_QUEUED_S * AUDIO_RATE) as usize;
        if q.len() > max {
            let extra = q.len() - max;
            q.drain(..extra);
        }
    }

    /// Fills `out` with queued audio, zeros when there's none.
    pub fn pop_into(&self, out: &mut [f32]) {
        let mut q = self.samples.lock();
        for o in out.iter_mut() {
            *o = q.pop_front().unwrap_or(0.0);
        }
    }

    pub fn clear(&self) {
        self.samples.lock().clear();
    }
}

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64) {
    let mut chunk = vec![Complex32::default(); 1 << 16];
    let mut chan = Vec::new();
    let mut audio = Vec::new();
    // (spec, tuner centre) the chain was built for.
    let mut active: Option<(ListenSpec, f64, Ddc, Demod, u64)> = None;
    while !stop.load(Ordering::Relaxed) {
        let spec = *shared.listen.lock();
        let tuner = shared.state.lock().center_hz;
        let Some(spec) = spec else {
            if active.take().is_some() {
                shared.audio.clear();
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if active.as_ref().is_none_or(|a| a.0 != spec || a.1 != tuner) {
            let (cutoff, fs) = spec.mode.channel();
            let ddc = Ddc::new(rate, spec.center_hz - tuner, cutoff, fs);
            let demod = Demod::new(spec.mode, ddc.fs_out);
            let head = shared.ring.lock().head();
            active = Some((spec, tuner, ddc, demod, head.saturating_sub((START_BEHIND_S * rate) as u64)));
            shared.audio.clear();
        }
        let Some((_, _, ddc, demod, pos)) = active.as_mut() else { continue };
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
            demod.process(&chan, &mut audio);
            shared.audio.push(&audio);
            *pos += n as u64;
            worked = true;
        }
        shared.audio.open.store(demod.is_open(), Ordering::Relaxed);
        if !worked {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
