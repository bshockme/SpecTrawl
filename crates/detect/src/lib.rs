//! Signal classification: features from channelised IQ, then the
//! sync bank for protocol identification (DESIGN.md §7).

pub mod ddc;
pub mod features;
pub mod sync;

use num_complex::Complex32;

pub use ddc::Ddc;
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
    /// Seconds of signal analysed.
    pub analysed_s: f64,
}

/// Classifies one channel: `x` is complex baseband centred on the signal,
/// sampled at `fs`; `bandwidth_hz` is the detector's occupied bandwidth.
pub fn classify(x: &[Complex32], fs: f64, bandwidth_hz: f64) -> Classification {
    let analysed_s = x.len() as f64 / fs;
    let mut c = Classification { label: String::new(), protocol: None, confidence: 0.0, features: Features::default(), sync: None, analysed_s };
    if analysed_s < 0.02 {
        c.label = "too short".into();
        return c;
    }
    // Gate on the envelope: TDMA off-slots and gaps between bursts are just
    // noise to the discriminator. Features use only "on" samples; for the
    // sync search, off samples are zeroed (no sign, so no false symbols).
    let gate = features::envelope_gate(x);
    let mut disc = features::discriminator(x, fs);
    let on: Vec<f32> = disc.iter().zip(&gate[1..]).filter(|(_, g)| **g).map(|(d, _)| *d).collect();
    let on_iq: Vec<Complex32> = x.iter().zip(&gate).filter(|(_, g)| **g).map(|(v, _)| *v).collect();
    c.features = features::extract(&on_iq, &on, fs);
    c.features.envelope_cv = features::envelope_cv(x);
    let off = c.features.offset_hz;
    for (v, g) in disc.iter_mut().zip(&gate[1..]) {
        *v = if *g { *v - off } else { 0.0 };
    }

    // Sync search for narrowband signals that aren't bare carriers.
    if (4.5e3..60e3).contains(&bandwidth_hz) {
        c.sync = sync::best(&sync::search(&disc, fs, &sync::table()));
    }
    let f = &c.features;
    if let Some(s) = &c.sync {
        c.protocol = Some(s.protocol.to_string());
        c.label = if s.detail.is_empty() { s.protocol.to_string() } else { format!("{} {}", s.protocol, s.detail) };
        c.confidence = s.confidence;
        return c;
    }
    let gated = gate.iter().filter(|g| !**g).count() as f32 / gate.len() as f32;
    let (label, conf) = if bandwidth_hz > 150e3 {
        (if f.envelope_cv < 0.25 { "Wideband FM".to_string() } else { "Wideband".to_string() }, 0.5)
    } else if bandwidth_hz < 4.5e3 && f.fsk_levels == 0 {
        // Too narrow for voice or common FSK: a carrier or very slow data.
        ("Carrier".to_string(), 0.6)
    } else if let (Some(r), 2 | 4) = (f.symbol_rate, f.fsk_levels) {
        (format!("{}FSK {:.0} Bd", f.fsk_levels, r), 0.7)
    } else if f.fsk_levels == 4 {
        // Two histogram peaks without a symbol rate is more likely sine-tone
        // FM (arcsine distribution) than FSK; four peaks isn't.
        ("4FSK".to_string(), 0.5)
    } else if let Some(r) = f.symbol_rate {
        (format!("FSK {:.0} Bd", r), 0.5)
    } else if f.envelope_cv > 0.35 && gated < 0.2 {
        // Envelope varies while the signal is present: amplitude modulation.
        ("AM / OOK".to_string(), 0.5)
    } else if (1000.0..7000.0).contains(&f.deviation_hz) {
        ("Analog FM".to_string(), 0.5)
    } else {
        ("Unknown".to_string(), 0.2)
    };
    c.label = label;
    c.confidence = conf;
    c
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

    #[test]
    fn pocsag_identified() {
        let fs = 48_000.0;
        let x = fsk2_with_sync(1200.0, 4500.0, fs, 1.5, 0x7CD215D8, 544);
        let c = classify(&x, fs, 18_000.0);
        assert_eq!(c.protocol.as_deref(), Some("POCSAG"), "{c:?}");
    }
}
