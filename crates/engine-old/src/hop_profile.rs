//! Saved hop profiles. The sweep estimates each hop's common response
//! (passband ripple, fixed-offset spurs) live, as the median across hops
//! smoothed over passes; it takes a few passes to settle. The settled
//! profile is kept per device, FFT size, usable bins and gain under
//! `Documents/SpecTrawl/profiles`, and a later sweep with the same key starts
//! from it, so its first row is already corrected.

use std::path::PathBuf;

use sdr_core::GainSetting;

/// Bump when the file layout changes; older files are ignored.
pub const VERSION: u32 = 1;

/// Where the profiles live.
pub fn dir() -> PathBuf {
    crate::sigmf::default_dir().parent().map(|p| p.join("profiles")).unwrap_or_else(|| "profiles".into())
}

/// The gain part of a profile's key: the same setting gives the same
/// passband shape, a different one need not.
pub fn gain_key(g: Option<GainSetting>) -> String {
    match g {
        None => "default".into(),
        Some(GainSetting::Auto) => "agc".into(),
        Some(GainSetting::Linearity(v)) => format!("lin{:02}", (v.clamp(0.0, 1.0) * 100.0).round() as u32),
        Some(GainSetting::Sensitivity(v)) => format!("sens{:02}", (v.clamp(0.0, 1.0) * 100.0).round() as u32),
        Some(GainSetting::Manual { lna, mixer, vga }) => format!("man{lna}-{mixer}-{vga}"),
    }
}

/// `profiles/<serial>_<fft>_<bins>_<gainkey>.json`.
pub fn path(serial: &str, fft: usize, bins: usize, gain_key: &str) -> PathBuf {
    let safe: String = serial.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    dir().join(format!("{safe}_{fft}_{bins}_{gain_key}.json"))
}

/// Loads a profile of exactly `bins` values, if a current-version file is there.
pub fn load(path: &std::path::Path, bins: usize) -> Option<Vec<f32>> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.get("version").and_then(|x| x.as_u64()) != Some(VERSION as u64) {
        return None;
    }
    let db: Vec<f32> = v.get("db")?.as_array()?.iter().map(|x| x.as_f64().map(|f| f as f32)).collect::<Option<_>>()?;
    if db.len() != bins || db.iter().any(|x| !x.is_finite()) {
        return None;
    }
    Some(db)
}

/// Writes a profile; `passes` is how many passes refined it, for the record.
pub fn save(path: &std::path::Path, db: &[f32], passes: u32) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    // Two decimals of dB keep the file small (a 3.6 k-bin profile ≈ 25 kB).
    let rounded: Vec<serde_json::Value> = db.iter().map(|x| serde_json::json!((*x * 100.0).round() / 100.0)).collect();
    let v = serde_json::json!({
        "version": VERSION,
        "passes": passes,
        "saved_unix": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "db": rounded,
    });
    std::fs::write(path, v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_keys() {
        assert_eq!(gain_key(None), "default");
        assert_eq!(gain_key(Some(GainSetting::Auto)), "agc");
        assert_eq!(gain_key(Some(GainSetting::Linearity(13.0 / 21.0))), "lin62");
        assert_eq!(gain_key(Some(GainSetting::Sensitivity(1.0))), "sens100");
    }

    #[test]
    fn file_round_trip_and_rejections() {
        let dir = std::env::temp_dir().join(format!("spectrawl_profile_{}", std::process::id()));
        let p = dir.join("x.json");
        let db: Vec<f32> = (0..64).map(|i| (i as f32 * 0.37).sin() * 3.0).collect();
        save(&p, &db, 20).unwrap();
        let back = load(&p, 64).unwrap();
        for (a, b) in db.iter().zip(&back) {
            assert!((a - b).abs() <= 0.006, "{a} vs {b}");
        }
        assert!(load(&p, 63).is_none(), "wrong bin count");
        std::fs::write(&p, r#"{"version":0,"db":[0,0]}"#).unwrap();
        assert!(load(&p, 2).is_none(), "old version");
        assert!(load(&dir.join("missing.json"), 64).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_is_per_serial_fft_bins_gain() {
        let p = path("A1B2:C3", 4096, 3686, "lin62");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "A1B2_C3_4096_3686_lin62.json");
        assert!(p.parent().unwrap().ends_with("profiles"));
    }
}
