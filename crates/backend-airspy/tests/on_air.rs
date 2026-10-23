use std::sync::{Arc, Mutex};
use std::time::Duration;

use backend_airspy::AirspyBackend;
use sdr_core::{Backend, Samples, StreamEvent, StreamSink};

#[derive(Default)]
struct Log {
    /// `(first_sample, len)` per block.
    blocks: Vec<(u64, usize)>,
    events: Vec<StreamEvent>,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Log>>);

impl Recorder {
    fn samples(&self) -> u64 {
        self.0.lock().unwrap().blocks.iter().map(|b| b.1 as u64).sum()
    }

    fn events(&self) -> Vec<StreamEvent> {
        self.0.lock().unwrap().events.clone()
    }
}

impl StreamSink for Recorder {
    fn on_samples(&mut self, first_sample: u64, samples: Samples<'_>) {
        self.0.lock().unwrap().blocks.push((first_sample, samples.len()));
    }

    fn on_event(&mut self, event: StreamEvent) {
        self.0.lock().unwrap().events.push(event);
    }
}

#[test]
#[ignore = "needs an Airspy: SPECTRAWL_AIRSPY_SERIAL=<serial> cargo test -p backend-airspy --test on_air -- --ignored"]
fn retunes_and_anchors_on_hardware() {
    let serial = std::env::var("SPECTRAWL_AIRSPY_SERIAL").expect("set SPECTRAWL_AIRSPY_SERIAL");
    let mut dev = AirspyBackend::new().open(&serial).expect("open");
    let rate = dev.sample_rate();
    let rec = Recorder::default();
    dev.start(Box::new(rec.clone())).expect("start");
    std::thread::sleep(Duration::from_millis(200));

    let centres = [440.5e6, 447.25e6, 454.0e6, 461.75e6, 469.5e6];
    let mut requested_at = Vec::new();
    for &hz in &centres {
        dev.set_center(hz).expect("set_center");
        requested_at.push(rec.samples());
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(800));
    dev.stop().expect("stop");

    let total = rec.samples();
    let events = rec.events();
    let retunes: Vec<(u64, f64)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Retune { at_sample, center_hz, .. } if *at_sample > 0 => Some((*at_sample, *center_hz)),
            _ => None,
        })
        .collect();
    println!("rate {rate}, total samples {total}");
    println!("retunes (at_sample, centre): {retunes:?}");
    println!("sample count at request: {requested_at:?}");
    assert_eq!(retunes.len(), centres.len(), "{retunes:?}");
    for (i, (at, hz)) in retunes.iter().enumerate() {
        assert_eq!(*hz, centres[i]);
        if i > 0 {
            assert!(*at > retunes[i - 1].0, "not strictly increasing: {retunes:?}");
        }
        assert!(at.abs_diff(requested_at[i]) <= 65_536, "retune {i} at {at}, requested at {}", requested_at[i]);
    }

    let anchors: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TimeAnchor { at_sample, .. } => Some(*at_sample),
            _ => None,
        })
        .collect();
    println!("anchors: {anchors:?}");
    assert!(anchors.len() as u64 >= total / 10_000_000, "{} anchors over {total} samples", anchors.len());
    assert!(anchors.windows(2).all(|w| w[1] - w[0] <= 10_000_000 + 65_536), "{anchors:?}");

    let overloads = events.iter().filter(|e| matches!(e, StreamEvent::Overload { .. })).count();
    println!("overloads: {overloads}");
    assert_eq!(overloads, 0);
}
