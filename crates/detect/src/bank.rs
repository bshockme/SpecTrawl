//! The classifier's rungs as separate detectors (rebuild spec §4.5): each looks at a prepared [`Window`] and says what it
//! found, with what it found it from. [`crate::classify_with`] is these, tried in its order and stopping at the first
//! that answers; a detector bank runs them side by side and keeps every answer as its own claim.

use num_complex::Complex32;

use crate::{
    AmKind, Features, Hints, NARROW_CUTOFF_HZ, OFDM_USEFUL_S, PERIOD_WINDOW_S, SyncPass, SyncResult, WEAK_DB, acars, am_evidence, blind,
    carrier, chirp, decode, features, flex, frames, gsm, longest_on_run, ofdm_label, prepare, prepare_cutoff, protocol_cutoff_hz, squelch,
    sunfsk, sync_disc, sync_features, sync_pass, weak_pass,
};

/// The shortest window any rung looks at: 20 ms.
pub const MIN_SECONDS: f64 = 0.02;

/// One channel window, prepared once for every rung: `raw` is complex baseband centred on the signal at `fs`;
/// `bandwidth_hz` is the detector's occupied bandwidth.
pub struct Window<'a> {
    /// What the shape is measured on: the window, or (see [`Window::with_full`]) the part of it the caller kept.
    pub raw: &'a [Complex32],
    /// The whole window, for the decoders and the sync search. A caller may trim the quiet ends off `raw` for the
    /// shape measurements, but a transmission can open in a fade, and its first frame's header is there.
    pub full: &'a [Complex32],
    pub fs: f64,
    pub bandwidth_hz: f64,
    /// The window's SNR from the wideband spectrum, when the caller measured it.
    pub snr_db: Option<f32>,
    /// See [`Hints::am_band`].
    pub am_band: bool,
    /// `raw` re-centred and, for narrowband, low-passed (see [`prepare`]).
    x: Vec<Complex32>,
    gate: Vec<bool>,
    disc: Vec<f32>,
    /// Measured over the samples where the envelope gate is on.
    pub features: Features,
}

