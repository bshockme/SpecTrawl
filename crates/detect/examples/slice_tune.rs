//! Tunes symbol slicing on real captures: pre-filter cutoff × smoothing ×
//! timing refinement, scored by what decodes (DMR burst checks, slot-type
//! and P25 NID bit errors).
//!
//!     cargo run --release -p detect --example slice_tune -- <path without extension>...

use detect::frames::SliceOpts;
use detect::{decode, dmr};
use num_complex::Complex32;

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
        println!("{name}");
        println!("  variant              |  DMR data frames  slot errs/frame  bursts ok | P25 NIDs  errs/NID | NXDN");
        let base = SliceOpts { smooth: 0.75, refine: true, ..Default::default() };
        let variants: Vec<(&str, f64, SliceOpts)> = vec![
            ("plain 6.5k/0.5", 6.5e3, SliceOpts::plain()),
            ("default 4k", 4.0e3, SliceOpts::default()),
            ("4k 0.75 refine", 4.0e3, base),
            ("3.5k 0.75 refine", 3.5e3, base),
            ("4k +lpf.625", 4.0e3, SliceOpts { post_lpf: Some(0.625), ..base }),
            ("6.5k +lpf.625", 6.5e3, SliceOpts { post_lpf: Some(0.625), ..base }),
            ("6.5k +lpf.625 sm1", 6.5e3, SliceOpts { post_lpf: Some(0.625), smooth: 1.0, ..base }),
            ("4k +robust", 4.0e3, SliceOpts { robust_levels: true, ..base }),
            ("4k +dc", 4.0e3, SliceOpts { dc_track: true, ..base }),
            ("4k +isi", 4.0e3, SliceOpts { isi_boost: true, ..base }),
            ("4k all", 4.0e3, SliceOpts { post_lpf: Some(0.625), robust_levels: true, dc_track: true, isi_boost: true, ..base }),
            ("6.5k all", 6.5e3, SliceOpts { post_lpf: Some(0.625), robust_levels: true, dc_track: true, isi_boost: true, ..base }),
        ];
        {
            for (label, cutoff, opts) in variants {
                {
                    let fr = detect::frames_with(&x, fs, cutoff, 54, 80, opts);
                    let (mut dmr_n, mut slot_errs, mut ok) = (0, 0u32, 0);
                    let (mut nids, mut nid_errs) = (0, 0u32);
                    let mut nxdn = 0;
                    for f in &fr {
                        match f.protocol {
                            "DMR" if f.detail.contains("data") => {
                                if let Some(st) = decode::dmr_slot_type(f) {
                                    dmr_n += 1;
                                    slot_errs += st.errors;
                                    ok += dmr::decode_burst(f, st.data_type).is_some() as usize;
                                }
                            }
                            "P25" if f.detail == "Phase 1" => {
                                if let Some(n) = decode::p25_nid(f) {
                                    nids += 1;
                                    nid_errs += n.errors;
                                }
                            }
                            "NXDN" => nxdn += 1,
                            _ => {}
                        }
                    }
                    println!(
                        "  {:<20} | {:>6} {:>14.2} {:>10} | {:>8} {:>9.2} | {:>4}",
                        label,
                        dmr_n,
                        slot_errs as f32 / dmr_n.max(1) as f32,
                        ok,
                        nids,
                        nid_errs as f32 / nids.max(1) as f32,
                        nxdn
                    );
                }
            }
        }
    }
}
