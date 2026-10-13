//! Cell identification on cellular blocks in a live source's span.
//!
//! The spectral identifier only infers "LTE/NR" from a flat block of a
//! standard width in a downlink allocation. This thread proves it: for each
//! known cellular occupant inside the tuned span it pulls the central
//! 1.4 MHz out of the look-back ring, runs `detect::lte::identify` (OFDM
//! numerology, PSS/SSS, MIB) over ~45 ms, and files the result on the
//! occupant. One block every few seconds, round-robin, the least recently
//! checked first; an identified block is rechecked far less often.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use detect::Ddc;
use detect::spectral::Kind;
use sdr_core::Complex32;

use crate::Shared;

/// Signal analysed per look: four radio frames plus a margin, which gives
/// the MIB every TTI phase to combine.
const LOOK_S: f64 = 0.046;
/// Between looks at different blocks...
const PACE: Duration = Duration::from_secs(3);
/// ...and before a block is looked at again: unidentified ones sooner.
const RECHECK_UNKNOWN: Duration = Duration::from_secs(20);
const RECHECK_KNOWN: Duration = Duration::from_secs(120);
/// Half the slice taken out of the carrier (the sync signals need 1.08 MHz).
const HALF_SLICE_HZ: f64 = 0.7e6;

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64) {
    let mut chunk = vec![Complex32::default(); 1 << 16];
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(PACE);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let tuner = shared.state.lock().center_hz;
        let half = rate * 0.45;
        // The block whose turn it is: inside the span with room for the
        // slice, due for a look, least recently looked at first.
        let due = {
            let occ = shared.occupancy.lock();
            let now = Instant::now();
            occ.known
                .iter()
                .filter(|k| matches!(k.occupant.kind, Kind::Cellular { .. }))
                .filter(|k| k.occupant.center_hz - HALF_SLICE_HZ > tuner - half && k.occupant.center_hz + HALF_SLICE_HZ < tuner + half)
                .filter(|k| {
                    let wait = if k.cell.as_ref().is_some_and(|c| c.sync.is_some()) { RECHECK_KNOWN } else { RECHECK_UNKNOWN };
                    k.cell_checked.is_none_or(|t| now.duration_since(t) > wait)
                })
                .min_by_key(|k| k.cell_checked)
                .map(|k| k.occupant.center_hz)
        };
        let Some(center_hz) = due else { continue };
        // The newest LOOK_S of ring, down-converted to the slice.
        let (start, end) = {
            let r = shared.ring.lock();
            let head = r.head();
            (head.saturating_sub((LOOK_S * rate) as u64).max(r.oldest()), head)
        };
        // Not back across a retune or a loss.
        let start = start.max(shared.state.lock().continuous_from(end));
        if end <= start {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut ddc = Ddc::new(rate, center_hz - tuner, 0.6e6, detect::lte::FS);
        let mut slice = Vec::with_capacity(((end - start) as f64 / rate * ddc.fs_out) as usize + 16);
        let mut pos = start;
        let mut ok = true;
        while pos < end {
            let n = ((end - pos) as usize).min(chunk.len());
            if !shared.ring.lock().read(pos, &mut chunk[..n]) {
                ok = false;
                break;
            }
            ddc.process(&chunk[..n], &mut slice);
            pos += n as u64;
        }
        if !ok || slice.len() < (0.012 * ddc.fs_out) as usize {
            continue;
        }
        let cell = detect::lte::identify(&slice, ddc.fs_out);
        if let Some(c) = &cell {
            log::debug!("cell monitor: {:.3} MHz: {}", center_hz / 1e6, c.label());
        }
        shared.occupancy.lock().note_cell(center_hz, cell);
    }
}
