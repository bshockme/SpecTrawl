//! Signal classification: features from channelised IQ, then the
//! sync bank for protocol identification (DESIGN.md §7).

pub mod ambe;
mod ambe_tables;
pub mod bandplan;
pub mod blind;
pub mod carrier;
pub mod chirp;
pub mod context;
pub mod decode;
pub mod dmr;
pub mod features;
pub mod flex;
pub mod flex_join;
pub mod frames;
pub mod gsm;
pub mod imbe;
mod imbe_tables;
pub mod lte;
pub mod mbe;
pub mod p25;
pub mod p25_lc;
pub mod p25_voice;
pub mod p25p2;
pub mod pocsag;
pub mod rds;
pub mod spectral;
pub mod squelch;
pub mod sync;
pub mod trunk;

use num_complex::Complex32;

pub use dsp::Ddc;
pub use dsp::{ddc, demod};
pub use features::Features;
pub use sync::SyncResult;

#[derive(Debug, Clone)]
pub struct Classification {
    /// Short human label, e.g. "DMR BS voice", "4FSK 4800 Bd", "Analog FM".
    pub label: String,
    /// Set when a protocol sync was found.
    pub protocol: Option<String>,
    pub confidence: f32,
    pub features: Features,
    pub sync: Option<SyncResult>,
    /// Decoded protocol fields (NAC, colour code, frame types), when a
    /// supported protocol was found.
    pub fields: Option<decode::Fields>,
    /// Seconds of signal analysed.
    pub analysed_s: f64,
}

/// Re-centres a narrowband channel on its measured frequency offset, then
/// low-passes it tightly. Channel filters upstream are deliberately loose
/// (record bandwidths are rough, centres can be kHz off); for 4FSK protocols
/// a 4-6.5 kHz cutoff after re-centring finds far more syncs on weak signals
/// (DMR capture at ~11 dB peak SNR: 1 sync → 51).
pub fn prepare(x: &[Complex32], fs: f64, bandwidth_hz: f64) -> Vec<Complex32> {
    if x.len() < 64 || bandwidth_hz > 25e3 {
        return x.to_vec();
    }
    prepare_cutoff(x, fs, (bandwidth_hz * 0.5 + 500.0).clamp(4_000.0, 6_500.0))
}

/// `prepare` with an explicit low-pass cutoff.
pub fn prepare_cutoff(x: &[Complex32], fs: f64, cutoff_hz: f64) -> Vec<Complex32> {
    if x.len() < 64 {
        return x.to_vec();
    }
    let gate = features::envelope_gate(x);
    let disc = features::discriminator(x, fs);
    let on: Vec<f32> = disc.iter().zip(&gate[1..]).filter(|(_, g)| **g).map(|(d, _)| *d).collect();
    let on_iq: Vec<Complex32> = x.iter().zip(&gate).filter(|(_, g)| **g).map(|(v, _)| *v).collect();
    let offset = features::extract(&on_iq, &on, fs).offset_hz as f64;
    let mut ddc = Ddc::new(fs, offset, cutoff_hz, fs);
    let mut y = Vec::with_capacity(x.len());
    ddc.process(x, &mut y);
    y
}

/// Pre-filter cutoff for the sync search and slicing of narrowband 4FSK
/// (P25, DMR, NXDN, dPMR): on the weak DMR capture, 4 kHz found 58 frames
/// where 6.5 kHz found 22 (`examples/slice_tune`). Wider-deviation signals
/// (POCSAG) fall back to `prepare`'s cutoff.
pub const NARROW_CUTOFF_HZ: f64 = 4_000.0;

/// Sync-aided frames for every sync occurrence in a channel: discriminator,
/// envelope gate and DC removal as in `classify`, then the sync search, then
/// slicing `before`/`after` symbols around each sync. Tries the narrow
/// pre-filter first, then the one `prepare` picks for the bandwidth.
pub fn frames(x: &[Complex32], fs: f64, bandwidth_hz: f64, before: usize, after: usize) -> Vec<frames::Frame> {
    if x.len() < 64 {
        return Vec::new();
    }
    let narrow = frames_from(&prepare_cutoff(x, fs, NARROW_CUTOFF_HZ), fs, before, after, frames::SliceOpts::default());
    if !narrow.is_empty() || bandwidth_hz > 25e3 {
        return narrow;
    }
    frames_from(&prepare(x, fs, bandwidth_hz), fs, before, after, frames::SliceOpts::default())
}

/// `frames` with an explicit pre-filter cutoff and slicing options.
pub fn frames_with(x: &[Complex32], fs: f64, cutoff_hz: f64, before: usize, after: usize, opts: frames::SliceOpts) -> Vec<frames::Frame> {
    if x.len() < 64 {
        return Vec::new();
    }
    frames_from(&prepare_cutoff(x, fs, cutoff_hz), fs, before, after, opts)
}

