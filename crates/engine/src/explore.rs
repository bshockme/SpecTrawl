//! Auto-explore: unattended visits to survey emitters (`docs/auto-explore.md`).
//!
//! This module is the pure part: the shared queue, the filters, the dwell
//! state machine and the verdict log. It owns no threads and no devices; the
//! app ticks it once a frame with what the survey and focus receivers know,
//! and does the retuning and the drawing.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime};

use crate::Emitter;

/// A named set of frequency ranges (MHz), for the filters.
pub struct Preset {
    pub name: &'static str,
    pub ranges_mhz: &'static [(f64, f64)],
}

/// The band presets the filters know. US allocations; coarse on purpose.
pub const PRESETS: &[Preset] = &[
    Preset { name: "FM broadcast", ranges_mhz: &[(87.5, 108.0)] },
    Preset { name: "TV", ranges_mhz: &[(54.0, 88.0), (174.0, 216.0), (470.0, 608.0)] },
    Preset {
        name: "Cellular",
        ranges_mhz: &[(617.0, 652.0), (663.0, 698.0), (698.0, 769.0), (775.0, 794.0), (824.0, 849.0), (869.0, 894.0), (1710.0, 1780.0)],
    },
    Preset { name: "Pagers", ranges_mhz: &[(152.0, 153.0), (157.45, 158.1), (929.0, 932.0)] },
    Preset { name: "Aviation", ranges_mhz: &[(108.0, 137.0), (225.0, 400.0)] },
    Preset {
        name: "Land mobile",
        ranges_mhz: &[
            (150.0, 174.0),
            (450.0, 470.0),
            (470.0, 512.0),
            (769.0, 775.0),
            (794.0, 806.0),
            (806.0, 824.0),
            (851.0, 869.0),
            (896.0, 901.0),
            (935.0, 940.0),
        ],
    },
    Preset {
        name: "Amateur",
        ranges_mhz: &[(50.0, 54.0), (144.0, 148.0), (222.0, 225.0), (420.0, 450.0), (902.0, 928.0), (1240.0, 1300.0)],
    },
    Preset { name: "ISM", ranges_mhz: &[(433.05, 434.79), (902.0, 928.0)] },
    Preset { name: "Satellite", ranges_mhz: &[(137.0, 138.0), (243.0, 270.0), (399.9, 403.0), (1525.0, 1559.0), (1616.0, 1627.0)] },
];

impl Preset {
    pub fn contains(&self, hz: f64) -> bool {
        let mhz = hz / 1e6;
        self.ranges_mhz.iter().any(|(lo, hi)| (*lo..=*hi).contains(&mhz))
    }
}

/// What a preset does to the emitters inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rule {
    /// No effect.
    #[default]
    Ignore,
    /// Never visited.
    Exclude,
    /// Visited after everything else.
    DePrioritise,
    /// Visited before everything else.
    Prioritise,
    /// Only emitters inside some `Restrict` preset or range are visited.
    Restrict,
}

impl Rule {
    pub const ALL: [Rule; 5] = [Rule::Ignore, Rule::Exclude, Rule::DePrioritise, Rule::Prioritise, Rule::Restrict];

    pub fn name(self) -> &'static str {
        match self {
            Rule::Ignore => "ignore",
            Rule::Exclude => "exclude",
            Rule::DePrioritise => "de-prioritise",
            Rule::Prioritise => "prioritise",
            Rule::Restrict => "restrict to",
        }
    }

    pub fn parse(s: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.name() == s)
    }
}

/// Filters and timings. The app keeps these in its settings file.
#[derive(Debug, Clone)]
pub struct Config {
    /// User-drawn ranges (MHz) to explore; empty = anywhere.
    pub ranges_mhz: Vec<(f64, f64)>,
    /// Preset name → rule, for presets that aren't `Ignore`.
    pub presets: BTreeMap<String, Rule>,
    /// Wait for the emitter to show in the focus span before calling it gone.
    pub settle_s: f64,
    /// Leave after this long without activity.
    pub silence_s: f64,
    /// Longest dwell.
    pub cap_s: f64,
}

impl Default for Config {
    fn default() -> Self {
        let presets = ["FM broadcast", "TV", "Cellular"].into_iter().map(|n| (n.to_string(), Rule::Exclude)).collect();
        Self { ranges_mhz: Vec::new(), presets, settle_s: 1.0, silence_s: 3.0, cap_s: 15.0 }
    }
}

