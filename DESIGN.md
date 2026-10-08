# SpecTrawl: design

*Spectral trawling.* An open, signal-centric SIGINT platform for commodity SDRs.

Status: design draft, 2026-10-08. Nothing is built yet.

---

## 1. What SpecTrawl is

You point one or more SDRs at sections of spectrum. SpecTrawl finds every emission there, records it as a **signal record**, then classifies it and decodes it where it can. The analyst works on those records, not on tools:
- browse them and search them
- tag them
- rewind to them, replay them and export them

**Reference points:**
- **3dB Labs SCEPTRE / Kestrel TSCM:** the analyst workflow we want. That means signal-centric analysis, a pre-detection look-back buffer, a signal database with tags, several receivers, and baseline comparison.
- **SDR++:** the bar for look and feel. Fast, attractive, smooth waterfall.
- **Sync-driven decoders:** the detection techniques. Sync search at every sampling phase with lookahead, masked sync tables, two-sync confirmation, and slicer levels set from each sync.
- **iNTERCEPT (smittix/intercept):** the anti-model. It's a web page that launches about 15 CLI tools, and each tool grabs a whole SDR. There's no shared spectrum and no common signal model.

**Goals:**
- Any supported SDR can fill any role. Each device is used according to its capabilities.
- One wideband IQ stream per SDR, shared by every consumer. No tool ever owns a device.
- Detect and confirm protocols reliably across many channels at once.
- Unknown signals still produce useful records: bandwidth, modulation family, symbol rate, timing.
- Look back in time: detectors and the user can both rewind into the recent past.
- Runs on Windows and Linux.

**Non-goals (for now):**
- Transmitting. SpecTrawl is receive-only.
- Decrypting anything.
- Voice vocoders (AMBE/IMBE). They're heavy and raise patent and legal questions. Decoding metadata comes first.
- Lab-grade calibrated measurements.
- Geolocation by time or frequency difference of arrival (TDOA/FDOA). It needs synchronized receivers at several sites, so it's a long-term stretch goal.

## 2. Settled decisions

| Area | Decision | Why |
|---|---|---|
| Platforms | **Windows + Linux**, both first-class, with CI builds for both from the start. macOS is opportunistic. | Windows is the main desktop. Linux is the SDR ecosystem and the place for headless remote sensors. |
| Language | **Rust** | Performance plus memory safety, easy threading per channel, good FFT/SIMD crates, and easy bindings to the C SDR libraries. |
| GUI | **egui / eframe + wgpu**, with **egui_dock** for docked panes | Immediate mode, the Rust counterpart of SDR++'s Dear ImGui. The waterfall is a custom wgpu renderer. |
| Engine vs GUI | **Separated by a defined API** (`api` crate). They run in one process for v1. | The same API later runs over the network, so a headless Linux node is just another source. |
| Hardware | **SDR-agnostic** backend/device/role layers. Native backends for performance, SoapySDR as the catch-all. | See §4. |
| Driver libraries | **Loaded dynamically at runtime** (`libloading`), not linked at build time | A missing `airspy.dll`/`.so` means the backend doesn't appear. One build runs everywhere. |

## 3. Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│ app (egui)  spectrum+waterfall · signal list · inspector · timeline · roles│
└───────────────▲───────────────────────────────────────┬──────────────────┘
                │ snapshots / events                     │ commands
┌───────────────┴───────────────────────────────────────▼──────────────────┐
│ api   engine ⇄ UI contract (in-process channels now, network later)       │
└───────────────▲───────────────────────────────────────┬──────────────────┘
┌───────────────┴───────────────────────────────────────▼──────────────────┐
│ engine                                                                    │
│  per source:  IQ ring ─► shared FFT ─┬─► spectrum frames (UI)             │
│                (look-back)           ├─► burst detector ─► signal records │
│                                      └─► channelizer ─► channels          │
│  per channel: features ─► classifier ─► detector bank ─► decoders        │
│  role assigner · scheduler · store (SQLite + SigMF clips)                 │
└───────────────▲──────────────────────────────────────────────────────────┘
                │ sample blocks + in-band events (the stream contract)