impl<'a> Window<'a> {
    /// `raw` must hold at least [`MIN_SECONDS`].
    pub fn new(raw: &'a [Complex32], fs: f64, bandwidth_hz: f64, hints: Hints) -> Window<'a> {
        let x = prepare(raw, fs, bandwidth_hz);
        let gate = features::envelope_gate(&x);
        let disc = features::discriminator(&x, fs);
        let on: Vec<f32> = disc.iter().zip(&gate[1..]).filter(|(_, g)| **g).map(|(d, _)| *d).collect();
        let on_iq: Vec<Complex32> = x.iter().zip(&gate).filter(|(_, g)| **g).map(|(v, _)| *v).collect();
        let mut features = features::extract(&on_iq, &on, fs);
        features.envelope_cv = features::envelope_cv(&x);
        features.snr_db = hints.snr_db.unwrap_or(f32::NAN);
        Window { raw, full: raw, fs, bandwidth_hz, snr_db: hints.snr_db, am_band: hints.am_band, x, gate, disc, features }
    }

    /// The same window, decoded from `full`, the whole of which `raw` is a part.
    pub fn with_full(self, full: &'a [Complex32]) -> Window<'a> {
        Window { full, ..self }
    }

    pub fn seconds(&self) -> f64 {
        self.raw.len() as f64 / self.fs
    }

    /// The share of the window the envelope gate holds on (a burst's duty within it).
    pub fn on_fraction(&self) -> f32 {
        if self.gate.is_empty() { 0.0 } else { self.gate.iter().filter(|g| **g).count() as f32 / self.gate.len() as f32 }
    }

    /// Within [`WEAK_DB`] of the floor beside it: noise statistics, too weak to describe by its modulation.
    pub fn weak(&self) -> bool {
        self.snr_db.is_some_and(|s| s < WEAK_DB)
    }
}

/// A signal named by something decoded from it (a header, a checked word): the protocol, a label, how sure, and what
/// decoded.
#[derive(Debug, Clone)]
pub struct Identified {
    /// `None` for analog FM named by its squelch code.
    pub protocol: Option<&'static str>,
    pub label: String,
    pub confidence: f32,
    pub fields: Option<decode::Fields>,
    /// The window's features, corrected by what decoded (the header's speed, say).
    pub features: Features,
}

/// The sync bank's answer: the protocol its syncs name, the frames sliced at them and what decoded from those.
#[derive(Debug, Clone)]
pub struct Synced {
    pub sync: SyncResult,
    pub frames: Vec<frames::Frame>,
    /// Present when any frame was tried.
    pub fields: Option<decode::Fields>,
    /// The window's features, with the protocol's symbol rate, deviation and level count.
    pub features: Features,
}

/// A description from the signal's shape alone.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub label: String,
    pub confidence: f32,
}

/// FLEX pagers: a 1600-baud header (sync 1 and a checked frame info word) identifies them outright; the frame after it
/// switches speed, so it has its own decoder rather than a sync-bank entry. 8–60 kHz windows only.
pub fn flex(w: &Window<'_>) -> Option<Identified> {
    if !(8e3..60e3).contains(&w.bandwidth_hz) {
        return None;
    }
    let headers = flex::headers(w.full, w.fs);
    let h = headers.first()?;
    let frames = flex::decode(w.full, w.fs);
    let pages: Vec<&flex::Page> = frames.iter().flat_map(|f| &f.pages).collect();
    let mut f = w.features.clone();
    // The header names the speed; FLEX deviates ±4.8 kHz at the outer levels.
    f.symbol_rate = Some(h.baud as f64);
    if f.symbol_rate_raw.is_none() {
        f.symbol_rate_raw = Some(h.baud as f64);
    }
    f.fsk_levels = h.levels;
    f.deviation_hz = 4_800.0;
    Some(Identified {
        protocol: Some("FLEX"),
        label: format!("FLEX {}", h.mode()),
        confidence: 0.95,
        fields: Some(decode::Fields {
            kinds: vec![("frame".into(), headers.len()), ("page".into(), pages.len())],
            decoded: headers.len(),
            tried: headers.len(),
            activity: Some(decode::Activity::Data),
            messages: pages.iter().map(|p| p.summary()).collect(),
            ..Default::default()
        }),
        features: f,
    })
}

/// ACARS, the air band's data link (AM, MSK audio at 2400 bit/s): blocks whose BCS checks name the aircraft. Only a
/// checked block counts, so this needs no SNR gate: the bursts are often weak and short, and recorded only as their
/// carrier. Channels up to 30 kHz.
pub fn acars(w: &Window<'_>) -> Option<Identified> {
    if w.bandwidth_hz > 30e3 {
        return None;
    }
    let blocks = acars::decode(w.full, w.fs);
    let ok: Vec<&acars::Block> = blocks.iter().filter(|b| b.crc_ok).collect();
    let first = ok.first()?;
    let mut labels: Vec<(String, usize)> = Vec::new();
    for b in &ok {
        match labels.iter_mut().find(|(l, _)| *l == b.label) {
            Some((_, n)) => *n += 1,
            None => labels.push((b.label.clone(), 1)),
        }
    }
    Some(Identified {
        protocol: Some("ACARS"),
        label: "ACARS".into(),
        confidence: 0.95,
        fields: Some(decode::Fields {
            id: Some(first.registration().to_string()).filter(|r| !r.is_empty()),
            kinds: labels.into_iter().map(|(l, n)| (format!("label {l}"), n)).collect(),
            decoded: ok.len(),
            tried: blocks.len(),
            activity: Some(decode::Activity::Data),
            messages: ok.iter().map(|b| b.summary()).collect(),
            ..Default::default()
        }),
        features: w.features.clone(),
    })
}

/// The sync bank, for narrowband signals that aren't bare carriers (4.5–60 kHz): first through the narrow 4FSK
/// pre-filter, then the bandwidth's own. Once a protocol is named, the channel is sliced again through the cutoff that
/// protocol does best with, and the better of the two passes kept.
///
/// Channels up to 25 kHz also get the weak pass ([`crate::weak_pass`]), and the pass that decoded more frames wins. So
/// do records narrower than 4.5 kHz, whose window is still a whole channel: on air at 800 MHz they were mostly the
/// 2–5 kHz cores of weak P25 channels, all that rose over the detector's threshold.
pub fn sync_bank(w: &Window<'_>) -> Option<Synced> {
    if w.bandwidth_hz >= 60e3 {
        return None;
    }
    let (raw, fs) = (w.full, w.fs);
    let narrow = w.bandwidth_hz <= 25e3;
    let mut hard: Option<SyncPass> = None;
    if w.bandwidth_hz >= 4.5e3 {
        let own_cutoff = (w.bandwidth_hz * 0.5 + 500.0).clamp(4_000.0, 6_500.0);
        let mut found: Option<(f64, SyncPass)> = None;
        for (cutoff, d) in [
            narrow.then(|| (NARROW_CUTOFF_HZ, sync_disc(&prepare_cutoff(raw, fs, NARROW_CUTOFF_HZ), fs))),
            Some((
                own_cutoff,
                if raw.len() == w.raw.len() { sync_disc(&w.x, fs) } else { sync_disc(&prepare(raw, fs, w.bandwidth_hz), fs) },
            )),
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
                if let Some(again) = sync_pass(&d, fs, Some(&pass.best))
                    && again.better_than(&pass)
                {
                    pass = again;
                }
            }
            hard = Some(pass);
        }
    }
    let weak = if narrow { weak_pass(raw, fs) } else { None };
    let pass = match (hard, weak) {
        (Some(h), Some(w)) => {
            if w.better_than(&h) {
                w
            } else {
                h
            }
        }
        (h, w) => h.or(w)?,
    };
    let mut f = w.features.clone();
    sync_features(&mut f, &pass.frames, &pass.best);
    let fields = (pass.fields.tried > 0).then_some(pass.fields);
    Some(Synced { sync: pass.best, frames: pass.frames, fields, features: f })
}