impl Config {
    fn rule(&self, name: &str) -> Rule {
        self.presets.get(name).copied().unwrap_or_default()
    }

    /// How the filters see an emitter at `hz`: None = excluded.
    fn standing(&self, hz: f64) -> Option<Standing> {
        let mhz = hz / 1e6;
        if !self.ranges_mhz.is_empty() && !self.ranges_mhz.iter().any(|(lo, hi)| (*lo..=*hi).contains(&mhz)) {
            return None;
        }
        let mut s = Standing::default();
        let restricting = PRESETS.iter().any(|p| self.rule(p.name) == Rule::Restrict);
        let mut inside_restrict = false;
        for p in PRESETS.iter().filter(|p| p.contains(hz)) {
            match self.rule(p.name) {
                Rule::Exclude => return None,
                Rule::DePrioritise => s.deprioritised = true,
                Rule::Prioritise => s.prioritised = true,
                Rule::Restrict => inside_restrict = true,
                Rule::Ignore => {}
            }
        }
        if restricting && !inside_restrict {
            return None;
        }
        Some(s)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Standing {
    prioritised: bool,
    deprioritised: bool,
}

/// How a visit ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Not present during the dwell (not logged).
    Gone,
    /// Present throughout, no stable label.
    Unknown,
    /// A stable label without an identity.
    Classified,
    /// A decoder produced an identity.
    Identified,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Gone => "gone",
            Kind::Unknown => "unknown",
            Kind::Classified => "classified",
            Kind::Identified => "identified",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub kind: Kind,
    pub label: Option<String>,
    pub id: Option<String>,
}

/// One logged visit (or an in-place determination).
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: SystemTime,
    pub receiver: char,
    /// The survey's emitter id.
    pub emitter: u64,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub kind: Kind,
    pub label: String,
    pub id: String,
    pub dwell_s: f64,
    /// Determined inside a focus span without a visit (§7).
    pub in_place: bool,
}

impl LogEntry {
    pub const CSV_HEADER: &'static str = "time_unix,receiver,emitter,center_mhz,bw_khz,verdict,label,id,dwell_s,in_place";

    pub fn csv_line(&self) -> String {
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let t = self.time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        format!(
            "{t:.3},{},{},{:.6},{:.2},{},{},{},{:.1},{}",
            self.receiver,
            self.emitter,
            self.center_hz / 1e6,
            self.bandwidth_hz / 1e3,
            self.kind.name(),
            q(&self.label),
            q(&self.id),
            self.dwell_s,
            self.in_place as u8
        )
    }
}

/// What the focus receiver can see of the emitter being visited, this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observation {
    /// Something is on the air in the emitter's extent right now.
    pub active: bool,
    /// Best label the focus side has for it (protocol or feature label).
    pub label: Option<String>,
    /// The label names a protocol.
    pub protocol: bool,
    /// A decoded identity ("NAC 0x2A5", "CC 9", a cell, a station).
    pub id: Option<String>,
}

/// A visit in progress on one receiver.
#[derive(Debug, Clone)]
pub struct Visit {
    pub emitter: u64,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub receiver: char,
    pub reason: String,
    pub started: Instant,
    pub appeared: Option<Instant>,
    last_active: Option<Instant>,
    /// Current label and since when it has held.
    pub label: Option<(String, Instant)>,
    pub protocol: bool,
    pub id: Option<String>,
    protocol_since: Option<Instant>,
}

/// A feature label must hold this long before it counts as classified.
const LABEL_HOLD: Duration = Duration::from_secs(2);
/// A protocol without an identity gets this long for one to arrive.
const ID_WAIT: Duration = Duration::from_secs(4);

impl Visit {
    pub fn dwell(&self, now: Instant) -> Duration {
        now.duration_since(self.started)
    }

