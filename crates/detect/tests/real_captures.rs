//! Regression tests on real captures: committed 10 s slices in testdata/small,
//! else testdata/captures (git-ignored; made with `cargo run -p engine-old
//! --example capture_identified`). A missing capture fails the test unless
//! SPECTRAWL_SKIP_CAPTURES=1 (see testdata/captures/MANIFEST.toml).

mod common;

fn check(name: &str, protocol: &str, id: Option<&str>, kind: &str, min_decoded: usize) {
    let Some((x, fs)) = common::capture(name) else { return };
    let c = detect::classify(&x, fs, 12_500.0);
    assert_eq!(c.protocol.as_deref(), Some(protocol), "{name}: {}", c.label);
    let f = c.fields.expect("fields");
    assert_eq!(f.id.as_deref(), id, "{name}");
    assert_eq!(f.kinds.first().map(|k| k.0.as_str()), Some(kind), "{name}: {:?}", f.kinds);
    assert!(f.decoded >= min_decoded, "{name}: decoded {}/{}", f.decoded, f.tried);
}

#[test]
fn p25_control_channel() {
    check("859.7677MHz_P25_Phase_1", "P25", Some("NAC 0x34D"), "TSDU", 75);
}

#[test]
fn p25_traffic_channel() {
    check("854.0130MHz_P25_Phase_1", "P25", Some("NAC 0x34D"), "TDULC", 25);
}

#[test]
fn dmr_control_channel() {
    check("463.6878MHz_DMR_BS_data", "DMR", Some("CC 9"), "CSBK", 8);
}

#[test]
#[ignore = "not on air in the owner's area"]
fn nxdn_control_channel() {
    check("460.5813MHz_NXDN_NXDN48", "NXDN", None, "control (RCCH)", 70);
}

#[test]
fn p25_control_channel_messages() {
    let Some((x, fs)) = common::capture("859.7677MHz_P25_Phase_1") else { return };
    // TSDUs carry three TSBKs: slice far enough past each sync.
    let frames = detect::frames(&x, fs, 12_500.0, 8, 380);
    let msgs = detect::p25::messages(&frames);
    assert!(msgs.len() >= 228, "{} CRC-valid TSBKs", msgs.len());
    let mut sys = detect::p25::SystemInfo::default();
    let mut grants = Vec::new();
    for m in &msgs {
        grants.extend(sys.apply(m));
    }
    assert_eq!(sys.wacn, Some(0xBEE00));
    assert_eq!(sys.system, Some(0x348));
    assert_eq!((sys.rfss, sys.site), (Some(1), Some(13)));
    // The site announces its own control channel: the one we're on.
    assert!((sys.control_hz.unwrap() - 859.7625e6).abs() < 1.0, "{:?}", sys.control_hz);
    assert!(sys.adjacent.len() >= 10);
    // TG 48595 granted on the FDMA channel at 852.575 MHz (no TDMA slot).
    let (tg, ch, _) = grants.iter().find(|g| g.0 == 48595).expect("grant");
    assert_eq!((*tg, sys.freq(*ch).map(|f| f.round()), sys.slot(*ch)), (48595, Some(852_575_000.0), None));
}

#[test]
fn capplus_status_rest_field() {
    let Some((x, fs)) = common::capture("463.6878MHz_DMR_BS_data") else { return };
    // Idle site: status announcing "rest" 1/2; no voice, so the channel's
    // own repeater number stays unknown.
    let f = detect::classify(&x, fs, 12_500.0).fields.expect("fields");
    assert_eq!((f.capplus_repeater, f.capplus_rest), (None, Some((1, 2))));
}

#[test]
fn p25_traffic_channel_link_control() {
    let Some((x, fs)) = common::capture("854.0130MHz_P25_Phase_1") else { return };
    // TDULC link control (Golay + Reed-Solomon) after calls: terminations,
    // then the site's adjacent-site and RFSS broadcasts.
    let f = detect::classify(&x, fs, 12_500.0).fields.expect("fields");
    let n = |k: &str| f.kinds.iter().find(|(name, _)| name == k).map_or(0, |e| e.1);
    assert!(n("LC adjacent site") >= 14 && n("LC call termination") >= 6, "{:?}", f.kinds);
}

#[test]
fn p25_voice_link_control_matches_grants() {
    let Some((x, fs)) = common::capture("852.5759MHz_P25_Phase_1") else { return };
    // The control channel (859.7677, recorded in the same run) granted
    // TG 48595 on this channel; LDU1/TDULC link control on the voice
    // channel names the same call, from radio 8343008 or 8343311.
    let mut talks = Vec::new();
    for w in x.chunks((1.5 * fs) as usize) {
        if let Some(t) = detect::classify(w, fs, 12_500.0).fields.and_then(|f| f.talk) {
            if t == "TG 48595 ← 8343008" || t == "TG 48595 ← 8343311" {
                return;
            }
            talks.push(t);
        }
    }
    panic!("TG 48595 not heard: {talks:?}");
}

