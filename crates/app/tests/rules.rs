//! The app's rules, enforced on `crates/app/src`: it renders snapshots and sends commands, nothing more.

use std::path::{Path, PathBuf};

/// Every `*.rs` file under `CARGO_MANIFEST_DIR/src`, with its text.
/// Only the part of each file before its `#[cfg(test)] mod tests` counts
/// (a `#[cfg(test)]` on a single helper does not end the production code).
fn sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let p = entry.expect("a directory entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&p).expect("read a source file");
                let cut = test_module_start(&text).map_or(text.as_str(), |i| &text[..i]).to_string();
                out.push((p, cut));
            }
        }
    }
    let mut out = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    assert!(!out.is_empty(), "no sources found");
    out
}

/// Where the first `#[cfg(test)]` immediately followed by `mod tests` starts
/// (only whitespace, `\n` or `\r\n`, between them).
fn test_module_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = text[from..].find("#[cfg(test)]") {
        let at = from + i;
        let rest = &text[at + "#[cfg(test)]".len()..];
        if rest.trim_start().starts_with("mod tests") {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

#[test]
fn the_app_creates_no_thread_takes_no_lock_and_reads_no_clock() {
    for (p, s) in sources() {
        for banned in [
            "thread::spawn(",
            "thread::Builder::new(",
            "Mutex",
            "RwLock",
            "parking_lot",
            ".lock()",
            "SystemTime::now(",
            "Instant::now(",
            "thread::sleep(",
        ] {
            assert!(!s.contains(banned), "{}: contains `{banned}`", p.display());
        }
    }
}

#[test]
fn the_app_reaches_the_engine_only_to_spawn_it() {
    for (p, s) in sources() {
        let is_lib = p.file_name().is_some_and(|f| f == "lib.rs");
        assert!(is_lib || !s.contains("engine::"), "{}: uses engine types; the app reads api only", p.display());
    }
}
