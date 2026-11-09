//! A land-mobile channel monitor (step 7): DMR, NXDN, POCSAG and FLEX, decoded natively with `detect`'s own rungs (the
//! same the bank's looks run) on successive blocks of the channel, and accumulated: frames decoded and tried, the
//! channel's identity by majority (a DMR colour code, an NXDN RAN), the calls heard, the pages decoded, and Capacity
//! Plus's rest channel. Blocks overlap so a frame or a POCSAG batch across a join is whole in one of them; only frames
//! that start past the overlap are counted, pages are kept once, and FLEX frames once per cycle and frame number.

use std::collections::{BTreeMap, VecDeque};

use api::{Claim, Evidence, Identity, Lamp};
use detect::bank::{self as rungs, Window};
use detect::flex::FlexFrame;
use detect::flex_join::{FragmentJoiner, TIMEOUT_FRAMES, frame_index};
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
    /// FLEX: frames already taken (blocks overlap), and long messages' fragments until their last one comes.
    joiner: FragmentJoiner,
    /// FLEX: the newest frame taken, (index in the hour, sample fed), to give up on a message's missing fragments
    /// when the channel falls quiet and no later frame comes to time them out.
    last_frame: Option<(u32, u64)>,
    /// POCSAG pages kept, as `pages` shows them.
    pocsag_seen: VecDeque<detect::pocsag::Page>,
    capplus_repeater: Option<u8>,
    capplus_rest: Option<(u8, u8)>,
    lamp: Lamp,
    /// Samples fed since `configure`.
    fed: u64,
    /// Where the decodes since the last [`Decoder::take_heard`] fell, in samples fed.
    heard: Vec<u64>,
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
            joiner: FragmentJoiner::new(),
            last_frame: None,
            pocsag_seen: VecDeque::new(),
            capplus_repeater: None,
            capplus_rest: None,
            lamp: Lamp::Searching,
            fed: 0,
            heard: Vec::new(),
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

    /// Decodes block `x`, which starts `start` samples after `configure`.
    /// Keeps a POCSAG page once: a block that ends inside a page decodes the start of it, and the next block (they
    /// overlap) the whole, so a page whose text starts another's to the same address is the same page; the fuller one
    /// is kept, in the older one's place. True if it is a new page.
    fn keep_pocsag(&mut self, p: detect::pocsag::Page) -> bool {
        let related = |a: &str, b: &str| a.starts_with(b) || b.starts_with(a);
        let same = |q: &detect::pocsag::Page| {
            q.address == p.address && q.function == p.function && (related(&q.alpha, &p.alpha) || related(&q.numeric, &p.numeric))
        };
        if let Some(i) = self.pocsag_seen.iter().position(same) {
            let q = &self.pocsag_seen[i];
            let fuller = (p.alpha.len(), !p.damaged) > (q.alpha.len(), !q.damaged);
            if fuller {
                let old = q.summary();
                if let Some(line) = self.pages.iter_mut().find(|l| **l == old) {
                    *line = p.summary();
                }
                self.pocsag_seen[i] = p;
            }
            return false;
        }
        Self::push(&mut self.pages, p.summary());
        self.pocsag_seen.push_back(p);
        while self.pocsag_seen.len() > KEPT {
            self.pocsag_seen.pop_front();
        }
        true
    }

    fn decode(&mut self, x: &[Complex32], start: u64) {
        let first = self.blocks == 0;
        self.blocks += 1;
        let w = Window::new(x, self.rate, self.bandwidth_hz, detect::Hints::default());
        let mut locked = false;
        let mut synced = false;
        if self.protocol == "FLEX" {
            let block_secs = w.raw.len() as f64 / w.fs;
            for f in detect::flex::decode(w.raw, w.fs) {
                synced = true;
                // A frame the block cuts off is left for the next block, which holds it whole (blocks overlap by more
                // than a frame): taken here, its pages would be cut short and the whole copy dropped as seen.
                if f.at as f64 / w.fs + detect::flex::FRAME_AFTER_SYNC1_SECS > block_secs {
                    continue;
                }
                locked |= self.take_flex(&f, start + f.at as u64);
            }
            self.expire_flex(start + x.len() as u64);
        } else if let Some(s) = rungs::sync_bank(&w).filter(|s| s.sync.protocol == self.protocol) {
            synced = true;
            self.label =
                if s.sync.detail.is_empty() { s.sync.protocol.to_string() } else { format!("{} {}", s.sync.protocol, s.sync.detail) };
            let (_, overlap) = timing(self.protocol);
            let skip = if first { 0 } else { (overlap * s.sync.baud) as usize };
            let new: Vec<detect::frames::Frame> = s.frames.iter().filter(|f| f.at_symbol >= skip).cloned().collect();
            let rate = self.rate;
            let at = |f: &detect::frames::Frame| start + (f.at_symbol as f64 / s.sync.baud * rate) as u64;
            if self.protocol == "POCSAG" {
                // Pages from the whole block (a page may start in the overlap), each kept once.
                self.tried += new.len() as u64;
                let pages = detect::pocsag::pages(&s.frames);
                locked = !pages.is_empty();
                let mut fresh = false;
                for p in pages {
                    if self.keep_pocsag(p) {
                        self.pages_total += 1;
                        self.decoded += 1;
                        *self.kinds.entry("page".into()).or_default() += 1;
                        fresh = true;
                    }
                }
                // A page ends in the block's newest batch, or one before it: either is inside a POCSAG transmission.
                if let Some(f) = s.frames.last().filter(|_| fresh) {
                    self.heard.push(at(f));
                }
            } else {
                let f = detect::decode::summarise(&new);
                self.tried += f.tried as u64;
                self.decoded += f.decoded as u64;
                locked = f.decoded > 0;
                if let Some(fr) = new.last().filter(|_| locked) {
                    self.heard.push(at(fr));
                }
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

    /// Takes FLEX frame `f`, whose sync ends at sample `at`, unless it was taken already. Fragments of a long message
    /// wait in the joiner; whole messages are kept as pages. True if the frame is new.
    fn take_flex(&mut self, f: &FlexFrame, at: u64) -> bool {
        if !self.joiner.new_frame(f.cycle, f.frame) {
            return false;
        }
        self.heard.push(at);
        self.last_frame = Some((frame_index(f.cycle, f.frame), at));
        self.tried += 1;
        self.decoded += 1;
        *self.kinds.entry("frame".into()).or_default() += 1;
        self.label = format!("FLEX {}", f.mode());
        let mut whole = Vec::new();
        for p in &f.pages {
            whole.extend(self.joiner.push(p.clone(), f.cycle, f.frame));
        }
        whole.extend(self.joiner.expire(f.cycle, f.frame));
        self.keep_flex(whole);
        true
    }

    /// Gives up on fragments that are overdue at sample `now`: frames come every 1.875 s whether or not a transmitter
    /// sends them, so the frame number is known from the time even on a quiet channel.
    fn expire_flex(&mut self, now: u64) {
        let Some((idx, at)) = self.last_frame.filter(|_| self.joiner.pending() > 0) else { return };
        let frames = (now.saturating_sub(at) as f64 / self.rate / detect::flex::FRAME_SECS) as u32;
        if frames <= TIMEOUT_FRAMES {
            return;
        }
        let idx = idx + frames;
        let released = self.joiner.expire((idx / 128 % 15) as u8, (idx % 128) as u8);
        self.keep_flex(released);
    }

    fn keep_flex(&mut self, pages: Vec<detect::flex::Page>) {
        for p in pages {
            if Self::push(&mut self.pages, p.summary()) {
                self.pages_total += 1;
                *self.kinds.entry("page".into()).or_default() += 1;
            }
        }
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
        self.fed = 0;
        self.heard.clear();
    }

    fn feed(&mut self, block: &[Complex32]) {
        if self.rate <= 0.0 {
            return;
        }
        self.buf.extend_from_slice(block);
        self.fed += block.len() as u64;
        let (len, overlap) = timing(self.protocol);
        let (n, keep) = ((len * self.rate) as usize, (overlap * self.rate) as usize);
        while self.buf.len() >= n {
            let x: Vec<Complex32> = self.buf[..n].to_vec();
            self.decode(&x, self.fed - self.buf.len() as u64);
            self.buf.drain(..n - keep);
        }
    }

    /// After a gap: what is buffered no longer joins what comes next. What was learned stays.
    fn reset(&mut self) {
        self.buf.clear();
        self.blocks = 0;
    }

    fn take_heard(&mut self) -> Option<Vec<u64>> {
        Some(std::mem::take(&mut self.heard))
    }

    fn recent(&self) -> Vec<String> {
        self.calls.iter().chain(self.pages.iter()).cloned().collect()
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
            // Counts only: the pages themselves are people's, and this sentence is shown and logged everywhere.
            let damaged = self.pages.iter().filter(|p| p.contains("[damaged]")).count();
            if damaged > 0 {
                s.push_str(&format!(", {damaged} of the last {} damaged", self.pages.len()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed synthetic FLEX slice: four 3200/4 frames carrying five invented pages, 50 kS/s.
    fn synthetic() -> Vec<Complex32> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/small/flex_synthetic_929.6125.sigmf-data");
        let raw = std::fs::read(path).expect("the synthetic FLEX slice (testdata/small)");
        raw.as_chunks::<8>()
            .0
            .iter()
            .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
            .collect()
    }

    #[test]
    fn a_flex_frame_cut_short_by_a_block_is_taken_whole_from_the_next() {
        // Blocks are 4 s, every 2 s: the second frame (about 2.1 s in) is cut off by the first block's end and whole in
        // the second. A monitor that kept the first copy it saw printed its pages damaged.
        for lead_secs in [0.0, 0.5, 1.0, 1.5] {
            let pages = monitor_pages(lead_secs);
            assert_eq!(pages.len(), 5, "{lead_secs} s lead: every page, once: {} lines", pages.len());
            let damaged = pages.iter().filter(|p| p.contains("[damaged]")).count();
            assert_eq!(damaged, 0, "{lead_secs} s lead: {damaged} of 5 pages damaged");
            assert!(pages.iter().any(|p| p.ends_with(": END OF SYNTHETIC SLICE")));
        }
    }

    fn fragment(capcode: u64, fragment: u8, continued: bool, text: &str) -> detect::flex::Page {
        detect::flex::Page { capcode, phase: 'A', kind: "alpha", text: text.into(), damaged: false, continued, fragment, message_no: 1 }
    }

    fn frame(cycle: u8, frame: u8, pages: Vec<detect::flex::Page>) -> FlexFrame {
        FlexFrame { at: 0, baud: 3200, levels: 4, cycle, frame, block_ok: [0; 11], block_off: [0.0; 11], soft_words: 0, pages }
    }

    #[test]
    fn a_long_flex_message_sent_in_fragments_is_one_page() {
        let mut m = Monitor::new("FLEX", "FLEX", 929.6125e6, 25e3);
        m.configure(50e3);
        let fs = 50e3 * detect::flex::FRAME_SECS;
        assert!(m.take_flex(&frame(3, 40, vec![fragment(1_234_567, 3, true, "FIRST HALF ")]), 0));
        assert!(m.recent().is_empty(), "a first fragment waits for the rest");
        // The overlapping block decodes the same frame again.
        assert!(!m.take_flex(&frame(3, 40, vec![fragment(1_234_567, 3, true, "FIRST HALF ")]), 0));
        assert!(m.take_flex(&frame(3, 41, vec![fragment(1_234_567, 0, false, "SECOND HALF")]), fs as u64));
        assert_eq!(m.recent(), vec!["001234567 A alpha: FIRST HALF SECOND HALF".to_string()]);
        assert_eq!(m.pages_total, 1);
    }

    #[test]
    fn a_message_whose_end_never_comes_is_shown_damaged_once_the_channel_is_quiet() {
        let mut m = Monitor::new("FLEX", "FLEX", 929.6125e6, 25e3);
        m.configure(50e3);
        let frame_samples = 50e3 * detect::flex::FRAME_SECS;
        assert!(m.take_flex(&frame(14, 127, vec![fragment(1_234_567, 3, true, "CUT OFF")]), 0));
        // No more frames: the transmitter has gone quiet. Time alone gives the message up, across the hour's wrap.
        m.expire_flex((frame_samples * TIMEOUT_FRAMES as f64) as u64);
        assert!(m.recent().is_empty(), "not overdue yet");
        m.expire_flex((frame_samples * (TIMEOUT_FRAMES + 2) as f64) as u64);
        let lines = m.recent();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("[damaged]") && lines[0].ends_with("CUT OFF …"), "{lines:?}");
    }

    /// The monitor's lines for the synthetic slice after `lead_secs` of silence, then 4 s more (a live channel goes on).
    fn monitor_pages(lead_secs: f64) -> Vec<String> {
        let mut x = vec![Complex32::new(0.0, 0.0); (lead_secs * 50e3) as usize];
        x.extend(synthetic());
        x.extend(std::iter::repeat_n(Complex32::new(0.0, 0.0), 200_000));
        let mut m = Monitor::new("FLEX", "FLEX", 929.6125e6, 15e3);
        m.configure(50e3);
        for chunk in x.chunks(500) {
            m.feed(chunk);
        }
        m.recent()
    }
}