#[test]
#[ignore = "P25 Phase 2 is on air here but its MAC does not decode on the local captures: see BACKLOG"]
fn p25_phase2_talkgroups_after_learning_the_nac() {
    let Some((x, fs)) = common::capture("P25P2_site_771.2067") else { return };
    // The system (from the Phase 1 control channel) is known; this site's
    // NAC (0x2A0) is learned from unscrambled end-of-PTT messages, after
    // which the scrambled FACCH/SACCH decode and name the calls.
    detect::p25p2::register_system(0xBEE00, 0x2A5);
    let mut talks = std::collections::BTreeSet::new();
    for w in x.chunks((1.5 * fs) as usize) {
        let c = detect::classify(w, fs, 12_500.0);
        if let Some(t) = c.fields.and_then(|f| f.talk) {
            talks.insert(t);
        }
    }
    assert!(talks.iter().any(|t| t.starts_with("TG 47018 ← ")), "{talks:?}");
    assert!(talks.iter().any(|t| t.starts_with("TG 47000 ← 4024312")), "{talks:?}");
}

#[test]
fn p25_phase1_voice_frames_and_encryption() {
    let Some((x, fs)) = common::capture("852.5759MHz_P25_Phase_1") else { return };
    // LDUs of a clear call (TG 48595), their IMBE frames mostly clean.
    let frames = detect::p25_voice_frames(&x, fs);
    assert!(frames.len() >= 10, "{} LDUs", frames.len());
    let coded: Vec<_> = frames.iter().filter_map(detect::imbe::ldu_frames).flatten().collect();
    let clean = coded.iter().filter(|c| c.total_errors() <= 3).count();
    assert!(clean * 10 >= coded.len() * 8, "{clean}/{}", coded.len());
    // LDU2 encryption sync names the clear algorithm (0x80). The slice holds
    // no encrypted call, so ADP (0xAA) is not checked here.
    let algs: std::collections::BTreeSet<u8> = frames.iter().filter_map(detect::p25_lc::ldu2).map(|e| e.algid).collect();
    assert!(algs.contains(&0x80), "{algs:x?}");
}

#[test]
#[ignore = "P25 Phase 2 is on air here but its MAC does not decode on the local captures: see BACKLOG"]
fn p25_phase2_voice_and_encryption_sync() {
    use detect::p25_voice::{LiveVoice, Phase2Stream};
    let Some((x, fs)) = common::capture("P25P2_site_769.1885") else { return };
    detect::p25p2::register_system(0xBEE00, 0x2A5);
    detect::p25p2::register_nac(0x2A5);
    // Channel 0 carries an ADP call (named by MAC_PTT and by the voice
    // bursts' encryption sync) and stays silent; channel 1 a clear one.
    let run = |ch: u8| {
        let mut vs = Phase2Stream::new(fs, Some(ch));
        let (mut audio, mut ciphers) = (0, std::collections::BTreeSet::new());
        for c in x.chunks((fs / 50.0) as usize) {
            audio += vs.push(c).audio.len();
            ciphers.extend(vs.cipher());
        }
        (audio as f64 / 8000.0, ciphers)
    };
    let (a0, c0) = run(0);
    assert!(c0.contains("ADP"), "{c0:?}");
    assert!(a0 < 5.0, "{a0} s of channel 0 audio");
    let (a1, c1) = run(1);
    assert!(a1 > 8.0 && c1.is_empty(), "{a1} s, {c1:?}");
}

/// A synthetic FLEX 3200/4 slice (no real pager traffic is committed): four frames carrying five invented pages through
/// a mild simulcast beat, as `detect::flex::simulcast::synthetic_slice` writes it.
#[test]
fn flex_pages_decode_from_the_committed_slice() {
    let Some((x, fs)) = common::capture("flex_synthetic_929.6125") else { return };
    let frames = detect::flex::decode(&x, fs);
    let pages: Vec<_> = frames.iter().flat_map(|f| &f.pages).collect();
    assert_eq!(pages.len(), 5, "{frames:?}");
    assert_eq!((pages[0].capcode, pages[0].text.as_str()), (1_234_567, "SYNTHETIC FLEX TEST PAGE ONE"));
    assert!(pages.iter().all(|p| !p.damaged), "{pages:?}");
}

/// An analog FM repeater close by (855.6375 MHz, an industrial railroad
/// channel): idle carrier carrying DCS (134.4 bit/s swinging ±1.2 kHz),
/// then voice over it. Half its windows used to read "2FSK 2400 Bd"; now
/// every window is analog FM and nearly every one reads the squelch code
/// (205 with a one as positive deviation, 263 the other way round).
#[test]
fn analog_fm_repeater_reads_its_dcs_code_and_is_never_fsk() {
    let Some((x, fs)) = common::capture("855.6375MHz_Analog_FM_repeater") else { return };
    for win in [0.5, 0.75, 1.0, 1.5] {
        let n = (win * fs) as usize;
        let (mut windows, mut coded) = (0, 0);
        for (k, w) in x.chunks(n).enumerate().filter(|(_, w)| w.len() == n) {
            let c = detect::classify(w, fs, 9_000.0);
            assert!(c.protocol.is_none() && !c.label.contains("FSK"), "window {k} of {win} s: {}", c.label);
            windows += 1;
            match c.fields.as_ref().and_then(|f| f.id.as_deref()) {
                Some("DCS 205") => coded += 1,
                Some(other) => panic!("window {k} of {win} s: {other}"),
                None => {}
            }
        }
        assert!(coded * 10 >= windows * 8, "{win} s: DCS read in {coded}/{windows} windows");
    }
}

