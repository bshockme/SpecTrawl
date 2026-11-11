# SpecTrawl

*Spectral trawling.* Point one or more SDRs at a stretch of spectrum. SpecTrawl finds every emission in it, records each one, and works out what it is. It decodes what it can, and it shows the evidence behind every label.

![The Records view: P25 traffic on the 800 MHz band, one record selected with its evidence](docs/images/records.png)

*Two receivers on air. Receiver A, an Airspy R2, watches an 800 MHz P25 site. B, a HydraSDR RFOne, watches the air band. The selected record is a 22-second P25 call. Its claims are listed with what produced each one and how sure it is:*
- *the band plan's guess (inferred, 20%);*
- *the sync bank (12 syncs, 11 of 11 frames, 97%);*
- *the decoder (NAC, talkgroup and source radio, 95%).*

> **Status:** a rebuild is under way. Steps 0–6 of 9 are on `master`; step 7 (native air band and land-mobile decoders) is on the `rebuild-step-7` branch. See [Status](#status).

## Contents

- [What it does](#what-it-does)
- [Screenshots](#screenshots)
- [What it recognises](#what-it-recognises)
- [Hardware](#hardware)
- [Building and running](#building-and-running)
- [Headless tools](#headless-tools)
- [Testing](#testing)
- [How it is built](#how-it-is-built)
- [Status](#status)
- [Documentation](#documentation)
- [Licence and legal](#licence-and-legal)

## What it does

SpecTrawl is signal-centric: you work on **records**, not on tools.

- **Finds** each burst or carrier in a radio's whole span, live, and turns it into a record. A record holds its frequency, bandwidth, start, duration, peak and mean power, and SNR. Records of one transmitter group into an **emitter**.
- **Looks back.** Each radio's IQ stream is kept in a RAM ring (30 s guaranteed by default, more when memory allows). Any record can be replayed or exported after it has ended.
- **Classifies** every live record. A pool of workers runs a multi-phase sync search and shape measurements on each one. Records of a protocol it can decode get a **decoder** attached, which follows the channel.
- **Shows evidence.** Every label is a *claim* that says what produced it, how sure it is and what it saw. Labels inferred from the band plan or a record's position look different from decoded ones. "Unknown" is an allowed answer, and no record is named after a digital mode without a sync or a decode.
- **Keeps** records, tags and notes in a SQLite store across restarts. A channelised IQ slice (SigMF) is kept for each record, under a disk cap; **Save IQ** copies a record's slice to `saved/`, outside the cap.
- **Replays** a record as audio (AM, NFM or WFM) from the ring, or from its slice once the ring has moved on, and saves it as WAV.

One wideband stream per radio is shared by everything. Channels are cut from a single FFT per radio, so no feature opens the device or down-converts the full-rate stream on its own.

## Screenshots

All three are from the same run on air (Saturday afternoon), drawn by the app itself; see [`readme_shots`](#headless-tools).

**Systems.** The detector bank found two P25 control channels in the span, and each got a native decoder. One row per site: NAC, WACN, system, RFSS and site, with control-channel messages (TSBKs) and grants counted.

![The Systems view: two P25 control channels decoded](docs/images/systems.png)

**Channels.** Air band calls are measured for AM voice and listed per 25 kHz channel. ACARS bursts on 130.450 MHz are decoded natively.

![The Channels view: air band voice and ACARS](docs/images/channels.png)

## What it recognises

| Kind | What SpecTrawl does with it |
|---|---|
| **P25 Phase 1** | Control channels get a decoder: site identity, TSBKs and grants, shown as a system row. Traffic channels are named with their NAC, talkgroup and source radio. |
| **P25 Phase 2** | Identified by sync. TDMA frames decode when the site's scrambling parameters are known, which works for some channels so far. |
| **DMR** | Base-station data: colour code, CSBKs, voice link control (talkgroup and source), and Capacity Plus system status and rest channel. A monitor follows each repeater. |
| **NXDN** | Identified by sync (NXDN48/96). A monitor exists, but nothing has been tested on air yet. |
| **dPMR, YSF, ProVoice** | Identified by sync only. |
| **FLEX pagers** | Decoded natively, including simulcast fading, with soft repair of damaged words. |
| **POCSAG pagers** | Decoded natively. Pages repaired from soft decisions, or cut short, are marked `[damaged]` and never shown as whole. |
| **ACARS** | Decoded natively (MSK, CRC-checked). |
| **Air band voice** | Calls are tested for AM by their sidebands and listed per channel (CHANNELS view). |
| **Analog FM** | Recognised by its shape. CTCSS tones and DCS codes are read. |
| **Broadcast FM** | Recognised by its 19 kHz pilot. |
| **Anything else** | Described by shape, never named: bursts and their period, FSK and its symbol rate, OFDM and its subcarrier spacing, a bare carrier, wideband. |

Voice codecs (AMBE/IMBE) are not decoded, and nothing is decrypted.

## Hardware

| Radio | Notes |
|---|---|
| **Airspy R2 / Mini** (R0/R1 too) | Through `libairspy`, loaded at run time. |
| **HydraSDR RFOne** | R2-compatible. Through `libhydrasdr` (the `hydrasdr-host-tools` package). Its sweep firmware lets Sweep mode sweep on the board, at about 375 hops/s; see [docs/hydrasdr-sweep-mode.md](docs/hydrasdr-sweep-mode.md). |
| **Simulated SDR** | Always present. Generates noise and FSK, FM, CW and P25-like emitters at fixed frequencies, for UI work and the test suite. |

Driver libraries are loaded at run time, not linked. If a library is missing, its radios don't appear. To point at a specific build:

| Variable | Use |
|---|---|
| `SPECTRAWL_AIRSPY_LIB` | Path to `libairspy.so` / `airspy.dll` |
| `SPECTRAWL_HYDRASDR_LIB` | Path to `libhydrasdr.so` / `hydrasdr.dll`. Needed for board sweeps until the patched library is packaged. |

Two radios at 10 MS/s need separate USB controllers. On one shared hub each gets about half its samples, and the app warns `LOSING n% OF SAMPLES`.

## Building and running

You need a stable Rust toolchain (the repository pins `stable` with rustfmt and clippy). On Linux you also need the windowing and audio development packages:

```sh
# Debian/Ubuntu (what CI installs)
sudo apt-get install -y libgtk-3-dev libasound2-dev libxkbcommon-dev libwayland-dev libudev-dev pkg-config
# Arch
sudo pacman -S --needed gtk3 alsa-lib libxkbcommon wayland systemd-libs pkgconf
```

Then:

```sh
cargo run --release -p spectrawl-next     # the app (new engine)
```

1. In **Devices**, choose **Open** on a radio. It becomes receiver A, B, and so on, with its own tab.
2. Set its frequency in the receiver's header. **Live** watches a 10 MHz span; **Sweep** surveys a wider range.
3. Click a box on the waterfall, or a row in **Records**, to see that record's claims. **Play** listens to it; **Save audio** writes the demodulated sound as a WAV; **Save IQ** keeps its IQ slice (`Documents/SpecTrawl/saved/record-<id>.sigmf-*`) and tags it `saved`.
4. **Systems** lists decoded trunking sites and every running decoder. **Channels** lists air band activity per channel.

| Where | What |
|---|---|
| `~/Documents/SpecTrawl/records.db` | The record store (`%USERPROFILE%\Documents\SpecTrawl` on Windows) |
| `~/Documents/SpecTrawl/slices/` | One SigMF IQ slice per record, cf32, under a 4 GB cap |

Debug builds optimise dependencies (`opt-level = 3`) and the workspace itself lightly, because DSP is unusably slow unoptimised. Use `--release` on air.

The first build's app is still in the tree as `spectrawl-old` (binary `spectrawl`) until step 8 of the rebuild replaces it.

## Headless tools

These run the engine on a radio without the window and print what it found. `<serial>` is the radio's serial as the Devices pane shows it.

```sh
# Detector bank and decoders for <secs>: decoders, P25 systems, emitter headlines, disagreements.
cargo run --release -p engine --example classify_air -- <serial> 855.5 180

# Air band: CHANNELS, ACARS, each raster channel's calls.
cargo run --release -p engine --example airband_air -- <serial> 130.0 600

# Records of 1 s and 10 dB or more, written as WAV from the ring and again from their slices.
cargo run --release -p engine --example replay_air -- <serial> 127.0 180 out/

# A record store, tuned through several stops ("MHz:secs,...").
cargo run --release -p engine --example records_db -- airspy <serial> my.db "855.5:150,930:90"

# One channel served as rtl_tcp, for programs that read it.
cargo run --release -p engine --example iqserve_air -- <serial> 855.5 855.35 2.5 600

# This README's screenshots, drawn offscreen by egui's wgpu renderer.
cargo run --release -p spectrawl-next --example readme_shots -- docs/images 180 \
    <R2 serial>@855.5 <RFOne serial>@127.0 --db <RFOne serial>=-92,-42 --records P25
```

`classify_air` and `airband_air` keep each record's slice when `PROBE_SLICES=<dir>` is set, and print every record when `DUMP=1` is set.

## Testing

```sh
cargo test --workspace
cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p api -p engine -p spectrawl-app -p spectrawl-next --all-targets -- -D warnings
```

CI runs both on Windows and Ubuntu for every push.

- **Simulator tests** run the whole engine in real time on the simulated radio: the ring, channelizer and bus, records, classification, decoders, replay, the app headless.
- **Real-capture tests** (`crates/detect/tests/real_captures.rs`, `crates/engine/tests/bank_captures.rs`) run on short SigMF slices recorded on air, committed under `testdata/small/`. A decoder change has to keep their verified results.
- **Large captures** are release assets listed in `testdata/captures/MANIFEST.toml`. Fetch them with `tools/fetch_captures.sh` (or `.ps1`). A test whose capture is missing **fails**, it does not skip. Set `SPECTRAWL_SKIP_CAPTURES=1` to skip them locally.
- **Rules tests** (`crates/engine/tests/rules.rs`) check the architecture rules in the code itself. For example, only the source holds the device, and only the stage, sweep and export read the full-rate ring.

## How it is built

```
app (egui)  ── reads Snapshots, sends Commands; depends on api only
   │
api         ── the engine/UI contract: plain data, commands, acknowledgements
   │
engine      ── per radio: IQ ring ─► one FFT ─┬─► spectrum rows ─► burst detector ─► records/emitters ─► store
               │                              └─► channelizer ─► channel bus ─► classify pool · decoders · replay · slices · rtl_tcp
               timeline: every sample time comes from the radio's anchors, not the wall clock
   │
sdr-core    ── Backend/Device traits and the stream contract
backend-airspy · backend-sim
```

| Crate | Role |
|---|---|
| `crates/api` | The contract between engine and UI: `Snapshot`, `Command`, `Handle` |
| `crates/app` | The egui interface (`spectrawl-app`): receiver panes, Records, Systems, Channels, Devices, Log |
| `crates/launcher` | `spectrawl-next`: starts the engine and the window |
| `crates/engine` | Ring, stage (FFT and channelizer), channel bus, records and emitters, classify pool, decoders, replay, store, recorder, sweep, rtl_tcp server |
| `crates/dsp` | FFT and spectrum rows, overlap-save channelizer, filters |
| `crates/detect` | Signal features, the sync bank and the protocol decoders (P25, DMR, NXDN, FLEX, POCSAG, ACARS, RDS, …) |
| `crates/sdr-core` | Hardware abstraction shared by every backend |
| `crates/backend-airspy` | Airspy and HydraSDR, through their libraries loaded at run time |
| `crates/backend-sim` | The simulated SDR |
| `crates/catalog` | Talkgroup, radio, site and network names imported from other tools' files |
| `crates/engine-old`, `crates/app-old` | The first build, kept runnable until step 8 |

## Status

The first build (October 2026) found signals well but explained them badly. The [rebuild](docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md) keeps the code proven on air (`detect` and the hardware layer) and replaces the engine and app, one step at a time:

| Step | | |
|---|---|---|
| 0–3 | Repository hygiene and CI; ring, segments and timeline; FFT, channelizer and bus; `api` and a minimal app | done |
| 4, 4a | Records, claims and the store; sweep on the new engine (board sweep on the RFOne) | done |
| 5 | Replay from the ring and from slices | done |
| 6 | Detector bank, classify pool, P25 control decoder and systems | done |
| 7 | Native air band (AM voice, ACARS, CHANNELS) and land-mobile monitors (DMR, NXDN, FLEX, POCSAG) | on `rebuild-step-7` |
| 8 | Presentation: inspector, catalogue, dock layouts, the new app as default | next |
| 9 | Explore, scan, RDS, cell monitor, voice following and call logs re-homed on the bus | |

[BACKLOG.md](BACKLOG.md) lists every requirement not yet built, with dates and on-air results. Nothing is dropped without a line there.

## Documentation

| File | What |
|---|---|
| [DESIGN.md](DESIGN.md) | The original design: goals, signal model, roles, the detection approach |
| [docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md](docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md) | The rebuild: architecture, build order, process rules, decisions |
| [BACKLOG.md](BACKLOG.md) | What is left, and what each step found on air |
| [docs/ui-design.md](docs/ui-design.md) | The interface design |
| [docs/hydrasdr-sweep-mode.md](docs/hydrasdr-sweep-mode.md) | The RFOne's on-board sweep |
| [docs/onair-code.md](docs/onair-code.md) | The ledger for any code taken from OnAir (none yet) |
| [docs/classification-review.md](docs/classification-review.md), [docs/algorithm-review.md](docs/algorithm-review.md) | Reviews of classification results and algorithms |

## Licence and legal

SpecTrawl is MIT-licensed; see [LICENSE](LICENSE). Code taken from OnAir (GPL-3.0-or-later) is allowed where it helps, and would make the crate holding it GPL. Each inclusion is listed in [docs/onair-code.md](docs/onair-code.md). None has been taken so far.

SpecTrawl only receives. It does not transmit, and it does not decrypt. The law on listening to radio traffic, and on recording or passing on what you hear, differs between countries and services. Know the rules where you use it.
