//! Minimal SigMF metadata writer.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    fn formats_known_instants() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_millis(1_791_547_624_123)), "2026-10-09T12:07:04.123Z");
    }
}
