# SpecTrawl rebuild: design

Status: design, 2026-10-22. Supersedes the build order in `DESIGN.md` §14 and the engine/app layout in §11. The requirements in `DESIGN.md` §1–§10 stand unless this document says otherwise.

## 1. Intent

SpecTrawl's purpose is unchanged: point SDRs at spectrum, find every emission, then explain what each one is. The first build (October 2026) got the finding right and the explaining wrong. An audit of `DESIGN.md` against the code found 109 requirements, of which 32 were built, 49 partly, 11 the opposite way, and 14 dropped without a backlog entry. The shared channelizer, the lock-free ring, the single timeline, the engine/UI contract and the classification worker pool were all specified and none were built. Every feature instead took a private path: its own thread, its own full-rate down-converter, its own clock.

This rebuild keeps the code that was verified on air (the `detect` crate and the hardware layer) and replaces the engine and the app. The foundation is built first and features are rebuilt on it one at a time. OnAir (github.com/adswill/OnAir, GPL-3) contributes two things: its presentation model, rebuilt from scratch in egui, and its decoders, used as a library.

**The acceptance test, in the owner's words.** A burst appears on the waterfall and disappears. Within a couple of seconds the user can click it and the app says what it was, for how long, at what frequency and time, with the evidence for that claim, and plays it back. "I don't know if that was a call" must not be possible. Concretely:

- Aviation: an AM voice call on 124.85 MHz lasting 4 s is listed, labelled "AM voice · aviation", replayable from the ring, with the channel's name if the frequency database knows it.
- P25: a control channel at 853.6 MHz produces a system row (WACN, system, site, NAC) and a call log within 10 s of appearing in the span, and the Trunking view never contradicts the waterfall. When it can't yet, it says which step is pending and why.
- Nothing on screen asserts more than the engine knows. Every label carries its evidence and its confidence, and inferred labels are visibly different from decoded ones.

## 2. Goals, non-goals, anti-goals

**Goals**

- Many channels decoded at once, bounded by decoder CPU, not by channel extraction.
- Any burst in the look-back window can be replayed, decoded or exported, from a record or from a region the user selects on the waterfall.
- Every classification shows what produced it (which detector, how many syncs, which frames decoded), its confidence, and the alternatives that were considered.
- Records persist across sessions and across retunes. Changing the survey span never discards knowledge.
- Clear state at all times: one sentence per receiver and per decoder saying what it is doing and, when it is waiting, what for.
- Windows and Linux, with CI from the first commit.

**Non-goals (unchanged from DESIGN.md)**

- Transmitting, decrypting, TDOA/FDOA, lab-grade calibration.
- Replacing OnAir. OnAir remains the better tool for broadcast TV and for modes SpecTrawl's survey never meets.

**Anti-goals: the rebuild has failed if**

- Any consumer reads the radio itself, or creates its own down-converter or clock.
- A label is shown with no evidence behind it, or an inferred label (band plan, neighbour, position) is styled like a decoded one.
- Two displays disagree about the same signal.
- A requirement disappears without a backlog line saying so.
- A test passes when its input data is missing.

## 3. Constraints

- Rust, egui/eframe/wgpu, egui_dock. The existing `detect`, `sdr-core`, `backend-airspy`, `backend-sim` and `catalog` crates are kept as libraries.
- The `airspy` backend is the only native backend until the foundation is done: the Airspy R2, and the R2-compatible HydraSDR RFOne through libhydrasdr (amended 2026-11-04, when one replaced an R2 here). The simulator is the test harness.
- The `detect` crate's on-air verification is its value, and it keeps growing: new detectors, decoders and classification rules are welcome alongside fixes (amended 2026-11-04; this used to say bug fixes only). What protects the value is evidence, not a freeze. A change is checked on the air, or on a committed real capture, before and after; a result that was verified right must still come out right; and a change that alters a verified result says why, with a capture that shows it. The regression tests in `detect/tests/real_captures.rs` run on committed slices in `testdata/small`, so a new detector that earns its place gets a slice and a test. §8 lists known defects.
- **Licence.** SpecTrawl is MIT. OnAir (GPL-3.0-or-later) may be included where it helps (owner, 2026-11-06, replacing "its code is never linked or copied"). Every inclusion is listed in `docs/onair-code.md` with its OnAir path, its upstream origin and what would replace it, so the tree can be made MIT again for a distribution. A crate that includes OnAir code is GPL-3.0-or-later, and so is any binary that links it. OnAir's name and logo are not used.

## 4. Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│ app (egui)   receiver panes · inspector · call log · systems · survey    │
│              reads Snapshot, sends Command. Never touches engine state. │
└───────────────▲──────────────────────────────────────┬───────────────────┘
                │ Snapshot (immutable, versioned)       │ Command
┌───────────────┴──────────────────────────────────────▼───────────────────┐
│ api          the only types the app and engine share                     │
└───────────────▲──────────────────────────────────────┬───────────────────┘
┌───────────────┴──────────────────────────────────────▼───────────────────┐
│ engine                                                                    │
│  per source:  IqRing (lock-free, segment table) ─► Fft ─┬─► rows         │
│                                                        ├─► BurstDetector │
│                                                        └─► Channelizer   │
│  Channelizer ─► ChannelBus ─► consumers (decoders, demod, classify, …)   │
│  ClassifyPool · DecoderBank · Replay · Store (SQLite + SigMF)            │
└───────────────▲───────────────────────────────────────────────────────────┘
                │ sample blocks + StreamEvent (unchanged contract)