    /// Feeds one observation; Some when the visit has reached its verdict.
    pub fn update(&mut self, obs: &Observation, cfg: &Config, now: Instant) -> Option<Verdict> {
        if obs.active {
            self.appeared.get_or_insert(now);
            self.last_active = Some(now);
        }
        match (&obs.label, &mut self.label) {
            (Some(l), Some((cur, since))) if cur != l => {
                *cur = l.clone();
                *since = now;
            }
            (Some(l), None) => self.label = Some((l.clone(), now)),
            _ => {}
        }
        if obs.protocol && self.protocol_since.is_none() {
            self.protocol_since = Some(now);
        }
        self.protocol |= obs.protocol;
        if obs.id.is_some() {
            self.id = obs.id.clone();
        }
        // A label from a decoder means the signal is there, even between bursts.
        if (obs.label.is_some() || obs.id.is_some()) && self.appeared.is_none() {
            self.appeared = Some(now);
            self.last_active = Some(now);
        }
        let dwell = self.dwell(now);
        let label = self.label.as_ref().map(|(l, _)| l.clone());
        if self.protocol && self.id.is_some() {
            return Some(Verdict { kind: Kind::Identified, label, id: self.id.clone() });
        }
        if let Some(since) = self.protocol_since {
            if now.duration_since(since) >= ID_WAIT {
                return Some(Verdict { kind: Kind::Classified, label, id: None });
            }
        }
        let Some(appeared) = self.appeared else {
            // Give the retune, the detector and the survey's own latency a chance.
            if dwell.as_secs_f64() >= cfg.settle_s + 2.0 {
                return Some(Verdict { kind: Kind::Gone, label: None, id: None });
            }
            return None;
        };
        if let Some((l, since)) = &self.label {
            if !self.protocol && now.duration_since(*since) >= LABEL_HOLD && now.duration_since(appeared).as_secs_f64() >= 3.0 {
                return Some(Verdict { kind: Kind::Classified, label: Some(l.clone()), id: None });
            }
        }
        let silent = self.last_active.is_some_and(|t| now.duration_since(t).as_secs_f64() >= cfg.silence_s);
        if silent || dwell.as_secs_f64() >= cfg.cap_s {
            let kind = if label.is_some() { Kind::Classified } else { Kind::Unknown };
            return Some(Verdict { kind, label, id: None });
        }
        None
    }
}

/// What the explorer remembers about a survey emitter.
#[derive(Debug, Clone)]
struct History {
    last: Instant,
    kind: Kind,
    gone_streak: u32,
}

/// Revisit intervals by last verdict.
const REVISIT_UNKNOWN: Duration = Duration::from_secs(300);
const REVISIT_GONE: Duration = Duration::from_secs(60);
const REVISIT_GONE_LONG: Duration = Duration::from_secs(600);
const REVISIT_LABELLED: Duration = Duration::from_secs(1800);
/// Gone this many times in a row: the long interval.
const GONE_STREAK: u32 = 3;

/// Why an emitter was picked (shown in the pane).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub emitter: u64,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub reason: String,
}

/// The shared queue, history and log.
#[derive(Default)]
pub struct Explorer {
    history: HashMap<u64, History>,
    pub log: Vec<LogEntry>,
    /// Receiver letter → emitter it is visiting.
    busy: HashMap<char, u64>,
    /// Emitters determined in place and their best kind so far, so the log
    /// only gains a row when a determination improves.
    in_place: HashMap<u64, Kind>,
}

/// An emitter overlapping any of these spans is in a focus receiver's view.
fn in_view(e: &Emitter, spans: &[(f64, f64)]) -> bool {
    spans.iter().any(|(lo, hi)| e.center_hz >= *lo && e.center_hz <= *hi)
}