/// The P25 Phase 1 voice frames (LDU1 and LDU2) in `x`, in time order,
/// sliced for `imbe::ldu_frames`. LDUs follow each other every 864
/// symbols, so a sync the search missed is recovered from its neighbours:
/// slicing where it must be is accepted when the NID there says LDU.
pub fn p25_voice_frames(x: &[Complex32], fs: f64) -> Vec<frames::Frame> {
    const LDU: usize = 864;
    if x.len() < 64 {
        return Vec::new();
    }
    let disc = sync_disc(&prepare_cutoff(x, fs, NARROW_CUTOFF_HZ), fs);
    let hits: Vec<sync::SyncHit> =
        sync::search(&disc, fs, &sync::table()).into_iter().filter(|h| h.entry.protocol == "P25" && h.entry.detail == "Phase 1").collect();
    let is_ldu = |f: &frames::Frame| matches!(decode::p25_nid(f).map(|n| n.duid), Some(0x5 | 0xA));
    let slice = |hs: &[sync::SyncHit]| frames::slice_with(&disc, fs, hs, 8, 880, frames::SliceOpts::default());
    let mut found: Vec<frames::Frame> = slice(&hits).into_iter().filter(is_ldu).collect();
    let max_symbol = (x.len() as f64 / fs * 4800.0) as usize;
    let mut frontier: Vec<sync::SyncHit> =
        found.iter().filter_map(|f| hits.iter().find(|h| h.symbol.abs_diff(f.at_symbol + 23) <= 3).cloned()).collect();
    // Walk out from every LDU in both directions while LDUs keep turning up.
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for h in &frontier {
            for s in [h.symbol + LDU, h.symbol.wrapping_sub(LDU)] {
                if s >= max_symbol || s < LDU || found.iter().any(|f| (f.at_symbol + 23).abs_diff(s) < LDU / 2) {
                    continue;
                }
                let mut g = h.clone();
                g.symbol = s;
                if let Some(f) = slice(std::slice::from_ref(&g)).into_iter().find(is_ldu) {
                    found.push(f);
                    next.push(g);
                }
            }
        }
        frontier = next;
    }
    found.sort_by_key(|f| f.at_symbol);
    found
}

/// One sync search and slice of a discriminator: the protocol named, its
/// frames and what decoded from them.
struct SyncPass {
    best: SyncResult,
    frames: Vec<frames::Frame>,
    fields: decode::Fields,
}

impl SyncPass {
    /// More decoded frames wins; then more sync occurrences.
    fn better_than(&self, other: &SyncPass) -> bool {
        (self.fields.decoded, self.best.count) > (other.fields.decoded, other.best.count)
    }
}

/// Symbols sliced around each sync: DMR bursts carry their payload either
/// side of the sync (54 symbols before, 78 after); others need only the
/// header after. P25 link control: TDULC ends ~210 dibits after the sync,
/// LDU1 ~860. Phase 2: a fragment is 720 dibits, its syncs at 360 and 540,
/// so take enough either side for whichever this is. A POCSAG batch: 16
/// codewords of 32 bits after the sync.
fn slice_extent(protocol: &str, detail: &str) -> (usize, usize) {
    match protocol {
        "DMR" => (54, 80),
        "POCSAG" => (0, 520),
        "P25" if detail == "Phase 2" => (560, 380),
        "P25" => (8, 880),
        _ => (8, 64),
    }
}

/// Pre-filter cutoff each protocol slices best through, measured on the
/// real captures (`examples/slice_tune`, `examples/burst_windows`):
/// narrowband 4FSK at 4 kHz (the weak DMR control channel: 58 frames
/// against 22 at 6.5 kHz; P25 Phase 1 NID errors 0.43 against 0.46-0.58);
/// NXDN48 is indifferent on a strong signal (81-82 LICH at every cutoff)
/// and decodes a few more frames through 6.5 kHz on a weak one (4 against
/// 1), so it gets the second look through the wide filter; the ±4.5 kHz
/// binary pagers want the wide filter outright. Phase 2 showed no
/// difference between 4, 5 and 6.5 kHz on the captures here.
pub fn protocol_cutoff_hz(protocol: &str, detail: &str) -> f64 {
    match (protocol, detail) {
        ("NXDN", "NXDN48") => 6_500.0,
        ("dPMR", _) => 3_000.0,
        ("POCSAG", _) | ("ProVoice", _) => 6_500.0,
        _ => NARROW_CUTOFF_HZ,
    }
}

