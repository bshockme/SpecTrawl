//! Runs a source live and reports what the pager monitors pick up: emitters
//! identified as FLEX/POCSAG, monitors running, page counts. Message text
//! only with `--show` (pager traffic is often personal or medical).
//!
//!     cargo run --release -p engine-old --example pager_check -- <serial> <MHz> <seconds> [--show]

use std::time::Duration;

use engine_old::{Mode, Source, SourceConfig};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (serial, mhz, secs) = (&a[1], a[2].parse::<f64>().unwrap(), a[3].parse::<u64>().unwrap());
    let show = a.iter().any(|x| x == "--show");
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    d.set_center(mhz * 1e6).unwrap();
    let src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
    for t in (10..=secs).step_by(10) {
        std::thread::sleep(Duration::from_secs(10));
        let pagers: Vec<String> = src
            .emitters()
            .iter()
            .filter_map(|e| {
                e.protocol_label()
                    .filter(|l| l.starts_with("FLEX") || l.starts_with("POCSAG"))
                    .map(|l| format!("{l} @ {:.4}", e.center_hz / 1e6))
            })
            .collect();
        let mons: Vec<String> =
            src.monitors().iter().map(|m| format!("{} {:.4} ({} pages)", m.label, m.center_hz / 1e6, m.messages)).collect();
        println!("{t:>4} s  pager emitters {pagers:?}  monitors {mons:?}  pages logged {}", src.pages(100_000).len());
    }
    let pages = src.pages(100_000);
    let damaged = pages.iter().filter(|p| p.damaged).count();
    println!("{} pages ({damaged} damaged), {} sends", pages.len(), pages.iter().map(|p| p.count).sum::<u32>());
    for p in pages.iter().rev() {
        let body = if show { p.text.clone() } else { format!("{} chars", p.text.chars().count()) };
        println!("  {} {:09} {:10} x{} {}{}", p.protocol, p.capcode, p.kind, p.count, if p.damaged { "[damaged] " } else { "" }, body);
    }
}
