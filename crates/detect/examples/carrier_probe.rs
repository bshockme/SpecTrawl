//! Measures what tells a carrier apart from a spur or a noise spike, on cf32
//! SigMF channel captures: line width, frequency drift, amplitude stability,
//! on/off duty, AM depth, FM deviation and sub-audible tones.
//!
//!     cargo run --release -p detect --example carrier_probe -- <path without extension>...

use num_complex::Complex32;
use rustfft::FftPlanner;

fn load(stem: &str) -> (Vec<Complex32>, f64) {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{stem}.sigmf-meta")).unwrap()).unwrap();
    let fs = meta["global"]["core:sample_rate"].as_f64().unwrap();
    let raw = std::fs::read(format!("{stem}.sigmf-data")).unwrap();
    let x = raw
        .chunks_exact(8)
        .map(|c| Complex32::new(f32::from_le_bytes(c[0..4].try_into().unwrap()), f32::from_le_bytes(c[4..8].try_into().unwrap())))
        .collect();
    (x, fs)
}

fn main() {
    for stem in std::env::args().skip(1) {
        let (x, fs) = load(&stem);
        let name = std::path::Path::new(&stem).file_name().unwrap().to_string_lossy().to_string();
        let f = detect::carrier::measure(&x, fs);
        println!("{name}");
        println!("  {f:.3?}");
        println!("  -> {}", detect::carrier::verdict(&f).name());
        let _ = FftPlanner::<f32>::new();
    }
}