/// Searches `d` for syncs (all protocols, or only `want`'s), slices the
/// winner's frames and decodes them.
fn sync_pass(d: &[f32], fs: f64, want: Option<&SyncResult>) -> Option<SyncPass> {
    let mut hits = sync::search(d, fs, &sync::table());
    if let Some(w) = want {
        hits.retain(|h| h.entry.protocol == w.protocol && h.entry.detail == w.detail && h.entry.baud == w.baud);
    }
    let best = sync::best(&hits)?;
    let mine: Vec<sync::SyncHit> = hits.into_iter().filter(|h| h.entry.protocol == best.protocol && h.entry.baud == best.baud).collect();
    let (before, after) = slice_extent(best.protocol, best.detail);
    let fr = frames::slice(d, fs, &mine, before, after);
    let mut fields = decode::summarise(&fr);
    if best.protocol == "POCSAG" {
        let pages = pocsag::pages(&fr);
        fields.tried = fr.len();
        fields.decoded = pages.len();
        fields.activity = Some(decode::Activity::Data);
        fields.kinds = vec![("page".into(), pages.len())];
        fields.messages = pages.iter().map(|p| format!("{} ({}): {}", p.address, (b'A' + p.function) as char, p.text)).collect();
    }
    Some(SyncPass { best, frames: fr, fields })
}

/// Features read off the sliced frames once a sync has named the signal:
/// the symbol rate is the protocol's, the deviation is the outer level the
/// sync measured (in Hz), and the level count comes from how many payload
/// symbols sit at the inner levels. The raw histogram and transition-line
/// measures these come from are blank or high on weak real signals
/// (noise overshoot widens the 98th percentile; the line is buried).
fn sync_features(f: &mut Features, frames: &[frames::Frame], best: &SyncResult) {
    f.symbol_rate = Some(best.baud);
    if f.symbol_rate_raw.is_none() {
        f.symbol_rate_raw = Some(best.baud);
    }
    if frames.is_empty() {
        return;
    }
    let mut outers: Vec<f32> = frames.iter().map(|fr| fr.outer.abs()).collect();
    outers.sort_by(f32::total_cmp);
    f.deviation_hz = outers[outers.len() / 2];
    let binary = matches!(best.protocol, "POCSAG" | "ProVoice");
    let (mut inner, mut total) = (0usize, 0usize);
    for fr in frames {
        for v in &fr.soft {
            total += 1;
            inner += (v.abs() < 2.0 / 3.0) as usize;
        }
    }
    f.fsk_levels = if binary {
        2
    } else if total > 0 && inner as f32 / total as f32 > 0.12 {
        4
    } else {
        2
    };
}

fn frames_from(x: &[Complex32], fs: f64, before: usize, after: usize, opts: frames::SliceOpts) -> Vec<frames::Frame> {
    let disc = sync_disc(x, fs);
    let hits = sync::search(&disc, fs, &sync::table());
    frames::slice_with(&disc, fs, &hits, before, after, opts)
}

/// The discriminator for the sync search: residual offset removed, and
/// zero where the envelope gate is off (TDMA off-slots and gaps between
/// bursts are just noise; zero has no sign, so no false symbols).
fn sync_disc(x: &[Complex32], fs: f64) -> Vec<f32> {
    let gate = features::envelope_gate(x);
    let mut disc = features::discriminator(x, fs);
    let on: Vec<f32> = disc.iter().zip(&gate[1..]).filter(|(_, g)| **g).map(|(d, _)| *d).collect();
    let on_iq: Vec<Complex32> = x.iter().zip(&gate).filter(|(_, g)| **g).map(|(v, _)| *v).collect();
    let off = features::extract(&on_iq, &on, fs).offset_hz;
    for (v, g) in disc.iter_mut().zip(&gate[1..]) {
        *v = if *g { *v - off } else { 0.0 };
    }
    disc
}

/// What the caller knows about a window that the channel alone can't show.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hints {
    /// The window's signal-to-noise ratio, dB, from the wideband spectrum
    /// (see `Features::snr_db`).
    pub snr_db: Option<f32>,
}

/// Classifies one channel: `x` is complex baseband centred on the signal,
/// sampled at `fs`; `bandwidth_hz` is the detector's occupied bandwidth.
pub fn classify(x: &[Complex32], fs: f64, bandwidth_hz: f64) -> Classification {
    classify_with(x, fs, bandwidth_hz, Hints::default())
}

