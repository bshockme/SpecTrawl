//! Ground truth for the detectors: tunes an Airspy through a list of centres
//! (or sweeps a range) and saves, per tuning, the mean and max-hold spectrum,
//! the emitter list with labels, and channel IQ (SigMF) of the strongest
//! emitters.
//!
//!     cargo run --release -p engine --example trawl -- live <serial> <secs> <out-dir> <MHz>...
//!     cargo run --release -p engine --example trawl -- sweep <serial> <lo MHz> <hi MHz> <secs> <out-dir>
//!
//! Spectra are written as `<stem>.spec` (little-endian f32: mean dB, then
//! max dB, `bins` each) with a `<stem>.json` describing the axis.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::sigmf::{ExportKind, ExportRequest};
use engine::{Mode, Source, SourceConfig, SpectrumRow, SweepPlan};
use sdr_core::Backend;

/// Channel captures per tuning (strongest confirmed emitters first).
const MAX_CAPTURES: usize = 10;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let b = backend_airspy::AirspyBackend::new();
    let _ = b.enumerate();
    match a[1].as_str() {
        "live" => {
            let (serial, secs, out) = (&a[2], a[3].parse::<f64>().unwrap(), PathBuf::from(&a[4]));
            let centres: Vec<f64> = a[5..].iter().map(|v| v.parse::<f64>().unwrap() * 1e6).collect();
            let mut d = b.open(serial).expect("open");
            d.set_center(centres[0]).unwrap();
            let mut src = Source::start(d, SourceConfig::default(), Mode::Live).unwrap();
            for c in centres {
                src.set_center(c).unwrap();
                // Let the detector's floor settle and old rows drain.
                std::thread::sleep(Duration::from_millis(1500));
                let seq0 = src.rows_after(None).last().map(|r| r.seq);
                let head0 = src.ring_head();
                std::thread::sleep(Duration::from_secs_f64(secs));
                let rows: Vec<_> = src.rows_after(seq0).into_iter().filter(|r| (r.tuning.center_hz - c).abs() < 1.0).collect();
                let stem = format!("live_{:.3}MHz", c / 1e6);
                save_spectrum(&out, &stem, &rows);
                save_emitters(&src, &out, &stem, c, head0);
            }
        }
        "sweep" => {
            let (serial, lo, hi, secs, out) = (
                &a[2],
                a[3].parse::<f64>().unwrap() * 1e6,
                a[4].parse::<f64>().unwrap() * 1e6,
                a[5].parse::<f64>().unwrap(),
                PathBuf::from(&a[6]),
            );
            let d = b.open(serial).expect("open");
            let src = Source::start(d, SourceConfig::default(), Mode::Sweep(SweepPlan { start_hz: lo, stop_hz: hi, ..Default::default() }))
                .unwrap();
            std::thread::sleep(Duration::from_secs_f64(secs));
            let rows = src.rows_after(None);
            // Skip the first pass or two (gain/floor settling).
            let rows = &rows[rows.len().min(2)..];
            save_spectrum(&out, &format!("sweep_{:.0}-{:.0}MHz", lo / 1e6, hi / 1e6), rows);
        }
        m => panic!("unknown mode {m}"),
    }
}

fn save_spectrum(out: &Path, stem: &str, rows: &[std::sync::Arc<SpectrumRow>]) {
    std::fs::create_dir_all(out).unwrap();
    let Some(first) = rows.first() else {
        println!("{stem}: no rows");
        return;
    };
    let n = first.db.len();
    let rows: Vec<_> = rows.iter().filter(|r| r.db.len() == n).collect();
    let mut mean = vec![0.0f64; n];
    let mut max = vec![f32::MIN; n];
    for r in &rows {
        for i in 0..n {
            mean[i] += 10f64.powf(r.db[i] as f64 / 10.0);
            max[i] = max[i].max(r.db[i]);
        }
    }
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(format!("{stem}.spec"))).unwrap());
    for m in &mean {
        f.write_all(&((10.0 * (m / rows.len() as f64).log10()) as f32).to_le_bytes()).unwrap();
    }
    for m in &max {
        f.write_all(&m.to_le_bytes()).unwrap();
    }
    let t = first.tuning;
    let meta = serde_json::json!({
        "center_hz": t.center_hz, "span_hz": t.span_hz, "bins": n, "rows": rows.len(),
        "bin_hz": t.span_hz / n as f64,
    });
    std::fs::write(out.join(format!("{stem}.json")), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    println!("{stem}: {} rows, {n} bins of {:.0} Hz", rows.len(), t.span_hz / n as f64);
}

fn save_emitters(src: &Source, out: &Path, stem: &str, centre: f64, since_sample: u64) {
    let span = src.snapshot().sample_rate * 0.5;
    let mut em: Vec<_> = src.emitters().into_iter().filter(|e| (e.center_hz - centre).abs() < span && e.confirmed).collect();
    em.sort_by(|x, y| x.center_hz.total_cmp(&y.center_hz));
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(format!("{stem}_emitters.csv"))).unwrap());
    writeln!(f, "center_mhz,bw_khz,records,duty,peak_snr_db,label,id,activity,tally").unwrap();
    for e in &em {
        let tally: Vec<String> = e.tally.iter().map(|(k, n)| format!("{k}x{n}")).collect();
        writeln!(
            f,
            "{:.6},{:.2},{},{:.3},{:.1},\"{}\",\"{}\",\"{}\",\"{}\"",
            e.center_hz / 1e6,
            e.bandwidth_hz / 1e3,
            e.records,
            e.duty(),
            e.peak_snr_db,
            e.label().unwrap_or_default(),
            e.id().unwrap_or(""),
            match (e.activity(), &e.last_talk) {
                (Some(a), Some(t)) => format!("{} {t}", a.name()),
                (Some(a), None) => a.name().to_string(),
                _ => String::new(),
            },
            tally.join("; ")
        )
        .unwrap();
    }
    drop(f);
    println!("  {} emitters", em.len());
    // Strongest first; skip ones too wide for a channel capture.
    em.retain(|e| e.bandwidth_hz < 300e3);
    em.sort_by(|x, y| y.peak_snr_db.total_cmp(&x.peak_snr_db));
    let (s, e) = src.ring_range();
    for em in em.iter().take(MAX_CAPTURES) {
        let label = em.label().unwrap_or_else(|| "unlabelled".into());
        let cstem = engine::sigmf::sanitize(&format!("{stem}_{:.4}MHz_{label}", em.center_hz / 1e6));
        let r = src
            .export(ExportRequest {
                start_sample: s.max(since_sample),
                end_sample: e,
                kind: ExportKind::Channel { center_hz: em.center_hz, bandwidth_hz: (em.bandwidth_hz * 1.5).max(12.5e3) },
                dir: out.join("iq"),
                stem: cstem.clone(),
                description: format!("{label} via SpecTrawl trawl"),
                annotations: vec![],
            })
            .join()
            .unwrap();
        match r {
            Ok(r) => println!("  {cstem}: {:.1} s", r.seconds),
            Err(e) => println!("  {cstem}: {e}"),
        }
    }
}
