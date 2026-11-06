# SpecTrawl Rebuild, Step 7: onair-rx

> On branch `rebuild-step-7` (worktree `.worktrees/step-7`).

> **Changed course 2026-11-06 (owner): OnAir is not the tool for this job.** What it would take is below: stock `onair-server` reports nothing it decodes, and air band runs only from the desktop app. The `onair-rx` crate was removed (it is in history at 264141f). `engine::iqserve` and `engine::workers` stay, for any program that reads rtl_tcp. Air band is native: see "Air band, native" at the end.

**Goal.** Spec §6 step 7: "`onair-rx` worker supervisor and protocol, with airband AM first (it is the multi-channel case), then the land-mobile modes, then the rest of the catalogue in the order the survey meets them. A GPL fork of OnAir carrying the JSON export patch lives in its own repository."

**Spec:** §4.6 (DecoderBank, OnAir's decoders, the licence boundary, the worker protocol), §5 (the slots), §9, §10 decisions 1–2.

## What OnAir offers (read from its repository, v3.6.0, 2026-11-04)

- **`onair-server`** (`docs/headless-server.md`) is OnAir without a window: one receiver on one radio at a time.
  - It is controlled over HTTP/1.1 and WebSocket on `127.0.0.1:8765`, the remote API of `docs/REMOTE-API.md`: `/api/v1/status`, `/modes`, `/mode`, `/frequency`, `/samplerate`, `/gain`, `/start`, `/stop`, `/telemetry`, `/events`, `/ws`. Loopback needs no token.
  - Its telemetry JSON "carries the main figures of each mode". Decoded messages arrive as JSON lines: `{"v":1,"type":"pager.message","time":…,"mode":…,"freqHz":…,"fields":{…}}`.
  - Its settings come from a `key=value` file given with `--config`: `mode`, `freq_mhz`, `rate_msps`, `net_radio`, `autostart`, `port`.
- **IQ in:** `net_radio=rtltcp,host,port`.
  - A server whose greeting is not `RTL0`, or whose tuner type is outside 1–6, is "another radio": it is asked for any rate from 0.25 to 20 MS/s, and its gain in tenths of a dB (`core/src/source/source_rtltcp.cpp`).
  - Commands are rtl_tcp's: 5 bytes, a command byte and a big-endian u32 (0x01 frequency, 0x02 rate, 0x03 gain mode, 0x04 gain, 0x08 AGC, …).
  - Samples are 8-bit offset IQ.
- **`onair-cli --standard <id>`** prints human-readable lines, not JSON. The worker is therefore `onair-server`.
- **Air band:** the "Airband" mode: AM voice on several 25 and 8.33 kHz channels at once, with scan and priority.
- **Distribution:** releases carry a portable Linux x86_64 build (`onair-3.6.0-linux-x86_64-portable.tar.gz`, 143 MB, checked against the release's `SHA256SUMS`) and a Windows zip.

## Running OnAir here (decided with the owner, 2026-11-05)

- **Where it lives:** OnAir lives in `~/SpecTrawl/onair/`, ignored by git (`.git/info/exclude` and `.gitignore`), so no OnAir file enters SpecTrawl's tracked tree:
  - the checksummed release build: `release/3.6.0`;
  - a shallow clone of its repository: `OnAir/`;
  - later, any fork or variant;
  - workers' run folders under `run/`.
- **Containment:** a worker runs with `HOME` and `XDG_CONFIG_HOME` set to its run folder. Beyond its `--config` file it writes only crash markers there.
- **Never a real radio:** `onair-server` opens the first radio it finds unless its settings name one. It tried the R2 the first time. A worker's settings always say `device=rtl_tcp`, and the supervisor checks `/api/v1/status` says so, or it kills the worker.

## What was verified with OnAir 3.6.0 (2026-11-05)

- **The worker reaches the feed:** with `net_radio=rtltcp,127.0.0.1,<port>`, `device=rtl_tcp`, `rate_msps=2.5` (or 1.25), OnAir connects to `engine::iqserve`. Its log reads *"SPTW", tuner type 0, 0 gains: not an RTL-SDR, any rate up to 20 Msps*. It asks for no other rate, sends one gain command, and tunes 50 kHz off its dial (it keeps DC off its channels).
- **FLEX decodes through it:** the FLEX pager at 929.6125 MHz, served from the R2 at 1.25 MS/s, decoded "locked", with 5367 blocks OK and 969 bad (85 % passing FEC), quality "good". No page events came: SpecTrawl's own bank found no pages in those frames either.
- **What the stock server reports, per mode:**
  - `/api/v1/telemetry` has the common block (`state`, `locked`, `snrDb`, `cfoHz`, `blocksOk`, `blocksBad`, `quality`) for every mode. Per-mode `details` exist only for DVB-T/T2, DVB-S/C, ATSC, DAB, FM, DTMB, LBJ, Horus and the railway modes (`core/src/results/rx_json.cpp`).
  - Decoded messages come through `/api/v1/events`.
  - Voice modes log calls as sessions (`/api/v1/sessions`).
- **The full worker path with the real OnAir:** `onair_air` drives the factory, supervisor and client through the engine. Two minutes on the FLEX pager: locked from the first poll, 11043 blocks OK and 1277 bad, no restarts.
- **The stock server reports no decoded content:**
  - `onair-server` 3.6.0 implements no message stream (`/api/v1/events`: *"no message stream in this program"*) and no sessions. Only the desktop app hooks OnAir's automation engine into its event ring (`app/platform/remote_api_app.cpp`, `applyStreamer`).
  - So through stock OnAir a worker gives its lamp and counters, never a page, a call or a channel.
  - The patch is the same hook in `server/server_host.cpp`: an `apipol::EventRing`, `eventsSince`, and `automation::global().setStreamer(...)`. Decoders emit to the automation engine from the core (ACARS, AIS, VDL2, ADS-B, …), so the server would see them.
- **Air band does not work headless in stock OnAir:**
  - the receiver decodes only the channels in its list, which the desktop app sets from its airport search and the server cannot set;
  - its telemetry has no air band details;
  - it writes no sessions.

  This is the patch the spec foresaw: a `airband_channels=` server setting (or one that takes every raster channel in the capture) and air band in the telemetry JSON (channels, open, level, last heard). It is built from source in a fork under `onair/`.

## Architecture

- **`engine::iqserve`, an rtl_tcp server for one bus channel.** SpecTrawl's side of the worker protocol. It needs nothing from OnAir and is built first.
  - **Listening:** `127.0.0.1` on a port of its own; one client at a time, a new one replacing the last.
  - **Greeting:** `SPTW`, tuner type 0, no gains, so OnAir takes it as "another radio" at any rate.
  - **The channel:** a bus subscription at the worker's priority, centred where the client tunes (inside the source's span), at a rate the channelizer gives exactly (`fs·K/N`: 2.5 or 1.25 MS/s at 10 MS/s). A rate command for any other rate is logged and the served rate kept: the worker is configured with `rate_msps` set to the served rate, so it never asks for another.
  - **Sample format:** a slow AGC holds the RMS at 16 LSB (18 dB under clipping), like a dongle's IF gain. The gain the worker sets is ignored.
  - **Back-pressure:** samples go to the socket from a writer thread through a bounded queue (0.5 s). A slow client loses whole blocks, counted, and never holds a bus worker.
  - The engine owns the threads and joins them on drop.
- **`onair-rx`, the crate.** It holds the process supervisor and the protocol client only (spec §4.6: no OnAir code, header or binding).
  - **Start:** `onair-server --config <SpecTrawl's own file>` per mode per span, its port free, `net_radio` pointing at the span's `iqserve`, `autostart=1`.
  - **Health:** `/api/v1/status`.
  - **Telemetry:** from the WebSocket (`{"telemetry":true,"events":true}`), into the `Decoder` trait's telemetry and claims.
  - **Restart** with a backoff; stopped when nothing uses it.
  - Which implementation serves a mode is a registry entry the app passes to the engine. The engine never depends on `onair-rx`.
- **`docs/decoder-worker.md`:** the protocol as SpecTrawl speaks it, so a worker can be anything.

## Building the patched OnAir: blocked (2026-11-05)

- **What's missing:** OnAir builds with CMake and Ninja. It needs libusb, FFmpeg (installed here) and libhackrf (not installed; GLFW is needed only for the desktop app).
- **What was tried:** building libhackrf from its upstream source into `onair/deps/prefix`, so nothing would be installed system-wide.
- **What happened:** the session's permission check refused it. Building the patched server from source waits on the owner. Two routes:
  - allow source builds under `onair/`;
  - or install `hackrf` from the distribution, then allow the OnAir build.

## Tasks

1. `engine::iqserve`, with tests. **Done.**
2. `docs/decoder-worker.md`. **Done.**
3. **The engine's worker interface** (`WorkerFactory`, `Worker`, `Command::StartWorker`/`StopWorker`, worker views, messages as claims). **Done.**
4. ~~`onair-rx`~~: built, verified with OnAir 3.6.0 on FLEX (lamp and counters only), then removed with the change of course.
5. **Air band, native** (below).
6. **The land-mobile modes**, native: DMR and NXDN control and voice-channel monitors, FLEX and POCSAG monitors.

## Air band, native

- **What the user gets:** the CHANNELS list (spec §5) for every air band in a source's span (118–137 MHz civil, 225–400 MHz military): each channel on its raster, open now or not, its level, its transmissions and airtime, and when it was last heard. A transmission plays from its record (step 5's replay).
- **Built on the records, not a second detector:** each record in an air band, snapped to the band's channel raster, is a transmission on that channel. The bank's look (the carrier rung) says whether it is AM voice; a record that isn't (a birdie, a hump of floor, a data burst) is not counted as a transmission.
- **Claims:** a channel monitor claims "Air band AM" on a record it counts, filed under `decoder`, with the channel as its identity; a record it rejects keeps the bank's verdict. So the record, the emitter and the CHANNELS row read the same thing.
