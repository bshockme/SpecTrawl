//! Minimal SigMF metadata writer, and the sample formats SpecTrawl reads back.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sdr_core::Complex32;

pub struct Meta {
    pub datatype: &'static str,
    pub sample_rate: f64,
    pub frequency: f64,
    pub datetime: Option<String>,
    pub hw: String,
    pub description: String,
    pub sample_count: u64,
}

/// Write a single-capture SigMF metadata file.
pub fn write_meta(path: &Path, m: &Meta) -> std::io::Result<()> {
    let mut capture = serde_json::json!({ "core:sample_start": 0, "core:frequency": m.frequency });
    if let Some(dt) = &m.datetime {
        capture["core:datetime"] = serde_json::Value::String(dt.clone());
    }
    let meta = serde_json::json!({
        "global": {
            "core:datatype": m.datatype,
            "core:sample_rate": m.sample_rate,
            "core:version": "1.0.0",
            "core:hw": m.hw,
            "core:recorder": "spectrawl",
            "core:description": m.description,
        },
        "captures": [capture],
        "annotations": [],
    });
    let text = serde_json::to_string_pretty(&meta).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// The IQ sample formats SpecTrawl writes: slices are `cf32_le` (lossless, since 2026-11-07), raw exports and slices
/// written before then `ci16_le`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Cf32,
    Ci16,
}

impl Format {
    pub fn from_datatype(datatype: &str) -> Option<Format> {
        match datatype {
            "cf32_le" => Some(Format::Cf32),
            "ci16_le" => Some(Format::Ci16),
            _ => None,
        }
    }

    pub fn datatype(self) -> &'static str {
        match self {
            Format::Cf32 => "cf32_le",
            Format::Ci16 => "ci16_le",
        }
    }

    /// Bytes per complex sample.
    pub fn bytes(self) -> usize {
        match self {
            Format::Cf32 => 8,
            Format::Ci16 => 4,
        }
    }

    /// The format a `.sigmf-meta` file names.
    pub fn of_meta(path: &Path) -> std::io::Result<Format> {
        let text = std::fs::read_to_string(path)?;
        let meta: serde_json::Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
        let dt = meta["global"]["core:datatype"].as_str().unwrap_or_default();
        Format::from_datatype(dt).ok_or_else(|| std::io::Error::other(format!("unsupported SigMF datatype {dt:?}")))
    }

    /// Decodes whole samples from `raw` onto `out` (a trailing partial sample is ignored).
    pub fn decode(self, raw: &[u8], out: &mut Vec<Complex32>) {
        match self {
            Format::Cf32 => out.extend(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| Complex32::new(f32::from_le_bytes([b[0], b[1], b[2], b[3]]), f32::from_le_bytes([b[4], b[5], b[6], b[7]]))),
            ),
            Format::Ci16 => out.extend(raw.as_chunks::<4>().0.iter().map(|b| {
                let (re, im) = (i16::from_le_bytes([b[0], b[1]]), i16::from_le_bytes([b[2], b[3]]));
                Complex32::new(f32::from(re) / 32767.0, f32::from(im) / 32767.0)
            })),
        }
    }

    /// Appends `x` to `out` in this format.
    pub fn encode(self, x: &[Complex32], out: &mut Vec<u8>) {
        match self {
            Format::Cf32 => {
                for c in x {
                    out.extend_from_slice(&c.re.to_le_bytes());
                    out.extend_from_slice(&c.im.to_le_bytes());
                }
            }
            Format::Ci16 => {
                for c in x {
                    for v in [c.re, c.im] {
                        out.extend_from_slice(&((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes());
                    }
                }
            }
        }
    }
}

/// `2026-10-08T21:47:04.123Z`
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, dd) = civil_from_days(days);
    format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, (rem / 60) % 60, rem % 60, d.subsec_millis())
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf32_round_trips_exactly_and_ci16_to_a_count() {
        // A weak channel's samples: well under one i16 count (1/32767) apart.
        let x: Vec<Complex32> = (0..64).map(|k| Complex32::new(1e-6 * k as f32, -3e-7 * k as f32)).collect();
        for (f, tol) in [(Format::Cf32, 0.0), (Format::Ci16, 0.5 / 32767.0)] {
            let mut raw = Vec::new();
            f.encode(&x, &mut raw);
            assert_eq!(raw.len(), x.len() * f.bytes());
            let mut y = Vec::new();
            f.decode(&raw, &mut y);
            assert_eq!(Format::from_datatype(f.datatype()), Some(f));
            assert!(x.iter().zip(&y).all(|(a, b)| (a.re - b.re).abs() <= tol && (a.im - b.im).abs() <= tol), "{f:?}");
            // What i16 does to such a channel: 64 distinct samples become four (re 0, 1 or 2 counts; im 0 or -1).
            let distinct = y.iter().map(|c| (c.re.to_bits(), c.im.to_bits())).collect::<std::collections::HashSet<_>>().len();
            assert_eq!(distinct, if f == Format::Cf32 { 64 } else { 4 }, "{f:?}");
        }
    }

    #[test]
    fn formats_known_instants() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_millis(1_791_547_624_123)), "2026-10-09T12:07:04.123Z");
    }
}