/// `classify` with what the caller measured about the window.
pub fn classify_with(x: &[Complex32], fs: f64, bandwidth_hz: f64, hints: Hints) -> Classification {
    let analysed_s = x.len() as f64 / fs;
    let mut c = Classification {
        label: String::new(),
        protocol: None,
        confidence: 0.0,
        features: Features::default(),
        sync: None,
        fields: None,
        analysed_s,
    };
    if analysed_s < 0.02 {
        c.label = "too short".into();
        return c;
    }
    let raw = x;
    let x = &prepare(x, fs, bandwidth_hz)[..];
    // Features use only the samples where the envelope gate is on.
    let gate = features::envelope_gate(x);
    let disc = features::discriminator(x, fs);
    let on: Vec<f32> = disc.iter().zip(&gate[1..]).filter(|(_, g)| **g).map(|(d, _)| *d).collect();
    let on_iq: Vec<Complex32> = x.iter().zip(&gate).filter(|(_, g)| **g).map(|(v, _)| *v).collect();
    c.features = features::extract(&on_iq, &on, fs);
    c.features.envelope_cv = features::envelope_cv(x);
    c.features.snr_db = hints.snr_db.unwrap_or(f32::NAN);

    // FLEX pagers: a 1600-baud header (sync 1 and a checked frame info
    // word) identifies them outright; the frame after it switches speed, so
    // it has its own decoder rather than a sync-bank entry.
    if (8e3..60e3).contains(&bandwidth_hz) {
        let headers = flex::headers(raw, fs);
        if let Some(h) = headers.first() {
            let frames = flex::decode(raw, fs);
            let pages: Vec<&flex::Page> = frames.iter().flat_map(|f| &f.pages).collect();
            c.protocol = Some("FLEX".into());
            c.label = format!("FLEX {}", h.mode());
            c.confidence = 0.95;
            // The header names the speed; FLEX deviates ±4.8 kHz at the outer levels.
            c.features.symbol_rate = Some(h.baud as f64);
            if c.features.symbol_rate_raw.is_none() {
                c.features.symbol_rate_raw = Some(h.baud as f64);
            }
            c.features.fsk_levels = h.levels;
            c.features.deviation_hz = 4_800.0;
            c.fields = Some(decode::Fields {
                kinds: vec![("frame".into(), headers.len()), ("page".into(), pages.len())],
                decoded: headers.len(),
                tried: headers.len(),
                activity: Some(decode::Activity::Data),
                messages: pages.iter().map(|p| p.summary()).collect(),
                ..Default::default()
            });
            return c;
        }
    }

    // Sync search for narrowband signals that aren't bare carriers: first
    // through the narrow 4FSK pre-filter, then the bandwidth's own. Once a
    // protocol is named, the channel is sliced again through the cutoff
    // that protocol does best with, and the better of the two passes kept.
    if (4.5e3..60e3).contains(&bandwidth_hz) {
        let narrow = bandwidth_hz <= 25e3;
        let own_cutoff = (bandwidth_hz * 0.5 + 500.0).clamp(4_000.0, 6_500.0);
        let mut found: Option<(f64, SyncPass)> = None;
        for (cutoff, d) in [
            narrow.then(|| (NARROW_CUTOFF_HZ, sync_disc(&prepare_cutoff(raw, fs, NARROW_CUTOFF_HZ), fs))),
            Some((own_cutoff, sync_disc(x, fs))),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(p) = sync_pass(&d, fs, None) {
                found = Some((cutoff, p));
                break;
            }
        }
        if let Some((used, mut pass)) = found {
            let pref = protocol_cutoff_hz(pass.best.protocol, pass.best.detail);
            if (pref - used).abs() > 1.0 {
                let d = sync_disc(&prepare_cutoff(raw, fs, pref), fs);
                if let Some(again) = sync_pass(&d, fs, Some(&pass.best)) {
                    if again.better_than(&pass) {
                        pass = again;
                    }
                }
            }
            sync_features(&mut c.features, &pass.frames, &pass.best);
            if pass.fields.tried > 0 {
                c.fields = Some(pass.fields);
            }
            c.sync = Some(pass.best);
        }
    }
    let f = &c.features;
    if let Some(s) = &c.sync {
        c.protocol = Some(s.protocol.to_string());
        c.label = if s.detail.is_empty() { s.protocol.to_string() } else { format!("{} {}", s.protocol, s.detail) };
        c.confidence = s.confidence;
        return c;
    }
    // Chirp spread spectrum (LoRa: 125/250/500 kHz): the first few bursts'
    // frequency follows ramps at BW²/2^SF.
    if (50e3..600e3).contains(&bandwidth_hz) {
        for b in chirp::bursts(x, fs).iter().take(4) {
            if let Some((bw, sf)) = chirp::lora(&x[b.0..b.1], fs) {
                c.label = format!("LoRa {:.0} kHz SF{sf}", bw / 1e3);
                c.confidence = 0.8;
                return c;
            }
        }
    }
    // A GSM beacon: FCCH tone bursts on the frame grid, BSIC from the SCH.
    // The tone bursts alone are not enough to name it (an FM broadcast
    // station's pilot and programme passed that test on air); a decoded
    // SCH is.
    if (150e3..300e3).contains(&bandwidth_hz) {
        if let Some(g) = gsm::detect(raw, fs).filter(|g| g.sch_ok > 0) {
            c.protocol = Some("GSM".into());
            c.label = g.label();
            c.confidence = 0.95;
            c.fields = Some(decode::Fields {
                id: g.id(),
                kinds: vec![("FCCH".into(), g.fcch_hits), ("SCH".into(), g.sch_ok)],
                decoded: g.sch_ok,
                tried: g.fcch_hits,
                activity: Some(decode::Activity::Control),
                ..Default::default()
            });
            return c;
        }
    }
    // Feature labels describe the modulation, so they need a signal to
    // describe: a window whose channel is within `WEAK_DB` of the floor
    // beside it has noise statistics (envelope CV ≈ 0.52, a one-hump
    // frequency histogram, a few kHz of "deviation"), which read as AM,
    // analog FM or a carrier. On air those were most of the wrong labels.
    if hints.snr_db.is_some_and(|s| s < WEAK_DB) {
        c.label = WEAK.into();
        c.confidence = 0.1;
        return c;
    }
    // Sub-audible squelch signalling identifies an analog FM transmitter
    // (a DCS code or CTCSS tone stays with a repeater for years), and is
    // what an idle repeater's carrier mostly carries. It goes before the
    // modulation rules because DCS (two levels swinging ±1 kHz at 134
    // bit/s) reads as 2FSK or 4FSK to the histogram, and voice over it
    // does too; the decoder itself (a 23-bit word repeating, passing its
    // Golay check) passes no data mode.
    if bandwidth_hz <= 30e3 && f.envelope_cv < 0.3 {
        if let Some(m) = squelch::measure(longest_on_run(&disc, &gate), fs) {
            c.label = "Analog FM".into();
            c.confidence = 0.8;
            c.fields = Some(decode::Fields { id: Some(m.squelch.label()), decoded: m.words, tried: m.words, ..Default::default() });
            return c;
        }
    }
    // A broadcast station's pilot and RDS subcarrier are as decisive as a
    // sync word. The detector often cuts a station into fragments of
    // 70–150 kHz (its centre, a sideband), whose discriminator still holds
    // the pilot but whose squared derivative also holds lines the FSK
    // rules took for "2FSK 2400 Bd"; so this goes first, for anything wide
    // enough to carry a 19 kHz subcarrier.
    let pilot = (bandwidth_hz >= 30e3).then(|| blind::fm_pilot(&disc, fs)).flatten();
    let (label, conf) = if let Some(p) = pilot {
        (if p.rds { "FM broadcast (stereo, RDS)".to_string() } else { "FM broadcast (stereo)".to_string() }, 0.9)
    } else if bandwidth_hz > 150e3 {
        if let Some(o) = blind::ofdm(x, fs, OFDM_USEFUL_S.0, OFDM_USEFUL_S.1) {
            (ofdm_label(&o), 0.7)
        } else if f.envelope_cv < 0.25 {
            ("Wideband FM".to_string(), 0.5)
        } else {
            ("Wideband".to_string(), 0.5)
        }
    } else if bandwidth_hz < 4.5e3 && f.fsk_levels == 0 {
        // Too narrow for voice or common FSK: a carrier or very slow data.
        ("Carrier".to_string(), 0.6)
    } else if f.fsk_levels <= 2 && f.tone_hz.is_some() && f.envelope_cv < 0.3 {
        // One pure tone carries the frequency: analog FM with a steady
        // tone (an idle repeater, a test tone). Its two-peak histogram and
        // the line at twice the tone would otherwise read as 2FSK.
        ("Analog FM".to_string(), 0.6)
    } else if let (Some(r), 2 | 4) = (f.symbol_rate, f.fsk_levels) {
        (format!("{}FSK {:.0} Bd", f.fsk_levels, r), 0.7)
    } else if f.fsk_levels == 4 {
        // Two histogram peaks without a symbol rate is more likely sine-tone
        // FM (arcsine distribution) than FSK; four peaks isn't.
        ("4FSK".to_string(), 0.5)
    } else if let (Some(r), true) = (f.symbol_rate, f.envelope_cv < 0.3) {
        // A timing line without clear levels is FSK only if the envelope is
        // steady: AM and pulses put a line at their modulation rate too.
        (format!("FSK {:.0} Bd", r), 0.5)
    } else if let Some(p) = (f.envelope_cv < 0.6).then(|| blind::psk_order(x, fs, bandwidth_hz * 0.25)).flatten() {
        // Phase modulation: the M-th power leaves a carrier line. The
        // rate is the cyclic line of the envelope (shaped PSK has one).
        let name = match p.order {
            2 => "BPSK",
            4 => "QPSK",
            _ => "8PSK",
        };
        match blind::envelope_rate(x, fs, 200.0, bandwidth_hz) {
            Some(r) => (format!("{name} {:.0} Bd", r.hz), 0.6),
            None => (name.to_string(), 0.5),
        }
    } else if let Some(o) = blind::ofdm(x, fs, OFDM_USEFUL_S.0, OFDM_USEFUL_S.1) {
        (ofdm_label(&o), 0.6)
    } else if f.envelope_cv > 0.35 {
        // The envelope varies. Amplitude
        // modulation shows one of two shapes: a carrier line standing clear
        // of its sidebands with voice-band envelope content (AM), or the
        // signal switching between two levels (OOK, pulses). A varying
        // envelope with neither is noise, a neighbour's skirt or a hump of
        // floor: not a modulation... unless it repeats: a slotted or
        // framed emission (TDMA whose sync the bank doesn't know, a beacon)
        // shows its period in the envelope.
        match am_evidence(raw, fs) {
            Some(AmKind::Voice) => ("AM".to_string(), 0.6),
            Some(AmKind::Keyed) => ("OOK".to_string(), 0.6),
            None => match (analysed_s >= PERIOD_WINDOW_S).then(|| blind::burst_period(x, fs, 0.005, 0.5)).flatten() {
                Some(p) if p.strength >= 0.5 => (format!("Bursts every {:.0} ms", p.period_s * 1e3), 0.4),
                _ => ("Unknown".to_string(), 0.2),
            },
        }
    } else if (1000.0..7000.0).contains(&f.deviation_hz) {
        ("Analog FM".to_string(), 0.5)
    } else {
        ("Unknown".to_string(), 0.2)
    };
    c.label = label;
    c.confidence = conf;
    c
}

