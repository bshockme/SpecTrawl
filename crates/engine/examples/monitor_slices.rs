//! Runs recorded FLEX or POCSAG slices through a pager monitor block by block, as the decoder bank feeds it, and
//! compares its pages with decoding each slice whole. Prints counts only: page text is pager traffic.
//!
//!     cargo run --release -p engine --example monitor_slices -- FLEX <path without extension>...

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
    let (mut whole_pages, mut whole_damaged, mut mon_pages, mut mon_damaged) = (0, 0, 0, 0);
    for stem in args {
        let (x, fs) = load(&stem);
        let whole: Vec<detect::flex::Page> =
            if protocol == "FLEX" { detect::flex::decode(&x, fs).into_iter().flat_map(|f| f.pages).collect() } else { Vec::new() };
        let mut m = Monitor::new(protocol, protocol, 0.0, 15e3);
        m.configure(fs);
        // The bus delivers about 10 ms at a time. A live channel carries on after the transmission, so the slice is
        // followed by 4 s of silence: every block a frame starts in has a successor, as on air.
        let mut fed = x.clone();
        fed.extend(std::iter::repeat_n(Complex32::new(0.0, 0.0), (4.0 * fs) as usize));
        for chunk in fed.chunks((fs / 100.0) as usize) {
            m.feed(chunk);
        }
        let lines = m.recent();
        let d = lines.iter().filter(|l| l.contains("[damaged]")).count();
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        println!(
            "{name}: {:.1} s; whole {} pages ({} damaged); monitor {} pages ({d} damaged)",
            x.len() as f64 / fs,
            whole.len(),
            whole.iter().filter(|p| p.damaged).count(),
            lines.len()
        );
        whole_pages += whole.len();
        whole_damaged += whole.iter().filter(|p| p.damaged).count();
        mon_pages += lines.len();
        mon_damaged += d;
    }
    println!("total: whole {whole_pages} pages ({whole_damaged} damaged); monitor {mon_pages} pages ({mon_damaged} damaged)");
}
