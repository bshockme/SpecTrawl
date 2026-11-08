//! Spec §7, enforced on `crates/engine/src` (rewritten with the owner 2026-11-06: each rule names the failure it
//! guards against, and checks code, not comments).

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
fn the_cut_skips_a_test_only_helper_and_tolerates_crlf() {
    let text = "a\r\n#[cfg(test)]\r\nfn helper() {}\r\nb\r\n#[cfg(test)]\r\nmod tests {}\r\n";
    assert_eq!(&text[..test_module_start(text).unwrap()], "a\r\n#[cfg(test)]\r\nfn helper() {}\r\nb\r\n");
    assert_eq!(test_module_start("fn f() {}\n"), None);
}

/// `text` without its `//` comments (line and doc comments), so a rule checks code, not prose about it.
fn code(text: &str) -> String {
    text.lines().map(|l| l.split_once("//").map_or(l, |(c, _)| c)).collect::<Vec<_>>().join("\n")
}

/// The path relative to `src/`, with forward slashes.
fn rel(p: &Path) -> String {
    let p = p.to_string_lossy().replace('\\', "/");
    p.split_once("/src/").map_or(p.clone(), |(_, r)| r.to_string())
}

#[test]
fn only_the_bus_the_sweep_and_the_export_read_the_ring() {
    // Spec §7 (owner, 2026-11-06): the first build failed because several features each read the full-rate stream
    // and down-converted it privately. The ring is read by the bus's stage (one FFT and channelizer for every
    // channel), the sweep (one FFT per hop) and the raw IQ export; everything else takes a channel from the bus, which
    // it may then re-centre, filter or resample as its job needs.
    for (p, s) in sources() {
        let r = rel(&p);
        let allowed = matches!(r.as_str(), "source.rs" | "stage.rs" | "export.rs") || r.starts_with("sweep/");
        let s = code(&s);
        for call in ["read_ring(", "read_ring_raw(", ".ring()"] {
            assert!(allowed || !s.contains(call), "{r}: reads the ring (`{call}`); take a channel from the bus");
        }
    }
}

#[test]
fn sample_times_come_from_the_timeline() {
    // Spec §7 (owner, 2026-11-06): a record's, a row's or a slice's time is its samples' time, from the timeline's
    // anchors, so it agrees across receivers and with replay, and the simulator may run faster than real time. Times
    // that are about the wall clock (when a note was written, a log line) are not covered.
    for (p, s) in sources() {
        let r = rel(&p);
        let sample_path = [
            "records/",
            "channels/",
            "rows.rs",
            "stage.rs",
            "timeline.rs",
            "segments.rs",
            "recorder.rs",
            "sigmf.rs",
            "export.rs",
            "replay.rs",
            "sweep/",
        ]
        .iter()
        .any(|m| r == *m || (m.ends_with('/') && r.starts_with(m)));
        assert!(!sample_path || !code(&s).contains("SystemTime::now("), "{r}: stamps samples with the wall clock; use the timeline");
    }
}

#[test]
fn only_the_source_holds_the_device_or_takes_its_stream() {
    // Spec §7: only the backend's driver thread reads the radio, and its samples reach the
    // rest of the engine through the ring. `engine.rs` only hands a device to `Source::start`.
    for (p, s) in sources() {
        let path = p.to_string_lossy().replace('\\', "/");
        let source = path.ends_with("src/source.rs");
        assert!(source || !s.contains("StreamSink for"), "{}: takes a device's stream", p.display());
        let holds = s.contains("dyn Device");
        assert!(!holds || source || path.ends_with("src/engine.rs"), "{}: holds a device", p.display());
    }
}
