//! Imports a DSD+ folder and looks up a few P25 names.
//!
//!     cargo run --release -p catalog --example import_check -- <DSD+ folder>

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("folder"));
    let mut c = catalog::Catalog::default();
    let t = std::time::Instant::now();
    let s = c.import_dsdplus_dir(&dir).expect("import");
    println!("imported {s} in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
    let (w, sys) = (Some(0xBEE00), Some(0x2A5));
    for tg in [48001u16, 48015, 47018, 10032, 7502, 16027, 10730] {
        println!("  TG {tg:>5}: {}", c.p25_group(w, sys, None, tg).map(|g| g.name()).unwrap_or("-"));
    }
    for (r, s) in [(2u8, 21u8), (2, 20), (2, 27), (3, 10)] {
        println!("  site {r}.{s}: {}", c.p25_site(0xBEE00, 0x2A5, r, s).unwrap_or("-"));
    }
}
