//! Continuous decoding of identified control channels ("monitors").
//!
//! The classifier only samples a channel now and then. A monitor follows one
//! emitter continuously: it streams that channel out of the look-back ring
//! through its own down-converter and decodes every new stretch, with a small
//! overlap so frames spanning a boundary aren't lost.
//!
//! Today: P25 Phase 1 control channels (emitters whose decoded frames include
//! TSDUs). Their messages build a per-source P25 book: system identity,
//! sites, band plans, and a log of talkgroup grants.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use detect::p25::{self, SystemInfo};
use detect::{Ddc, decode};
use sdr_core::Complex32;

use crate::Shared;

/// Most monitors per source.
const MAX_MONITORS: usize = 2;
/// Decode once this much new channel signal has accumulated...
const BLOCK_S: f64 = 0.6;
/// ...keeping this much of the previous block, so boundary frames decode.
const OVERLAP_S: f64 = 0.12;
/// Identical grants within this window are one event.
const GRANT_DEDUP: Duration = Duration::from_secs(3);
const MAX_GRANTS: usize = 5000;

#[derive(Debug, Clone)]
pub struct GrantEvent {
    pub first: SystemTime,
    pub last: SystemTime,
    pub nac: u16,
    pub talkgroup: u16,
    pub freq_hz: Option<f64>,
    pub slot: Option<u8>,
    pub source: Option<u32>,
    /// Times the grant was (re)announced.
    pub count: u32,
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub emitter: u64,
    pub center_hz: f64,
    pub label: String,
    pub messages: u64,
}

/// What the monitors have learned on one source.
#[derive(Default)]
pub(crate) struct P25Book {
    pub systems: HashMap<u16, SystemInfo>,
    pub grants: VecDeque<GrantEvent>,
    pub monitors: Vec<MonitorInfo>,
}

impl P25Book {
    fn record(&mut self, nac: u16, msgs: &[p25::Message], now: SystemTime) {
        let sys = self.systems.entry(nac).or_insert_with(|| SystemInfo { nac: Some(nac), ..Default::default() });
        let mut new = Vec::new();
        for m in msgs {
            for (tg, ch, src) in sys.apply(m) {
                new.push(GrantEvent {
                    first: now,
                    last: now,
                    nac,
                    talkgroup: tg,
                    freq_hz: sys.freq(ch),
                    slot: sys.slot(ch),
                    source: src,
                    count: 1,
                });
            }
        }
        for g in new {
            let same = self.grants.iter_mut().rev().take(64).find(|o| {
                o.nac == g.nac
                    && o.talkgroup == g.talkgroup
                    && o.freq_hz == g.freq_hz
                    && o.slot == g.slot
                    && g.first.duration_since(o.last).map(|d| d < GRANT_DEDUP).unwrap_or(true)
            });
            match same {
                Some(o) => {
                    o.last = g.last;
                    o.count += 1;
                    if o.source.is_none() {
                        o.source = g.source;
                    }
                }
                None => {
                    if self.grants.len() >= MAX_GRANTS {
                        self.grants.pop_front();
                    }
                    self.grants.push_back(g);
                }
            }
        }
    }
}

struct Mon {
    emitter: u64,
    center_hz: f64,
    tuner_hz: f64,
    ddc: Ddc,
    pos: u64,
    buf: Vec<Complex32>,
    messages: u64,
    label: String,
}

pub(crate) fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, rate: f64) {
    let mut mons: Vec<Mon> = Vec::new();
    let mut chunk = vec![Complex32::default(); 1 << 18];
    let mut last_pick = Instant::now() - Duration::from_secs(10);
    while !stop.load(Ordering::Relaxed) {
        let tuner = shared.state.lock().center_hz;
        // Drop monitors from an old tuning; look for new candidates now and then.
        mons.retain(|m| m.tuner_hz == tuner);
        if last_pick.elapsed() > Duration::from_secs(2) {
            last_pick = Instant::now();
            let candidates: Vec<(u64, f64, String)> = {
                let st = shared.records.lock();
                st.emitters
                    .map
                    .values()
                    .filter(|e| is_p25_control(e))
                    // "CC 0x2A5": control channel and its NAC.
                    .map(|e| (e.id, e.center_hz, format!("CC {}", e.id().map(|i| i.trim_start_matches("NAC ")).unwrap_or("P25"))))
                    .collect()
            };
            for (id, c, label) in candidates {
                if mons.len() >= MAX_MONITORS || mons.iter().any(|m| m.emitter == id || (m.center_hz - c).abs() < 6e3) {
                    continue;
                }
                let head = shared.ring.lock().head();
                mons.push(Mon {
                    emitter: id,
                    center_hz: c,
                    tuner_hz: tuner,
                    ddc: Ddc::new(rate, c - tuner, 8e3, 48e3),
                    pos: head.saturating_sub((0.5 * rate) as u64),
                    buf: Vec::new(),
                    messages: 0,
                    label,
                });
            }
        }
        let mut worked = false;
        for m in mons.iter_mut() {
            let (oldest, head) = {
                let r = shared.ring.lock();
                (r.oldest(), r.head())
            };
            if m.pos < oldest {
                // Fell behind the ring: skip ahead (some frames are lost).
                m.pos = head.saturating_sub((0.2 * rate) as u64);
                m.buf.clear();
            }
            while m.pos < head {
                let n = ((head - m.pos) as usize).min(chunk.len());
                if !shared.ring.lock().read(m.pos, &mut chunk[..n]) {
                    break;
                }
                m.ddc.process(&chunk[..n], &mut m.buf);
                m.pos += n as u64;
                worked = true;
            }
            let fs = m.ddc.fs_out;
            if m.buf.len() as f64 >= (BLOCK_S + OVERLAP_S) * fs {
                let frames = detect::frames(&m.buf, fs, 12_500.0, 8, 380);
                // The NAC from the NIDs that decoded identifies the system.
                let mut nacs: HashMap<u16, usize> = HashMap::new();
                for f in &frames {
                    if let Some(n) = decode::p25_nid(f) {
                        *nacs.entry(n.nac).or_default() += 1;
                    }
                }
                let msgs = p25::messages(&frames);
                m.messages += msgs.len() as u64;
                if let Some((&nac, _)) = nacs.iter().max_by_key(|(_, n)| **n) {
                    shared.p25.lock().record(nac, &msgs, SystemTime::now());
                }
                let keep = (OVERLAP_S * fs) as usize;
                let cut = m.buf.len() - keep;
                m.buf.drain(..cut);
            }
        }
        shared.p25.lock().monitors = mons
            .iter()
            .map(|m| MonitorInfo { emitter: m.emitter, center_hz: m.center_hz, label: m.label.clone(), messages: m.messages })
            .collect();
        if !worked {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A P25 Phase 1 emitter whose decoded frames include TSDUs (trunking
/// signalling): a control channel.
fn is_p25_control(e: &crate::Emitter) -> bool {
    e.has_protocol()
        && e.label().is_some_and(|l| l == "P25 Phase 1")
        && e.best.as_ref().and_then(|c| c.fields.as_ref()).is_some_and(|f| f.kinds.iter().any(|(k, _)| k == "TSDU"))
}
