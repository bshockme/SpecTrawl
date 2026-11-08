# The decoder-worker protocol

A decoder worker is a separate program that decodes modes SpecTrawl has no native decoder for (spec §4.6). SpecTrawl feeds it IQ over loopback and reads what it decodes back. The worker's code never enters a SpecTrawl crate: this protocol is the whole contract. Any program that reads rtl_tcp can be one. OnAir's `onair-server` (GPL-3) was the first tried (2026-11-05): it decodes through this feed, but the stock server reports only its lamp and counters, so SpecTrawl does not use it (2026-11-06).

Status (2026-11-05):

- **IQ in** is built and tested (`engine::iqserve`, `crates/engine/tests/iqserve_sim.rs`).
- **Control and telemetry**, as described for `onair-server`, come from OnAir's documentation (`docs/headless-server.md`, `docs/REMOTE-API.md`) and its source (`core/src/source/source_rtltcp.cpp`). No `onair-server` has been run against SpecTrawl yet.

## IQ in: rtl_tcp on loopback

SpecTrawl is the server; the worker connects to `127.0.0.1:<port>`. One client at a time; a new one is served after the last has gone.

**Greeting.** On connect SpecTrawl sends 12 bytes:

| Bytes | Value |
|---|---|
| 0–3 | `SPTW` |
| 4–7 | tuner type, big-endian u32: `0` (unknown) |
| 8–11 | number of gains, big-endian u32: `0` |

An rtl_tcp client that checks for an RTL-SDR (`RTL0` with a tuner type 1–6) therefore treats this as another kind of radio. OnAir then allows any rate from 0.25 to 20 MS/s, instead of holding it to 2.56 MS/s.

**Commands from the worker.** These are rtl_tcp's: 5 bytes each, a command byte and a big-endian u32.

| Command | Argument | What SpecTrawl does |
|---|---|---|
| `0x01` frequency | Hz | Re-centres the channel there if the channel fits in the source's usable span. Otherwise it refuses the tune, counts it, and the stream stays where it was. SpecTrawl never retunes the radio for a worker. |
| `0x02` sample rate | S/s | Nothing if it is the served rate. Any other rate is counted and the served rate kept. |
| `0x04` gain, `0x03`, `0x08` gain mode / AGC | — | Counted and ignored: SpecTrawl's AGC sets the level. |
| others | — | Ignored. |

**Samples.** Interleaved 8-bit offset I/Q: a byte `b` means `(b − 127.5) / 127.5`.

- **The channel:** a bus channel of the source, centred where the worker tuned. Its rate is one the channelizer gives exactly, `fs·K/N` for a power of two `K ≤ N/4`: 2.5 or 1.25 MS/s at 10 MS/s.
- **Usable band:** the passband is `rate / 1.6` wide (62.5 % of the rate). The rest of the stream is the channel filter's roll-off.
- **Level:** a slow AGC (0.5 s) holds the RMS at 16 counts per component, 18 dB under clipping.
- **Gaps:** gaps in the source (a retune, lost USB transfers) are not marked in the stream. A worker sees them as a jump in its signal; SpecTrawl counts them.
- **A slow worker:** it loses whole blocks once 0.5 s are queued, counted. It never holds the bus.

**Configuring the worker for it.** A worker should be told the served rate up front, so it never asks for another. For `onair-server`, in the settings file SpecTrawl writes for it:

```
net_radio=rtltcp,127.0.0.1,<port>
rate_msps=2.5
freq_mhz=<the channel's centre>
mode=<mode id from /api/v1/modes>
autostart=1
port=<a free port for its control API>
```

## Control and telemetry: `onair-server`'s remote API

`onair-server --config <file>` listens on `127.0.0.1:<port>`; loopback needs no token.

- **Health:** `GET /api/v1/status`, which gives running, device, mode, frequency, rate and gain.
- **Modes:** `GET /api/v1/modes` lists `family`, `name`, `sub`. `POST /api/v1/mode` takes `{"family":N,"sub":"id"}` while stopped.
- **Tuning inside the stream:** `POST /api/v1/frequency` with `{"hz":…}`, which reaches SpecTrawl as the rtl_tcp `0x01` command above.
- **What stock `onair-server` 3.6.0 offers:**
  - It implements only status, devices, modes, telemetry, spectrum, start/stop, mode, frequency, rate, bandwidth, gain and audio (`server/server_host.h`).
  - `/api/v1/events` answers *"no message stream in this program"*, and there is no `/api/v1/sessions`.
  - So a stock worker reports its lamp and its counters, but not what it decoded. The patched OnAir adds both (see the step 7 plan).
- **Telemetry:**
  - `GET /api/v1/telemetry` returns the mode's figures.
  - Over the WebSocket `/api/v1/ws`, send `{"telemetry":true,"events":true}`. Frames then come as `{"type":"telemetry","data":{…}}`, `{"type":"event","data":{…}}`, and `{"type":"gap"}` when the worker dropped messages because SpecTrawl read too slowly.
- **Decoded messages**, in the desktop app and in the patched server, are one JSON object each, for example `{"v":1,"type":"pager.message","time":…,"mode":"POCSAG","freqHz":…,"fields":{…}}`.

## How SpecTrawl uses a worker

- **The process:** a worker's crate implements `engine::workers::WorkerFactory`; its supervisor starts one worker per mode per span, on demand, writing its settings file in SpecTrawl's own data folder.
- **Health and restarts:** it checks health, restarts with a backoff, and stops the worker when nothing uses it.
- **Results:** telemetry becomes the `Decoder` trait's telemetry and `Decoder` claims on the records it concerns.
- **Registry:** which implementation serves a mode, native or worker, is a registry entry the app hands the engine.
