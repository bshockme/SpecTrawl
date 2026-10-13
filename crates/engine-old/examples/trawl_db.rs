//! Headless classification run into the record store, for checking the
//! classifier on air without the app: one Airspy, a list of stops, every
//! record, classification and emitter filed in a SQLite file.
//!
//!     cargo run --release -p engine --example trawl_db -- <serial> <db path> <MHz:seconds>[,<MHz:seconds>...] [note]
//!
//! A stop of `lo-hi:seconds` runs the survey (sweep) over that range instead
//! of a live tuning. Afterwards, per stop, the emitters and their labels are
//! printed, and the whole run can be queried with sqlite3.

use std::time::Duration;

use engine_old::{Mode, Source, SourceConfig, Store, SweepPlan};
use sdr_core::Backend;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: trawl_db <serial> <db path> <MHz:seconds>[,...] [note]");
        std::process::exit(2);
    }
    let (serial, db, stops, note) = (&a[1], std::path::Path::new(&a[2]), &a[3], a.get(4).cloned().unwrap_or_default());
    let store = Store::open(db, &format!("trawl_db {serial} {stops} {note}")).expect("store");
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    let mut d = b.open(serial).expect("open");
    let mut first = true;
    let mut source: Option<Source> = None;
    for stop in stops.split(',') {
        let (where_, secs) = stop.split_once(':').expect("MHz:seconds");
        let secs: u64 = secs.parse().expect("seconds");
        let mode = match where_.split_once('-') {
            Some((lo, hi)) => Mode::Sweep(SweepPlan {
                start_hz: lo.parse::<f64>().unwrap() * 1e6,
                stop_hz: hi.parse::<f64>().unwrap() * 1e6,
                ..Default::default()
            }),
            None => Mode::Live,
        };
        let mhz: Option<f64> = where_.split_once('-').is_none().then(|| where_.parse().unwrap());
        if first {
            if let Some(m) = mhz {
                d.set_center(m * 1e6).unwrap();
            }
            let s = Source::start(std::mem::replace(&mut d, dummy()), SourceConfig::default(), mode).unwrap();
            s.set_store(Some(&store));
            source = Some(s);
            first = false;
        } else {
            let s = source.as_mut().unwrap();
            match (mhz, &mode) {
                (Some(m), Mode::Live) => {
                    if matches!(s.mode(), Mode::Sweep(_)) {
                        s.set_mode(Mode::Live);
                    }
                    s.set_center(m * 1e6).unwrap();
                }
                _ => s.set_mode(mode),
            }
        }
        let s = source.as_ref().unwrap();
        store.note(&format!("stop {where_} MHz for {secs} s"));
        std::thread::sleep(Duration::from_secs(secs));
        let mut em = s.emitters();
        em.retain(|e| e.confirmed);
        em.sort_by(|x, y| x.center_hz.total_cmp(&y.center_hz));
        println!("== {where_} MHz after {secs} s: {} confirmed emitters", em.len());
        for e in &em {
            let tally: Vec<String> = e.tally.iter().map(|(k, v)| format!("{k}×{v}")).collect();
            println!(
                "{:>11.4} {:>7.1} kHz recs {:>4} snr {:>3.0}  {:<28} {}  [{}]",
                e.center_hz / 1e6,
                e.bandwidth_hz / 1e3,
                e.records,
                e.peak_snr_db,
                e.label().unwrap_or_else(|| "-".into()),
                e.id_label(),
                tally.join(", ")
            );
        }
    }
    store.flush();
    println!("filed in {} (session {})", store.path().display(), store.session());
}

/// A placeholder device so the real one can move into the source.
fn dummy() -> Box<dyn sdr_core::Device> {
    let b = backend_sim::SimBackend;
    b.open(&b.enumerate().unwrap()[0].serial).unwrap()
}
