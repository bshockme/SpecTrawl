//! Step 5's acceptance on the simulator (no air-band capture exists yet): a 4 s AM burst is heard from the ring and
//! from its slice, with the tone it carries; a record with nothing left to play is refused with the reason; a stop
//! ends a play at once and a source going away ends one early (plan review focus 1–5).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{AudioMode, PlayFrom, PlayId, PlayState, PlayTarget, PlayTo, PlayView, SignalRecord};
use backend_sim::{Emitter, Keying, Modulation, SimConfig, SimDevice};
use engine::audio::{AudioConfig, RATE};
use engine::recorder::SliceConfig;
use engine::{BudgetConfig, BusConfig, Engine, EngineConfig, Mode, SourceConfig};

/// The burst: 337.5 kHz below the tuner, AM with a 1 kHz tone, 4 s on in every 9 s, starting 3.6 s in (once the
/// detector has settled).
const AM_HZ: f64 = 124.9125e6;
const TUNER_HZ: f64 = 125.25e6;
const TONE_HZ: f64 = 1000.0;
const BURST_S: f64 = 4.0;

fn device() -> Box<dyn sdr_core::Device> {
    let emitters = vec![Emitter {
        freq_hz: AM_HZ,
        level_dbfs: -40.0,
        modulation: Modulation::AmTone { tone_hz: TONE_HZ, depth: 0.6 },
        keying: Keying::Bursts { period_s: 9.0, duty: BURST_S / 9.0, phase: 0.4 },
    }];
    let (d, _) =
        SimDevice::with_config(SimConfig { emitters, center_hz: TUNER_HZ, paced: true, report_lag_gaps: false, ..Default::default() });
    Box::new(d)
}

