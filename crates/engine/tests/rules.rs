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
fn threads_are_created_only_by_the_keeper_the_stage_the_pool_the_publisher_and_the_store() {
    // The store's writer is the one thread spec §4.8 adds ("written by a single store thread off a queue").
    let allowed = ["ring/keeper.rs", "stage.rs", "channels/sched.rs", "snapshot/mod.rs", "store/mod.rs"];
    for (p, s) in sources() {
        let spawns = s.contains("thread::spawn(") || s.contains("thread::Builder::new(");
        let ok = allowed.iter().any(|a| p.to_string_lossy().replace('\\', "/").ends_with(a));
        assert!(!spawns || ok, "{}: creates a thread", p.display());
    }
}