/// Chirp spread spectrum (LoRa: 125/250/500 kHz): the first few bursts' frequency follows ramps at BW²/2^SF. 50–600 kHz
/// windows only.
pub fn lora(w: &Window<'_>) -> Option<Described> {
    if !(50e3..600e3).contains(&w.bandwidth_hz) {
        return None;
    }
    chirp::bursts(&w.x, w.fs).iter().take(4).find_map(|b| {
        chirp::lora(&w.x[b.0..b.1], w.fs).map(|(bw, sf)| Described { label: format!("LoRa {:.0} kHz SF{sf}", bw / 1e3), confidence: 0.8 })
    })
}

/// A GSM beacon: FCCH tone bursts on the frame grid, BSIC from the SCH. The tone bursts alone are not enough to name it
/// (an FM broadcast station's pilot and programme passed that test on air); a decoded SCH is. 150–300 kHz windows only.
pub fn gsm(w: &Window<'_>) -> Option<Identified> {
    if !(150e3..300e3).contains(&w.bandwidth_hz) {
        return None;
    }
    let g = gsm::detect(w.full, w.fs).filter(|g| g.sch_ok > 0)?;
    Some(Identified {
        protocol: Some("GSM"),
        label: g.label(),
        confidence: 0.95,
        fields: Some(decode::Fields {
            id: g.id(),
            kinds: vec![("FCCH".into(), g.fcch_hits), ("SCH".into(), g.sch_ok)],
            decoded: g.sch_ok,
            tried: g.fcch_hits,
            activity: Some(decode::Activity::Control),
            ..Default::default()
        }),
        features: w.features.clone(),
    })
}