/// The longest contiguous stretch of `disc` where the envelope gate is on
/// (the gate is one longer than the discriminator and leads it by one).
fn longest_on_run<'a>(disc: &'a [f32], gate: &[bool]) -> &'a [f32] {
    let (mut best, mut start) = ((0usize, 0usize), None);
    for (i, g) in gate[1..].iter().chain(std::iter::once(&false)).enumerate() {
        match (g, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s > best.1 - best.0 {
                    best = (s, i);
                }
                start = None;
            }
            _ => {}
        }
    }
    &disc[best.0..best.1.min(disc.len())]
}

/// Least window for a burst period to be read: a single 90 ms hop burst in
/// a 0.1 s window showed "periods" of 6–8 ms from its own envelope
/// structure; slots and frames (30 ms and up) need a few repeats.
const PERIOD_WINDOW_S: f64 = 0.3;

/// Useful OFDM symbol lengths looked for, s: from LTE/NR's 66.7 µs (and
/// shorter numerologies) up to the few ms of HD Radio and DAB-like systems.
const OFDM_USEFUL_S: (f64, f64) = (20e-6, 6e-3);

fn ofdm_label(o: &blind::Ofdm) -> String {
    if o.subcarrier_hz >= 1e3 {
        format!("OFDM {:.1} kHz subcarriers", o.subcarrier_hz / 1e3)
    } else {
        format!("OFDM {:.0} Hz subcarriers", o.subcarrier_hz)
    }
}

