//! Spec §7, enforced on `crates/engine/src`.

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

#[test]
fn nothing_in_the_engine_constructs_a_down_converter_or_reads_the_wall_clock() {
    for (p, s) in sources() {
        assert!(!s.contains("Ddc::new("), "{}: the channelizer in dsp is the only down-converter", p.display());
        assert!(!s.contains("SystemTime::now("), "{}: time comes from the timeline", p.display());
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
