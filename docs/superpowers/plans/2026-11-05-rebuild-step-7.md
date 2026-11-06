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

- **What the user gets:** the CHANNELS list (spec §5): every 25 kHz raster channel in a source's span (118–137 MHz civil, 225–400 MHz military) that something was heard on. Each row has open now or not, its voice, data and carrier-only records, airtime, peak level, last heard, and the newest decoded summary. Clicking a row selects its newest record, which the Records tab plays (step 5's replay).
- **Built on the records, not a second detector** (`engine::decoders::airband`):
  - A record on a raster channel (within 8 kHz: offset-carrier sites put theirs up to 7.5 kHz off; at most 25 kHz wide) that the bank's look called AM gets an "Air band voice" claim (`Decoder`, mode "Air band AM", 0.8), filed under `airband`, with the channel as its identity (`Identity::Channel`). Its emitter then headlines as the channel's voice.
  - A record another decoder named (ACARS) is data, not voice.
  - A record with neither, on a channel that has one, is that channel's carrier: an AM call's pauses are its bare carrier, and the detector records them as their own narrow records.
  - A channel with only carrier records isn't listed: a birdie sits on a raster point as readily.
  - The claims are refreshed every decoder-manager tick (200 ms) from the book's view; a later look that no longer says AM withdraws the claim.

### What the air band looked like before (2026-11-06, the R2 at 124.9 and 130.0 MHz)

- **Every call read "Carrier":**
  - The detector records an air band call 0.6–4 kHz wide: its carrier, plus whatever sidebands clear its threshold in 610 Hz bins.
  - The modulation rung called anything under 4.5 kHz without FSK levels "Carrier", before the AM test.
  - On 53 looks at 118–129 MHz, all 0.6–4 kHz records read "Carrier". A strong 6.1 kHz call at 128.375 MHz got no claim at all.
- **The fix (`detect::carrier::coherent_am`):**
  - The look's window is a 12.5 kHz channel, which holds the sidebands the detector missed.
  - It measures the voice band (300–2500 Hz) in phase with the carrier against the same band in quadrature and against 3.5–4.8 kHz in phase.
  - AM puts its modulation in phase. Noise splits evenly. FM and phase noise sit in quadrature.
  - On air, bare carriers measured −0.3 to +0.6 dB on both; calls measured 3–6 dB on both. The threshold is 2 dB.
  - The narrow branch now says "AM" when it holds. A weak call with nothing measurable stays "Carrier", which is honest.
- **ACARS** is AM too (MSK data, 0.1–0.9 s bursts on 129.125–131.725 MHz). Through the coherent test alone it would have read as AM voice, so it has its own rung.

### ACARS, native (`detect::acars`, the bank's identifiers)

- Written from the protocol's description:
  - AM, audio MSK at 2400 bit/s (1200/2400 Hz);
  - 7-bit characters with odd parity, least significant bit first;
  - pre-key, then `+ * SYN SYN SOH`, mode, address (7), ack, label (2), block id, then `STX` and text, then `ETX` or `ETB`, then the BCS.
  - The BCS is CRC-16 CCITT, reflected (0x8408), from zero, over the characters as sent (parity bits included) through the ETX or ETB, and the two BCS bytes leave zero. This was found on air: of four variants, only this one checked a clean block.
- **The demodulator:**
  - The AM is detected two ways, since neither always wins on air: the envelope (low-passed to ±5 kHz), and coherently, in phase with the carrier.
  - The audio's analytic signal over 300 Hz–3.6 kHz (through an FFT) is shifted down by 1800 Hz.
  - Bit timing comes from the phase each bit moved (±π/2), timed over the whole burst and re-timed every 64 bits.
  - Bits are decided coherently as offset QPSK: the carrier phase from the fourth power at the bit ends, then a half-sine matched filter on the real and imaginary axes in turn, its signs flipping every two bits.
  - Noncoherent decisions left about 1% of bits near zero and the clean block with 2–4 parity errors; coherent decisions decode it with none.
  - The four phase ambiguities, and which axis comes first, are settled by `SYN SYN SOH`, allowed 2 bit errors.
- **Mapping lessons:**
  - The tones are not the bits: a steady tone is the pre-key (all ones), but neither "high tone = 1" nor "no change = 1" finds the sync.
  - The bit is the offset-QPSK symbol: the sign of the cosine or sine of the accumulated quarter-turn phase, flipping every two bits. That found `SYN SYN SOH` with no errors.
- **Correction:** one flipped bit in each of up to 3 characters whose parity failed, the combination the BCS accepts. Only a checked block makes a claim (`Decoder`, mode "ACARS", identity the registration). The bursts are often weak and short, so there is no SNR gate.
- **Tests:** two committed slices: a clean H1 downlink from N365NB (flight NW2320, an A319, KRSW to KJFK), and a short block from the same aircraft that checks after two corrections. The first still decodes under 6 dB more noise, and noise alone never makes a checked block. A look costs 7 ms per 0.75 s window in release.

### On air through the engine (2026-11-06, `airband_air` on the R2 at 130.0 MHz, 600 s)

- **The pool:** looked at 52 records, missed none, at 3.6 % of real time.
- **ACARS:** 4 blocks, every BCS checked, from N357NB (flight NW2314, an A319): an H1 position report on 130.025 MHz, a `_` DEL ack, and two Q0s on 130.450 MHz.
- **CHANNELS:**
  - 128.225 and 128.375 MHz are air band voice (1 and 4 calls measured AM; 6 and 9 carrier pieces).
  - 130.025 and 130.450 MHz are ACARS.
  - Channels whose calls never measured AM (126.375, 126.650, 128.775, 132.825 MHz: weak at this antenna) are not listed. Their records stay "Carrier" or have no claim.
- **Before this:** one session earlier, at 124.9 MHz, the same channels read only "Carrier".