struct Dir(PathBuf);
impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("spectrawl-replay-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Dir(d)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 7 s of ring: the burst and its pre-roll are still held when it closes.
fn engine(slices: Option<&Dir>) -> Engine {
    Engine::new(EngineConfig {
        budget: BudgetConfig { guaranteed_secs: 7.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 },
        bus: BusConfig::default(),
        source: SourceConfig::default(),
        store: None,
        slices: slices.map(|d| SliceConfig::new(&d.0)),
        audio: AudioConfig::Manual,
        classify: engine::classify::ClassifyConfig::off(),
    })
}

fn wait<T>(secs: f64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed().as_secs_f64() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The burst's record once it has closed (and, with slices on, has its slice). Not the tail of a burst already on
/// when the stream started.
fn burst(e: &Engine, sliced: bool) -> Arc<SignalRecord> {
    wait(15.0, "the AM burst's record", || {
        e.records()
            .view()
            .records
            .iter()
            .find(|r| (r.center_hz() - AM_HZ).abs() < 5e3 && !r.open && r.first_sample > 0 && (!sliced || r.iq_clip.is_some()))
            .cloned()
    })
}

fn play_view(e: &Engine, id: PlayId) -> PlayView {
    e.audio_view().plays.into_iter().find(|p| p.id == id).expect("the play is in the view")
}

/// Pulls the mix, 10 ms at a time, until the play is over; the audio it played.
fn pull_until_over(e: &Engine, id: PlayId, secs: f64) -> (Vec<f32>, PlayView) {
    let mut audio = Vec::new();
    let mut chunk = vec![0.0f32; 480];
    let t0 = Instant::now();
    loop {
        let v = play_view(e, id);
        if v.state.is_over() {
            return (audio, v);
        }
        assert!(t0.elapsed().as_secs_f64() < secs, "the play did not end: {v:?}");
        e.player().mixer().render(&mut chunk);
        audio.extend_from_slice(&chunk);
        // The ring path fills faster than real time; let the bus worker run.
        if !v.complete {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Power at `hz` (Goertzel over the whole clip).
fn power_at(x: &[f32], hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / RATE;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = f64::from(v) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2) / (x.len() as f64).powi(2)
}

/// The tone over the strongest other voice-band frequency, in dB.
fn tone_margin_db(x: &[f32]) -> f64 {
    let tone = power_at(x, TONE_HZ);
    let other = [400.0, 650.0, 1500.0, 2000.0, 2600.0, 3000.0].iter().map(|&f| power_at(x, f)).fold(0.0, f64::max);
    10.0 * (tone / other).log10()
}

/// Seconds of the clip the tone is heard in (0.1 s windows with the tone at least 10 dB over the rest).
fn heard_secs(x: &[f32]) -> f64 {
    let w = (0.1 * RATE) as usize;
    x.chunks(w).filter(|c| c.len() == w && tone_margin_db(c) > 10.0).count() as f64 * 0.1
}

fn read_wav(p: &std::path::Path) -> Vec<f32> {
    let b = std::fs::read(p).expect("the WAV");
    assert_eq!(&b[..4], b"RIFF");
    b[44..].as_chunks::<2>().0.iter().map(|s| f32::from(i16::from_le_bytes(*s)) / 32767.0).collect()
}

#[test]
fn a_4_s_am_burst_is_heard_from_the_ring_and_from_its_slice() {
    let dir = Dir::new("burst");
    let e = engine(Some(&dir));
    let src = e.add_source(device(), Mode::Live).unwrap();
    let r = burst(&e, true);
    let rec_s = (r.end_sample - r.first_sample) as f64 / 10e6;
    assert!((rec_s - BURST_S).abs() < 0.3, "a {rec_s:.2} s record for a {BURST_S} s burst");

    // From the ring, to the speaker; the engine picks AM for 124.9 MHz.
    let id = e.play(PlayTarget::Record(r.id), None, 1.0, PlayTo::Speaker).expect("playable from the ring");
    // Pulled only once it is all there: pulling faster than a slow machine fills it would underrun, and the silences
    // would spread the tone over more 0.1 s windows than it lasts. (The jitter buffer has its own unit test.)
    wait(20.0, "the ring play's audio", || play_view(&e, id).complete.then_some(()));
    let (ring_audio, v) = pull_until_over(&e, id, 20.0);
    assert_eq!((v.from, v.mode, v.mode_guessed, &v.state), (PlayFrom::Ring, AudioMode::Am, true, &PlayState::Finished));
    let ring_s = v.length_s;
    // The record and its pre-roll, to within a block or two (and the jitter buffer's wait in what was pulled).
    assert!((ring_s - (rec_s + 0.05)).abs() < 0.2, "{ring_s:.3} s from the ring for a {rec_s:.3} s record");
    assert!(ring_audio.len() as f64 / RATE >= ring_s, "less pulled than played");
    let margin = tone_margin_db(&ring_audio);
    assert!(margin > 20.0, "the 1 kHz tone is only {margin:.1} dB over the rest");
    let heard = heard_secs(&ring_audio);
    println!("ring: {ring_s:.3} s for a {rec_s:.3} s record, tone {margin:.1} dB over the rest, heard {heard:.1} s");
    assert!((heard - BURST_S).abs() < 0.3, "the tone is heard for {heard:.2} s of a {BURST_S} s burst");

    // With its source gone, from its slice, written out.
    assert!(e.remove_source(src.id()));
    let wav = dir.0.join("burst.wav");
    let id = e.play(PlayTarget::Record(r.id), None, 1.0, PlayTo::Wav(wav.clone())).expect("playable from the slice");
    let v = wait(10.0, "the WAV", || Some(play_view(&e, id)).filter(|v| v.state.is_over()));
    assert_eq!((v.from, &v.state), (PlayFrom::Slice, &PlayState::Finished));
    let slice_audio = read_wav(&wav);
    let slice_s = slice_audio.len() as f64 / RATE;
    assert!((slice_s - ring_s).abs() < 0.05 * ring_s, "{slice_s:.3} s from the slice, {ring_s:.3} s from the ring");
    let margin = tone_margin_db(&slice_audio);
    assert!(margin > 20.0, "from the slice the tone is only {margin:.1} dB over the rest");
    let heard = heard_secs(&slice_audio);
    println!("slice: {slice_s:.3} s, tone {margin:.1} dB over the rest, heard {heard:.1} s");
    assert!((heard - BURST_S).abs() < 0.3, "from the slice the tone is heard for {heard:.2} s");
}

#[test]
fn a_record_with_nothing_left_to_play_is_refused_with_the_reason() {
    let e = engine(None);
    let src = e.add_source(device(), Mode::Live).unwrap();
    let r = burst(&e, false);
    e.remove_source(src.id());
    let err = e.play(PlayTarget::Record(r.id), Some(AudioMode::Am), 1.0, PlayTo::Speaker).unwrap_err();
    assert!(err.contains("its source is gone") && err.contains("no slice"), "{err}");
    assert!(e.audio_view().plays.is_empty(), "a refused play shows as a play");
    let err = e.play(PlayTarget::Record(api::RecordId(999_999)), None, 1.0, PlayTo::Speaker).unwrap_err();
    assert!(err.contains("not in memory"), "{err}");
}

#[test]
fn a_stop_ends_a_play_at_once_and_a_source_going_away_ends_one_early() {
    let e = engine(None);
    let src = e.add_source(device(), Mode::Live).unwrap();
    // The burst while it is still on the air: an open record, followed at the live edge.
    let r = wait(10.0, "the open burst", || {
        e.records()
            .view()
            .records
            .iter()
            .find(|r| (r.center_hz() - AM_HZ).abs() < 5e3 && r.open && r.first_sample > 0 && r.rows > 20)
            .cloned()
    });
    let a = e.play(PlayTarget::Record(r.id), None, 1.0, PlayTo::Speaker).unwrap();
    let b = e.play(PlayTarget::Record(r.id), None, 0.5, PlayTo::Speaker).unwrap();
    let mut chunk = vec![0.0f32; 4800];
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(100));
        e.player().mixer().render(&mut chunk);
    }
    assert_eq!(play_view(&e, a).state, PlayState::Playing);
    assert!(e.stop_play(a));
    assert_eq!(play_view(&e, a).state, PlayState::Stopped);
    assert!(!e.stop_play(a), "stopped twice");
    // The other plays on until its source goes.
    assert!(!play_view(&e, b).complete);
    e.remove_source(src.id());
    let (_, v) = pull_until_over(&e, b, 10.0);
    assert_eq!(v.state, PlayState::EndedEarly("the source went away".into()), "{v:?}");
}