/// 802.15.4g SUN FSK frames (Wi-SUN smart meters at 902–928 MHz): preamble and SFD found (see [`sunfsk`]). 60–600 kHz
/// windows only (the rates' occupied bands are about 75–450 kHz).
pub fn sun_fsk(w: &Window<'_>) -> Option<Identified> {
    if !(60e3..600e3).contains(&w.bandwidth_hz) {
        return None;
    }
    let s = sunfsk::detect(w.full, w.fs)?;
    let mut f = w.features.clone();
    f.symbol_rate = Some(s.rate);
    f.symbol_rate_raw = Some(s.rate);
    f.fsk_levels = 2;
    let mut kinds = Vec::new();
    if s.coded > 0 {
        kinds.push(("coded frame".to_string(), s.coded));
    }
    if s.uncoded > 0 {
        kinds.push(("uncoded frame".to_string(), s.uncoded));
    }
    Some(Identified {
        protocol: Some("802.15.4g"),
        label: s.label(),
        confidence: 0.95,
        fields: Some(decode::Fields {
            kinds,
            decoded: s.frames(),
            tried: s.frames(),
            activity: Some(decode::Activity::Data),
            ..Default::default()
        }),
        features: f,
    })
}

/// Sub-audible squelch signalling identifies an analog FM transmitter (a DCS code or CTCSS tone stays with a repeater
/// for years), and is what an idle repeater's carrier mostly carries. The decoder (a 23-bit word repeating, passing its
/// Golay check) passes no data mode; DCS itself reads as 2FSK or 4FSK to the histogram. Up to 30 kHz, steady envelope.
pub fn squelch(w: &Window<'_>) -> Option<Identified> {
    if w.bandwidth_hz > 30e3 || w.features.envelope_cv >= 0.3 {
        return None;
    }
    let m = squelch::measure(longest_on_run(&w.disc, &w.gate), w.fs)?;
    Some(Identified {
        protocol: None,
        label: "Analog FM".into(),
        confidence: 0.8,
        fields: Some(decode::Fields { id: Some(m.squelch.label()), decoded: m.words, tried: m.words, ..Default::default() }),
        features: w.features.clone(),
    })
}

/// A narrow signal is a carrier only with a steadier envelope than this: noise's CV is about 0.52.
pub const CARRIER_MAX_CV: f32 = 0.42;

