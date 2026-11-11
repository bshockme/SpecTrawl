//! A P25 Phase 1 control channel, decoded natively with `detect` (re-homing `engine-old`'s monitor): frames on 0.6 s
//! blocks with 0.12 s of overlap, the NAC by majority over the frames' NIDs, and the TSBKs folded into one
//! `SystemInfo`.

use std::collections::BTreeMap;

use api::{Claim, Evidence, Identity, Lamp};
use detect::p25::{self, SystemInfo};
use sdr_core::Complex32;

use super::{Decoder, Telemetry};

/// Seconds of channel decoded at a time, and how much of each block the next one repeats (a frame across the join is
/// whole in one of them).
const BLOCK_S: f64 = 0.6;
const OVERLAP_S: f64 = 0.12;
/// Symbols per second.
const BAUD: f64 = 4800.0;
/// A decoder on the channel, not a 1.5 s look: surer than the bank's own claim of the site.
pub const CONFIDENCE: f32 = 0.999;

pub struct P25Control {
    center_hz: f64,
    rate: f64,
    buf: Vec<Complex32>,
    /// Blocks decoded so far (the first has no overlap to skip).
    blocks: u64,
    info: SystemInfo,
    nacs: BTreeMap<u16, u64>,
    tsbks: u64,
    grants: u64,
    /// TSDU frames whose TSBKs all failed their CRC.
    bad: u64,
    lamp: Lamp,
    /// The site has been registered for Phase 2 descrambling.
    registered: bool,
}

impl P25Control {
    pub fn new(center_hz: f64) -> P25Control {
        P25Control {
            center_hz,
            rate: 0.0,
            buf: Vec::new(),
            blocks: 0,
            info: SystemInfo::default(),
            nacs: BTreeMap::new(),
            tsbks: 0,
            grants: 0,
            bad: 0,
            lamp: Lamp::Searching,
            registered: false,
        }
    }

    /// The NAC most NIDs name, once two do.
    fn nac(&self) -> Option<u16> {
        self.nacs
            .iter()
            .max_by_key(|&(nac, n)| (*n, std::cmp::Reverse(*nac)))
            .filter(|(_, n)| **n >= crate::classify::bank::MIN_NIDS as u64)
            .map(|(nac, _)| *nac)
    }

    fn identity(&self) -> Option<Identity> {
        let i = &self.info;
        Some(Identity::P25 { nac: self.nac()?, wacn: i.wacn, system: i.system, rfss: i.rfss, site: i.site })
    }

    fn decode(&mut self, x: &[Complex32]) {
        let skip = if self.blocks == 0 { 0 } else { (OVERLAP_S * BAUD) as usize };
        self.blocks += 1;
        // TSDUs carry up to three TSBKs: slice far enough past each sync. A weak control channel gives few exact syncs;
        // the weak pass's slicing (soft syncs on the symbol discriminator) is kept when more of its NIDs decode.
        let nids = |f: &[detect::frames::Frame]| f.iter().filter(|f| detect::decode::p25_nid(f).is_some()).count();
        let mut frames: Vec<_> = detect::frames(x, self.rate, 12_500.0, 8, 380).into_iter().filter(|f| f.at_symbol >= skip).collect();
        let weak: Vec<_> = detect::p25_weak_frames(x, self.rate, 380).into_iter().filter(|f| f.at_symbol >= skip).collect();
        if nids(&weak) > nids(&frames) {
            frames = weak;
        }
        let mut locked = false;
        for f in &frames {
            let Some(nid) = detect::decode::p25_nid(f) else { continue };
            *self.nacs.entry(nid.nac).or_default() += 1;
            if nid.duid != 0x7 {
                continue;
            }
            let msgs = p25::messages(std::slice::from_ref(f));
            if msgs.is_empty() {
                self.bad += 1;
            }
            for m in &msgs {
                self.grants += self.info.apply(m).len() as u64;
            }
            self.tsbks += msgs.len() as u64;
            locked |= !msgs.is_empty();
        }
        // Phase 2 channels are scrambled by their system's WACN, system and NAC: what this control channel names lets
        // the bank's sync pass decode the site's TDMA traffic (detect's registry is still process-global: spec §8).
        if let (Some(w), Some(sys), Some(nac)) = (self.info.wacn, self.info.system, self.nac())
            && !self.registered
        {
            detect::p25p2::register_system(w, sys);
            detect::p25p2::register_nac(nac);
            self.registered = true;
        }
        self.lamp = if locked {
            Lamp::Locked
        } else if !frames.is_empty() {
            Lamp::Syncing
        } else {
            Lamp::Searching
        };
    }
}

impl Decoder for P25Control {
    fn mode(&self) -> &'static str {
        "P25 control"
    }

    fn configure(&mut self, rate: f64) {
        self.rate = rate;
        self.buf.clear();
    }

    fn feed(&mut self, block: &[Complex32]) {
        if self.rate <= 0.0 {
            return;
        }
        self.buf.extend_from_slice(block);
        let (n, keep) = ((BLOCK_S * self.rate) as usize, (OVERLAP_S * self.rate) as usize);
        while self.buf.len() >= n {
            let x: Vec<Complex32> = self.buf[..n].to_vec();
            self.decode(&x);
            self.buf.drain(..n - keep);
        }
    }

    /// After a gap: what is buffered no longer joins what comes next. What was learned stays.
    fn reset(&mut self) {
        self.buf.clear();
        self.blocks = 0;
    }

    fn telemetry(&self) -> Telemetry {
        Telemetry { lamp: self.lamp, blocks_ok: self.tsbks, blocks_bad: self.bad, identity: self.identity() }
    }

    /// "P25 control 859.7677 MHz, NAC 0x34D WACN BEE00 SYS 348 RFSS 1 SITE 13: 212 TSBKs, 14 grants".
    fn summary(&self) -> String {
        let at = format!("P25 control {:.4} MHz", self.center_hz / 1e6);
        match (self.identity(), self.lamp) {
            (Some(id), _) if self.tsbks > 0 => format!("{at}, {}: {} TSBKs, {} grants", id.line(), self.tsbks, self.grants),
            (_, Lamp::Searching) => format!("{at}: searching"),
            _ => format!("{at}: syncing, no TSBK yet"),
        }
    }

    fn claim(&self) -> Option<Claim> {
        let identity = self.identity().filter(|_| self.tsbks > 0)?;
        Some(Claim::new(
            "P25 Phase 1 control",
            CONFIDENCE,
            Evidence::Decoder {
                mode: "P25 control".into(),
                locked: self.lamp == Lamp::Locked,
                summary: self.summary(),
                identity: Some(identity),
            },
        ))
    }
}
