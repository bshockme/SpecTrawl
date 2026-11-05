//! The RFOne's firmware sweep through the backend, no engine: hop rate, hop
//! sizes, the time anchors, and that the stream comes back as IQ at the live
//! centre afterwards.
//!
//! `cargo run -p backend-airspy --example native_sweep -- <serial> [seconds] [dwell]`

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use backend_airspy::AirspyBackend;
use parking_lot::Mutex;
use sdr_core::{Backend, NativeSweep, Samples, StreamEvent, StreamSink};

#[derive(Default)]
struct Log {
    samples: u64,
    retunes: Vec<(u64, f64)>,
    anchors: Vec<(u64, SystemTime)>,
    gaps: u64,
    /// Peak |sample| per hop.
    peak: f32,
    over: u64,
}

struct Sink(Arc<Mutex<Log>>);

impl StreamSink for Sink {
    fn on_samples(&mut self, _first: u64, s: Samples<'_>) {
        let mut l = self.0.lock();
        l.samples += s.len() as u64;
        if let Samples::Cf32(v) = s {
            l.peak = v.iter().map(|c| c.norm()).fold(l.peak, f32::max);
            l.over += v.iter().filter(|c| c.re.abs() > 1.0 || c.im.abs() > 1.0).count() as u64;
        }
    }
    fn on_event(&mut self, e: StreamEvent) {
        let mut l = self.0.lock();
        match e {
            StreamEvent::Retune { at_sample, center_hz, .. } => l.retunes.push((at_sample, center_hz)),
            StreamEvent::TimeAnchor { at_sample, host_time } => l.anchors.push((at_sample, host_time)),
            StreamEvent::Gap { .. } => l.gaps += 1,
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let serial = args.get(1).expect("serial");
    let secs: f64 = args.get(2).map_or(3.0, |s| s.parse().unwrap());
    let dwell: u8 = args.get(3).map_or(1, |s| s.parse().unwrap());
    let mut d = AirspyBackend::new().open(serial).expect("open");
    println!("{} native_sweep={}", d.info().label, d.capabilities().native_sweep);
    let log = Arc::new(Mutex::new(Log::default()));
    d.set_center(855.25e6).unwrap();
    d.start(Box::new(Sink(log.clone()))).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let live_before = log.lock().samples;
    let plan = NativeSweep { first_center_hz: 28.5e6, step_hz: 9e6, steps: 197, dwell_blocks: dwell, block_samples: 0 };
    let t0 = Instant::now();
    let got = d.start_native_sweep(plan).expect("start");
    println!("granted {got:?}");
    let (s0, r0, a0) = {
        let l = log.lock();
        (l.samples, l.retunes.len(), l.anchors.len())
    };
    std::thread::sleep(Duration::from_secs_f64(secs / 2.0));
    // The board refuses gain requests while sweeping; the backend pauses it around them.
    let before = log.lock().retunes.len();
    d.set_gain(sdr_core::GainSetting::Linearity(0.5)).expect("gain while sweeping");
    std::thread::sleep(Duration::from_millis(100));
    println!("gain changed mid-sweep; {} hops in the 100 ms after", log.lock().retunes.len() - before);
    std::thread::sleep(Duration::from_secs_f64(secs / 2.0 - 0.1));
    let (s1, r1, a1) = {
        let l = log.lock();
        (l.samples, l.retunes.len(), l.anchors.len())
    };
    let el = t0.elapsed().as_secs_f64();
    {
        let l = log.lock();
        let hops = &l.retunes[r0..r1];
        let mut sizes: Vec<u64> = hops.windows(2).map(|w| w[1].0 - w[0].0).collect();
        sizes.sort();
        let wraps = hops.windows(2).filter(|w| w[1].1 < w[0].1).count();
        println!(
            "{} hops in {el:.2} s = {:.0} hops/s, {wraps} wraps; hop sizes {:?}..{:?} (median {:?}); {} samples ({:.2} MS/s); {} anchors",
            hops.len(),
            hops.len() as f64 / el,
            sizes.first(),
            sizes.last(),
            sizes.get(sizes.len() / 2),
            s1 - s0,
            (s1 - s0) as f64 / el / 1e6,
            a1 - a0
        );
        let anchors = &l.anchors[a0..a1];
        if let (Some(f), Some(e)) = (anchors.first(), anchors.last()) {
            let dt = e.1.duration_since(f.1).unwrap_or_default().as_secs_f64();
            println!("anchors span {dt:.3} s of device time over {el:.3} s of host time");
        }
        let first: Vec<f64> = hops.iter().take(5).map(|h| h.1 / 1e6).collect();
        println!("first hop centres {first:?}; peak |x| {:.3}, {} samples beyond ±1; gaps {}", l.peak, l.over, l.gaps);
    }
    d.stop_native_sweep().expect("stop");
    std::thread::sleep(Duration::from_millis(500));
    let l = log.lock();
    let after = l.samples - s1;
    println!("after stop: {:.2} MS/s, last retune {:?}, live before {live_before}", after as f64 / 0.5 / 1e6, l.retunes.last());
    drop(l);
    d.stop().unwrap();
}