┌───────────────┴──────────────────────────────────────────────────────────┐
│ sdr-core   Device interface · Capabilities · stream contract             │
│ backends   airspy · hydrasdr · hackrf · rtlsdr · soapy · file(SigMF) ·   │
│            net (later)                                                    │
└──────────────────────────────────────────────────────────────────────────┘
```

## 4. Hardware layer

Three layers:

### 4.1 Backends

A backend knows how to enumerate and open one family of devices.

| Backend | Library | Notes |
|---|---|---|
| `airspy` | libairspy | Owned: Airspy R2. Packed 12-bit samples. Real-IF, so no DC spike. |
| `hydrasdr` | libhydrasdr | HydraSDR RFOne: Airspy R2-derived, R828D tuner, 12-bit, 10 MHz. Not owned yet. If its API is close to libairspy's, it may share a module with `airspy`. Check against the real headers. |
| `hackrf` | libhackrf | Native **sweep mode** for fast surveys. 8-bit, zero-IF. |
| `rtlsdr` | librtlsdr | Includes direct sampling. 8-bit, zero-IF. |
| `soapy` | SoapySDR | Catch-all for SDRplay, LimeSDR, USRP, PlutoSDR and others. Loses device-specific extras. |
| `file` | built-in | **SigMF** playback with time control. Both the test harness and the offline-analysis mode. |
| `net` (later) | built-in | Remote SpecTrawl nodes; maybe rtl_tcp/SpyServer clients. |

Native backends come first for the hardware we own. Soapy covers everything else until a native backend is worth writing.

### 4.2 Device and Capabilities

An open device exposes a common `Device` interface: tune, set the sample rate, set gain, start/stop the stream. It also publishes a **Capabilities** record:

| Field | Used for |
|---|---|
| Frequency range(s), tuning step, retune latency | Role fit, sweep speed |
| Sample rates, **usable bandwidth fraction** | The channelizer masks band edges where filters roll off and images appear. Example: the Airspy at 10 MSPS is about 9 MHz clean. |
| Native sample format (packed 12-bit / i16 / i8 / f32) | The IQ ring stores the native format |
| Gain model: named stages plus linearity/sensitivity presets | One gain control in the UI for every device |
| IF architecture: real-IF / zero-IF | On zero-IF devices, offset-tune and mask the DC spike |
| Native sweep support | Use the hardware sweep, otherwise fall back to generic retune-and-dwell |
| Number of channels, external clock input, bias-T, hardware timestamps | Multi-channel devices, future coherent work |

Devices are identified by **serial number**. Per-serial settings live in config:
- ppm correction
- gain table → dBFS→dBm calibration offsets
- passband flatness
- preferred roles

### 4.3 The stream contract

Every backend emits the same stream, including Soapy and file sources. It's a sequence of **sample blocks** numbered by a running **sample counter**, interleaved with **in-band events**:

- `Retune { at_sample, center_hz, rate }`: downstream always knows which samples belong to which tuning. The waterfall, records and look-back never mix two tunings.
- `Gap { at_sample, lost_samples }`: a USB overflow or drop becomes explicit. Detectors know a burst was cut off and don't decode garbage.
- `TimeAnchor { at_sample, host_time, [hw_time] }`: maps the sample counter to the wall clock. Records from different SDRs share one timeline.
- `GainChange`, `Overload` (ADC clipping), `DeviceLost`.

The driver's USB callback **only copies data into a lock-free ring**. It never blocks, allocates or does DSP.

### 4.4 Roles

Roles state requirements. They never name devices.

| Role | Needs | Natural fit |
|---|---|---|
| **Survey** | Frequency span to cover, revisit time | HackRF (native sweep) |
| **Focus** | Contiguous bandwidth, dynamic range | Airspy / HydraSDR |
| **Park** | One fixed frequency, no retunes (control channel, pager frequency) | RTL-SDR |
| **Follow** | Fast retune on command (trunked voice grants, hand-off from Survey) | Any free device |

The **role assigner** scores each attached device against each role's needs and proposes an assignment. The user can override it. When devices disappear, the app degrades rather than breaks. For example, without a HackRF the Airspy can still survey by stepping, just more slowly. **Hand-off**, like Kestrel's dual-receiver mode: Survey finds something, and the user or a rule sends a Focus or Follow device to it.

## 5. Engine

### 5.1 IQ ring (look-back)

- Holds the last *N* seconds of wideband IQ **in the device's native format**. At 10 MS/s int16 that's 40 MB/s, half the size of f32. Samples are converted to complex float only when read.
- RAM tier by default, configurable. 10 s ≈ 400 MB at 10 MS/s.
- An optional **disk-backed rolling tier**: 40 MB/s is trivial for an NVMe drive. That gives hours of look-back and SCEPTRE-style unattended capture.
- Readers: the FFT (live edge), the channelizer (live edge or rewound), detectors that confirmed late (rewound to the start of the burst), the user (scrubbing the waterfall), and export ("save this region as SigMF").

### 5.2 Shared FFT

- One overlap-save FFT stream per source, sized for both the channelizer and display. For example, a 64k-point FFT at 10 MS/s gives ~153 Hz bins.
- The same FFT output feeds the **spectrum/waterfall frames**, the **burst detector** and the **channelizer**.

### 5.3 Burst/energy detection

- A noise-floor estimate per bin (minimum statistics or median over time) plus a CFAR-style threshold.
- Connected regions in time and frequency become **candidate emissions**. Each one opens or extends a signal record: centre, occupied bandwidth, start and end time, peak and mean power.

### 5.4 Channelizer

- A **fast-convolution channelizer**: each channel takes its FFT bins times a filter response, then a small inverse FFT, which gives decimated complex baseband.
- Channels can be **added or dropped at any centre and bandwidth at runtime**, cheaply. This is why we don't use GNU Radio for the core.
- The output rate is chosen per consumer. For example, the symbol engine wants 10 samples per symbol.
- Channels can be **back-filled from the IQ ring**, so a channel created late still starts at the beginning of the burst.

## 6. Signal records

The core data object. Every view in the UI is a view of records.

```
SignalRecord {
  id, source_id, role,
  t_start, t_end,                    // shared timeline (TimeAnchor)
  center_hz, bandwidth_hz,
  power { peak_dbfs, mean_dbfs, [dbm if calibrated] },
  features {                         // §7.2
    envelope_const, mod_family, fsk_levels, symbol_rate, deviation_hz,
    duty_cycle, burst_period, psk_on_fm, …
  },
  classification [ { label, confidence, by } ],   // e.g. "DMR BS voice", 0.99, "sync-bank"
  decoded { protocol-specific fields: IDs, talkgroups, pager text, … },
  iq_clip → SigMF file (optional),
  tags [ … ], notes,
  group_id                           // links repeats of the same emitter
}
```

Storage: **SQLite** for records, tags and groups, plus **SigMF** files for IQ clips. Grouping links records that look like the same emitter: same frequency, same features, same IDs. That turns "this 12.5 kHz 4FSK thing every 30 s" into one entity with a history.

## 7. Detection and classification

### 7.1 The symbol engine (per channel)

One reusable engine with **all state per channel**.

1. **Normalize the input** to a fixed samples per symbol (10), using the channelizer's output rate. For FSK/FM protocols, demodulate first with a quadrature discriminator on complex baseband. The front end becomes ours, not FMP24's.
2. **Lookahead delay ring**: the decoder runs behind the detector, so the detector sees the future.
3. **Sync search at every sampling phase**: 10 parallel phase channels, each with its own sign-bit history. No timing recovery is needed before sync.
4. **Masked sync/signature tables as data** `{rate, len, pattern, mask, proto, frame_period, …}`. A new protocol is mostly a table entry. Hamming-distance tolerance is configurable per entry. POCSAG, for example, allows 1–2 bit errors.
5. **Gates:** rate class (zero crossings plus hysteresis), amplitude consistency (mean|x|/|x_i| between 0.4 and 2.5), and **two-sync confirmation** within one frame period + 3%.
6. **Timing from syncs**: phase commits at the sync position; drift is estimated from the spacing between syncs.
7. **Adaptive slicer** (2- or 4-level): levels from the sync symbols, DC tracking on the inner symbols, an adaptive inner-threshold ratio, an ISI boost for isolated symbols, and **re-slicing** of buffered symbols after the levels change.

First protocol sets:
- **POCSAG** (512/1200/2400) and **FLEX**
- **DMR**, **P25 Phase 1/2**, **NXDN**: the published sync words, as metadata
- Later: dPMR, D-STAR, YSF, ProVoice, and ISM pulse-timing signatures (rtl_433-style, with repeat confirmation)

### 7.2 Features for every record (including unknowns)

- Occupied bandwidth and spectral shape
- **Constant envelope or not** (FM/FSK/PSK vs AM/OOK/QAM)
- Instantaneous-frequency histogram (number of FSK levels, deviation)
- **Symbol-rate estimation**: zero crossings, the spectral line of the squared envelope, cyclostationary features
- **PSK-on-FM test**: a phase-binned energy ratio. With raw IQ it's simply one more feature.
- Burst timing: duration, period, duty cycle, hop pattern

### 7.3 Classifier layers

1. **Sync bank**: definitive when a protocol is confirmed.
2. **Rules on features**: e.g. "12.5 kHz, 4FSK, 4800 baud, bursts every 60 ms → DMR-like".
3. Later, a **small ML modulation classifier** trained on synthetic data plus our own captures, for the Aaronia IQ Pulse Inspector-style "pick a burst → ASK/FSK/PSK/QAM → bits".

## 8. Decoders and plugins

- **In-tree modules** behind cargo features for our own detectors and decoders.
- **External-process decoders**: the engine feeds a channel's IQ or audio to a child process's stdin and reads JSON events back. That's how existing tools (multimon-ng, AIS-catcher, direwolf, …) can be used **without owning an SDR**. It's the opposite of iNTERCEPT.
- Rust has no stable ABI, so native dynamic plugins wait until there's a real need. A C-ABI boundary would be the way then.

## 9. UI

Built with egui/eframe + wgpu, using an egui_dock layout. The bar is SDR++-level smoothness.

**Panes:**
- **Spectrum + waterfall**, the main view, with signal records drawn as labelled boxes
- **Signal list**: filter, sort, search
- **Record inspector**: features, decoded data, IQ clip tools
- **Timeline**: records over time, grouped by emitter
- **Sources and roles**: devices, capabilities, assignment, gain
- **Map**: later, for walk-around signal-strength mapping and site plans

**Waterfall rules:**
- GPU **ring texture**: one row uploaded per FFT frame, colour map applied in a shader. Scrolling and zooming are just texture-coordinate changes.
- **The FFT rate is independent of the UI frame rate**. The UI draws the newest frame at the display's refresh rate.
- Power is averaged in the linear domain, with peak-hold and decay traces. When there are more bins than pixels, take the **max per pixel** so narrow signals survive.
- **Look-back scrub**: pause the live view, drag back in time, select a region, then decode it or export it as SigMF.

**Interaction:** drag-to-tune, scroll-wheel zoom centred on the cursor, digit-wheel frequency entry, no modal dialogs during live work. Dark theme, one good font, a few good colour maps.

## 10. Threading and performance

- **Per source:** a USB callback thread, which only copies into the ring, plus an FFT/channelizer thread.
- **Detectors** run on a worker pool. Each channel's state is independent, so this is embarrassingly parallel.
- **UI thread**: never blocks on the engine. It reads the latest snapshot through the `api` layer.
- **Budget (estimates, not benchmarked):**
  - Sync search is about 2M integer ops/s per channel.
  - Slicer, DC tracking and drift correction are trivial.
  - The channelizer handles filtering for all channels together.
  - Hundreds of channel detectors on a desktop CPU looks plausible. **Benchmark early.**
- SIMD through `rustfft` / `realfft`, plus explicit SIMD where profiling says it's needed.

## 11. Proposed workspace layout

```
SpecTrawl/
  Cargo.toml              workspace
  crates/
    sdr-core/             Device trait, Capabilities, stream contract, events
    backend-airspy/       (+ hydrasdr, if the APIs are close enough to share)
    backend-hackrf/
    backend-rtlsdr/
    backend-soapy/
    backend-file/         SigMF read/write
    dsp/                  FFT, filters, channelizer, demodulators
    engine/               rings, burst detection, records, role assigner, scheduler
    detect/               symbol engine, sync tables, protocol detectors, features
    store/                SQLite + SigMF clips
    api/                  engine ⇄ UI contract
    app/                  eframe GUI
  testdata/               SigMF captures (large files kept out of git or in LFS)
  docs/
