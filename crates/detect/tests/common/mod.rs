//! Shared loaders for the golden-data tests. A missing data file fails the
//! test unless `SPECTRAWL_SKIP_CAPTURES` is set, so absence never passes
//! vacuously.

#![allow(dead_code)]

use std::path::PathBuf;

use num_complex::Complex32;

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

fn skip_requested() -> bool {
    std::env::var_os("SPECTRAWL_SKIP_CAPTURES").is_some()
}

/// The policy for an absent file: skip (None) when asked, otherwise panic.
fn absent<T>(name: &str, skip: bool) -> Option<T> {
    if skip {
        eprintln!("skipping: {name} not present (SPECTRAWL_SKIP_CAPTURES set)");
        None
    } else {
        panic!(
            "capture {name} missing: run tools/fetch_captures (see testdata/captures/MANIFEST.toml) or set SPECTRAWL_SKIP_CAPTURES=1 to skip locally"
        );
    }
}

/// A cf32 SigMF capture and its sample rate: `testdata/small/<name>` if
/// present, else `testdata/captures/<name>`.
pub fn capture(name: &str) -> Option<(Vec<Complex32>, f64)> {
    capture_with(name, skip_requested())
}

pub fn capture_with(name: &str, skip: bool) -> Option<(Vec<Complex32>, f64)> {
    let small = testdata().join("small").join(name);
    let stem =
        if PathBuf::from(format!("{}.sigmf-meta", small.display())).exists() { small } else { testdata().join("captures").join(name) };
    // Names contain dots (769.5782MHz…), so append rather than with_extension.
    let Ok(meta) = std::fs::read_to_string(format!("{}.sigmf-meta", stem.display())) else { return absent(name, skip) };
    let Ok(raw) = std::fs::read(format!("{}.sigmf-data", stem.display())) else { return absent(name, skip) };
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or_else(|e| panic!("{name}: bad sigmf-meta: {e}"));
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap_or_else(|| panic!("{name}: no core:sample_rate"));
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    Some((x, fs))
}

/// The bytes of `testdata/spectra/<name>.<ext>`.
pub fn spectrum_file(name: &str, ext: &str) -> Option<Vec<u8>> {
    spectrum_file_with(name, ext, skip_requested())
}

pub fn spectrum_file_with(name: &str, ext: &str, skip: bool) -> Option<Vec<u8>> {
    match std::fs::read(testdata().join("spectra").join(format!("{name}.{ext}"))) {
        Ok(b) => Some(b),
        Err(_) => absent(&format!("{name}.{ext}"), skip),
    }
}

/// The JSON text of `testdata/spectra/<name>.json`.
pub fn spectrum(name: &str) -> Option<String> {
    spectrum_file(name, "json").map(|b| String::from_utf8(b).unwrap_or_else(|e| panic!("{name}.json: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "capture no_such_capture missing")]
    fn missing_capture_panics_when_not_skipping() {
        let _ = super::capture_with("no_such_capture", false);
    }

    #[test]
    fn missing_capture_skips_when_asked() {
        assert!(super::capture_with("no_such_capture", true).is_none());
    }
}