impl Explorer {
    /// Picks the next emitter for `receiver` from the survey's emitters.
    /// `focus_spans` are the other focus receivers' spans; `baseline` is
    /// when exploring started (emitters first seen after it are new).
    pub fn next(
        &self,
        receiver: char,
        emitters: &[Emitter],
        cfg: &Config,
        focus_spans: &[(f64, f64)],
        baseline: SystemTime,
        now: Instant,
    ) -> Option<Candidate> {
        let mut best: Option<(Vec<i64>, &Emitter, String)> = None;
        for e in emitters.iter().filter(|e| e.confirmed) {
            if self.busy.iter().any(|(r, id)| *r != receiver && *id == e.id) {
                continue;
            }
            let Some(st) = cfg.standing(e.center_hz) else { continue };
            let labelled = e.has_protocol() || e.label().is_some_and(|l| !l.is_empty() && l != "Wideband");
            let (wait, since) = match self.history.get(&e.id) {
                None => (Duration::ZERO, Duration::MAX),
                Some(h) => {
                    let wait = match h.kind {
                        Kind::Gone if h.gone_streak >= GONE_STREAK => REVISIT_GONE_LONG,
                        Kind::Gone => REVISIT_GONE,
                        Kind::Unknown => REVISIT_UNKNOWN,
                        Kind::Classified | Kind::Identified => REVISIT_LABELLED,
                    };
                    (wait, now.duration_since(h.last))
                }
            };
            if since < wait {
                continue;
            }
            let new = e.first_seen > baseline;
            let view = in_view(e, focus_spans);
            // Lower sorts first.
            let key = vec![
                -(st.prioritised as i64),
                st.deprioritised as i64,
                view as i64,
                -(e.active as i64),
                labelled as i64,
                -(new as i64),
                -(since.min(Duration::from_secs(1 << 20)).as_secs() as i64),
            ];
            let mut why: Vec<&str> = Vec::new();
            if st.prioritised {
                why.push("prioritised");
            }
            if e.active {
                why.push("active now");
            }
            if !labelled {
                why.push("unlabelled");
            }
            if new {
                why.push("new");
            }
            if self.history.get(&e.id).is_none() {
                why.push("never visited");
            } else if since > Duration::from_secs(60) {
                why.push("not visited lately");
            }
            if view {
                why.push("in a focus span");
            }
            if best.as_ref().is_none_or(|(k, _, _)| key < *k) {
                best = Some((key, e, why.join(", ")));
            }
        }
        best.map(|(_, e, reason)| Candidate { emitter: e.id, center_hz: e.center_hz, bandwidth_hz: e.bandwidth_hz, reason })
    }

    /// Starts a visit on `receiver`.
    pub fn begin(&mut self, receiver: char, c: &Candidate, now: Instant) -> Visit {
        self.busy.insert(receiver, c.emitter);
        Visit {
            emitter: c.emitter,
            center_hz: c.center_hz,
            bandwidth_hz: c.bandwidth_hz,
            receiver,
            reason: c.reason.clone(),
            started: now,
            appeared: None,
            last_active: None,
            label: None,
            protocol: false,
            id: None,
            protocol_since: None,
        }
    }

    /// Ends a visit: records the history and, for anything but *gone*, a
    /// log row (returned, so the app can append it to the CSV).
    pub fn finish(&mut self, v: &Visit, verdict: &Verdict, now: Instant) -> Option<LogEntry> {
        self.busy.remove(&v.receiver);
        let h = self.history.entry(v.emitter).or_insert(History { last: now, kind: verdict.kind, gone_streak: 0 });
        h.last = now;
        h.kind = verdict.kind;
        h.gone_streak = if verdict.kind == Kind::Gone { h.gone_streak + 1 } else { 0 };
        if verdict.kind == Kind::Gone {
            return None;
        }
        let entry = LogEntry {
            time: SystemTime::now(),
            receiver: v.receiver,
            emitter: v.emitter,
            center_hz: v.center_hz,
            bandwidth_hz: v.bandwidth_hz,
            kind: verdict.kind,
            label: verdict.label.clone().unwrap_or_default(),
            id: verdict.id.clone().unwrap_or_default(),
            dwell_s: v.dwell(now).as_secs_f64(),
            in_place: false,
        };
        self.log.push(entry.clone());
        Some(entry)
    }

    /// A visit abandoned without a verdict (the receiver left Explore).
    pub fn abandon(&mut self, receiver: char) {
        self.busy.remove(&receiver);
    }

    /// An emitter inside a focus span was determined without a visit (§7).
    /// Logged when it is the first determination or a better one.
    pub fn determined_in_place(&mut self, receiver: char, e: &Emitter, obs: &Observation, now: Instant) -> Option<LogEntry> {
        let kind = if obs.protocol && obs.id.is_some() {
            Kind::Identified
        } else if obs.label.is_some() {
            Kind::Classified
        } else {
            return None;
        };
        if self.in_place.get(&e.id).is_some_and(|k| *k >= kind) {
            return None;
        }
        self.in_place.insert(e.id, kind);
        self.history.insert(e.id, History { last: now, kind, gone_streak: 0 });
        let entry = LogEntry {
            time: SystemTime::now(),
            receiver,
            emitter: e.id,
            center_hz: e.center_hz,
            bandwidth_hz: e.bandwidth_hz,
            kind,
            label: obs.label.clone().unwrap_or_default(),
            id: obs.id.clone().unwrap_or_default(),
            dwell_s: 0.0,
            in_place: true,
        };
        self.log.push(entry.clone());
        Some(entry)
    }