```

Start with fewer crates and split them when the boundaries prove themselves. `sdr-core` and `api` are the two interfaces that should be kept stable from day one.

## 12. Testing

- **SigMF corpus**: real captures from our hardware, labelled with what's in them. Record them as soon as milestone 1 works.
- **Synthetic generators**: POCSAG, 4FSK/C4FM, and DMR-style bursts with controllable SNR, frequency offset, drift and ISI. Each detector gets **detection-rate and false-alarm tests** against them.
- **Golden-output tests**: known captures → expected records.
- **CI**: Windows + Linux builds and tests on every push.

## 13. Scope: how close to the commercial tools

| Layer | Realistic outcome |
|---|---|
| RF hardware (bandwidth, frequency range, dynamic range) | **Ceiling set by hardware.** We have 10–20 MHz of bandwidth up to 6 GHz. Better hardware (Signal Hound, USRP) can be added later as backends. |
| Engine, visualization, IQ record/playback, look-back, hand-off between receivers | **Fully achievable.** |
| Land-mobile, pager and ISM protocol detection | **Can beat the commercial tools.** It's our specialty. |
| Automatic modulation classification | Good, with iteration (research-flavoured). |
| Wi-Fi / LTE / BLE | Detect and label by spectral shape only. Decoding them is out of scope. |
| TSCM workflow: baselines and differences, site/floor-plan tagging, licence databases (FCC ULS, ISED), reports | **Very achievable.** It's mostly database and UI work. |
| Geolocation | Signal-strength walk mapping: yes. TDOA/FDOA: stretch goal. |

## 14. Build order

1. **Skeleton:** `sdr-core`, the Airspy and SigMF-file backends, the IQ ring, the shared FFT, and the egui app with a GPU waterfall, pause and look-back.
   - *Done when* it shows a smooth live waterfall from the Airspy, you can scrub back 10 s, and you can export a SigMF clip.
2. **Burst detection → signal records:** the signal list, boxes on the waterfall, SQLite storage.
3. **Channelizer + symbol engine + POCSAG**, the first end-to-end detector, with synthetic tests.
4. **DMR / P25 / NXDN** detection and metadata.
5. **Features for unknown signals**, rule-based classification, grouping by emitter.
6. **HackRF sweep (Survey role), RTL-SDR (Park role), the role assigner and hand-off.**
7. Soapy backend, HydraSDR backend, FLEX, ISM signatures, TSCM baselines, a remote node over the network.

## 15. Open questions

Resolve each one when the work actually needs it, not before.

- Project licence (GPL vs MIT/Apache). Check the driver libraries' licences first; dynamic loading helps.
- Does the HydraSDR API match libairspy's closely enough to share a backend?
- Exact `api` transport for remote nodes (QUIC? plain TCP + a binary protocol?).
- Disk-tier format for the IQ ring (raw segments + index vs SigMF segments).

## 16. Legal note

SpecTrawl is a receive-only analysis tool. Laws on intercepting and disclosing communications content vary. In the US, for example, the ECPA covers some paging and cellular content. SpecTrawl does no decryption, and the decode features should make it easy to log metadata without content.
