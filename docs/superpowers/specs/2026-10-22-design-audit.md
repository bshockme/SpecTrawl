# SpecTrawl: design vs code audit (2026-10-22)

Status: reference. Produced by a read-only review of DESIGN.md, docs/ui-design.md, docs/auto-explore.md and BACKLOG.md against every crate under `crates/`, on 2026-10-22. It is the input for the rebuild design (`2026-10-22-spectrawl-rebuild-design.md`) and for the BACKLOG section that step 0 of the rebuild adds. Items the design already covers (channelizer, `api`, store, other backends, per-segment tuning) were verified separately and are not in the table. Line numbers refer to the code as of commit f2a69ef.


I read DESIGN.md (all 328 lines), docs/ui-design.md, docs/auto-explore.md and BACKLOG.md, and read or grepped every source crate under D:\SpecTrawl\crates. I changed nothing. As you asked, I left out the items you already verified: channelizer, `api`, `store`, the file/HackRF/RTL/Soapy/HydraSDR backends, and per-segment tuning history.

The short version: the protocol decoders in `detect` are solid. The parts the design depends on for finding and explaining every emission (records, classification throughput, the event contract, threading) are where the code has drifted or does the opposite of the design.

Paths below are relative to `D:\SpecTrawl\crates\` unless they start with `D:\`.

## Requirement table

### §1 What SpecTrawl is, and §2 Settled decisions

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| Find every emission, record it, then classify and decode it (L11) | PARTIAL | engine/src/classify.rs:29, :129, :272 | One classifier thread capped at 60% of a core. It only looks at the newest 200 closed records, and anything over 1 MHz is labelled "Wideband" without analysis. |
| Browse, search, filter and sort records (L12, L236) | PARTIAL | app/src/main.rs:4257, :293-305 | The Records table has a fixed sort and no text filter. Only Pages, Calls and Log have filters. |
| Tag records; notes (L13, L181) | SILENTLY DROPPED | engine/src/bursts.rs:89-119 | There is no tags or notes field anywhere, and BACKLOG doesn't mention it. |
| Rewind to, replay and export records (L14) | PARTIAL | engine/src/sigmf.rs; engine/src/listen.rs:81, :88, :395 | Export works. Listening always starts at the live edge (0.05 s or 0.5 s behind), so a past record can't be replayed in the app. |
| Baseline comparison; TSCM baselines (L17, L302) | MISSING | D:\SpecTrawl\BACKLOG.md:7, :25 | "New" just means first seen within N seconds. A saved baseline is noted in BACKLOG as still to do. |
| Several receivers (L17) | BUILT | app/src/main.rs:40-55 | Receiver letters, one `Source` per device. |
| Any SDR can fill any role, matched on capabilities (L23) | PARTIAL | engine/src/lib.rs:65-68, :333; classify.rs:40 | Only Sweep and Live modes exist. Capabilities are read for the usable fraction and the spur grid, and nothing else. |
| One stream per SDR shared by all consumers; no tool owns a device (L24) | BUILT | engine/src/lib.rs:213-221, :331-351 | — |
| Reliable detection across many channels at once (L25) | PARTIAL | classify.rs:39-94 | Channels are classified one at a time on one thread. |
| Unknown signals get bandwidth, modulation family, symbol rate and timing (L26) | PARTIAL | detect/src/features.rs:7-21 | Features come only from the FM discriminator, so there is no PSK, QAM or OFDM family. |
| Look back in time; detectors rewind (L27) | BUILT | classify.rs:261 | The classifier reads back from the record's first row. |
| Windows and Linux (L28) | PARTIAL | backend-airspy/src/lib.rs:122-137 | The Linux code paths exist, but nothing builds or tests them. |
| No decryption (L32) | BUILT | listen.rs:419-421 | Encrypted calls stay silent. |
| Voice vocoders are a non-goal (L33) | CONTRADICTED | detect/src/imbe.rs, ambe.rs, mbe.rs; listen.rs | IMBE and AMBE+2 are implemented, played and recorded to WAV. BACKLOG:80-82 records this, including the AMBE+2 patent note. |
| CI builds for Windows and Linux from the start (L41, L291) | SILENTLY DROPPED | there is no .github or any other CI config | — |
| egui / eframe / wgpu / egui_dock (L43) | BUILT | app/Cargo.toml | — |
| Custom wgpu waterfall: ring texture, colour map in a shader (L43, L243) | PARTIAL | app/src/spectrum_view.rs:346-347, :350 | Colour mapping is done on the CPU and rows are pushed with `set_partial` into an ordinary egui texture. |
| Driver libraries loaded at runtime (L46) | BUILT | backend-airspy/src/lib.rs:78-151 | — |

### §4 Hardware layer

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| Capabilities: frequency range, tuning step, retune latency (L100) | BUILT | sdr-core/src/lib.rs:95-100; sweep.rs:173 | — |
| Usable bandwidth fraction masks band edges (L101) | PARTIAL | lib.rs:333; bursts.rs:331 | Only the live burst detector uses it. Classify and explore ignore it. |
| IQ ring stores the device's native format, converted only on read (L102, L142) | CONTRADICTED | engine/src/ring.rs:7-9, :78-81; backend-airspy/src/lib.rs:247, :268 | The ring is always i16. The Airspy delivers float32 (libairspy does the conversion), and the callback converts f32 to i16. sdr-core/src/lib.rs:24-25 still claims native storage. BACKLOG:89 mentions ci16. |
| One gain control in the UI for every device (L103) | BUILT | sdr-core/src/lib.rs:82-91 | — |
| Zero-IF: offset-tune and mask the DC spike (L104) | SILENTLY DROPPED | `if_arch` is never read anywhere | Moot until a zero-IF backend exists, but see finding 2 in section (d). |
| Native sweep, else retune-and-dwell (L105) | PARTIAL | engine/src/sweep.rs | Only the retune-and-dwell fallback exists. |
| Channels, external clock, bias-T, hardware timestamps (L106) | BUILT | sdr-core/src/lib.rs:110-113 | These exist as fields only. |
| Per-serial config: ppm, gain→dBm, passband flatness, preferred roles (L108-112) | PARTIAL | sweep.rs:554-557; hop_profile.rs | Only passband flatness exists, as the hop profile. dBm calibration is in BACKLOG:106. ppm and preferred roles are not mentioned anywhere. |
| `GainChange`, `Overload`, `DeviceLost` events (L121) | MISSING | lib.rs:234-237 is the only consumer; no backend emits them | BACKLOG:113 marks "recover when a device is unplugged" as done, but it can never fire. See finding 1 in section (d). |
| USB callback only copies into a lock-free ring; never blocks (L123) | CONTRADICTED | lib.rs:218-220; ring.rs:78-81 | The callback takes a `parking_lot` Mutex and converts f32 to i16. |
| Roles: Survey, Focus, Park, Follow (L127-134) | PARTIAL | lib.rs:65-68; listen.rs:353 | Only Sweep and Live. "Follow" is call-following inside the current span, not a device retune. |
| Role assigner (L136) | SILENTLY DROPPED | app/src/main.rs:635 | A ranking hack, with the comment "Until the role assigner exists". |
| Without a HackRF, the Airspy can survey by stepping (L136) | BUILT | sweep.rs | — |
| Hand-off from Survey to Focus (L136) | BUILT | engine/src/explore.rs; app/src/main.rs:1803-1817 | Click-to-tune and auto-explore. |

### §5 Engine

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| RAM tier, configurable (L143) | BUILT | app/src/main.rs:861 | 2–600 s. |
| Disk-backed rolling tier (L144) | SILENTLY DROPPED | — | Appears only as an open question at L323. |
| Ring readers include the user scrubbing (L145) | PARTIAL | spectrum_view.rs:35, :58 | Scrubbing scrolls stored spectrum rows, not IQ. |
| One overlap-save FFT per source, about 64k points (L149) | CONTRADICTED | lib.rs:60, :640-652 | It is a non-overlapped 16k FFT. When it falls behind it silently jumps to the live edge without a Gap. |
| Per-bin noise floor plus CFAR threshold (L154) | PARTIAL | bursts.rs:275-325, :53-67 | The floor is the 20th percentile of 1024-bin blocks with smoothing, and thresholds are fixed dB values. |
| Regions become records with centre, occupied bandwidth, start/end, peak and mean power (L155) | PARTIAL | bursts.rs:101-103, :394-409 | It stores mean SNR, not mean power. Track extent only ever grows (union). |

### §6 Signal records

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| `id`, `source_id`, `role` (L170) | PARTIAL | bursts.rs:89-92 | No role field. |
| `t_start`/`t_end` on the shared TimeAnchor timeline (L171) | CONTRADICTED | lib.rs:670; bursts.rs:382 | Records are stamped with the processing wall clock (`SystemTime::now()`). Only exports use the anchor (sigmf.rs:83). |
| Centre and bandwidth (L172) | BUILT | bursts.rs:138-144 | — |
| Power: peak/mean dBFS, dBm if calibrated (L173) | PARTIAL | bursts.rs:100-103 | Peak only; no mean power, no dBm. |
| Features on each record (L174-177) | PARTIAL | detect/src/lib.rs:46 | Only through the classification. No duty cycle, burst period or PSK-on-FM per record. |
| Classification as a list of {label, confidence, by} (L178) | PARTIAL | bursts.rs:113; emitters.rs:70 | A single optional class with no record of which layer produced it. Emitters keep a string-keyed tally. |
| Decoded protocol fields (L179) | BUILT | detect/src/decode.rs:196 | — |
| `iq_clip` link to a SigMF file (L180) | PARTIAL | record_all.rs:357 | The SigMF file names the record, but the record has no link back to the file. |
| `group_id` from same frequency, features and IDs (L182, L186) | PARTIAL | emitters.rs:183-199 | Emitters are grouped by centre and bandwidth only. |

### §7 Detection and classification

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| One reusable symbol engine with all state per channel (L192) | CONTRADICTED | detect/src/p25p2.rs:50-84; decode.rs:317 | The detector is batch functions, and P25 Phase 2 uses a process-global mutable `Known` set. |
| Normalize to 10 samples per symbol (L194) | PARTIAL | sync.rs:127-135; classify.rs:283 | Works at whatever rate it gets and interpolates. |
| Quadrature discriminator on complex baseband (L194) | BUILT | features.rs:58-61 | — |
| Lookahead delay ring (L195) | SILENTLY DROPPED | — | Moot under the batch design. |
| Sync search at every sampling phase, 10 phases (L196) | BUILT | sync.rs:112, :130 | — |
| Masked sync tables as data incl. `frame_period`, tolerance per entry (L197) | PARTIAL | sync.rs:8-22, :74-76 | No `frame_period`; tolerance is set by pattern length. |
| Rate-class gate: zero crossings plus hysteresis (L198) | SILENTLY DROPPED | — | — |
| Amplitude-consistency gate (L198) | PARTIAL | sync.rs:82-99 | Adapted to 0.6–1.6 of the median. |
| Two-sync confirmation within one frame period + 3% (L198) | CONTRADICTED | sync.rs:192-195 | Any two syncs anywhere in a window of up to 3 s. |
| Timing from syncs; drift from sync spacing (L199) | PARTIAL | frames.rs:179-203 | Each frame's timing is refined separately; no drift estimate. |
| Adaptive slicer and re-slicing (L200) | PARTIAL | frames.rs:155-251; detect/src/lib.rs:356-365 | The inner threshold is fixed at 2/3, not adaptive. |
| POCSAG and FLEX (L203) | BUILT | detect/src/pocsag.rs, flex.rs | — |
| DMR, P25 Phase 1/2, NXDN (L204) | BUILT | detect/src/decode.rs | NXDN decodes the LICH only (BACKLOG:78). |
| dPMR, D-STAR, YSF, ProVoice, ISM pulse timing (L205) | PARTIAL | sync.rs:50-58 | dPMR, YSF and ProVoice have sync entries only. D-STAR and rtl_433-style pulse signatures don't exist and aren't in BACKLOG. |
| Occupied bandwidth and spectral shape (L209) | PARTIAL | detect/src/spectral.rs:134 | Spectral shape is used only for wideband occupants. |
| Constant envelope or not (L210) | BUILT | features.rs:47-55 | — |
| Instantaneous-frequency histogram: levels and deviation (L211) | BUILT | features.rs:92-127 | — |
| Symbol rate by zero crossings, envelope line and cyclostationary features (L212) | PARTIAL | features.rs:132 | Only the line of the squared frequency derivative. |
| PSK-on-FM test (L213) | SILENTLY DROPPED | grep finds nothing | — |
| Burst timing: duration, period, duty, hop pattern (L214) | PARTIAL | emitters.rs:162 | Only `duty()`. No period or hop pattern (BACKLOG:57 mentions hop grouping). |
| Sync bank is definitive (L218) | BUILT | detect/src/lib.rs:374-378 | — |
| Rule-based classification on features (L219) | PARTIAL | detect/src/lib.rs:409-429 | Rules use bandwidth, levels and rate only. No burst-timing rules. |
| Small ML modulation classifier, "later" (L220) | SILENTLY DROPPED | — | Deferred in DESIGN, but not in BACKLOG. |

### §8 Decoders and plugins

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| In-tree modules behind cargo features (L224) | SILENTLY DROPPED | no `[features]` section in any Cargo.toml | — |
| External-process decoders over stdin/JSON (L225) | SILENTLY DROPPED | the only `Command::new` is the file browser at app/src/main.rs:4947 | — |

### §9 UI

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| Records drawn as labelled boxes on the waterfall (L235) | BUILT | spectrum_view.rs:701-786 | — |
| Timeline pane (L238) | SILENTLY DROPPED | — | docs/ui-design.md quietly left it out. |
| Map pane, "later" (L240) | SILENTLY DROPPED | — | — |
| FFT rate independent of UI frame rate (L244) | BUILT | lib.rs:621-676 | — |
| Linear power averaging, peak-hold with decay, max per pixel (L245) | PARTIAL | spectrum_view.rs:226-228; dsp/src/lib.rs:110 | The trace is smoothed in dB, not linear. Peak-hold with decay and max-per-pixel are present. |
| Look-back scrub: pause, drag back, select a region, decode or export it (L246) | PARTIAL | app/src/main.rs:3165-3180, :3059 | Exports either the whole ring or one record. No region selection and no decoding of a past region. |
| Drag-to-tune, cursor-centred wheel zoom, digit-wheel entry, no modal dialogs (L248) | PARTIAL | spectrum_view.rs:500, :943; app/src/main.rs:2772, :2062 | Drag pans instead of tuning; no digit wheel; `rfd` file pickers block the UI thread. |
| Dark theme; a few colour maps (L248) | PARTIAL | app/src/theme.rs:102 | One colour map. |

### §10 Threading, §11 Crate layout, §12 Testing

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| Per source: one USB thread plus one FFT/channelizer thread (L252) | PARTIAL | lib.rs:336-350 | Seven worker threads per live source. |
| Detectors run on a worker pool (L253) | CONTRADICTED | classify.rs:29, :92 | One throttled thread per source. |
| UI never blocks on the engine (L254) | CONTRADICTED | lib.rs:363-369, :407-427; app/src/main.rs:1756-1817 | The UI takes engine mutexes directly, joins worker threads on mode change, and runs the explore state machine itself. |
| Benchmark early (L259) | PARTIAL | `ClassifierLoad` | Runtime load metrics only; no benchmarks. |
| `dsp` crate: FFT, filters, channelizer, demodulators (L274) | CONTRADICTED | dsp/src/lib.rs (150 lines); detect/src/ddc.rs, demod.rs | The DDC, filters and demodulators live in `detect`. |
| Labelled SigMF corpus (L288) | PARTIAL | D:\SpecTrawl\.gitignore (`/testdata/captures/`) | The captures exist only on the author's machine. |
| Synthetic generators with detection-rate and false-alarm tests (L289) | PARTIAL | detect/src/lib.rs:479-512; sync.rs:256-315; bursts.rs:529 | Single-point tests; no SNR or offset sweeps. |
| Golden-output tests on known captures (L290) | PARTIAL | detect/tests/real_captures.rs:22-26 | They silently skip when the captures are absent, so they pass without testing anything. |

### §13 Scope, §14 Build order, §16 Legal

| Requirement (DESIGN.md line) | Status | Evidence | Note |
|---|---|---|---|
| Wi-Fi/LTE/BLE: label by spectral shape only; decoding out of scope (L301) | CONTRADICTED | detect/src/lte.rs:345, :764; engine/src/cell_monitor.rs | LTE PSS/SSS/MIB is decoded (recorded in BACKLOG:41). |
| Milestone 1: skeleton, GPU waterfall, scrub 10 s, export a SigMF clip (L307-308) | PARTIAL | — | No file backend; scrubbing is spectrum-only. |
| Milestone 2: records, signal list, boxes, SQLite (L309) | PARTIAL | — | No SQLite and no persistence. |
| Milestone 3: channelizer, symbol engine, POCSAG with synthetic tests (L310) | PARTIAL | — | No channelizer. |
| Milestone 4: DMR / P25 / NXDN (L311) | BUILT | — | — |
| Milestone 5: unknown-signal features, rules, grouping (L312) | PARTIAL | — | — |
| Milestone 6: HackRF sweep, RTL-SDR Park, role assigner, hand-off (L313) | PARTIAL | — | Hand-off only. |
| Milestone 7: Soapy, HydraSDR, FLEX, ISM signatures, TSCM baselines, remote node (L314) | PARTIAL | — | FLEX only. |
| Make it easy to log metadata without content (L328) | SILENTLY DROPPED | monitor.rs:386-396; listen.rs | Pager text and voice WAVs are stored by default, with no switch. |

### docs/ui-design.md and docs/auto-explore.md

| Requirement | Status | Evidence | Note |
|---|---|---|---|
| ui §2.1 `ReceiverId` letters and slots | BUILT | app/src/main.rs:40 | — |
| ui §2.2 `Selection` enum incl. Talkgroup/Radio/Capcode | BUILT | app/src/main.rs:68 | — |
| ui §2.3 `TrunkSystem` for any protocol | PARTIAL | catalog/src/lib.rs:368-377 | P25 only (BACKLOG:11). |
| ui §3 pane headers, inspector, transport bar, tabs, icon rail | BUILT | app/src/main.rs:2242, :391-419, :1158 | BACKLOG:11 says these were only smoke-tested on the simulator. |
| ui §4–6 docking, pop-out windows, workspaces | BUILT | app/src/main.rs:233, :1215, :462-482 | — |
| ui §7 command palette and keys | BUILT | app/src/main.rs:1089-1123 | — |
| explore §1/§4 shared queue, filters, priorities | BUILT | explore.rs:61-140 | — |
| explore §4 "new since baseline" ordering | PARTIAL | app/src/main.rs:1771 | "Baseline" is the time explore started. |
| explore §3/§7 verdicts and in-view determination | PARTIAL | app/src/main.rs:1741-1748, :1788-1794 | An "identified" verdict can be borrowed from a wideband occupant. See finding 3 in section (d). |
| explore §5 adaptive dwell | BUILT | explore.rs:113 | — |
| explore §6 log tab, CSV, optional capture | BUILT | app/src/main.rs:1640-1656, :1856 | — |
| explore §8 patrol revisits, thumbnails | MISSING | — | Listed in the doc and BACKLOG:23. |

## (a) Counts

Out of 109 rows:

| Status | Count |
|---|---|
| BUILT | 32 |
| PARTIAL | 49 |
| CONTRADICTED | 11 |
| SILENTLY DROPPED | 14 |
| MISSING (but in BACKLOG or the docs) | 3 |

## (b) Five most consequential gaps for "find every emission, then explain what it is"

1. **Records go unexplained, and nothing says so.**
   - One classifier thread per source, throttled to 60% of a core (classify.rs:29, :92), works through jobs one at a time.
   - It only looks at the newest 200 closed records (classify.rs:129), and IQ expires after about 10 s.
   - Anything over 1 MHz gets "Wideband" with no analysis (classify.rs:272).
   - No metric counts records that were never classified.
2. **Unknown signals can't be named beyond FM/FSK.**
   - Every feature comes from the FM discriminator (features.rs:63-88).
   - There is no PSK, QAM or OFDM family, no PSK-on-FM test, no envelope symbol-rate line and no cyclostationary analysis.
   - The fallback rules end in "Analog FM", "AM / OOK" or "Unknown" (detect/src/lib.rs:409-429).
3. **Protocol confirmation is weak, and wrong labels stick.**
   - Any two syncs anywhere in a window of up to 3 s count as confirmation (sync.rs:192-195).
   - There is no rate gate, and 32-bit binary syncs allow 2 bit errors (sync.rs:74-76).
   - The backlog shows the result: ProVoice reported at 97% on a front-end intermodulation product (BACKLOG:64, :69), and Phase 1 labels on Phase 2 traffic (BACKLOG:48).
   - Emitter-level stickiness then locks in errors (see section (d)).
4. **There is no durable record model to work on.**
   - Records live in memory, capped at 20k (lib.rs:138), and are wiped whenever a source changes mode (lib.rs:318).
   - No tags, notes, search or baseline.
   - The emitter, not the record, is the real entity, and its identity is a set of string tallies (emitters.rs:70-83).
5. **The detection front end loses or merges emissions.**
   - The non-overlapped 16k FFT skips to the live edge when behind, without a Gap (lib.rs:649-652).
   - The block-percentile floor fails under wideband carriers (BACKLOG:85).
   - Tracks merge on any overlap and their extent only grows (bursts.rs:394-409), which can glue adjacent channels together.
   - Squelch is on by default and hides emitters inside known wideband occupants (app/src/main.rs:278, :602).

## (c) Which crates to keep

- **`detect`: keep.** It is a clean library, not tied to the engine.
  - It depends only on num-complex and rustfft (detect/Cargo.toml) and never imports `engine` or reads a clock.
  - Its API takes slices and returns data, for example `flex::decode(x, fs)`, `pocsag::pages(&frames)`, `lte::identify(x, fs)`.
  - It has 99 tests and 31 examples.
  - Defects to fix:
    - The process-global `Known` state in p25p2.rs:50-84 is mutated as a side effect of `decode::summarise` (decode.rs:317).
    - `classify()` is one long ladder with hard-coded bandwidth gates (detect/src/lib.rs:309, :340, :382, :392).
    - Protocol identity is stringly typed, and downstream code parses label prefixes (monitor.rs:269; emitters.rs:45).
    - US band-plan policy lives here (context.rs:79-95; bandplan.rs) and belongs in a policy layer.
    - The real-capture tests skip silently.
- **`sdr-core`: keep** (173 lines, clean). Fix the contract: three events are never emitted, and the native-format comment is wrong.
- **`backend-airspy`: keep.**
  - It needs to emit DeviceLost and Overload.
  - Retune events are stamped at the start of the transfer that follows `set_freq`, while its own comment says the change lands 1.4–2.4 ms into that transfer (backend-airspy/src/lib.rs:242-244, :303-306).
- **`backend-sim`, `catalog`: keep** as they are.
- **`dsp`: replace.** It is 150 lines and should become the channelizer's home.
- **`engine`: this is where the design drift lives.**
  - `Shared` is a god-struct with about 20 mutexes (lib.rs:145-177).
  - Each live source runs 7 ad-hoc threads, each doing its own full-rate down-conversion.
  - `SignalRecord` is a detector-track snapshot with UI fields injected at read time (lib.rs:127-135).
  - Classifications sit in a side map keyed by record id.
  - `bursts.rs` and `emitters.rs` hold useful algorithms, but they are built around this model.
- **`app` (main.rs alone is 5003 lines): also drifted.** It runs engine logic itself: explore and scan stepping (main.rs:1756-1817), LO masks, occupant hints, device recovery (main.rs:1359) and waterfall backfill. It reaches into engine mutexes directly.

## (d) Worse-than-missing problems

1. **Device-loss recovery is dead code, but BACKLOG calls it done.**
   - No backend ever emits `DeviceLost` (grep: only lib.rs:235 consumes it). `device_lost` therefore never becomes true, so the reopen logic at app/src/main.rs:1359 never runs.
   - The Diagnostics "Overloads" count (main.rs:4061) is always 0 for the same reason.
2. **Position-based labels override real identifications.**
   - "DC offset" and "Internal spur" are sticky: `Emitter::label()` returns them ahead of any protocol vote (emitters.rs:45, :112-114).
   - "DC offset" is assigned to any narrow (<5 kHz), unidentified signal within 1.5 kHz of the tuner centre (context.rs:39-44). This happens even on the real-IF Airspy, which DESIGN L84 says has no DC spike.
   - Explore tunes the receiver exactly onto the emitter it is visiting (app/src/main.rs:1811), so the act of looking puts the target in the "DC offset" zone.
   - "Internal spur" catches narrow signals within 2.5 kHz of any multiple of 5 MHz (context.rs:45-53), e.g. 155.000 or 460.000 MHz.
   - One window that fails sync is enough to relabel an emitter permanently.
3. **Explore "identifies" emitters by borrowing a neighbour's label.**
   - When a visit window overlaps a known wideband occupant, the explorer takes that occupant's label as the identity (app/src/main.rs:1741-1748, :1788-1794).
   - `adopt()` then writes it onto the survey emitter as a strong protocol hit (emitters.rs:392-403).
   - So a narrowband intruder inside an LTE, TV or FM channel is reported as that broadcaster, the opposite of what a TSCM tool should do.
4. **P25 Phase 2 descrambling can quietly stop working on long runs.**
   - `register_nac` adds a NAC on every Phase 1 NID decode, including wrong ones; the set never expires (decode.rs:317).
   - `candidate_sequences()` only tries the first 64 system×NAC pairs in BTreeSet order (p25p2.rs:77-82). Once enough NACs accumulate, the real system's key can fall outside the 64.
   - The global also makes `classify()` results depend on what the process saw earlier (real_captures.rs:175 registers a NAC).
5. **Lock hazards.**
   - The USB callback takes the ring Mutex (lib.rs:218-220).
   - At least seven reader threads hold that same lock while converting chunks of up to 2^18 samples (classify.rs:292, monitor.rs:315, sigmf.rs:110, record_all.rs:305). This contention can make the driver drop samples.
   - monitor.rs:345-348 holds the `p25` lock while taking the `calls` lock. This breaks the documented one-lock rule at lib.rs:142-144. I found no reverse ordering today, so it is a latent deadlock rather than an active one.
6. **Lost samples produce output that looks complete.**
   - record_all skips past a lost stretch (record_all.rs:297-301) but writes SigMF metadata that implies contiguous samples.
   - The live FFT skip (lib.rs:649-652) and the monitor skip (monitor.rs:308-312) raise no Gap.
7. **Burst tracker defects.**
   - Merging and union-only extents can fuse adjacent channels (bursts.rs:394-409).
   - On merge, `mean_snr_db` is inflated: the SNR sums are added but the row count takes the max (bursts.rs:401, :404).
8. **Two clocks for one record.**
   - Records use the processing wall clock (lib.rs:670).
   - Exports use a single TimeAnchor, taken once before `start_rx` and never refreshed (backend-airspy/src/lib.rs:398), so it drifts without bound.
9. **Tests and exports that look healthier than they are.**
   - The golden tests pass when the captures are missing (real_captures.rs:22-26).
   - The full look-back export starts at the ring's tail and must outrun 40 MB/s of overwriting; any stall makes it fail (app/src/main.rs:3165 → sigmf.rs:78-92).
