//! A bus channel served as rtl_tcp (step 7, `engine::iqserve`): a client reads the greeting, finds the simulator's
//! carriers where they are, retunes, is refused outside the span and at a rate the channelizer can't give, and a client
//! that stops reading loses blocks without holding the bus.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::iqserve::{IqServeConfig, MAGIC, TARGET_RMS, from_u8};
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, Priority, SourceConfig};

const TUNER_HZ: f64 = 855.25e6;
const A_HZ: f64 = TUNER_HZ + 100e3;
const B_HZ: f64 = TUNER_HZ - 800e3;
/// `fs·K/N` with K = N/4: the widest rate the channelizer gives at 10 MS/s.
const RATE: f64 = 2.5e6;

fn engine() -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: None,
        audio: engine::audio::AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    })
}

fn device() -> Box<dyn sdr_core::Device> {
    let cw = |freq_hz, level_dbfs| Emitter { freq_hz, level_dbfs, modulation: Modulation::Cw, keying: Keying::Continuous };
    let (d, _) = SimDevice::with_config(SimConfig {
        emitters: vec![cw(A_HZ, -40.0), cw(B_HZ, -46.0)],
        center_hz: TUNER_HZ,
        paced: true,
        report_lag_gaps: false,
        ..Default::default()
    });
    Box::new(d)
}

fn command(s: &mut TcpStream, cmd: u8, arg: u32) {
    let b = arg.to_be_bytes();
    s.write_all(&[cmd, b[0], b[1], b[2], b[3]]).unwrap();
}

fn read_secs(s: &mut TcpStream, secs: f64) -> Vec<u8> {
    let mut b = vec![0u8; (secs * RATE) as usize * 2];
    s.read_exact(&mut b).unwrap();
    b
}

/// The offset of the strongest line, Hz, and how far it stands over the median bin, dB.
fn peak(bytes: &[u8]) -> (f64, f32) {
    const N: usize = 4096;
    let x = from_u8(bytes);
    let mut sa = dsp::SpectrumAnalyzer::new(N);
    for c in x.as_chunks::<N>().0 {
        sa.push(c);
    }
    let mut db = Vec::new();
    assert!(sa.take_db(&mut db));
    let (k, top) = db.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(k, v)| (k, *v)).unwrap();
    let mut sorted = db.clone();
    sorted.sort_by(f32::total_cmp);
    ((k as f64 - (N / 2) as f64) * RATE / N as f64, top - sorted[N / 2])
}

#[test]
fn a_client_finds_the_carriers_retunes_and_is_refused_outside_the_span() {
    let e = engine();
    let src = e.add_source(device(), Mode::Live).unwrap();
    let srv = e.serve_iq(src.id(), IqServeConfig { center_hz: TUNER_HZ, rate: RATE, priority: Priority::EmitterDecoder }).unwrap();
    let mut c = TcpStream::connect(srv.addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut greeting = [0u8; 12];
    c.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting[..4], MAGIC);
    assert_eq!(&greeting[4..], &[0u8; 8], "tuner type 0 and no gains: \"another radio\" to OnAir");
    command(&mut c, 0x02, RATE as u32);
    command(&mut c, 0x04, 300);

    let (f, over) = peak(&read_secs(&mut c, 0.4));
    assert!((f - 100e3).abs() < 1.5e3 && over > 30.0, "the 855.35 MHz carrier at {f:+.0} Hz, {over:.0} dB over the median");
    let rms = {
        let b = read_secs(&mut c, 0.2);
        (b.iter().map(|v| (f32::from(*v) - 127.5).powi(2)).sum::<f32>() / b.len() as f32).sqrt()
    };
    assert!((rms / TARGET_RMS - 1.0).abs() < 0.5, "the AGC holds {rms:.1} counts RMS for {TARGET_RMS}");

    // Tuned 500 kHz up, the same carrier is 400 kHz below the centre.
    command(&mut c, 0x01, (TUNER_HZ + 500e3) as u32);
    let t0 = Instant::now();
    let (f, _) = loop {
        let p = peak(&read_secs(&mut c, 0.2));
        if (p.0 + 400e3).abs() < 1.5e3 {
            break p;
        }
        assert!(t0.elapsed() < Duration::from_secs(3), "never retuned: the line is at {:+.0} Hz", p.0);
    };
    assert!((f + 400e3).abs() < 1.5e3);
    // Outside the usable span: refused, the stream carries on where it was.
    command(&mut c, 0x01, 870_000_000);
    command(&mut c, 0x02, 2_048_000);
    let _ = read_secs(&mut c, 0.3);
    let (f, _) = peak(&read_secs(&mut c, 0.3));
    assert!((f + 400e3).abs() < 1.5e3, "the refused tune moved the stream: {f:+.0} Hz");
    let s = srv.stats();
    println!("{s:?}");
    assert_eq!((s.refused_tunes, s.refused_rates, s.gain_commands, s.clients), (1, 1, 1, 1), "{s:?}");
    assert_eq!(s.center_hz, TUNER_HZ + 500e3);
    assert_eq!(s.dropped_blocks, 0, "a client reading at the rate loses nothing: {s:?}");
}

#[test]
fn a_client_that_stops_reading_loses_blocks_not_the_bus() {
    let e = engine();
    let src = e.add_source(device(), Mode::Live).unwrap();
    let srv = e.serve_iq(src.id(), IqServeConfig { center_hz: TUNER_HZ, rate: RATE, priority: Priority::EmitterDecoder }).unwrap();
    let mut c = TcpStream::connect(srv.addr()).unwrap();
    let mut greeting = [0u8; 12];
    c.read_exact(&mut greeting).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let s = srv.stats();
    assert!(s.dropped_blocks > 0, "{s:?}");
    let bus = e.bus().stats();
    assert!(bus.consumers.iter().all(|c| c.lag_secs < 0.5), "the stalled client held the bus: {bus:?}");
    assert_eq!(bus.shed_total, 0);
    // It reads again and is served from where the stream is now.
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = vec![0u8; 200_000];
    c.read_exact(&mut b).unwrap();
}

#[test]
fn a_rate_the_channelizer_cannot_give_is_refused() {
    let e = engine();
    let src = e.add_source(device(), Mode::Live).unwrap();
    let err = e.serve_iq(src.id(), IqServeConfig { center_hz: TUNER_HZ, rate: 2.048e6, priority: Priority::EmitterDecoder }).err().unwrap();
    assert!(err.contains("fs·K/N"), "{err}");
    assert!(e.serve_iq(src.id(), IqServeConfig { center_hz: TUNER_HZ, rate: 1.25e6, priority: Priority::EmitterDecoder }).is_ok());
}
