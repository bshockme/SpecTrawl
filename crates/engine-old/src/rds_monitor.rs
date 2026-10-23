//! RDS monitor: names the FM broadcast stations inside a live source's span.
//!
//! The spectral identifier finds the stations (`occupancy`); this thread
//! visits them in turn, oldest look first: it down-converts the station's
//! ±100 kHz out of the look-back ring at the live edge, demodulates it to
//! the multiplex and runs `detect::rds` for a few seconds, until the
//! programme service name and radio text are in or the dwell is up. The
//! result is kept on the station's `KnownOccupant`, which the UI labels
//! with it, and refreshed every `REVISIT`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use detect::Ddc;
use detect::rds::{Discriminator, RdsDecoder, RdsInfo};
use detect::spectral::Kind;
use sdr_core::Complex32;

use crate::Shared;

/// What RDS said about a station.
#[derive(Debug, Clone, PartialEq)]
pub struct StationInfo {
    pub pi: u16,
    /// US call sign, when the PI encodes one.
    pub call_sign: Option<String>,
    /// Programme service name (8 characters).
    pub ps: Option<String>,
    /// Radio text (song, slogan).
    pub rt: Option<String>,
    /// RBDS programme type name ("" when none).
    pub pty: &'static str,
    pub groups_ok: u32,
    pub groups_bad: u32,
}

impl StationInfo {
    fn from_info(i: &RdsInfo) -> Option<Self> {
        Some(Self {
            pi: i.pi?,
            call_sign: i.call_sign(),
            ps: i.ps.clone(),
            rt: i.rt.clone(),
            pty: i.pty_name(),
            groups_ok: i.groups_ok,
            groups_bad: i.groups_bad,
        })
    }

    /// Keeps earlier text where a fresh look didn't get it.
    fn merge(&mut self, old: &StationInfo) {
        if self.ps.is_none() {
            self.ps = old.ps.clone();
        }
        if self.rt.is_none() {
            self.rt = old.rt.clone();
        }
    }

    /// Call sign (else PI) and the service name: `KXYZ "TEST  FM"`.
    pub fn short(&self) -> String {
        let id = self.call_sign.clone().unwrap_or_else(|| format!("PI {:04X}", self.pi));
        match &self.ps {
            Some(ps) => format!("{id} \"{}\"", ps.trim()),
            None => id,
        }
    }
}

/// A station is looked at again after this.
const REVISIT: Duration = Duration::from_secs(10);
/// Longest look at one station.
const MAX_DWELL: Duration = Duration::from_secs(6);
/// A look ends early once the name is in and either the radio text is
/// complete or this many groups have been read.
const ENOUGH_GROUPS: u32 = 24;
const CHANNEL_RATE: f64 = 240e3;
const CHANNEL_CUTOFF: f64 = 100e3;

/// The FM station in view that has gone longest without a look, if any.
fn pick(shared: &Shared, tuner: f64, half: f64) -> Option<f64> {
    let occ = shared.occupancy.lock();
    occ.known
        .iter()
        .filter(|k| matches!(k.occupant.kind, Kind::FmBroadcast { .. }))
        .filter(|k| k.occupant.center_hz > tuner - half && k.occupant.center_hz < tuner + half)
        .filter(|k| k.station_at.is_none_or(|t| t.elapsed() > REVISIT))
        .min_by_key(|k| k.station_at)
        .map(|k| k.occupant.center_hz)
}

/// Runs `f` on the known FM occupant at `center_hz`.
fn with_station(shared: &Shared, center_hz: f64, f: impl FnOnce(&mut crate::KnownOccupant)) {
    let mut occ = shared.occupancy.lock();
    if let Some(k) = occ
        .known
        .iter_mut()
        .filter(|k| matches!(k.occupant.kind, Kind::FmBroadcast { .. }))
        .min_by(|a, b| (a.occupant.center_hz - center_hz).abs().total_cmp(&(b.occupant.center_hz - center_hz).abs()))
        .filter(|k| (k.occupant.center_hz - center_hz).abs() < 50e3)
    {
        f(k);
    }
}

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64) {
    let mut chunk = vec![Complex32::default(); 1 << 17];
    let mut chan: Vec<Complex32> = Vec::new();
    let mut mpx: Vec<f32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let tuner = shared.state.lock().center_hz;
        let Some(c) = pick(&shared, tuner, rate * 0.45) else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        // Stamp the attempt first, so a station that yields nothing isn't
        // retried at once.
        with_station(&shared, c, |k| k.station_at = Some(Instant::now()));
        let mut ddc = Ddc::new(rate, c - tuner, CHANNEL_CUTOFF, CHANNEL_RATE);
        let mut disc = Discriminator::new(ddc.fs_out);
        let mut dec = RdsDecoder::new(ddc.fs_out);
        let mut pos = shared.ring.lock().head().saturating_sub((0.1 * rate) as u64);
        let started = Instant::now();
        while !stop.load(Ordering::Relaxed) && started.elapsed() < MAX_DWELL {
            if shared.state.lock().center_hz != tuner {
                break;
            }
            let (oldest, head) = {
                let r = shared.ring.lock();
                (r.oldest(), r.head())
            };
            if pos < oldest {
                pos = head.saturating_sub((0.1 * rate) as u64);
            }
            if pos >= head {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let n = ((head - pos) as usize).min(chunk.len());
            if !shared.ring.lock().read(pos, &mut chunk[..n]) {
                break;
            }
            pos += n as u64;
            chan.clear();
            ddc.process(&chunk[..n], &mut chan);
            mpx.clear();
            disc.process(&chan, &mut mpx);
            dec.push(&mpx);
            let i = dec.info();
            if i.pi.is_some() && i.ps.is_some() && (i.rt_complete || i.groups_ok >= ENOUGH_GROUPS) {
                break;
            }
        }
        if let Some(mut s) = StationInfo::from_info(dec.info()) {
            log::debug!("RDS at {:.1} MHz: {} ({} groups)", c / 1e6, s.short(), s.groups_ok);
            with_station(&shared, c, |k| {
                if let Some(old) = &k.station {
                    s.merge(old);
                }
                k.station = Some(s);
            });
        }
    }
}