/// The modulation as the signal's shape describes it (FM broadcast by its pilot, OFDM, FSK levels and rate, PSK, AM,
/// OOK, bursts), or "Unknown". It describes noise too: a caller gates it on [`Window::weak`].
pub fn modulation(w: &Window<'_>) -> Described {
    let (raw, x, fs, f, bw) = (w.raw, &w.x[..], w.fs, &w.features, w.bandwidth_hz);
    // A broadcast station's pilot or RDS subcarrier is as decisive as a sync word. The detector often cuts a station
    // into fragments of 70–150 kHz (its centre, a sideband), whose discriminator still holds the pilot but whose squared
    // derivative also holds lines the FSK rules took for "2FSK 2400 Bd"; so this goes first, for anything wide enough to
    // carry a 19 kHz subcarrier.
    let station = (bw >= 30e3).then(|| blind::fm_broadcast(&w.disc, fs)).flatten();
    let (label, confidence) = if let Some(p) = station {
        let label = match (p.pilot_db.is_some(), p.rds) {
            (true, true) => "FM broadcast (stereo, RDS)",
            (true, false) => "FM broadcast (stereo)",
            _ => "FM broadcast (mono, RDS)",
        };
        (label.to_string(), 0.9)
    } else if bw > 150e3 {
        if let Some(o) = blind::ofdm(x, fs, OFDM_USEFUL_S.0, OFDM_USEFUL_S.1) {
            (ofdm_label(&o), 0.7)
        } else if f.envelope_cv < 0.25 {
            ("Wideband FM".to_string(), 0.5)
        } else {
            ("Wideband".to_string(), 0.5)
        }
    } else if bw < 4.5e3 && f.fsk_levels == 0 {
        // Too narrow for voice or common FSK: a carrier or very slow data... or AM whose sidebands sit under the
        // detector's threshold. On air, air band calls were recorded 0.6–4 kHz wide, their carrier and a little more;
        // `raw` is the wider channel the look was cut from, which holds the sidebands. Only where AM voice is (see
        // `Hints::am_band`).
        if w.am_band && carrier::coherent_am(raw, fs).is_some_and(|m| m.is_am()) {
            ("AM".to_string(), 0.6)
        } else if f.envelope_cv < CARRIER_MAX_CV {
            ("Carrier".to_string(), 0.6)
        } else {
            // Noise's envelope (CV ≈ 0.52), not a carrier's: on air (2026-11-10) nearly every "Carrier" was a 1–4 kHz
            // record at 11–12 dB with CV ≥ 0.42 and 3–5 kHz of "deviation", the core of a weak 4800 Bd channel at
            // 800 MHz or a distant FM station's peak; the real carriers measured CV < 0.3.
            ("Unknown".to_string(), 0.0)
        }
    } else if f.fsk_levels <= 2 && f.tone_hz.is_some() && f.envelope_cv < 0.3 {
        // One pure tone carries the frequency: analog FM with a steady tone (an idle repeater, a test tone). Its two-peak
        // histogram and the line at twice the tone would otherwise read as 2FSK.
        ("Analog FM".to_string(), 0.6)
    } else if let (Some(r), 2 | 4) = (f.symbol_rate, f.fsk_levels) {
        (format!("{}FSK {:.0} Bd", f.fsk_levels, r), 0.7)
    } else if f.fsk_levels == 4 {
        // Two histogram peaks without a symbol rate is more likely sine-tone FM (arcsine distribution) than FSK; four
        // peaks isn't.
        ("4FSK".to_string(), 0.5)
    } else if let (Some(r), true) = (f.symbol_rate, f.envelope_cv < 0.3) {
        // A timing line without clear levels is FSK only if the envelope is steady: AM and pulses put a line at their
        // modulation rate too.
        (format!("FSK {:.0} Bd", r), 0.5)
    } else if let Some(p) = (f.envelope_cv < 0.6).then(|| blind::psk_order(x, fs, bw * 0.25)).flatten() {
        // Phase modulation: the M-th power leaves a carrier line. The rate is the cyclic line of the envelope (shaped PSK
        // has one).
        let name = match p.order {
            2 => "BPSK",
            4 => "QPSK",
            _ => "8PSK",
        };
        match blind::envelope_rate(x, fs, 200.0, bw) {
            Some(r) => (format!("{name} {:.0} Bd", r.hz), 0.6),
            None => (name.to_string(), 0.5),
        }
    } else if let Some(o) = blind::ofdm(x, fs, OFDM_USEFUL_S.0, OFDM_USEFUL_S.1) {
        (ofdm_label(&o), 0.6)
    } else if f.envelope_cv > 0.35 {
        // The envelope varies. Amplitude modulation shows one of two shapes: a carrier line standing clear of its
        // sidebands with voice-band envelope content (AM), or the signal switching between two levels (OOK, pulses). A
        // varying envelope with neither is noise, a neighbour's skirt or a hump of floor: not a modulation... unless it
        // repeats: a slotted or framed emission (TDMA whose sync the bank doesn't know, a beacon) shows its period in the
        // envelope.
        match am_evidence(raw, fs) {
            Some(AmKind::Voice) => ("AM".to_string(), 0.6),
            Some(AmKind::Keyed) => ("OOK".to_string(), 0.6),
            None => match (w.seconds() >= PERIOD_WINDOW_S).then(|| blind::burst_period(x, fs, 0.005, 0.5)).flatten() {
                Some(p) if p.strength >= 0.5 => (format!("Bursts every {:.0} ms", p.period_s * 1e3), 0.4),
                _ => ("Unknown".to_string(), 0.2),
            },
        }
    } else if (1000.0..7000.0).contains(&f.deviation_hz) {
        ("Analog FM".to_string(), 0.5)
    } else {
        ("Unknown".to_string(), 0.2)
    };
    Described { label, confidence }
}
