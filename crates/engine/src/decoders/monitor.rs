//! A land-mobile channel monitor (step 7): DMR, NXDN, POCSAG and FLEX, decoded natively with `detect`'s own rungs (the
//! same the bank's looks run) on successive blocks of the channel, and accumulated: frames decoded and tried, the
//! channel's identity by majority (a DMR colour code, an NXDN RAN), the calls heard, the pages decoded, and Capacity
//! Plus's rest channel. Blocks overlap so a frame or a POCSAG batch across a join is whole in one of them; only frames
//! that start past the overlap are counted, pages are kept once, and FLEX frames once per cycle and frame number.

use std::collections::{BTreeMap, VecDeque};

use api::{Claim, Evidence, Identity, Lamp};
use detect::bank::{self as rungs, Window};
use sdr_core::Complex32;

use super::{Decoder, Telemetry};

/// A decoder on the channel, surer than one look's claim.
pub const CONFIDENCE: f32 = 0.999;
/// Calls and pages kept, newest last.
const KEPT: usize = 16;

/// The protocols a monitor decodes, as the bank names them.
pub const PROTOCOLS: [&str; 4] = ["DMR", "NXDN", "POCSAG", "FLEX"];

/// Seconds decoded at a time and repeated by the next block: FLEX's frames are 1.875 s, so its blocks hold two;
/// POCSAG's batches are under 0.5 s at 1200 bit/s (1.1 s at 512).
fn timing(protocol: &str) -> (f64, f64) {
    match protocol {
        "FLEX" => (4.0, 2.0),
        "POCSAG" => (2.5, 1.2),
        _ => (1.5, 0.25),
    }
}

pub struct Monitor {
    protocol: &'static str,
    /// The bank's label for it ("DMR BS data"), until the monitor's own blocks name it.
    label: String,
    center_hz: f64,
    bandwidth_hz: f64,
    rate: f64,
    buf: Vec<Complex32>,
    blocks: u64,
    ids: BTreeMap<String, u64>,
    kinds: BTreeMap<String, u64>,
    decoded: u64,
    tried: u64,
    calls: VecDeque<String>,
    pages: VecDeque<String>,
    pages_total: u64,
    /// FLEX frames already counted: (cycle, frame).
    flex_seen: VecDeque<(u8, u8)>,
    capplus_repeater: Option<u8>,
    capplus_rest: Option<(u8, u8)>,
    lamp: Lamp,
}

impl Monitor {
    /// `protocol` is one of [`PROTOCOLS`]; `bandwidth_hz` the emitter's occupied bandwidth.
    pub fn new(protocol: &'static str, label: &str, center_hz: f64, bandwidth_hz: f64) -> Monitor {
        Monitor {
            protocol,
            label: label.to_string(),
            center_hz,
            bandwidth_hz,
            rate: 0.0,
            buf: Vec::new(),
            blocks: 0,
            ids: BTreeMap::new(),
            kinds: BTreeMap::new(),
            decoded: 0,
            tried: 0,
            calls: VecDeque::new(),
            pages: VecDeque::new(),
            pages_total: 0,
            flex_seen: VecDeque::new(),
            capplus_repeater: None,
            capplus_rest: None,
            lamp: Lamp::Searching,
        }
    }

    /// The identity most blocks named.
    fn id(&self) -> Option<&str> {
        self.ids.iter().max_by_key(|(id, n)| (**n, std::cmp::Reverse(*id))).map(|(id, _)| id.as_str())
    }

    fn push(list: &mut VecDeque<String>, s: String) -> bool {
        if list.contains(&s) {
            return false;
        }
        list.push_back(s);
        while list.len() > KEPT {
            list.pop_front();
        }
        true
    }

