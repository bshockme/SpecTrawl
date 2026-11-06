# SpecTrawl Rebuild, Step 7: onair-rx

> On branch `rebuild-step-7` (worktree `.worktrees/step-7`).

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

## Open question for the owner (blocking the worker half)

Running OnAir's binaries on this machine was refused by the session's permission check ("code from an external source"). Nothing that needs OnAir running (the supervisor, the protocol client against a real `onair-server`, the air band acceptance) can be verified until the owner allows it, and says where OnAir should live:

- a release build unpacked outside SpecTrawl's tree, e.g. `~/onair/3.6.0`;
- or a build from source in its own repository (the fork the spec foresees).

The fork is only needed if a patch is (richer per-mode JSON). Creating it on GitHub is an outward action, done only when asked.

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

## Tasks

1. `engine::iqserve`, with tests (a client reads the greeting, tunes, and finds the simulator's emitters where they are; a slow client loses blocks, not the bus).
2. `docs/decoder-worker.md`.
3. *(Needs OnAir.)* `onair-rx`: the supervisor and the client against a real `onair-server`.
4. *(Needs OnAir.)* Air band through it, on air on 118–137 MHz, against the native AM replay of step 5.
5. *(Needs OnAir.)* The land-mobile modes, then the rest of the catalogue as the survey meets them.
