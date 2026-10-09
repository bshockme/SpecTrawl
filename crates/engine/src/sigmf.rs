//! SigMF export from the look-back ring (DESIGN.md §5.1, §6).
//!
//! Two kinds:
//! - `Wideband`: the raw ring samples (ci16_le) at the device rate.
//! - `Channel`: one signal down-converted to complex baseband (cf32_le).
//!
//! Exports run on their own thread; the ring keeps filling meanwhile, so data
//! is read oldest-first and the export fails cleanly if it falls behind.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use detect::Ddc;
use sdr_core::Complex32;

use crate::Shared;

#[derive(Debug, Clone)]
pub enum ExportKind {
    Wideband,
    Channel { center_hz: f64, bandwidth_hz: f64 },
}

/// A region of interest written into the metadata, positioned in ring samples.
#[derive(Debug, Clone)]
pub struct ExportAnnotation {
    pub start_sample: u64,
    pub end_sample: u64,
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    pub label: String,
    pub comment: String,
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub start_sample: u64,
    pub end_sample: u64,
    pub kind: ExportKind,
    pub dir: PathBuf,
    /// File stem (no extension); `.sigmf-data` / `.sigmf-meta` are added.
    pub stem: String,
    pub description: String,
    pub annotations: Vec<ExportAnnotation>,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub data_path: PathBuf,
    pub meta_path: PathBuf,
    pub samples: u64,
    pub bytes: u64,
    pub seconds: f64,
}

pub(crate) struct ExportContext {
    pub shared: Arc<Shared>,
    pub sample_rate: f64,
    pub tuner_center_hz: f64,
    pub hw: String,
}

const CHUNK: usize = 1 << 18;

pub(crate) fn run(ctx: ExportContext, req: ExportRequest) -> Result<ExportResult, String> {
    std::fs::create_dir_all(&req.dir).map_err(|e| format!("create {}: {e}", req.dir.display()))?;
    let data_path = req.dir.join(format!("{}.sigmf-data", req.stem));
    let meta_path = req.dir.join(format!("{}.sigmf-meta", req.stem));
    let file = std::fs::File::create(&data_path).map_err(|e| format!("create {}: {e}", data_path.display()))?;
    let mut w = BufWriter::with_capacity(1 << 22, file);

    let (oldest, head) = {
        let r = ctx.shared.ring.lock();
        (r.oldest(), r.head())
    };
    let start = req.start_sample.max(oldest);
    let end = req.end_sample.min(head);
    if end <= start {
        return Err("that stretch of IQ is no longer in the look-back buffer".into());
    }
    let start_time = ctx.shared.state.lock().time_of(start, ctx.sample_rate);

    let (datatype, out_rate, out_center, decim, mut written) = match &req.kind {
        ExportKind::Wideband => {
            let mut raw = vec![[0i16; 2]; CHUNK];
            let mut pos = start;
            while pos < end {
                let n = ((end - pos) as usize).min(CHUNK);
                if !ctx.shared.ring.lock().read_raw(pos, &mut raw[..n]) {
                    return Err("export fell behind the look-back buffer (disk too slow?)".into());
                }
                for s in &raw[..n] {
                    w.write_all(&s[0].to_le_bytes()).and_then(|_| w.write_all(&s[1].to_le_bytes())).map_err(|e| e.to_string())?;
                }
                pos += n as u64;
            }
            ("ci16_le", ctx.sample_rate, ctx.tuner_center_hz, 1.0, end - start)
        }
        ExportKind::Channel { center_hz, bandwidth_hz } => {
            let bw = bandwidth_hz.max(5e3);
            let mut ddc = Ddc::new(ctx.sample_rate, center_hz - ctx.tuner_center_hz, bw * 0.65 + 1e3, (bw * 2.5).max(48e3));
            let mut buf = vec![Complex32::default(); CHUNK];
            let mut out = Vec::with_capacity(CHUNK / 8);
            let mut count = 0u64;
            let mut pos = start;
            while pos < end {
                let n = ((end - pos) as usize).min(CHUNK);
                if !ctx.shared.ring.lock().read(pos, &mut buf[..n]) {
                    return Err("export fell behind the look-back buffer".into());
                }
                out.clear();
                ddc.process(&buf[..n], &mut out);
                for c in &out {
                    w.write_all(&c.re.to_le_bytes()).and_then(|_| w.write_all(&c.im.to_le_bytes())).map_err(|e| e.to_string())?;
                }
                count += out.len() as u64;
                pos += n as u64;
            }
            ("cf32_le", ddc.fs_out, *center_hz, ctx.sample_rate / ddc.fs_out, count)
        }
    };
    w.flush().map_err(|e| e.to_string())?;
    drop(w);
    if written == 0 {
        written = 1;
    }

    // Annotations are positioned in output samples.
    let annotations: Vec<serde_json::Value> = req
        .annotations
        .iter()
        .filter(|a| a.end_sample > start && a.start_sample < end)
        .map(|a| {
            let s = (a.start_sample.max(start) - start) as f64 / decim;
            let e = (a.end_sample.min(end) - start) as f64 / decim;
            serde_json::json!({
                "core:sample_start": s as u64,
                "core:sample_count": ((e - s).max(1.0)) as u64,
                "core:freq_lower_edge": a.f_lo_hz,
                "core:freq_upper_edge": a.f_hi_hz,
                "core:label": a.label,
                "core:comment": a.comment,
            })
        })
        .collect();
    let meta = serde_json::json!({
        "global": {
            "core:version": "1.2.0",
            "core:datatype": datatype,
            "core:sample_rate": out_rate,
            "core:hw": ctx.hw,
            "core:recorder": "SpecTrawl",
            "core:description": req.description,
        },
        "captures": [{
            "core:sample_start": 0,
            "core:frequency": out_center,
            "core:datetime": rfc3339(start_time),
        }],
        "annotations": annotations,
    });
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).map_err(|e| format!("write {}: {e}", meta_path.display()))?;
    let bytes = std::fs::metadata(&data_path).map(|m| m.len()).unwrap_or(0);
    Ok(ExportResult { data_path, meta_path, samples: written, bytes, seconds: (end - start) as f64 / ctx.sample_rate })
}

/// `2026-10-08T21:47:04.123Z`
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, dd) = civil_from_days(days);
    format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, (rem / 60) % 60, rem % 60, d.subsec_millis())
}

/// `20261008_214704` for file names.
pub fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
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

/// Default folder for captures: `<Documents>/SpecTrawl/captures`.
pub fn default_dir() -> PathBuf {
    let home =
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("SpecTrawl").join("captures")
}

/// Makes a string safe for a file name.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_734), (2026, 10, 8));
        let t = UNIX_EPOCH + Duration::from_millis(1_791_496_024_123);
        assert_eq!(rfc3339(t), "2026-10-08T21:47:04.123Z");
        assert_eq!(stamp(t), "20261008_214704");
    }
}
