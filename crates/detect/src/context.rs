//! Refines a classification with what is known about where the signal sits:
//! the tuner centre (DC), the radio's own spur grid, the band plan, and, for
//! bare carriers, a closer look at the line (spur, birdie or transmitter).

use num_complex::Complex32;

use crate::Classification;
use crate::bandplan::{self, Service};
use crate::carrier::{self, Verdict};

/// Where a channel sits and what the radio is known to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context {
    /// Absolute centre of the signal, Hz.
    pub center_hz: f64,
    /// Tuner centre when it was captured, Hz.
    pub tuner_hz: f64,
    pub bandwidth_hz: f64,
    /// See `sdr_core::Capabilities::spur_grid_hz`.
    pub spur_grid_hz: Option<f64>,
    /// Tone the radio's USB transfers put on its own spurs (rate / transfer).
    pub transfer_tone_hz: Option<f64>,
}

/// Narrow: a carrier-like signal, not voice or wideband data.
const NARROW_HZ: f64 = 5e3;
/// Within this of the tuner centre a narrow line is the DC offset.
const DC_HZ: f64 = 1.5e3;
/// Within this of a spur grid point a narrow line is the radio's own.
const GRID_HZ: f64 = 2.5e3;

/// Rewrites `c.label` (never a decoded protocol) using the context, and for
/// narrow unidentified signals, the carrier measurements on `x` (the channel
/// at `fs`).
pub fn refine(c: &mut Classification, ctx: &Context, x: &[Complex32], fs: f64) {
    if c.protocol.is_some() || matches!(c.label.as_str(), "too short" | "IQ expired" | "interrupted") {
        return;
    }
    let narrow = ctx.bandwidth_hz < NARROW_HZ;
    if narrow && (ctx.center_hz - ctx.tuner_hz).abs() < DC_HZ {
        c.label = "DC offset".into();
        c.confidence = 0.9;
        return;
    }
    if narrow
        && ctx.spur_grid_hz.is_some_and(|g| {
            let k = (ctx.center_hz / g).round();
            (ctx.center_hz - k * g).abs() < GRID_HZ
        })
    {
        c.label = "Internal spur".into();
        c.confidence = 0.9;
        return;
    }
    if narrow || c.label == "Carrier" {
        let f = carrier::measure(x, fs);
        if let (Some(t), Some(tt)) = (f.tone_hz, ctx.transfer_tone_hz)
            && (t as f64 - tt).abs() < 0.8
        {
            c.label = "Internal spur".into();
            c.confidence = 0.8;
            return;
        }
        match carrier::verdict(&f) {
            Verdict::FmCtcss(t) => {
                c.label = format!("FM, CTCSS {t:.1} Hz");
                c.confidence = 0.8;
            }
            // A weak pure line's envelope is noise, which reads as AM: any
            // narrow signal that is really a steady line is one.
            Verdict::Spur if c.label == "Carrier" || narrow => {
                c.label = "CW line (birdie)".into();
                c.confidence = 0.6;
            }
            _ => {}
        }
    }
    // Band context: name the service where the modulation fits it.
    let Some(band) = bandplan::lookup(ctx.center_hz) else { return };
    let am = c.label.starts_with("AM");
    let fm = c.label.contains("FM");
    let named = match (band.service, band.name) {
        (Service::Aviation, _) if am => Some("Aviation AM".to_string()),
        (Service::Military, _) if am => Some("Military air AM".to_string()),
        (Service::AviationNav, "ADS-B / Mode S") if am || c.label.contains("OOK") => Some("ADS-B / Mode S".to_string()),
        (Service::AviationNav, "SSR interrogation") if am => Some("Mode S interrogation".to_string()),
        (Service::Weather, "NOAA Weather Radio") if fm => Some("NOAA Weather Radio".to_string()),
        (Service::Marine, _) if fm => Some(format!("Marine {}", c.label)),
        (_, "FRS/GMRS") | (_, "FRS") if fm => Some(format!("FRS/GMRS {}", c.label)),
        (Service::Ism, name) if !c.label.starts_with("ISM") => Some(format!("{name} {}", c.label)),
        _ => None,
    };
    if let Some(n) = named {
        c.label = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(label: &str) -> Classification {
        Classification {
            label: label.into(),
            protocol: None,
            confidence: 0.5,
            features: Default::default(),
            sync: None,
            fields: None,
            analysed_s: 1.0,
        }
    }

    fn tone(fs: f64, n: usize, f: f64) -> Vec<Complex32> {
        (0..n).map(|i| Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * f * i as f64 / fs) as f32)).collect()
    }

    #[test]
    fn dc_and_grid_spurs() {
        let x = tone(48e3, 48_000, 100.0);
        let mut c = class("Carrier");
        refine(&mut c, &Context { center_hz: 100.0005e6, tuner_hz: 100.0e6, bandwidth_hz: 1.8e3, ..Default::default() }, &x, 48e3);
        assert_eq!(c.label, "DC offset");
        let mut c = class("Carrier");
        let ctx = Context { center_hz: 920.0004e6, tuner_hz: 924e6, bandwidth_hz: 2.4e3, spur_grid_hz: Some(5e6), transfer_tone_hz: None };
        refine(&mut c, &ctx, &x, 48e3);
        assert_eq!(c.label, "Internal spur");
    }

    #[test]
    fn band_names() {
        let x = tone(48e3, 48_000, 100.0);
        let ctx = |mhz: f64, bw: f64| Context { center_hz: mhz * 1e6, tuner_hz: 0.0, bandwidth_hz: bw, ..Default::default() };
        let mut c = class("AM / OOK");
        refine(&mut c, &ctx(123.0145, 20e3), &x, 48e3);
        assert_eq!(c.label, "Aviation AM");
        let mut c = class("AM / OOK");
        refine(&mut c, &ctx(433.92, 20e3), &x, 48e3);
        assert_eq!(c.label, "ISM 433 AM / OOK");
        let mut c = class("Analog FM");
        refine(&mut c, &ctx(162.475, 12e3), &x, 48e3);
        assert_eq!(c.label, "NOAA Weather Radio");
    }
}