┌───────────────┴───────────────────────────────────────────────────────────┐
│ sdr-core · backend-airspy · backend-sim · backend-file (SigMF playback)   │
└───────────────────────────────────────────────────────────────────────────┘
          detect (kept) ──┐
          onair-rx (new)  ├─► DecoderBank
          dsp (new)       ┘
```

### 4.1 The ring and the timeline

- **Lock-free.** The device callback writes into a single-producer ring without taking a mutex. Readers take a snapshot of `(head, oldest)` and read a range; a read that races the writer returns `Err(Overwritten)` and the reader skips ahead and reports a gap.
- **Segment table.** The ring records every `Retune` and `Gap` as a segment boundary: `(from_sample, center_hz, sample_rate, host_time_anchor)`. Any read of a range is tagged with the segment it falls in, and a range spanning two segments is refused. This fixes the look-back export bug and makes replay after a retune correct.
- **One clock.** Every record, row and event is timestamped by sample index. Wall-clock time is derived from the nearest `TimeAnchor`, which the backend refreshes every second. Nothing in the engine calls `SystemTime::now()` for data.
- **Native format.** i16 interleaved is kept (it is half the size of f32 and the Airspy's float output is converted once in the callback). The DESIGN.md "native format" wording is amended to say this.
- **Block ring.** The ring is a chain of fixed-size blocks (64 MB each, builder may tune), not one slab. Growing appends a block; shrinking retires the oldest block once no reader holds a position in it. Readers already handle "that sample is gone" (`Err(Overwritten)`), so a retired block is the same error path. Resizing never copies and never restarts the device.
- **Sizing is by memory pressure, not by a percentage.** Two windows per live source:
  - *Guaranteed window*: the newest N seconds (default 30 s, configurable), committed memory that is never offered to the OS. Detector back-fill and the user's own reaction time count on this.
  - *Opportunistic window*: beyond that, blocks keep accumulating up to a ceiling (default half of physical RAM, or a user cap in seconds or bytes, lower wins). Each block is offered to the OS as it ages (Windows: `OfferVirtualMemory`/`ReclaimVirtualMemory`, priority falling with age; Linux: `madvise(MADV_FREE)` with a per-page marker that reads back as zero if the kernel dropped the page; elsewhere: poll available memory and retire the oldest blocks under a floor). The OS discards offered history first when something else needs RAM; the ring notices on the next read, retires the block and reports it. On an idle machine nearly all of the ceiling survives. Offered memory stays committed, so the ceiling is also bounded by RAM plus pagefile.
  - The ring is never pinned (`VirtualLock` and friends): the machine must not fight the user's other programs for it.
- **Budget across sources.** Sweep sources hold a fixed ~1 s ring (one hop's dwell plus slack to decouple the USB callback); look-back on hopped samples is meaningless. The guaranteed window applies per live source; the opportunistic ceiling is shared across live sources so each holds the same number of *seconds* (not bytes) of history. A mode change redistributes: a source going Sweep→Live grows into its share and the others shrink to theirs, and vice versa. A per-source weight or pin in the config overrides the equal split.
- **Shown in seconds.** The health slot (§5) reports "holding 2 m 40 s, guaranteed 30 s" per source at its current rate; the number visibly shrinks when a source moves to a higher rate.
- Disk tier for the raw ring: still deferred (40 MB/s continuous is 3.5 TB/day of SSD writes; a file-backed mapping is not used for this reason), listed in BACKLOG. Per-record slices on disk are a different thing and are in scope (§4.8).

### 4.2 Shared FFT and channelizer

- One overlap-save FFT per source. Its size is set by the narrowest *first-stage* channel, which is 6.25 kHz (NXDN; everything else at VHF/UHF is 8.33 kHz or wider): 64k points at 10 MS/s gives 153 Hz bins, about 41 bins across such a channel, enough to shape a filter with real skirts, and a 6.5 ms block, which is the latency floor for every channel. The same 64k FFT at an HF-rate source (768 kS/s) gives 12 Hz bins and 85 ms blocks. The waterfall decimates from this; it does not run its own FFT.
- **Narrow modes are a second stage, not a bigger first FFT.** A channel narrower than a few kHz (CW, RTTY, PSK31, FT8 sub-channels, narrow telemetry) is cut by a nested channelizer running on a first-stage slice (3 to 25 kHz at a low rate), so the long filter a narrow channel needs costs only that slice's consumers. Growing the first-stage FFT instead would push block length to 100 ms or more for every channel and multiply buffer memory; it is not done. The `Channel` request carries a `parent: Option<ChannelId>` so a second-stage channel is expressed the same way as a first-stage one.
- **Fast-convolution channelizer** per DESIGN.md §5.4: a channel is `(center_hz, bandwidth_hz, output_rate)`; its output is the inverse FFT of its bins times a filter response. Channels are added and dropped at runtime. Cost is dominated by the one forward FFT, so hundreds of channels are affordable.
- **Back-fill.** A channel can be created at a ring position in the past; it is fed from the ring from that position until it catches the live edge. This is how late-confirmed bursts and user replay get their samples.
- Lives in the `dsp` crate, which also takes over `Ddc`, `Demod` and the filters from `detect`. `detect` keeps a thin dependency on `dsp`.
- **Rule:** nothing else in the engine may construct a down-converter. `Ddc::new` is `pub(crate)` to `dsp` and used only by the channelizer and by offline tools.

### 4.3 ChannelBus and consumers

A consumer is anything that wants a channel's samples: a decoder, a demodulator, the classifier, a recorder, an exporter. It registers `(channel spec, start position, callback)` with the bus and gets blocks in order with gap markers. The bus owns the threads: a small pool sized to the machine, with consumers scheduled round-robin and a per-consumer lag metric exposed in the snapshot. When the pool is saturated the bus sheds the lowest-priority consumers and says so, rather than silently falling behind.

Priorities, highest first: user listen/replay, trunking control channels, decoders attached to confirmed emitters, classification jobs, pager monitors, recorders.

No cap on monitors. The cap is the pool, and it is visible.

### 4.4 Records and emitters

`SignalRecord` is the core object, per DESIGN.md §6, with these changes from the first build:

- `t_start`/`t_end` are sample indices plus a derived wall time.
- `segment_id` so replay and export know the tuning.
- `classification: Vec<Claim>` where `Claim { label, confidence, by: Evidence }` and `Evidence` is an enum: `SyncBank { protocol, syncs, frames_tried, frames_decoded }`, `Features { … }`, `Decoder { mode, locked, summary }`, `BandPlan { entry }`, `Neighbour { emitter }`, `Position { dc | spur }`. The UI styles each kind differently and never collapses them to a string.
- Records keep their own claims. An emitter's label is computed from its records' claims; a record never borrows a label without the UI showing it as borrowed.
- `audio_clip` and `iq_clip` are paths the record owns, written by the recorder consumer and read back by the inspector.
- `tags`, `notes`, and a stable id that survives restart.
- Emitter grouping and confirmation logic from `emitters.rs` is kept as an algorithm, re-homed on the new record type, with these fixes: no sticky position labels (a `Position` claim is just one claim), neighbour labels are `Neighbour` claims with confidence capped at 0.5, explore verdicts are claims with their own evidence rather than adopted at 1.0.

### 4.5 Classification: a detector bank, not a ladder

The current `classify()` is a ladder of feature rules ending in a string. It is replaced by a bank of detectors that each return `Option<Claim>` for a channel window, run in parallel on the pool:

1. Position and shape (spur grid, DC, wideband occupant shape): `Position` and `Features` claims, low confidence.
2. The `detect` sync bank: `SyncBank` claims. Confirmation tightened to DESIGN.md §7.1: two syncs within one frame period + 3%, rate gate on.
3. OnAir decoders, tried in order of the sync bank's suggestion and the band plan's suggestion: a decoder that locks returns a `Decoder` claim with its telemetry summary. Trying a decoder on a 1.5 s window is cheap because the channel already exists on the bus.
4. Band plan: a `BandPlan` claim only, never a rename.

The record shows all claims. The headline is the highest-confidence decoded claim, else the highest sync-bank claim, else a measured analog modulation, else "unknown" with the features. "Unknown" is a valid, honest result and is shown as such. *Rewritten 2026-11-06 (owner): a digital mode needs concrete evidence, a sync word or a decode, before it is named. A shape alone matches the first closely shaped type: DMR, ProVoice and the other 4FSK voice modes look alike. AM and analog FM carry no code words and are measured positively (a carrier with voice-band sidebands, a steady tone, a broadcast pilot), so those readings may headline; FM read from its deviation alone may not (`api::records::is_analog_shape`).*

Throughput: a pool, not a thread. Every record gets at least one look before its IQ leaves the ring; the snapshot reports how many did not, so the budget is a number on screen.

### 4.6 DecoderBank and OnAir's decoders

OnAir's receivers share one shape: `configure(rate)`, `setCenterHz`, `feed(cf32*, n)` (non-blocking), `reset()`, `telemetry(out, lastSeq)`, plus a per-mode telemetry struct and a `Summary(telemetry) -> string`. That shape is the `Decoder` trait of the bank:

```rust
trait Decoder {
    fn mode(&self) -> ModeId;
    fn configure(&mut self, rate: f64);
    fn feed(&mut self, block: &[Complex32]);           // never blocks
    fn reset(&mut self);
    fn telemetry(&self) -> Telemetry;                  // state, snr, lock, counters, mode tables
    fn summary(&self) -> String;                       // one sentence
}
```

- `detect`'s P25, DMR, NXDN, FLEX, POCSAG, RDS, LTE and GSM code is wrapped to this trait first (it already exists and is verified).
- `onair-rx` is a new crate that implements the same trait by driving OnAir **out of process**: one OnAir worker process per mode per span (not per channel, since OnAir's airband, pager and DMR/P25 modes already decode a whole span of channels at once). SpecTrawl feeds the span's IQ to the worker over a local socket using a network IQ format OnAir already accepts (rtl_tcp or SpyServer framing), and reads telemetry back as JSON lines. The worker is OnAir's own `onair-cli` or headless server, unmodified or lightly patched; any patch (such as richer per-mode JSON, which OnAir exports only for a few modes today) is published as a GPL fork or contributed upstream, and stays outside SpecTrawl's tree. **The whole OnAir catalogue is in scope** (145 modes: land mobile, aviation, maritime, pagers, broadcast TV and radio, HF digital and amateur, satellite, IoT), because the owner wants every mode SpecTrawl can reach. Workers are started on demand when a mode is selected for a span or a record, and stopped when nothing uses them, so the number of processes is bounded by what is active, not by the catalogue. Modes that need hardware the survey receiver lacks (HF below 24 MHz needs an Airspy HF+ or similar) appear in the catalogue greyed with the reason.
- **Native first for the hot path.** Modes where latency, audio or multi-channel density matter most are implemented in `detect` (MIT) and preferred over the worker when both exist: P25 Phase 1 and 2, DMR, NXDN, air band AM, POCSAG and FLEX, RDS, LTE and GSM identity. The bank may run both and compare, which is also how a native implementation is validated against OnAir's before it takes over.
- **The worker protocol is the contract.** IQ in as rtl_tcp or SpyServer framing over loopback; telemetry out as JSON lines with the common block and the mode table; commands (tune, gain, sub-mode, reset) as JSON lines in. The protocol is documented in `docs/decoder-worker.md` so a worker can be anything, not only OnAir.
- **Licence.** OnAir code may be compiled into a SpecTrawl crate where it helps, ported or copied, and is listed in `docs/onair-code.md` (owner, 2026-11-06; this bullet used to forbid any OnAir code, header or binding in a SpecTrawl crate).
- Which implementation serves a mode is a registry entry; the UI does not know.
- The decoder's telemetry struct is what the inspector renders. Each `Telemetry` carries a common block (`state: Searching | Syncing | Locked`, `snr_db`, `level_db`, `cfo_hz`, `blocks_ok`, `blocks_bad`) and a mode-specific table (channels, calls, aircraft, pages).
- Latency and cost: an IQ span over a local socket at 10 MS/s i16 is 40 MB/s per worker, well within loopback bandwidth; the price is one extra copy and a few ms of latency. Narrow modes get a channelized slice at a lower rate instead of the whole span, so most workers cost far less. If a mode needs tighter coupling than the worker gives, the answer is a native implementation in `detect`, never a link against OnAir.

Decoders that produce speech (P25 IMBE/AMBE+2 from `detect`, and OnAir's DMR, P25, NXDN and D-STAR through its codec plug-ins) implement an additional `AudioOut` trait that pushes PCM blocks to the audio mixer. Voice following ("Follow calls": priority, lockout, per-transmission WAVs) is re-homed as a bus consumer at the "user listen" priority. This keeps a feature that `DESIGN.md` §1 listed as a non-goal; that line is amended, since the feature was built and verified on air (designer's call, owner may veto).

### 4.6a Survey (sweep) in the rebuilt pipeline

The retune-and-dwell sweep (`sweep.rs`, hop profiles, segments, spur blanking, LO masks) is kept as the row producer for a source in Sweep mode. It writes into the same ring, and every hop is a segment in the segment table; the table is a plain append-only vector, so 150 segments per second is not a cost. In Sweep mode the shared FFT, channelizer and bus are off; the sweep publishes stitched rows through the same `Snapshot` path as live rows. Survey records and emitters go through the new `SignalRecord` and `Claim` model so that explore verdicts, busy-band chips and survey-to-focus backfill read the same data as everything else. Re-homing the sweep onto the new ring and snapshot is step 4a in the build order.

### 4.7 Replay

`Replay` is a consumer that reads a ring range through a channel (or, when the range has left the ring, the record's stored slice from §4.8) and either demodulates it to the audio queue, feeds it to a decoder, or writes it out. It is created from a record ("play this", "decode this as DMR", "export this") or from a waterfall region selection. Audio mixing: the audio queue takes several sources with per-source gain, so a live listen and a replay can coexist, and two live channels can be heard at once.

### 4.8 Store

SQLite for records, claims, emitters, systems, calls, pages, tags and notes. SigMF clips and WAV audio on disk, referenced by path from the record.

**Slice tier (own switch, default on, disk cap).** Every record the detector produces has its channelized slice (native-rate cf32 SigMF, ~125 kB/s for a 12.5 kHz channel; i16 until 2026-11-07, when the owner chose floats because i16 left a weak channel a few bits) written to a content directory by the store thread, with the record row holding path and sample offsets. This is what makes a record replayable minutes or hours after its IQ has left the ring: replay (§4.7) reads the slice when the ring no longer has the range. Eviction is oldest-first under a configurable disk cap (default 4 GB, builder may adjust); a record whose slice was evicted keeps its metadata and says so. The owner can keep a record's slice (Save IQ, added 2026-11-10): it is copied to `saved/`, outside the cap, at once or as soon as it is written, and the record is tagged `saved`. The slice tier is independent of the content switch below, because a slice is raw signal, not decoded content.

**Content storage is off by default:** pager text, decoded messages and voice audio are kept only when the user enables it (globally or per mode); metadata (who, when, where, how long) is always kept. This follows DESIGN.md §16. Written by a single store thread off a queue; the engine never blocks on it. Survives restarts; survey span changes do not clear it. The `backend-file` SigMF source replays any clip into the whole pipeline, which is also how the real-capture regression tests run.

### 4.9 The engine/UI contract

The `api` crate defines `Snapshot` and `Command`. The engine publishes a new immutable `Snapshot` at a fixed cadence (20 Hz) containing: per-receiver state (tuning, segment, gain, lag per consumer), the newest rows, records and emitters changed since the last version, decoder telemetry, system books, call and page logs, and a per-receiver one-line status with a reason. The app holds one `Arc<Snapshot>` per frame and never blocks on the engine (DESIGN.md §10): the interface crate (`crates/app`) depends on `api` alone, never on `engine`, and its `Handle` has two calls, neither of which can block (`latest`, a lock-free load; `send`, an unbounded channel); the launcher (`crates/launcher`) starts the engine and hands the interface its handle. (Rewritten 2026-11-06 with the owner: this was enforced by a text search for `engine::` in the app and a ban on threads, locks, sleeps and clock reads anywhere in it; the app may use those for its own work, and the crate split enforces the contract by type.) Commands are an enum sent over a channel and acknowledged in a later snapshot. Explore, scan, device recovery and LO masking are engine state machines driven by commands, not app code.

## 5. Presentation: OnAir's model, rebuilt in egui

OnAir reads clearly because every mode fills the same slots and the shell places them identically. SpecTrawl adopts the slots, not the code:

| Slot | What it holds | SpecTrawl source |
|---|---|---|
| Status line | one sentence per receiver and per decoder: "Monitoring P25 control 853.6 MHz, NAC 0x340: 212 TSBKs, 14 grants" or "P25 control seen at 853.6, not yet trusted: 1 of 2 confirmations" | `Snapshot.receivers[].status`, `Decoder::summary()` |
| Lamps | Searching / Syncing / Locked with a colour, per decoder | `Telemetry.state` |
| The thing | the list titled by what the domain cares about: CHANNELS for airband, SYSTEMS and CALLS for trunking, PAGES for pagers, STATIONS for broadcast | the decoder's mode table |
| Inspector | for a selected record: headline claim, then every claim with its evidence kind, confidence and numbers; the decoder's telemetry; replay/decode/export; tags and notes | `SignalRecord.claims`, `Telemetry` |
| Health | three small plots under the content: SNR, frames ok %, lag | `Telemetry` common block, bus lag |
| Catalogue | modes grouped by domain with counts and a one-line blurb; search | the decoder registry |
| Log | the engine's narration: every retune, every consumer started or shed, every claim that changed a headline, with the reason | engine events |

Rules:

- Inferred claims (band plan, neighbour, position) render in a muted style with the word "inferred". Decoded claims render solid. Sync-bank claims render with their sync count.
- The Records table shows events, not occupants: by default, records shorter than 60 s that are not part of a known wideband occupant. Occupants have their own view.
- Two views of the same signal read from the same claims and therefore cannot disagree.
- The survey pane, waterfall rendering, zoom, pan, docking, workspaces, pop-outs and the command palette are carried over as they are. They were not the problem.

## 6. Build order

Each step ends with a test on the simulator and, where it touches a decoder, on a real capture in CI. No step begins a feature before the step below it is done.

0. **Repository hygiene.** CI for Windows and Linux. Real-capture tests fail, not skip, when captures are absent; a small set of captures is committed (under 50 MB). BACKLOG gains a line for every DESIGN.md requirement not yet built.
1. **Ring + segments + clock.** `sdr-core` emits `Overload`, `DeviceLost`, refreshed `TimeAnchor`; Airspy backend stamps `Retune` at the sample it takes effect. Block ring with guaranteed and opportunistic windows (§4.1). Tests: export after a retune has the right centre; under simulated memory pressure the oldest offered blocks are lost first, a read of them reports the loss, and the guaranteed window is never lost; two live sources end up with equal seconds of history and a Sweep→Live change redistributes without a device restart.
2. **FFT + channelizer + bus** in `dsp`. Test: 200 simultaneous channels at 10 MS/s on the simulator with measured lag; a channel back-filled from 5 s ago matches a direct DDC to within filter tolerance.
3. **`api` + minimal app** showing waterfall, rows and status lines from snapshots only.
4. **Records and claims + store.** Burst detector re-homed, with the merge and SNR fixes from §8. Test: records survive restart.
4a. **Sweep re-homed** on the new ring, segments and snapshot (§4.6a). Test: a survey pass on the simulator produces the same row as the old sweep.
5. **Replay.** Aviation acceptance test on a recorded capture: click a 4 s AM burst, hear it, see its claims.
6. **Detector bank** with `detect` wrapped and the pool. P25 acceptance test on the owner's 851–860 MHz capture: system row within 10 s, no contradiction between views.
7. **`onair-rx`** worker supervisor and protocol, with airband AM first (it is the multi-channel case), then the land-mobile modes, then the rest of the catalogue in the order the survey meets them. A GPL fork of OnAir carrying the JSON export patch lives in its own repository. *Amended 2026-11-06 (owner): OnAir is not the tool for this. Stock `onair-server` reports nothing it decodes and runs air band only from the desktop app, so using it meant a GPL fork SpecTrawl would maintain. Step 7 is now: the worker interface (`engine::iqserve`, `engine::workers`) for any program that reads rtl_tcp, and air band AM native, as a CHANNELS monitor built on the records; then the land-mobile modes native.*
8. **Presentation slots**, the inspector and the catalogue.
9. Re-home explore (with its CSV log), scan, RDS, cell monitor, record-all, voice following and the calls/grants merge as bus consumers and engine state machines. The catalog name imports move from the app into the engine's system book.

**Crate layout during the rebuild.** The new `engine`, `api`, `dsp` and `app` crates are created beside the old ones (`engine-old`, `app-old` after a rename in step 0). The old app stays buildable and usable on air until step 8 switches the workspace default to the new app, after which the old crates are deleted in one commit. Nothing is ported by moving code from `app-old`: panes are rewritten against `Snapshot`, using the old code as reference.

**Captures for tests.** Small captures (under 50 MB total, cf32 or i16 SigMF slices) are committed. Wideband captures (a 10 s i16 capture at 10 MS/s is 400 MB) are published as release assets of the repository and fetched by CI into `testdata/captures/` before the test run. The real-capture tests fail when the file they name is absent; a developer without the files sets `SPECTRAWL_SKIP_CAPTURES=1` to skip them locally, and CI never sets it. The `backend-file` source reads both the existing cf32 channel slices and i16 wideband SigMF.

## 7. Process rules

- Only the backend's driver thread reads the SDR, and only the source (`engine/src/source.rs`) holds the device. A feature may start a thread when its job needs one (replay, audio output), provided the engine owns it and joins it on shutdown. (Rewritten 2026-11-05 with the owner: the rule was aimed at private reads of the radio, not at threads as such.)
- Only the bus's stage (one FFT and channelizer for every channel), the sweep (one FFT per hop) and the raw IQ export read the full-rate ring. Everything else takes a channel from the bus, and may then re-centre, filter or resample that channel as its job needs: the channelizer gives fixed rates and centres, and decoders need finer ones. (Rewritten 2026-11-06 with the owner: it banned every down-converter in the engine, but the first build's failure was several features each down-converting the full-rate stream privately.)
- A sample's time (a record's, a row's, a slice's) comes from the timeline's anchors, never the wall clock, so it agrees across receivers and with replay, and the simulator may run faster than real time. Times about the wall clock itself (when a note was written, a log line) may read it. (Narrowed 2026-11-06 with the owner from a ban on reading the clock anywhere in the engine.)
- The rules tests (`crates/engine/tests/rules.rs`) check code, not comments, and each names the failure it guards against.
- A requirement that is cut goes to BACKLOG with a reason the same day.
- Parallel work (agent worktrees) is allowed for steps 6–9 only. Steps 0–5 are done serially on one branch.
- Every claim shown to the user has evidence; "unknown" is an allowed answer.

## 8. Known defects to fix in kept crates

`detect`: process-global P25 Phase 2 `Known` state (make it per-system, expiring, and passed in); 64-candidate cap on scrambling keys; stringly typed protocol labels (an enum); US band-plan policy moved out to the engine's `BandPlan` claim; sync confirmation per DESIGN.md §7.1; tests that skip when data is missing.

`backend-airspy`: emit `DeviceLost` and `Overload`; stamp `Retune` at the sample where it lands; refresh `TimeAnchor`.

Burst tracker (carried over): merge inflates mean SNR (sums added, row count max'd); extents only grow; adjacent channels fuse on overlap. The last two are fixed in `engine-old` (2026-11-04): a record's extent is the span of bins hit in at least a tenth of its rows, and overlapping tracks merge only when close (an FSK tone pair), frequent or touching, otherwise the segment is split between their cores. Carry that over when the tracker is re-homed.

## 9. Left to the builder

Ring block size; the exact offer-priority schedule by age; the Linux page-marker scheme; slice file layout under the content directory; FFT size and overlap; channelizer filter design, including the slice rates and FFT size of the second-stage channelizer for narrow modes; pool sizing; snapshot encoding; SQLite schema; the worker protocol's exact framing and the supervisor's restart policy for `onair-rx`; the exact catalogue grouping; the order in which OnAir modes beyond the native hot-path set are wired up; the simulator's new scenarios; colour and typography within the existing theme.

Chosen in step 2 (2026-10-23): FFT size N = 65536, overlap-save with hop N/2; channel FFT size K = the smallest power of two ≥ 64 with `fs·K/N ≥ max(min_rate, 1.6·bandwidth)`, K ≤ N/4, giving an output rate of `fs·K/N`; channel filter = a Blackman-windowed sinc of K/2+1 taps at the output rate, cutoff bandwidth/2; spectrum cache 32 blocks; waterfall rows every 50 ms, history 64 rows; pool = `available_parallelism − 2` workers (at least 1); shedding = when at least 90 % of block arrivals in the last second found every worker busy with work queued and a consumer lags more than 2 s, the lowest-priority laggard is shed, at most one per second; nested (second-stage) channels deferred.

Chosen in step 3 (2026-10-23): snapshot cadence 20 Hz (50 ms); a snapshot carries each receiver's newest 16 finished rows, the last 64 acks and the last 200 log lines; waterfall history 1024 rows of 4096 max-reduced columns, texture 1024 rows by the displayed width, zoom no narrower than 24 history columns; receivers take letters `A`, `B`, … in order of opening, lowest free letter first (`S` is reserved for the sweep, step 4a); default budget 30 s guaranteed, ceiling half of physical RAM.

Changed in step 4 (2026-11-04): waterfall rows every 20 ms (ceil'd to whole blocks: 7 blocks, 22.9 ms, at 10 MS/s), the burst detector's verified row rate, so it keeps the configuration its on-air results came from; a snapshot carries each receiver's newest 48 rows (1.1 s, so a stalled tick still loses none); the app's waterfall history is 2048 rows (about 47 s).

Chosen in step 4 (2026-11-04): the burst detector reads each row summed 4:1 in power (610 Hz bins at 10 MS/s) with `engine-old`'s live configuration; the record book keeps all open records, the newest 5000 closed ones and every emitter seen in the last 24 h in memory, and rebuilds the snapshot's `RecordsView` only when something changed; record and emitter ids come from the store's counters; the store is `<Documents>/SpecTrawl/records.db`, schema 1 (records, claims with their evidence as JSON, emitters, tags, notes, sessions, counters), WAL, a 4096-operation queue drained at most every 250 ms, closed with a 2 s wait; emitters are per device; `Position` claims are 0.3 sure for the spur grid and DC (zero-IF only), 0.5 for an image pair or a comb; slices are on, ci16 SigMF (cf32 since 2026-11-07, step 7) at the channelizer's rate for `max(1.25 × width, 12.5 kHz)` (since 2026-11-07: collected 12 kHz wider each side and re-centred on the record's final centre and width when it closes), 50 ms pre-roll (150 ms pre- and post-roll since 2026-11-07), at most 120 s, cut back to the record's end, under `<Documents>/SpecTrawl/slices/<day>/<record id>`, capped at 4 GB oldest first.

Chosen in step 4a (2026-11-05): the sweep runs on the source's stage thread while it is in Sweep mode; each hop is measured from the landing sample its retune got in the segment table plus `max(1 ms, the plan's guard, the device's latency − 2 ms)` (×3 on jumps over three hops), a retune unseen after 500 ms is measured from the head and counted; a pass is one row under a stitched segment id (bit 63 set, plus the plan's generation), centre = the hop grid's middle, rate = its width; hop profiles stay where `engine-old` keeps them; a sweeping receiver takes the letter `S` if free; LO leaks blanked at an Airspy's centre + 5 MHz ± 15 kHz.

Chosen with the RFOne's sweep firmware (2026-11-05, `docs/hydrasdr-sweep-mode.md`): a device with `Capabilities::native_sweep` (an RFOne whose firmware version says `sweep`, through libhydrasdr) sweeps by itself in Sweep mode. `Device::start_native_sweep` hands it the hop grid; the backend turns each tagged raw block (20 MS/s real, inverted, 5 MHz IF) into 4088 IQ samples at 10 MS/s (j^n shift, 95-tap Kaiser half-band, libhydrasdr's float scale), and every hop arrives as a `Retune` plus its blocks, captured after the board saw PLL lock. The engine reads the hops off the segment table (no retunes, guards or settle checks) and stitches them with the same grid, blanking and profile code as the retune sweep (its own profile key, `-board`). `SweepPlan::native_dwell` = blocks per hop, default 4 (the retune sweep's four FFTs), 0 = retune anyway. It is started and stopped through the patched libhydrasdr's `hydrasdr_sweep_*` calls, resolved as optional symbols (the first version sent vendor requests on a second libusb handle, which WinUSB would refuse).

Chosen in step 5 (2026-11-05): a replay is a bus subscription at `Listen` priority back-filled from 50 ms before the record (its slice's pre-roll), at the record's centre and 1.25 × its width bounded by the mode's own and twice that (AM 10 kHz, NFM 14 kHz, WFM 200 kHz), demodulated with the squelch open and ending at the record's end plus the channel filter's delay (following the live edge while the record is open); when the ring no longer holds the start, or the source is gone, the record's slice is demodulated on the `replay` thread; otherwise the play is refused with the reason. A record's source counts as holding it only when it is the same device and its timeline puts the record's first sample within 1 s of the record's start (source ids repeat across runs). The mode is the caller's, or AM on 108–137 and 225–400 MHz, WFM on 88–108 MHz and NFM elsewhere, shown as chosen "by band". Audio is 48 kHz mono, at most 120 s a play; the mixer sums voices with their gains behind a 100 ms jitter buffer and counts underruns; cpal's default device plays it from its own thread (resampled linearly to the device's rate); a WAV goes to `<Documents>/SpecTrawl/audio` unless the path is absolute.

Chosen in step 6 (2026-11-05):

- **The bank.** `detect`'s classifier is split into its rungs (`detect::bank`: the sync pass, FLEX, GSM, LoRa, squelch codes, the modulation rules), which `classify_with` still tries in its old order. The engine's bank runs them side by side:
  - a `SyncBank` claim, with its sync count and frames, plus a `Decoder` claim when frames decoded and named the signal (a P25 site from two or more agreeing NIDs and its TSBKs; a colour code);
  - `Decoder` claims for FLEX, GSM and squelch codes;
  - a `Features` claim (at most 0.6) from the modulation rules, only when nothing decoded, and only above the weak gate.
  - Confidence: `detect`'s own for syncs (0.8, 0.9, 0.97), 0.95 for identifiers, 0.99 for a P25 site named in a window, 0.999 for a decoder's claim.
- **The pool.**
  - Size: `max(1, (available_parallelism − 2) / 4)` threads, with a 64-window queue that drops the newest window when full.
  - A look: a bus window at `Classify` priority for every live record. It is back-filled from the record's first sample (skipping a leading gap at a retune), at its centre, `max(1.25 × width, 12.5 kHz)` wide (at most 1 MHz), at least `max(48 kHz, 2.5 × width)`. It collects up to 1.5 s or to the record's end, trimmed of the quiet at its ends.
  - Re-looks: a record still open 30 s after its last window gets another from the live edge, which replaces the first look's claims only if it decoded something or the first didn't.
  - The weak gate takes the window's own SNR (in-band density over the 20th percentile of the channel's other passband bins). Where the record fills its channel, it takes the record's SNR instead.
- **Sync confirmation** (DESIGN §7.1): a second sync must be on the protocol's grid within its longest sync-to-sync gap + 3 %, give or take 3 symbols + 0.05 %. The gaps are P25 Phase 1 864 symbols, Phase 2 720, a DMR voice superframe 1728, and two periods elsewhere. A transmission's count is its longest chain. Entries with no grid need six syncs evenly spaced on a grid of their own.
- **The weak pass** (added 2026-11-11). Channels up to 25 kHz also get a second sync pass for weak signals, and the pass that decoded more frames wins:
  - It covers P25 Phase 1 and DMR data, whose frames carry a code strong enough to say a sync was real.
  - Symbols come from the phase turned over one symbol (`features::symbol_discriminator`), not the sample discriminator's average. Near the noise floor that average keeps every sample's phase clicks.
  - A sync word may have up to a sixth of its symbols on the wrong side of zero if its symbols correlate with the pattern at 0.8 or better (`sync::soft_match`).
  - Those syncs name nothing by themselves (noise passes them about 100 times a second). The pass names a protocol only when two or more of its frames pass their code and agree on the system: a P25 NID's NAC (never the all-zeros or all-ones codeword), or a DMR colour code behind a burst that passes its CRC. Frames decoding to anyone else are dropped.
  - Records narrower than 4.5 kHz get the weak pass too: their window is still a whole channel. On air at 800 MHz they were the 2–5 kHz cores of weak P25 channels.
  - The P25 control decoder keeps the weak pass's frames when more of their NIDs decode. A TSBK whose hard decision fails its CRC is decoded again by a soft-decision Viterbi.
- **The book.**
  - Claims from the bank and decoders are kept per record and per maker, across an open record's rebuilds.
  - Every record gets the band plan's entry as a `BandPlan` claim, 0.2 sure.
  - An emitter's headline is voted: the newest decoder claim (unless it is a thinner identity of the same site), else a sync protocol that two records name, or one record names strongly (4 syncs, or half its frames decoded), else what the store had.
- **Decoders.**
  - `P25Control` decodes on 0.6 s blocks with 0.12 s of overlap and registers its site for Phase 2.
  - It attaches at `TrunkControl` priority to an emitter whose records carry a `P25 control` claim, back-filled from its open record's start.
  - At most 4 per source; one detaches 30 s after its emitter has no open record.
- **The systems view** is built from emitters' decoder headlines only, so a row, its emitter and its records read one `Identity`.
- **The channelizer:** a channel's passband must be in the usable span, and its output band within the span (before, the whole output band had to be usable, and records within ~40 kHz of the usable edge could not be cut).
- **Acceptance.** The owner's wideband capture was never recorded, so the acceptance stream is the simulator's scenario at 855.5 MHz with the committed P25 control channel played into it (`Modulation::Iq`).

## 10. Decisions taken with the owner (2026-10-22)

1. **Licence: MIT.** SpecTrawl stays MIT; OnAir is used only as a separate process (§3, §4.6). *Amended 2026-11-06 (owner): OnAir code may be included where it helps, each inclusion listed in `docs/onair-code.md` so it can be replaced to clear the licence for a distribution.*
2. **Mode scope: everything.** The owner wants every mode OnAir offers, including broadcast, HF and amateur. OnAir is used out of process by the owner's choice (the GPL is unacceptable for SpecTrawl's own code; in-process linking was offered and declined). Native MIT implementations in `detect` are preferred for the hot path and may replace worker modes over time. *Amended 2026-11-06: OnAir is dropped as the worker (§6 step 7). Which programs serve the modes beyond the native set is open. The owner has also reconsidered the GPL: OnAir code may now be included where it helps (decision 1).*
3. **Content storage: configurable, default off** (§4.8). **Slice tier: its own switch, default on, with a disk cap** (§4.8, decided 2026-10-23).
4. **Acceptance captures.** Two wideband captures are recorded by `tools/acceptance_captures.ps1` with `crates/engine-old/examples/capture_wideband` (i16 SigMF at 10 MS/s): 60 s of air band centred 124.9 MHz and 30 s of 851–860 MHz, two rounds. The usable 10 s and 30 s windows are cut from them and published as release assets (§6). Until they exist, steps 5 and 6 build against the simulator.
5. **Ring sizing (2026-10-23).** A 10 s ring is too short for a person to notice something and ask for it. The ring is sized by OS memory pressure (guaranteed window plus opportunistic window offered to the OS, §4.1), not by a fixed percentage; sweep sources do not get a share; per-record slices go to disk rather than into the ring's memory.
