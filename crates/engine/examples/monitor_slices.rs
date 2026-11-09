//! Runs recorded FLEX or POCSAG slices through a pager monitor block by block, as the decoder bank feeds it, and
//! compares its pages with decoding each slice whole. Prints counts only: page text is pager traffic.
//!
//!     cargo run --release -p engine --example monitor_slices -- FLEX <path without extension>...
//!
//! With `ONE_CHANNEL=1` the slices (one channel's, oldest first) go through one monitor, as a live channel would, so
//! long messages can join across slices.

use engine::decoders::Decoder;
use engine::decoders::monitor::Monitor;
use sdr_core::Complex32;

fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let protocol: &'static str = match args.next().as_deref() {
        Some("FLEX") => "FLEX",
        Some("POCSAG") => "POCSAG",
        _ => panic!("usage: monitor_slices FLEX|POCSAG <stem>..."),
    };
    let one = std::env::var_os("ONE_CHANNEL").is_some();
    let (mut whole_pages, mut whole_damaged, mut mon_pages, mut mon_damaged) = (0, 0, 0, 0);
    let (mut whole_frag, mut mon_frag) = (0, 0);
    let (mut joined_total, mut same_total, mut near_total, mut other_total) = (0, 0, 0, 0);
    let mut shared = Monitor::new(protocol, protocol, 0.0, 25e3);
    let mut configured = false;
    for stem in args {
        let (x, fs) = load(&stem);
        let whole: Vec<detect::flex::Page> =
            if protocol == "FLEX" { detect::flex::decode(&x, fs).into_iter().flat_map(|f| f.pages).collect() } else { Vec::new() };
        let mut own = Monitor::new(protocol, protocol, 0.0, 25e3);
        let m = if one { &mut shared } else { &mut own };
        if !one || !configured {
            m.configure(fs);
            configured = true;
        }
        // The bus delivers about 10 ms at a time. A live channel carries on after the transmission, so the slice is
        // followed by silence (`PAD_SECS`, 20 s): every block a frame starts in has a successor, as on air, and a
        // message whose last fragments the slice cut off is given up and shown.
        let pad: f64 = std::env::var("PAD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0);
        let mut fed = x.clone();
        fed.extend(std::iter::repeat_n(Complex32::new(0.0, 0.0), (pad * fs) as usize));
        for chunk in fed.chunks((fs / 100.0) as usize) {
            m.feed(chunk);
        }
        if one {
            continue;
        }
        let lines = m.recent();
        let d = lines.iter().filter(|l| l.contains("[damaged]")).count();
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        // How the two agree, line for line: the monitor's lines that are exactly a whole-decode page, and those that
        // are not but share a capcode with one (another copy, cut or joined differently).
        let whole_lines: Vec<String> = detect::flex_join::join_frames(&detect::flex::decode(&x, fs)).iter().map(|p| p.summary()).collect();
        let same = lines.iter().filter(|l| whole_lines.contains(l)).count();
        let capcode = |l: &str| l.split(' ').next().unwrap_or("").to_string();
        let near = lines.iter().filter(|l| !whole_lines.contains(l) && whole_lines.iter().any(|w| capcode(w) == capcode(l))).count();
        println!(
            "{name}: {:.1} s; whole {} pages ({} damaged), {} joined; monitor {} pages ({d} damaged), {same} as joined, {near} same capcode",
            x.len() as f64 / fs,
            whole.len(),
            whole.iter().filter(|p| p.damaged).count(),
            whole_lines.len(),
            lines.len()
        );
        whole_pages += whole.len();
        whole_damaged += whole.iter().filter(|p| p.damaged).count();
        mon_pages += lines.len();
        mon_damaged += d;
        joined_total += whole_lines.len();
        same_total += same;
        near_total += near;
        other_total += lines.len() - same - near;
        whole_frag += whole.iter().filter(|p| p.continued || p.fragment != 3).count();
        mon_frag += lines.iter().filter(|l| is_fragment(l)).count();
    }
    if one {
        let lines: Vec<String> = shared.pages().cloned().collect();
        let d = lines.iter().filter(|l| l.contains("[damaged]")).count();
        println!(
            "one channel: {} pages kept of {} ({d} of the kept damaged, {} fragments)",
            lines.len(),
            shared.summary().split(": ").nth(1).unwrap_or(""),
            lines.iter().filter(|l| is_fragment(l)).count()
        );
        return;
    }
    println!("fragments: whole decode {whole_frag}, monitor {mon_frag}");
    println!(
        "lines: whole decode joined {joined_total}; monitor {same_total} the same, {near_total} differ (same capcode), {other_total} not in the whole decode"
    );
    println!("total: whole {whole_pages} pages ({whole_damaged} damaged); monitor {mon_pages} pages ({mon_damaged} damaged)");
}

/// A page line that is not a whole message: still to be continued, or a continuation whose start was lost.
fn is_fragment(line: &str) -> bool {
    line.contains("[continued]") || line.split_once(": ").is_some_and(|(_, t)| t.starts_with('…'))
}