/// Window SNR below which no feature label is given (see `classify_with`).
pub const WEAK_DB: f32 = 7.0;
/// The label of a window too weak to describe. Not a classification: the
/// emitter book doesn't count it and the explorer doesn't log it.
pub const WEAK: &str = "Weak";

enum AmKind {
    Voice,
    Keyed,
}

/// Positive evidence of amplitude modulation on a channel (see `classify_with`).
fn am_evidence(x: &[Complex32], fs: f64) -> Option<AmKind> {
    if x.len() < 4096 {
        return None;
    }
    // Keyed: the envelope sits at two levels 12 dB apart, nearly every
    // sample at one or the other (noise alone is Rayleigh: its 10th and
    // 90th percentiles are 13 dB apart too, but only 40% of it is near
    // either), and the carrier is plain while on (a TDMA burst is keyed
    // but carries FSK; a transmission starting mid-window is keyed but
    // carries FM).
    let mut mags: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let n = mags.len();
    let (i10, i90) = (n / 10, n * 9 / 10);
    let hi = *mags.clone().select_nth_unstable_by(i90, f32::total_cmp).1;
    let lo = *mags.select_nth_unstable_by(i10, f32::total_cmp).1;
    if hi > 4.0 * lo.max(1e-9) {
        let near_hi = x.iter().filter(|c| (c.norm() - hi).abs() <= 0.25 * hi).count() as f32 / n as f32;
        let near_lo = x.iter().filter(|c| c.norm() < 0.25 * hi).count() as f32 / n as f32;
        if near_hi + near_lo >= 0.7 {
            // Instantaneous frequency while on, smoothed over 8 samples
            // (one sample's phase step is mostly noise even at 25 dB; a
            // symbol of FSK is 10 samples or more at these rates).
            let k = (fs / (2.0 * std::f64::consts::PI)) as f32;
            let inst: Vec<f32> = x
                .windows(2)
                .filter(|w| w[0].norm() > 0.5 * hi && w[1].norm() > 0.5 * hi)
                .map(|w| (w[1] * w[0].conj()).arg() * k)
                .collect();
            let mut dev: Vec<f32> = inst.windows(8).map(|w| (w.iter().sum::<f32>() / 8.0).abs()).collect();
            if dev.len() >= 256 {
                let i98 = dev.len() * 98 / 100;
                let p98 = *dev.select_nth_unstable_by(i98, f32::total_cmp).1;
                if p98 < 600.0 {
                    return Some(AmKind::Keyed);
                }
            }
        }
    }
    // Voice: a carrier at least 10 dB over the noise beside it, modulated
    // in the voice band (a bare carrier's ratio is about -40 dB), and
    // the carrier is what the channel is: near its centre, and holding
    // its frequency while the envelope is up. A keyed or FSK data
    // transmitter near the edge of the channel also has a line and
    // envelope content (on air, idle pager channels read as "AM").
    let m = carrier::measure(x, fs);
    if m.seconds > 0.0
        && m.line_snr_db >= 10.0
        && m.purity >= AM_CARRIER_SHARE
        && m.am_voice_db > -25.0
        && m.line_hz.abs() <= AM_CARRIER_OFFSET * fs
        && steady_frequency(x, fs) < AM_MAX_FREQUENCY_SWING_HZ
    {
        return Some(AmKind::Voice);
    }
    None
}