    fn decode(&mut self, x: &[Complex32]) {
        let first = self.blocks == 0;
        self.blocks += 1;
        let w = Window::new(x, self.rate, self.bandwidth_hz, detect::Hints::default());
        let mut locked = false;
        let mut synced = false;
        if self.protocol == "FLEX" {
            for f in detect::flex::decode(w.raw, w.fs) {
                synced = true;
                if self.flex_seen.contains(&(f.cycle, f.frame)) {
                    continue;
                }
                self.flex_seen.push_back((f.cycle, f.frame));
                while self.flex_seen.len() > 64 {
                    self.flex_seen.pop_front();
                }
                self.tried += 1;
                self.decoded += 1;
                locked = true;
                *self.kinds.entry("frame".into()).or_default() += 1;
                self.label = format!("FLEX {}", f.mode());
                for p in &f.pages {
                    if Self::push(&mut self.pages, p.summary()) {
                        self.pages_total += 1;
                        *self.kinds.entry("page".into()).or_default() += 1;
                    }
                }
            }
        } else if let Some(s) = rungs::sync_bank(&w).filter(|s| s.sync.protocol == self.protocol) {
            synced = true;
            self.label =
                if s.sync.detail.is_empty() { s.sync.protocol.to_string() } else { format!("{} {}", s.sync.protocol, s.sync.detail) };
            let (_, overlap) = timing(self.protocol);
            let skip = if first { 0 } else { (overlap * s.sync.baud) as usize };
            let new: Vec<detect::frames::Frame> = s.frames.iter().filter(|f| f.at_symbol >= skip).cloned().collect();
            if self.protocol == "POCSAG" {
                // Pages from the whole block (a page may start in the overlap), each kept once.
                self.tried += new.len() as u64;
                let pages = detect::pocsag::pages(&s.frames);
                locked = !pages.is_empty();
                for p in pages {
                    let line = format!("{} ({}): {}", p.address, (b'A' + p.function) as char, p.text);
                    if Self::push(&mut self.pages, line) {
                        self.pages_total += 1;
                        self.decoded += 1;
                        *self.kinds.entry("page".into()).or_default() += 1;
                    }
                }
            } else {
                let f = detect::decode::summarise(&new);
                self.tried += f.tried as u64;
                self.decoded += f.decoded as u64;
                locked = f.decoded > 0;
                if let Some(id) = f.id {
                    *self.ids.entry(id).or_default() += f.decoded.max(1) as u64;
                }
                for (k, n) in f.kinds {
                    *self.kinds.entry(k).or_default() += n as u64;
                }
                if let Some(t) = f.talk {
                    Self::push(&mut self.calls, t);
                }
                self.capplus_repeater = f.capplus_repeater.or(self.capplus_repeater);
                self.capplus_rest = f.capplus_rest.or(self.capplus_rest);
            }
        }
        self.lamp = if locked {
            Lamp::Locked
        } else if synced {
            Lamp::Syncing
        } else {
            Lamp::Searching
        };
    }

    /// The newest calls (DMR, NXDN) or pages (POCSAG, FLEX), newest last.
    pub fn calls(&self) -> impl Iterator<Item = &String> {
        self.calls.iter()
    }

    pub fn pages(&self) -> impl Iterator<Item = &String> {
        self.pages.iter()
    }

    fn identity(&self) -> Option<Identity> {
        self.id().map(|id| Identity::Other { id: id.to_string() })
    }
}

impl Decoder for Monitor {
    fn mode(&self) -> &'static str {
        self.protocol
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
        let (len, overlap) = timing(self.protocol);
        let (n, keep) = ((len * self.rate) as usize, (overlap * self.rate) as usize);
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
        Telemetry {
            lamp: self.lamp,
            blocks_ok: self.decoded,
            blocks_bad: self.tried.saturating_sub(self.decoded),
            identity: self.identity(),
        }
    }

    /// "DMR BS data 855.3125 MHz, CC 9: CSBK×120, voice×40, 160 of 170 frames; rest 2/1 | TG 1 ← 123" or
    /// "POCSAG 931.7125 MHz: 12 pages | 1234567 (A): text".
    fn summary(&self) -> String {
        let at = format!("{} {:.4} MHz", self.label, self.center_hz / 1e6);
        if self.lamp == Lamp::Searching && self.tried == 0 {
            return format!("{at}: searching");
        }
        let mut s = at;
        if let Some(id) = self.id() {
            s.push_str(&format!(", {id}"));
        }
        if matches!(self.protocol, "POCSAG" | "FLEX") {
            s.push_str(&format!(": {} pages", self.pages_total));
            if self.protocol == "FLEX" {
                s.push_str(&format!(" in {} frames", self.decoded));
            }
            if let Some(p) = self.pages.back() {
                s.push_str(&format!(" | {p}"));
            }
            return s;
        }
        let mut kinds: Vec<(&String, &u64)> = self.kinds.iter().collect();
        kinds.sort_by_key(|(k, n)| (std::cmp::Reverse(**n), (*k).clone()));
        let k: Vec<String> = kinds.iter().take(3).map(|(k, n)| format!("{k}×{n}")).collect();
        s.push_str(&format!(": {}{}{} of {} frames", k.join(", "), if k.is_empty() { "" } else { ", " }, self.decoded, self.tried));
        if let Some((rep, slot)) = self.capplus_rest {
            s.push_str(&format!("; Cap+ rest {rep}/{slot}"));
        }
        if let Some(rep) = self.capplus_repeater {
            s.push_str(&format!("; Cap+ repeater {rep}"));
        }
        if let Some(c) = self.calls.back() {
            s.push_str(&format!(" | {c}"));
        }
        s
    }

    fn claim(&self) -> Option<Claim> {
        if self.decoded == 0 {
            return None;
        }
        Some(Claim::new(
            self.label.clone(),
            CONFIDENCE,
            Evidence::Decoder {
                mode: self.protocol.into(),
                locked: self.lamp == Lamp::Locked,
                summary: self.summary(),
                identity: self.identity(),
            },
        ))
    }
}
