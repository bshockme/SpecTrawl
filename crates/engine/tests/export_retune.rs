mod common;

use common::{cfg, sim, wait_until};
use engine::{BudgetConfig, ExportError, Keeper, Mode, RingBudget, Source};

#[test]
fn export_after_a_retune_carries_the_new_centre() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() >= 5_000_000);
    src.set_center(860e6).unwrap();
    wait_until(|| src.segment_at(src.head() - 1).center_hz == 860e6 && src.head() - src.segment_at(src.head() - 1).from_sample > 2_000_000);
    let seg = src.segment_at(src.head() - 1);
    let dir = std::env::temp_dir().join(format!("spectrawl-export-{}", std::process::id()));
    let start = seg.from_sample + 100_000;
    let r = src.export_wideband(start, start + 1_000_000, &dir, "after").unwrap();
    assert_eq!(std::fs::metadata(&r.data_path).unwrap().len(), 4_000_000);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&r.meta_path).unwrap()).unwrap();
    assert_eq!(meta["captures"][0]["core:frequency"], 860e6);
    assert_eq!(meta["global"]["core:datatype"], "ci16_le");
    assert_eq!(meta["global"]["core:sample_rate"], 10e6);
    assert!(meta["captures"][0]["core:datetime"].as_str().unwrap().ends_with('Z'));
    let spanning = src.export_wideband(seg.from_sample - 1_000, seg.from_sample + 1_000, &dir, "spanning");
    assert!(matches!(spanning, Err(ExportError::Spans(_))));
    assert!(!dir.join("spanning.sigmf-data").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn export_errors_before_writing() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 16 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.oldest() > 1_000_000);
    let dir = std::env::temp_dir().join(format!("spectrawl-export-err-{}", std::process::id()));
    let head = src.head();
    assert!(matches!(
        src.export_wideband(head - 10, head + 10_000, &dir, "future"),
        Err(ExportError::Ring(engine::ring::ReadError::Future))
    ));
    assert!(matches!(src.export_wideband(0, 1000, &dir, "gone"), Err(ExportError::Ring(engine::ring::ReadError::Overwritten))));
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