/// Least share of the on-signal power an AM carrier holds (two thirds for
/// a sine at full modulation, more for voice; a keyed or FSK data
/// transmitter's strongest line holds a fifth or so).
const AM_CARRIER_SHARE: f32 = 0.5;
/// Where an AM carrier may sit in its channel, as a fraction of the
/// channel sample rate (2.5 × the record width): within a fifth of the
/// width of the centre.
const AM_CARRIER_OFFSET: f64 = 0.08;
/// How far the instantaneous frequency of an AM channel may swing (90th
/// percentile of its smoothed deviation from the median) while the
/// envelope is up: an AM carrier holds still; FSK, FM and keyed data
/// move by kilohertz.
const AM_MAX_FREQUENCY_SWING_HZ: f32 = 400.0;

/// The 90th percentile of the smoothed instantaneous frequency's distance
/// from its median, over the samples where the envelope is above its
/// median (an AM trough is all noise), Hz.
fn steady_frequency(x: &[Complex32], fs: f64) -> f32 {
    let mut mags: Vec<f32> = x.iter().map(|c| c.norm()).collect();
    let n = mags.len();
    let med = *mags.select_nth_unstable_by(n / 2, f32::total_cmp).1;
    let k = (fs / (2.0 * std::f64::consts::PI)) as f32;
    let inst: Vec<f32> =
        x.windows(2).filter(|w| w[0].norm() >= med && w[1].norm() >= med).map(|w| (w[1] * w[0].conj()).arg() * k).collect();
    let mut smooth: Vec<f32> = inst.windows(8).map(|w| w.iter().sum::<f32>() / 8.0).collect();
    if smooth.len() < 64 {
        return f32::MAX;
    }
    let m = smooth.len() / 2;
    let centre = *smooth.clone().select_nth_unstable_by(m, f32::total_cmp).1;
    for v in smooth.iter_mut() {
        *v = (*v - centre).abs();
    }
    let i90 = smooth.len() * 9 / 10;
    *smooth.select_nth_unstable_by(i90, f32::total_cmp).1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2FSK at `baud`, ±`dev`, random bits with `sync` (32 bits, 1 = negative)
    /// inserted every `period` bits, at sample rate `fs`.
    fn fsk2_with_sync(baud: f64, dev: f64, fs: f64, secs: f64, sync: u32, period: usize) -> Vec<Complex32> {
        let n = (secs * fs) as usize;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let k = (i as f64 * baud / fs) as usize;
                let pos = k % period;
                let neg = if pos < 32 {
                    sync >> (31 - pos) & 1 == 1
                } else {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(k as u64);
                    (x >> 63) == 1
                };
                phase += 2.0 * std::f64::consts::PI * if neg { -dev } else { dev } / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    /// 2FSK of the given bits (1 = −dev) at `baud`, with a little noise.
    fn fsk2_bits(bits: &[bool], baud: f64, dev: f64, fs: f64) -> Vec<Complex32> {
        let n = (bits.len() as f64 * fs / baud) as usize;
        let mut phase = 0.0f64;
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        (0..n)
            .map(|i| {
                let neg = bits[((i as f64 * baud / fs) as usize).min(bits.len() - 1)];
                phase += 2.0 * std::f64::consts::PI * if neg { -dev } else { dev } / fs;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let nz = ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.3;
                Complex32::new(phase.cos() as f32 + nz, phase.sin() as f32 - nz)
            })
            .collect()
    }

    #[test]
    fn pocsag_page_decodes_end_to_end() {
        // Preamble, then two batches: an alpha page to address 1234567.
        let mut cws: Vec<u32> = vec![0x7A89_C197; 14];
        cws.push(pocsag::encode((1_234_567u32 >> 3) << 2 | 3));
        let text = "SPECTRAWL TEST";
        let bits: Vec<u32> = text.bytes().flat_map(|c| (0..7).map(move |i| (c >> i & 1) as u32)).collect();
        cws.extend(
            bits.chunks(20).map(|c| pocsag::encode(1 << 20 | c.iter().chain(std::iter::repeat(&0)).take(20).fold(0, |a, b| a << 1 | b))),
        );
        cws.resize(32, 0x7A89_C197);
        let mut stream: Vec<bool> = (0..576).map(|i| i % 2 == 0).collect();
        for batch in cws.chunks(16) {
            stream.extend((0..32).rev().map(|i| 0x7CD2_15D8u32 >> i & 1 == 1));
            for cw in batch {
                stream.extend((0..32).rev().map(|i| cw >> i & 1 == 1));
            }
        }
        stream.extend((0..200).map(|i| i % 2 == 0));
        let fs = 48_000.0;
        let x = fsk2_bits(&stream, 1200.0, 4500.0, fs);
        let c = classify(&x, fs, 18_000.0);
        assert_eq!(c.protocol.as_deref(), Some("POCSAG"));
        let f = c.fields.expect("fields");
        assert_eq!(f.messages, vec!["1234567 (D): SPECTRAWL TEST".to_string()], "{f:?}");
    }

    /// Complex Gaussian noise, unit power.
    fn noise(n: usize, seed: u64) -> Vec<Complex32> {
        let mut seed = seed | 1;
        let mut uni = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uni().ln()).sqrt() * std::f64::consts::FRAC_1_SQRT_2;
                let th = 2.0 * std::f64::consts::PI * uni();
                Complex32::new((r * th.cos()) as f32, (r * th.sin()) as f32)
            })
            .collect()
    }

    #[test]
    fn noise_is_not_a_modulation() {
        let fs = 48_000.0;
        let x = noise(72_000, 7);
        // What a 25 kHz record of floor used to come out as.
        let c = classify(&x, fs, 25_000.0);
        assert_eq!(c.label, "Unknown", "{c:?}");
        // Told the window is weak, it says so and stops.
        let c = classify_with(&x, fs, 25_000.0, Hints { snr_db: Some(3.0) });
        assert_eq!(c.label, WEAK);
        assert_eq!(c.features.snr_db, 3.0);
    }

    #[test]
    fn am_voice_and_ook_have_evidence() {
        let fs = 48_000.0;
        let n = 72_000;
        let nz = noise(n, 11);
        // AM: a carrier modulated 60% by a 1 kHz tone, 25 dB over the noise.
        let am: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let a = 1.0 + 0.6 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
                Complex32::new(a as f32, 0.0) * 18.0 + nz[i]
            })
            .collect();
        let c = classify_with(&am, fs, 8_000.0, Hints { snr_db: Some(25.0) });
        assert_eq!(c.label, "AM", "{c:?}");
        // OOK: the carrier keyed at 40 Hz, half on.
        let ook: Vec<Complex32> = (0..n)
            .map(|i| {
                let on = ((i as f64 / fs * 40.0) as u64).is_multiple_of(2);
                Complex32::new(if on { 18.0 } else { 0.0 }, 0.0) + nz[i]
            })
            .collect();
        let c = classify_with(&ook, fs, 8_000.0, Hints { snr_db: Some(25.0) });
        assert_eq!(c.label, "OOK", "{c:?}");
    }

    #[test]
    fn noise_never_names_a_protocol() {
        // Forty 1.5 s windows of floor, as an empty channel gives the
        // classifier all day: no sync-bank entry may confirm on them.
        let fs = 48_000.0;
        for seed in 0..40u64 {
            let x = noise(72_000, 1000 + seed);
            let c = classify(&x, fs, 12_500.0);
            assert_eq!(c.protocol, None, "seed {seed}: {c:?}");
        }
    }

    #[test]
    fn fm_repeater_tone_is_analog_fm() {
        // 855.6375 MHz on air: an analog FM repeater whose idle tone came
        // out "2FSK 2400 Bd" in half its windows.
        let fs = 48_000.0;
        let nz = noise(72_000, 5);
        let mut ph = 0.0f64;
        let x: Vec<Complex32> = (0..72_000)
            .map(|i| {
                let t = i as f64 / fs;
                ph += 2.0 * std::f64::consts::PI * 1300.0 * (2.0 * std::f64::consts::PI * 1200.0 * t).sin() / fs;
                Complex32::from_polar(30.0, ph as f32) + nz[i]
            })
            .collect();
        let c = classify_with(&x, fs, 9_000.0, Hints { snr_db: Some(33.0) });
        assert_eq!(c.label, "Analog FM", "{c:?}");
    }

    #[test]
    fn pocsag_identified() {
        let fs = 48_000.0;
        let x = fsk2_with_sync(1200.0, 4500.0, fs, 1.5, 0x7CD215D8, 544);
        let c = classify(&x, fs, 18_000.0);
        assert_eq!(c.protocol.as_deref(), Some("POCSAG"), "{c:?}");
    }
}