/// An ACARS downlink on 131.550 MHz, recorded by the slice tier: the detector saw a 5.5 kHz burst. One block, its BCS
/// checking, from N365NB flying NW2320 (an A319, KRSW to KJFK).
#[test]
fn acars_downlink_decodes_from_the_committed_slice() {
    let Some((x, fs)) = common::capture("acars_131.5500_N365NB") else { return };
    let blocks = detect::acars::decode(&x, fs);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    let b = &blocks[0];
    assert!(b.crc_ok && b.parity_errors == 0, "{b:?}");
    assert_eq!((b.registration(), b.label.as_str(), b.block_id, b.flight()), ("N365NB", "H1", '6', Some("NW2320")));
    assert!(b.text.contains("KRSWKJFK"), "{}", b.text);
}

/// The same burst under 6 dB more noise still decodes (with its parity and BCS correcting what they can), and noise
/// alone never makes a block: only a checked BCS counts.
#[test]
fn acars_survives_added_noise_and_noise_alone_is_nothing() {
    let Some((x, fs)) = common::capture("acars_131.5500_N365NB") else { return };
    // The burst's own floor: the median power of the slice's quietest tenth (the slice starts before the burst).
    let mut p: Vec<f32> = x.iter().map(|v| v.norm_sqr()).collect();
    p.sort_by(f32::total_cmp);
    let floor = p[p.len() / 20];
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut gauss = || {
        let mut u = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let (a, b) = (u(), u());
        let r = (-2.0 * a.ln()).sqrt();
        num_complex::Complex32::new((r * (std::f64::consts::TAU * b).cos()) as f32, (r * (std::f64::consts::TAU * b).sin()) as f32)
    };
    let sd = (floor * 3.0 / 2.0).sqrt();
    let noisy: Vec<_> = x.iter().map(|v| v + gauss() * sd).collect();
    let blocks = detect::acars::decode(&noisy, fs);
    assert!(blocks.iter().any(|b| b.crc_ok && b.registration() == "N365NB"), "{blocks:?}");
    let noise: Vec<_> = (0..x.len()).map(|_| gauss()).collect();
    assert!(detect::acars::decode(&noise, fs).iter().all(|b| !b.crc_ok));
}

/// A second, short block from the same aircraft (its label `_` DEL, a general response), weaker: two characters fail
/// their parity, and the BCS accepts the one correction of each that makes it check.
#[test]
fn acars_corrects_a_weak_block_by_parity_and_bcs() {
    let Some((x, fs)) = common::capture("acars_131.5500_N365NB_short") else { return };
    let blocks = detect::acars::decode(&x, fs);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    let b = &blocks[0];
    assert!(b.crc_ok && b.parity_errors == 2, "{b:?}");
    assert_eq!((b.registration(), b.label.as_str(), b.flight()), ("N365NB", "_\u{7f}", Some("NW2320")));
}

#[test]
fn a_smart_meter_frame_is_802_15_4g_fsk_at_150_kbps() {
    // One Wi-SUN frame from the 902–928 MHz mesh (R2, 2026-11-10), which the shape rules called "Wideband".
    let Some((x, fs)) = common::capture("923.9953MHz_802.15.4g_FSK_150k") else { return };
    let f = detect::sunfsk::detect(&x, fs).expect("the frame's preamble and SFD");
    assert_eq!((f.rate, f.coded, f.uncoded), (150e3, 1, 0), "{f:?}");
    let w = detect::bank::Window::new(&x, fs, 200e3, detect::Hints { snr_db: Some(30.0), am_band: false });
    let i = detect::bank::sun_fsk(&w).expect("identified");
    assert_eq!((i.label.as_str(), i.protocol), ("802.15.4g FSK 150 kb/s", Some("802.15.4g")));
}

#[test]
fn a_weak_p25_control_channel_is_named_by_the_weak_pass() {
    // 851.1625 MHz on air (R2, 2026-11-11): 11 dB, only its 4 kHz core over the detector's threshold, so its records were
    // 3.7 kHz wide and named by nothing. The exact sync search still finds nothing to confirm in it.
    let Some((x, fs)) = common::capture("851.1625MHz_P25_Phase_1_weak") else { return };
    assert!(detect::sync::best(&detect::sync_hits(&x, fs, detect::NARROW_CUTOFF_HZ)).is_none());
    let c = detect::classify(&x, fs, 3_700.0);
    assert_eq!(c.label, "P25 Phase 1");
    let f = c.fields.expect("fields");
    assert_eq!(f.id.as_deref(), Some("NAC 0x344"));
    assert_eq!(f.kinds.first().map(|k| k.0.as_str()), Some("TSDU"), "{:?}", f.kinds);
    assert!(f.decoded >= 4, "decoded {}/{}", f.decoded, f.tried);
}
