//! The inspector explains a record's name: every claim, its numbers, and why it did or didn't win (spec §5; the owner's
//! main frustration is classification mistakes, so a wrong name must be explainable from the inspector alone).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use api::{Claim, EmitterId, Evidence, RecordId, Role, SegmentId, SignalRecord, Snapshot, SourceId};
use arc_swap::ArcSwap;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use spectrawl_app::app::App;
use spectrawl_app::selection::Selection;
use spectrawl_app::tabs::Tab;

fn record(claims: Vec<Claim>) -> SignalRecord {
    let t = SystemTime::now() - Duration::from_secs(5);
    SignalRecord {
        id: RecordId(42),
        source: SourceId(1),
        serial: "SIM-0001".into(),
        role: Role::Live,
        segment: SegmentId(1),
        tuner_hz: 855.5e6,
        first_sample: 0,
        end_sample: 10_000_000,
        t_start: t,
        t_end: t + Duration::from_secs(1),
        f_lo_hz: 857.2318e6,
        f_hi_hz: 857.2434e6,
        bin_hz: 2441.0,
        peak_dbfs: -48.5,
        mean_dbfs: -52.0,
        mean_snr_db: 40.0,
        max_snr_db: 51.0,
        rows: 40,
        open: false,
        claims,
        emitter: Some(EmitterId(7)),
        tags: Vec::new(),
        notes: Vec::new(),
        iq_clip: None,
        slice_evicted: false,
    }
}

#[test]
fn a_selected_record_shows_every_claim_and_why_the_headline_won() {
    let shape = Evidence::Features {
        bandwidth_hz: 11.6e3,
        snr_db: 41.0,
        duty: 1.0,
        envelope_cv: Some(0.05),
        symbol_rate: Some(4800.0),
        fsk_levels: Some(4),
        deviation_hz: Some(1944.0),
        carrier_hz: None,
    };
    let claims = vec![
        Claim::new("4FSK", 0.6, shape),
        Claim::new("DMR BS voice", 0.8, Evidence::SyncBank { protocol: "DMR".into(), syncs: 25, frames_tried: 0, frames_decoded: 0 }),
        Claim::new("Public safety 800", 0.4, Evidence::BandPlan { entry: "800 MHz trunking".into() }),
    ];
    let mut snap = Snapshot::empty();
    snap.version = 1;
    let mut view = (*snap.records).clone();
    view.records.push(Arc::new(record(claims)));
    snap.records = Arc::new(view);
    let (tx, _rx) = api::command_channel();
    let handle = api::Handle::new(Arc::new(ArcSwap::from_pointee(snap)), tx);

    let mut app = App::new(handle);
    app.show_tab(Tab::Inspector);
    app.set_selection(Selection::Record(RecordId(42)));
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(|ui, app: &mut App| app.frame(ui), app);
    // The first frame installs the fonts; the next ones draw.
    h.run_steps(3);

    for text in [
        "DMR BS voice",
        "the name: the strongest kind of claim, and the most confident of its kind",
        "never the name: a shape names only AM or analog FM, measured at 60 % or more",
        "4800 Bd, 4 levels",
        "inferred from band plan",
        "never the name: the band plan describes where it is, never what it is",
    ] {
        assert!(h.query_all_by_label_contains(text).next().is_some(), "the inspector should show {text:?}");
    }
}

#[test]
fn the_page_bar_switches_layouts_and_a_saved_layout_joins_it() {
    let (tx, _rx) = api::command_channel();
    let handle = api::Handle::new(Arc::new(ArcSwap::from_pointee(Snapshot::empty())), tx);
    let mut h =
        Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(|ui, app: &mut App| app.frame(ui), App::new(handle));
    h.run_steps(3);
    assert_eq!(h.state().page(), "Survey");
    // Paging puts the Pages table in front: its empty-table sentence shows.
    h.get_by_label("Paging").click();
    h.run_steps(2);
    assert_eq!(h.state().page(), "Paging");
    assert!(h.query_all_by_label_contains("a page appears once a POCSAG or FLEX monitor decodes one").next().is_some());
    // Saved under a name, it joins the page bar and is the page in use.
    h.state_mut().save_page("Night watch");
    h.run_steps(2);
    assert_eq!(h.state().page(), "Night watch");
    h.get_by_label("Survey").click();
    h.run_steps(2);
    assert_eq!(h.state().page(), "Survey");
    h.get_by_label("Night watch").click();
    h.run_steps(2);
    assert_eq!(h.state().page(), "Night watch");
    // A preset's name can't be taken.
    h.state_mut().save_page("Survey");
    assert_eq!(h.state().page(), "Night watch");
}

#[test]
fn the_palette_runs_its_first_entry_on_enter() {
    let (tx, _rx) = api::command_channel();
    let handle = api::Handle::new(Arc::new(ArcSwap::from_pointee(Snapshot::empty())), tx);
    let mut h =
        Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(|ui, app: &mut App| app.frame(ui), App::new(handle));
    h.run_steps(3);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    h.run_steps(2);
    assert!(h.state().palette_open());
    // The palette's box has the focus: typing goes to it, as a keyboard's would.
    h.event(egui::Event::Text("pop out catalogue".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert!(!h.state().palette_open(), "Enter should run the entry and close the palette");
    assert_eq!(h.state().popped_out(), [Tab::Catalogue]);
}

#[test]
fn save_iq_on_a_record_asks_for_its_slice_and_shows_where_it_went() {
    let mut snap = Snapshot::empty();
    snap.version = 1;
    let mut view = (*snap.records).clone();
    view.records.push(Arc::new(record(Vec::new())));
    snap.records = Arc::new(view);
    let swap = Arc::new(ArcSwap::from_pointee(snap.clone()));
    let (tx, mut rx) = api::command_channel();
    let handle = api::Handle::new(swap.clone(), tx);

    let mut app = App::new(handle);
    app.show_tab(Tab::Inspector);
    app.set_selection(Selection::Record(RecordId(42)));
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(|ui, app: &mut App| app.frame(ui), app);
    h.run_steps(3);
    h.get_by_label("Save IQ").click();
    h.run_steps(2);
    let sent: Vec<_> = rx.drain().unwrap_or_default().into_iter().map(|(_, c)| c).collect();
    assert!(sent.iter().any(|c| matches!(c, api::Command::SaveIq { record: RecordId(42) })), "{sent:?}");

    // The engine's answer shows under the slice.
    snap.version = 2;
    snap.acks = vec![api::Ack {
        seq: 1,
        result: Ok(api::Outcome::SavedIq { record: RecordId(42), path: "/data/saved/record-42".into(), pending: true }),
    }];
    swap.store(Arc::new(snap));
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("IQ saved to /data/saved/record-42.sigmf-data once its slice is written").next().is_some());
}