    /// Emitters with a verdict so far.
    pub fn visited(&self) -> usize {
        self.history.len()
    }

    /// How many of `emitters` the queue still holds for `receiver`.
    pub fn queue_len(&self, receiver: char, emitters: &[Emitter], cfg: &Config, now: Instant) -> usize {
        emitters
            .iter()
            .filter(|e| e.confirmed && cfg.standing(e.center_hz).is_some())
            .filter(|e| !self.busy.iter().any(|(r, id)| *r != receiver && *id == e.id))
            .filter(|e| match self.history.get(&e.id) {
                None => true,
                Some(h) => {
                    let wait = match h.kind {
                        Kind::Gone if h.gone_streak >= GONE_STREAK => REVISIT_GONE_LONG,
                        Kind::Gone => REVISIT_GONE,
                        Kind::Unknown => REVISIT_UNKNOWN,
                        _ => REVISIT_LABELLED,
                    };
                    now.duration_since(h.last) >= wait
                }
            })
            .count()
    }
}

/// Builds the focus side's observation of a survey emitter from the focus
/// receiver's emitters that overlap it (`lo..hi` is the survey emitter's
/// extent, widened by a survey bin). The best overlapping emitter wins:
/// one with a protocol, else the most-seen one.
pub fn observe(focus: &[Emitter], lo: f64, hi: f64, active_within: Duration, now: SystemTime) -> Observation {
    let mut obs = Observation::default();
    let mut best: Option<(i64, &Emitter)> = None;
    for e in focus.iter().filter(|e| e.center_hz >= lo && e.center_hz <= hi) {
        let recent = e.active || now.duration_since(e.last_seen).map(|d| d <= active_within).unwrap_or(true);
        obs.active |= recent;
        let score = (e.has_protocol() as i64) << 40 | (e.id().is_some() as i64) << 39 | e.records as i64;
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, e));
        }
    }
    if let Some((_, e)) = best {
        obs.protocol = e.has_protocol();
        obs.label = e.label().filter(|l| !matches!(l.as_str(), "too short" | "IQ expired" | "interrupted" | "Wideband"));
        let id = e.id_label();
        obs.id = if obs.protocol && !id.is_empty() { Some(id) } else { None };
        // Pagers identify themselves by decoding at all.
        if obs.protocol && obs.id.is_none() && obs.label.as_deref().is_some_and(|l| l.starts_with("FLEX") || l.starts_with("POCSAG")) {
            obs.id = obs.label.clone();
        }
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitter(id: u64, mhz: f64, active: bool, first: SystemTime) -> Emitter {
        Emitter {
            id,
            center_hz: mhz * 1e6,
            bandwidth_hz: 12_500.0,
            first_seen: first,
            last_seen: SystemTime::now(),
            records: 5,
            on_time_s: 1.0,
            active,
            confirmed: true,
            peak_snr_db: 20.0,
            tally: Default::default(),
            protocols: Default::default(),
            ids: Default::default(),
            activity: Default::default(),
            talk: Default::default(),
            last_talk: None,
            capplus_repeater: None,
            strong: Default::default(),
            class: None,
            best: None,
            class_at: None,
            group_misses: 0,
            last_end_sample: 0,
            tuner_center_hz: 0.0,
            bin_hz: 54e3,
            solid_records: 5,
        }
    }

    #[test]
    fn filters_exclude_presets_and_ranges() {
        let cfg = Config::default();
        assert!(cfg.standing(101.1e6).is_none(), "FM excluded by default");
        assert!(cfg.standing(751e6).is_none(), "LTE excluded by default");
        assert!(cfg.standing(155.475e6).is_some());
        let mut c = cfg.clone();
        c.ranges_mhz = vec![(450.0, 470.0)];
        assert!(c.standing(155.475e6).is_none());
        assert!(c.standing(460.5e6).is_some());
        let mut c = cfg.clone();
        c.presets.insert("Pagers".into(), Rule::Restrict);
        assert!(c.standing(155.475e6).is_none());
        assert!(c.standing(929.6e6).is_some());
    }

    #[test]
    fn queue_order_active_unlabelled_new_first_and_in_view_last() {
        let now = Instant::now();
        let base = SystemTime::now() - Duration::from_secs(600);
        let old = base - Duration::from_secs(60);
        let fresh = SystemTime::now();
        let mut idle_old = emitter(1, 155.0, false, old);
        idle_old.records = 9;
        let active_new = emitter(2, 156.0, true, fresh);
        let active_old = emitter(3, 157.0, true, old);
        let mut labelled = emitter(4, 158.0, true, fresh);
        labelled.protocols.insert("DMR".into(), 3);
        labelled.tally.insert("DMR".into(), 3);
        let in_view = emitter(5, 159.0, true, fresh);
        let ex = Explorer::default();
        let all = vec![idle_old.clone(), active_new.clone(), active_old.clone(), labelled.clone(), in_view.clone()];
        let cfg = Config::default();
        let pick = ex.next('A', &all, &cfg, &[(158.9e6, 159.1e6)], base, now).unwrap();
        assert_eq!(pick.emitter, 2, "active, unlabelled and new comes first: {}", pick.reason);
        let pick = ex.next('A', &[idle_old.clone(), labelled.clone(), in_view.clone()], &cfg, &[(158.9e6, 159.1e6)], base, now).unwrap();
        assert_eq!(pick.emitter, 4, "an active labelled emitter beats an idle one and one in view");
        let pick = ex.next('A', &[idle_old.clone(), in_view.clone()], &cfg, &[(158.9e6, 159.1e6)], base, now).unwrap();
        assert_eq!(pick.emitter, 1, "in view goes last");
    }

    #[test]
    fn visit_reaches_each_verdict() {
        let cfg = Config::default();
        let c = Candidate { emitter: 7, center_hz: 155e6, bandwidth_hz: 12_500.0, reason: String::new() };
        let t0 = Instant::now();
        let mut ex = Explorer::default();
        // Gone: nothing shows up.
        let mut v = ex.begin('A', &c, t0);
        assert_eq!(v.update(&Observation::default(), &cfg, t0 + Duration::from_secs(2)), None);
        let verdict = v.update(&Observation::default(), &cfg, t0 + Duration::from_millis(3100)).unwrap();
        assert_eq!(verdict.kind, Kind::Gone);
        assert!(ex.finish(&v, &verdict, t0 + Duration::from_secs(4)).is_none(), "gone isn't logged");
        // Identified: protocol with an identity.
        let mut v = ex.begin('A', &c, t0);
        let obs = Observation { active: true, label: Some("P25 Phase 1".into()), protocol: true, id: Some("NAC 0x2A5".into()) };
        let verdict = v.update(&obs, &cfg, t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(verdict.kind, Kind::Identified);
        let e = ex.finish(&v, &verdict, t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(e.id, "NAC 0x2A5");
        // Classified: a feature label that holds.
        let mut v = ex.begin('B', &c, t0);
        let obs = Observation { active: true, label: Some("Analog FM".into()), ..Default::default() };
        assert_eq!(v.update(&obs, &cfg, t0 + Duration::from_secs(1)), None);
        assert_eq!(v.update(&obs, &cfg, t0 + Duration::from_millis(3100)), None, "needs 3 s on the air");
        let verdict = v.update(&obs, &cfg, t0 + Duration::from_millis(4100)).unwrap();
        assert_eq!(verdict.kind, Kind::Classified);
        assert_eq!(verdict.label.as_deref(), Some("Analog FM"));
        // Unknown: active with no label until silence.
        let mut v = ex.begin('B', &c, t0);
        let on = Observation { active: true, ..Default::default() };
        assert_eq!(v.update(&on, &cfg, t0 + Duration::from_secs(1)), None);
        assert_eq!(v.update(&Observation::default(), &cfg, t0 + Duration::from_secs(3)), None);
        let verdict = v.update(&Observation::default(), &cfg, t0 + Duration::from_millis(4100)).unwrap();
        assert_eq!(verdict.kind, Kind::Unknown);
        // Unknown at the cap when it never goes quiet.
        let mut v = ex.begin('B', &c, t0);
        assert_eq!(v.update(&on, &cfg, t0 + Duration::from_secs(14)), None);
        assert_eq!(v.update(&on, &cfg, t0 + Duration::from_secs(15)).unwrap().kind, Kind::Unknown);
        assert_eq!(ex.log.len(), 1);
    }

    #[test]
    fn visited_emitters_wait_their_turn_and_gone_backs_off() {
        let cfg = Config::default();
        let base = SystemTime::now() - Duration::from_secs(600);
        let t0 = Instant::now();
        let a = emitter(1, 155.0, true, SystemTime::now());
        let b = emitter(2, 156.0, true, SystemTime::now());
        let mut ex = Explorer::default();
        let c = ex.next('A', &[a.clone(), b.clone()], &cfg, &[], base, t0).unwrap();
        let v = ex.begin('A', &c, t0);
        let gone = Verdict { kind: Kind::Gone, label: None, id: None };
        ex.finish(&v, &gone, t0);
        let c2 = ex.next('A', &[a.clone(), b.clone()], &cfg, &[], base, t0).unwrap();
        assert_ne!(c2.emitter, c.emitter, "the one just visited waits");
        // Both visited: nothing is due until the gone interval passes.
        let v = ex.begin('A', &c2, t0);
        ex.finish(&v, &gone, t0);
        assert!(ex.next('A', &[a.clone(), b.clone()], &cfg, &[], base, t0 + Duration::from_secs(30)).is_none());
        assert!(ex.next('A', &[a.clone(), b.clone()], &cfg, &[], base, t0 + REVISIT_GONE + Duration::from_secs(1)).is_some());
        // Three gones in a row: the long interval.
        for _ in 0..2 {
            let v = ex.begin('A', &c, t0);
            ex.finish(&v, &gone, t0);
        }
        assert_eq!(ex.history[&c.emitter].gone_streak, 3);
        assert!(ex.next('A', &[a.clone()], &cfg, &[], base, t0 + REVISIT_GONE + Duration::from_secs(1)).is_none());
        // Another receiver can't take an emitter being visited.
        let v = ex.begin('A', &c2, t0);
        assert!(ex.next('B', &[b.clone()], &cfg, &[], base, t0 + Duration::from_secs(3600)).is_none());
        ex.abandon('A');
        let _ = v;
        assert!(ex.next('B', &[b.clone()], &cfg, &[], base, t0 + Duration::from_secs(3600)).is_some());
    }

    #[test]
    fn in_place_logs_only_improvements() {
        let mut ex = Explorer::default();
        let e = emitter(9, 460.5, true, SystemTime::now());
        let t = Instant::now();
        let classified = Observation { active: true, label: Some("4FSK 4800 Bd".into()), ..Default::default() };
        assert!(ex.determined_in_place('A', &e, &classified, t).is_some());
        assert!(ex.determined_in_place('A', &e, &classified, t).is_none(), "same again: no new row");
        let identified = Observation { active: true, label: Some("NXDN48".into()), protocol: true, id: Some("RAN 12".into()) };
        let row = ex.determined_in_place('A', &e, &identified, t).unwrap();
        assert!(row.in_place && row.kind == Kind::Identified);
        assert!(ex.determined_in_place('A', &e, &classified, t).is_none(), "a worse one isn't logged");
        assert_eq!(ex.log.len(), 2);
        assert!(row.csv_line().contains("identified"));
    }

    #[test]
    fn observation_from_focus_emitters() {
        let now = SystemTime::now();
        let mut a = emitter(1, 155.0, true, now);
        a.tally.insert("Analog FM".into(), 4);
        let mut b = emitter(2, 155.01, false, now);
        b.protocols.insert("DMR BS".into(), 3);
        b.tally.insert("DMR BS".into(), 3);
        b.ids.insert("CC 9".into(), 12);
        let obs = observe(&[a.clone(), b.clone()], 154.95e6, 155.05e6, Duration::from_secs(1), now);
        assert!(obs.active && obs.protocol);
        assert_eq!(obs.id.as_deref(), Some("CC 9"));
        let obs = observe(&[a], 154.95e6, 155.05e6, Duration::from_secs(1), now);
        assert_eq!(obs.label.as_deref(), Some("Analog FM"));
        assert!(!obs.protocol && obs.id.is_none());
        let none = observe(&[b], 160e6, 161e6, Duration::from_secs(1), now);
        assert_eq!(none, Observation::default());
    }
}
