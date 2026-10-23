//! Spec §7, enforced on `crates/engine/src`.

use std::path::{Path, PathBuf};

/// Every `*.rs` file under `CARGO_MANIFEST_DIR/src`, with its text.
/// Only the part of each file before its first `#[cfg(test)]` counts.
fn sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let p = entry.expect("a directory entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&p).expect("read a source file");
                let cut = text.find("#[cfg(test)]").map_or(text.as_str(), |i| &text[..i]).to_string();
                out.push((p, cut));
            }
        }
    }
    let mut out = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    assert!(!out.is_empty(), "no sources found");
    out
}

#[test]
fn nothing_in_the_engine_constructs_a_down_converter_or_reads_the_wall_clock() {
    for (p, s) in sources() {
        assert!(!s.contains("Ddc::new("), "{}: the channelizer in dsp is the only down-converter", p.display());
        assert!(!s.contains("SystemTime::now("), "{}: time comes from the timeline", p.display());
    }
}

#[test]
fn threads_are_created_only_by_the_keeper_the_stage_and_the_pool() {
    let allowed = ["ring/keeper.rs", "stage.rs", "channels/sched.rs"];
    for (p, s) in sources() {
        let spawns = s.contains("thread::spawn(") || s.contains("thread::Builder::new(");
        let ok = allowed.iter().any(|a| p.to_string_lossy().replace('\\', "/").ends_with(a));
        assert!(!spawns || ok, "{}: creates a thread", p.display());
    }
}
