# SpecTrawl Rebuild, Step 4: Records, Claims and the Store

> Steps use checkbox (`- [ ]`) syntax for tracking. Tasks are done in order, serially, on branch `rebuild-step-4` (worktree `.worktrees/step-4`).

**Goal:** Every emission the new engine sees becomes a `SignalRecord`: timed by sample index on the shared timeline, inside one segment, with occupied bandwidth, peak and mean power, its own list of claims, a stable id, tags and notes, and (by default) a channelized IQ slice on disk. Records group into emitters. Records, claims, emitters, tags and notes are written to SQLite by one store thread and survive a restart. The minimal app draws records on the waterfall and lists them. This is the spec's step 4: "Records and claims + store. Burst detector re-homed, with the merge and SNR fixes from §8. Test: records survive restart."

**Architecture.** The stage thread (one per source, already there) hands each finished row to a `RowSink`, which is the engine's `RecordBook`: per source a `Detector` (the burst detector re-homed from `engine-old::bursts`, with the 2026-11-04 extent and bridging rules and the §8 SNR fix) turns rows into records, and an `EmitterBook` (re-homed from `engine-old::emitters`, grouping and confirmation only) groups them. The book keeps the open records and a bounded window of recent closed ones, and exposes them as one immutable `Arc<RecordsView>` that it rebuilds only when something changed; the snapshot publisher puts that `Arc` in every snapshot, so an interface that skips snapshots never misses a record. Closed records, emitter changes, tags and notes go over a queue to the store thread (SQLite, WAL), which also answers history queries and loads the recent window and the next ids at start. A recorder consumer on the bus (priority `Recorder`) writes each confirmed record's channelized slice as i16 SigMF under a disk cap.

**Tech stack:** Rust 2024 workspace; `rusqlite` 0.40 with `bundled` (engine, as in `engine-old`); `backend-sim` for every test; `backend-airspy` for the on-air checks.

**Spec:** `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` §4.4 (records and emitters), §4.8 (store, slice tier), §4.9 (contract), §6 step 4, §7, §8 (burst tracker defects), §9. `DESIGN.md` §5 (detection) and §6 (the record) stand where the spec does not amend them. BACKLOG lines tagged "step 4" (DESIGN §1 L12–13, §5 L154–155, §6 L170–182, §14 L309) are each closed or re-tagged with a reason in Task 9.

## Global constraints

- SpecTrawl stays MIT; no OnAir code (spec §3).
- "No feature creates a thread, a down-converter or a clock" (spec §7). The one new thread is the store's, which spec §4.8 calls for ("written by a single store thread off a queue"): the rules test's allowed list gains `store/mod.rs` and nothing else. The detector runs on the stage thread through `RowSink`; the recorder is a bus consumer on the pool. `SystemTime::now(` stays out of `crates/engine/src`: a record's wall times come from `Source::time_of` (the timeline), and the store's own timestamps (`updated`) are the record's times, not the clock.
- The engine never blocks on the store. The queue is bounded (4096 operations); a full queue drops the operation, counts it, and the snapshot shows the count (`StoreView::dropped`). Queries are answered asynchronously into a later snapshot.
- The app keeps its rules: no thread, no lock, no clock, `api` types only (except `engine::snapshot::spawn` in `lib.rs`). Tags, notes and queries are `Command`s.
- Kept crates (`detect`, `sdr-core`, backends, `catalog`) need no change in this step. `detect` is not called: classification is step 6. Records carry the claims step 4 can make (`Position`, and the record's `Features` summary); `SyncBank`, `Decoder` and `BandPlan` claims are defined but produced from step 6.
- `cargo fmt --all --check`, `cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p api -p engine -p spectrawl-app --all-targets -- -D warnings` and `cargo test --workspace` stay clean.
- `engine-old` and `app-old` are reference only: the detector and emitter algorithms are re-homed (rewritten against the new types, keeping their tests), not moved by `use engine_old`. The new engine must not depend on `engine-old`.

## Builder choices (spec §9), fixed by this plan

- **Detector input.** The verified detector ran on 16 384-bin rows (610 Hz at 10 MS/s) at 50 rows/s, and its thresholds, extents and tests are tuned there. It keeps that view: the stage's row period becomes 20 ms (from 50 ms), and the detector reduces each 65 536-bin row by summing groups of four bins (power), giving 610 Hz bins. The waterfall shows the full-resolution rows as before; its history grows from 1024 to 2048 rows (41 s at 20 ms).
- **Detector configuration** = `engine-old::bursts::DetectorConfig::live(50.0)`, with `usable_fraction` from the device's `usable_bandwidth_fraction`, and the 2026-11-04 rules (`CORE_SHARE = 10`, `FSK_PAIR_HZ = 11 kHz`, bridging by close / frequent / touching).
- **In-memory window.** The book keeps all open records and the newest 5000 closed records across sources, and every emitter seen in the last 24 h. Older ones live only in the store (reached by `QueryRecords`).
- **Ids.** `RecordId` and `EmitterId` are `u64`, allocated by the book from the store's `next_record_id` / `next_emitter_id` (loaded at start, written back with every batch), so they are stable across restarts and never reused.
- **Store file.** `<Documents>/SpecTrawl/records.db` (the old app's `spectrawl.db` has a different schema and stays its own). Schema version 1: `meta`, `records`, `claims`, `emitters`, `record_tags`, `record_notes`. WAL, `synchronous=NORMAL`, one transaction per batch (the writer drains the queue, at most every 250 ms).
- **Slice tier.** On by default. Each record is sliced once it is confirmed (`rows ≥ min_rows`), from 50 ms before its first sample (back-filled from the ring) until it closes or reaches 120 s; channel bandwidth = 1.25 × the record's, at least 12.5 kHz; output rate the channelizer's choice for that bandwidth; i16 interleaved SigMF; files under `<Documents>/SpecTrawl/slices/<yyyy-mm-dd>/<record id>.sigmf-{meta,data}`; disk cap 4 GB, oldest first; an evicted slice leaves the record with `iq_clip = None` and `slice_evicted = true`. A record whose start had already left the ring gets a slice that starts later and says so (`IqClip::from_sample`).
- **Emitters** are per device serial (radios differ in their spurs, and `Position` claims are per device); merging the same transmitter across receivers is left to BACKLOG.

## Review focus

Failure modes the step 4 tests must pin, each with its task:

1. **A record spans a retune or a hole.** A segment boundary (retune or gap) closes every open record at the boundary; a record never covers samples from two segments, and `segment` names the one it is in. (Task 3.)
2. **Smeared rows widen or fuse records.** The 2026-11-03/04 cases: occasional smeared rows don't widen a record; one smeared row doesn't fuse established neighbours 25 kHz apart; FSK tones stay one record; an FM swing keeps its width; pieces of a wide signal become one record. All `engine-old::bursts` tests are carried over. (Task 3.)
3. **Merging inflates SNR** (spec §8): two tracks overlapping in time merge into the larger row count with each mean (SNR, level) the row-weighted mean of the two, never above either; `engine-old` already did this (the fix predates the spec's note), and the port keeps it under a test. (Task 3.)
4. **Records lost or renumbered across a restart.** Close the engine with records, tags and notes; open a new engine on the same store; the same ids, times, extents, claims, tags and notes come back, and new records get ids above all old ones. (Task 6.)
5. **The store stalls the engine.** A store whose writes take 2 s each: rows, records and snapshots keep flowing, the queue fills, drops are counted and shown, nothing deadlocks, shutdown waits at most the configured time. (Task 6.)
6. **An interface misses records by skipping snapshots.** Read every fifth snapshot for 10 s of simulator time: the app's record list equals the book's. (Tasks 5 and 8.)
7. **The slice is not the record's signal.** A sliced record's file holds the record's emitter (simulator CW at a known offset: the slice's spectrum peaks there, within one channel bin) and its timing (the slice's first sample is `iq_clip.from_sample`, within one block of `first_sample − pre-roll` when the ring still held it). (Task 7.)
8. **The disk cap is ignored.** With a 400 kB cap (a 0.6 s slice of the narrowest channel is ~94 kB at 39 kS/s), the slice directory never exceeds the cap plus two slices, the oldest go first, and their records say so. (Task 7.)

---

## File map

| Path | Responsibility |
|---|---|
| `crates/api/src/records.rs` | `RecordId`, `EmitterId`, `SignalRecord`, `Claim`, `Evidence`, `IqClip`, `EmitterView`, `RecordsView`, `RecordQuery`, `QueryResult`, `StoreView` |
| `crates/api/src/command.rs` | `Command::{Tag, Untag, Note, QueryRecords}`, `Outcome::Queried` |
| `crates/api/src/snapshot.rs` | `Snapshot::{records, store, query}` |
| `crates/engine/src/stage.rs` | `RowSink`, row period 20 ms |
| `crates/engine/src/records/mod.rs` | module docs, re-exports |
| `crates/engine/src/records/detector.rs` | `Detector`, `DetectorConfig`, tracks, cores, bridging (re-homed `bursts.rs`) |
| `crates/engine/src/records/emitters.rs` | `EmitterBook` grouping, confirmation, containment, refit/fold, duty, spur/image `Position` claims |
| `crates/engine/src/records/book.rs` | `RecordBook`: per-source detectors, emitter books, the window, `RecordsView`, the `RowSink` impl |
| `crates/engine/src/store/mod.rs` | `Store`, `StoreConfig`, the "store" thread, the queue |
| `crates/engine/src/store/schema.rs` | schema v1, row ↔ type mapping, load, query |
| `crates/engine/src/recorder.rs` | the slice consumer, `SliceConfig`, the cap |
| `crates/engine/src/snapshot/build.rs` | records, store view and query result into the snapshot |
| `crates/engine/src/snapshot/devices.rs` | `Tag`/`Untag`/`Note`/`QueryRecords` handling |
| `crates/engine/tests/records_sim.rs` | detector + book on the simulator (review focus 1, 3, 6) |
| `crates/engine/tests/store_restart.rs` | review focus 4 and 5 |
| `crates/engine/tests/slices_sim.rs` | review focus 7 and 8 |
| `crates/engine/examples/records_db.rs` | headless on-air run into a store (the new engine's `trawl_db`) |
| `crates/app/src/records_pane.rs` | the records list: filter, sort, tags, notes, history query |
| `crates/app/src/receiver_pane/body.rs` | record boxes on the waterfall |
| `crates/app/tests/headless_sim.rs` | records appear, boxes align, skipped snapshots lose nothing |

---

### Task 1: Rows at the detector's rate, and the row hook

**Files:** `crates/engine/src/stage.rs`, `crates/engine/src/channels/mod.rs`, `crates/engine/src/engine.rs`, `crates/app/src/receiver_pane/body.rs` (history depth), spec §9.

- [ ] Test first (`stage_sim.rs`): at 10 MS/s with default `StageConfig`, rows arrive every 20 ms of samples (±1 block) and each covers `ceil(0.02·fs/L)` blocks; a `RowSink` registered at attach is called once per row, in order, on the stage thread, with the same `Arc<Row>` the history holds.
- [ ] `pub trait RowSink: Send + Sync { fn on_row(&self, source: SourceId, row: &Arc<Row>); }`. `RowHistory::push` returns the `Arc`; the stage calls the sink after pushing. A sink that panics is caught and logged once; the stage keeps running (pin it).
- [ ] `EngineConfig::row_sink: Option<Arc<dyn RowSink>>`, passed by `ChannelBus::attach` to `Stage::start` (the bus holds it in `BusConfig`, `None` by default so step 2 and 3 tests are unchanged).
- [ ] `StageConfig::default().row_period_secs = 0.02`. The app's history depth becomes 2048 rows; the snapshot still carries the newest 16 rows (320 ms at 20 ms, more than a 50 ms tick).
- [ ] Existing stage, snapshot and headless tests pass with the new period (adjust only the expectations that count rows per second).

### Task 2: The record types in `api`

**Files:** `crates/api/src/records.rs`, `lib.rs`, `snapshot.rs`, `command.rs`.

- [ ] `RecordId(u64)`, `EmitterId(u64)`.
- [ ] `SignalRecord { id, source: SourceId, serial: Arc<str>, role: Role, segment: SegmentId, tuner_hz, first_sample, end_sample, t_start: SystemTime, t_end: SystemTime, f_lo_hz, f_hi_hz, bin_hz, peak_dbfs, mean_dbfs, mean_snr_db, max_snr_db, rows, open: bool, claims: Vec<Claim>, emitter: Option<EmitterId>, tags: Vec<String>, notes: Vec<Note>, iq_clip: Option<IqClip>, slice_evicted: bool }` with `center_hz()`, `bandwidth_hz()`, `duration()`, and `headline() -> Option<&Claim>` (spec §4.5: the highest-confidence `Decoder` claim, else the highest `SyncBank`, else `None`, which the UI shows as "unknown").
- [ ] `Role { Live, Survey }` (DESIGN §6 L170; `Survey` is used from step 4a).
- [ ] `Claim { label: String, confidence: f32, by: Evidence }`; `Evidence` exactly as spec §4.4: `SyncBank { protocol, syncs, frames_tried, frames_decoded }`, `Features { bandwidth_hz, snr_db, duty, envelope_cv: Option<f32> }`, `Decoder { mode, locked, summary }`, `BandPlan { entry }`, `Neighbour { emitter: EmitterId }`, `Position(PositionKind)` with `PositionKind { Dc, SpurGrid { grid_hz }, Image { of: EmitterId }, Comb { spacing_hz, lines } }`. `Neighbour` confidence is capped at 0.5 by its constructor.
- [ ] `IqClip { path: PathBuf, from_sample, samples, rate, center_hz }`; `Note { text, time: SystemTime }`.
- [ ] `EmitterView { id, serial, center_hz, bandwidth_hz, first_seen, last_seen, records, on_time_s, duty, confirmed, headline: Option<Claim> }` (computed from its records' claims, spec §4.4: "an emitter's label is computed from its records' claims").
- [ ] `RecordsView { version, records: Vec<Arc<SignalRecord>>, emitters: Vec<Arc<EmitterView>> }` (open records first, then closed newest first); `StoreView { path, written, queued, dropped, error: Option<String> }`; `RecordQuery { serial, from, to, f_lo_hz, f_hi_hz, text, tag, limit }`; `QueryResult { seq, records: Vec<Arc<SignalRecord>>, truncated }`.
- [ ] `Snapshot { records: Arc<RecordsView>, store: Option<StoreView>, query: Option<Arc<QueryResult>> }`; `Snapshot::empty()` gets an empty view.
- [ ] Commands: `Tag { record, tag }`, `Untag { record, tag }`, `Note { record, text }`, `QueryRecords(RecordQuery)`; `Outcome::Queried`. Unit tests for `headline`, the `Neighbour` cap and `Debug` staying short (a record's `Debug` doesn't print claims' full evidence lists beyond 8).

### Task 3: The detector, re-homed

**Files:** `crates/engine/src/records/detector.rs`, `records/mod.rs`.

- [ ] Carry over every test in `engine-old/src/bursts.rs` (including the 2026-11-03/04 ones: smeared rows, neighbours, FSK tones, FM swing, wide-signal pieces), rewritten to feed `api::Row`s (linear power) at 65 536 bins; they fail before the port.
- [ ] Add: a retune boundary closes every open record at the last row of the old segment, and the next record starts in the new segment (review focus 1); a `Gap` segment does the same; the §8 SNR rule (review focus 3: merging a 30 dB and a 10 dB track of 10 overlapping rows each gives a 20 dB mean over 10 rows); `mean_dbfs` is the mean over rows of the mean power across the record's core bins.
- [ ] Port `Track` (hits, core, reach, extend/cover/absorb), `associate` (close / frequent / touching bridging, splitting at core midpoints), the floor (block percentile, usable fraction) and `to_record`, onto sample indices: `first_sample` = the first row's `first_sample`, `end_sample` = the last row's `end_sample`; wall times from the book (Task 5).
- [ ] The 4:1 reduction (sum of linear power over four bins, then dB) lives here, with a test that a tone exactly on a reduced bin edge lands in one of the two neighbours at full power.
- [ ] Ids are allocated by a closure the book passes (`FnMut() -> RecordId`), not a static counter.

### Task 4: Emitters, re-homed (grouping only)

**Files:** `crates/engine/src/records/emitters.rs`.

- [ ] Carry over `engine-old::emitters` tests for grouping and confirmation: `find` (containing emitter first, else the closest it fits), `contains`, `fits`, `covers`, `refit` (an emitter built from a record's early fragments takes its final shape), `fold`, `duty` (counting the open record), confirmation (records within the window), expiry of unconfirmed emitters, and partial records not moving centre or width.
- [ ] Spur and image context become `Position` claims on the record (`SpurGrid`, `Image`, `Comb` with the `COMB_MIN_DUTY = 0.5` rule), never a sticky label (spec §4.4): a record keeps the claim; the emitter's headline ignores `Position` claims when any other claim exists.
- [ ] Not ported, and listed in BACKLOG by Task 9 with "step 6": label voting and weights, the strong-protocol rule, FM-station folding by label, explore verdicts (they consume claims from the detector bank).
- [ ] `EmitterBook::load(Vec<EmitterView>)` for restart; emitters older than 24 h are kept in the store only.

### Task 5: The record book and the snapshot

**Files:** `crates/engine/src/records/book.rs`, `crates/engine/src/engine.rs`, `crates/engine/src/snapshot/{build,mod}.rs`, `crates/engine/tests/records_sim.rs`.

- [ ] `RecordBook` implements `RowSink`: per source (created on the first row, removed on `Engine::remove_source`, which closes its open records at the head) a `Detector` and the serial's `EmitterBook`; records get wall times from `Source::time_of`; closed records and emitter changes are sent to the store (Task 6) and kept in the window.
- [ ] `RecordBook::view() -> Arc<RecordsView>`: rebuilt at most once per call when the book's version moved, else the same `Arc` (pin both: two calls without a change return `Arc::ptr_eq` views).
- [ ] The publisher puts `book.view()` in every snapshot. Lock order: the book's lock is never held while calling the store or the bus (pin with a test that tags a record while rows flow, under `parking_lot`'s deadlock detection as in step 2).
- [ ] `records_sim.rs` on the default simulator scenario (paced, 20 s): every continuous emitter is one open record at its frequency (±1 reduced bin) and bandwidth within 25 % of its occupied bandwidth; every bursty one yields one closed record per burst, timed within 2 rows; the CW at 855.000 MHz gives a narrow record; no record spans the retune the test makes at 10 s (review focus 1); reading every fifth snapshot, the union of views equals the book (review focus 6).

### Task 6: The store

**Files:** `crates/engine/src/store/{mod,schema}.rs`, `crates/engine/Cargo.toml` (`rusqlite`), `crates/engine/tests/{rules,store_restart}.rs`.

- [ ] `StoreConfig { path, queue: 4096, batch_every: 250 ms, window: 5000, emitter_window: 24 h }`; `Store::open(cfg) -> (Store, Loaded { records, emitters, next_record_id, next_emitter_id })`; `EngineConfig::store: Option<StoreConfig>` (`None` keeps every earlier test store-free).
- [ ] The "store" thread drains the queue into one transaction per batch (`UpsertRecord`, `UpsertEmitter`, `Tag`, `Untag`, `Note`, `SetNextIds`), answers `Query` (filter in SQL, newest first, `limit` ≤ 5000, `truncated` when more), and on drop flushes what is queued and stops within `StoreConfig::shutdown` (2 s default).
- [ ] Schema v1 in `schema.rs`, created if absent, refused with a clear error if `meta.schema` is newer; claims as rows (`record_id, ord, label, confidence, kind, evidence_json`); times stored as Unix seconds (f64) plus the sample indices and segment id for the session they belong to (`records.session`, a row in `sessions` per engine start).
- [ ] `store_restart.rs` (review focus 4): simulator 6 s → tag one record, note another → drop the engine (store flushes) → new engine, same file → the loaded view has the same records (id, times to 1 ms, extents, claims, tags, notes), emitters with their ids, and a new record's id is above every old one.
- [ ] `store_restart.rs` (review focus 5): a store wrapper whose writes sleep 2 s; 5 s of simulator; snapshot versions keep rising at 20 Hz (±20 %), `StoreView::queued` rises, `dropped` counts past 4096 (small queue in the test), the engine drops in under `shutdown + 1 s`.
- [ ] Rules test: allowed thread creators gain `store/mod.rs`.

### Task 7: The slice tier

**Files:** `crates/engine/src/recorder.rs`, `crates/engine/src/records/book.rs`, `crates/engine/tests/slices_sim.rs`.

- [ ] `SliceConfig { enabled: true, dir, cap_bytes: 4 GB, pre_roll: 50 ms, max: 120 s }` in `EngineConfig`.
- [ ] When a record is confirmed the book asks the recorder to slice it: a bus subscription (`Priority::Recorder`, `Start::At(first_sample − pre_roll)`, channel `center_hz`, `max(1.25·bw, 12.5 kHz)`) writing i16 interleaved SigMF (`core:sample_start` = the slice's first sample's offset, `spectrawl:record_id`, `spectrawl:source_sample`); ends when the record closes or `max` is reached; the record's `iq_clip` is set on the next view.
- [ ] A `Start::At` the ring no longer holds starts at the oldest readable block and `IqClip::from_sample` says where (pin it).
- [ ] The cap: after each finished slice the recorder sums the directory and deletes oldest-first to the cap, sending `SliceEvicted(record)` to the book and the store (review focus 8, 400 kB cap in the test).
- [ ] `slices_sim.rs` (review focus 7): a CW emitter at +312.5 kHz: its record's slice, read back, has its spectral peak at the emitter (within one channel bin) and its first sample within one block plus the channel filter's delay (N/4) of `first_sample − pre_roll`; the slice is cut back to the record's end once the record closes (the end is known only a hangover later).
- [ ] A shed recorder (pool saturated) leaves the record with a partial slice and a log line; nothing else changes.

### Task 8: Records in the app

**Files:** `crates/app/src/records_pane.rs`, `crates/app/src/{app,tabs}.rs`, `crates/app/src/receiver_pane/body.rs`, `crates/app/tests/headless_sim.rs`.

- [ ] Waterfall boxes: each record of the receiver's segment whose samples are inside the shown history is outlined at `f_lo..f_hi` × its rows (open records with an open top edge); the box's colour is the theme's neutral for "unknown" (step 6 colours by claim kind, spec §2 anti-goal: inferred and decoded labels look different). Click selects the record (shared selection with the list).
- [ ] Records pane (a dock tab): columns time, receiver, MHz, kHz, duration, peak dBFS, SNR, headline ("unknown" when none, styled as such), emitter, tags; sort by any column; filter text (frequency, tag, note words); a "Search history" field that sends `QueryRecords` and shows the result in its own list when its `seq` matches; tag add/remove and a note editor per selected record (commands).
- [ ] Headless: on the simulator for 6 s, the pane lists the default scenario's emitters at their frequencies; a box sits over each continuous emitter's column; tagging a record shows the tag in a later snapshot; feeding the app every fifth snapshot gives the same list (review focus 6). The app's rules tests still pass.

### Task 9: On air, docs and BACKLOG

**Files:** `crates/engine/examples/records_db.rs`, BACKLOG.md, spec §9, `docs/` notes.

- [ ] `records_db <backend> <serial> <db> "MHz:secs,..."`: the new engine headless into a store, printing per stop the confirmed emitters (centre, width, records). Run it on the HydraSDR while `engine-old`'s `trawl_db` runs on the R2 through the same multicoupler (two USB controllers), same plan `855.5:150,930:90,98:60,460:60`. Pass: P25-site channels 9–14 kHz wide at 35+ dB (verified ~12 kHz), FLEX channels 12–16 kHz, FM stations one emitter each 150–200 kHz, emitter counts per stop within 20 % of `engine-old`'s, no record wider than 150 kHz at 851–861 MHz. Record the numbers in BACKLOG.
- [ ] Spec §9: "Chosen in step 4" with this plan's builder choices.
- [ ] BACKLOG: each "step 4" line closed or re-tagged with its reason. Expected: DESIGN §5 L154 (CFAR) stays PARTIAL, re-tagged step 6 (the block-percentile floor is what was verified on air; CFAR is judged against it on the acceptance captures); §6 L178 (classification list) is DONE as a type and its producers come in step 6; §1 L12 (browse) and L13 (tags, notes) DONE; §14 L309 DONE.
- [ ] The memory of what step 4 left: emitters per serial (cross-receiver merge), label voting (step 6).

## Self-review notes

- Every review-focus item has a test in a named task. The only new thread is the store's, which the spec calls for; the rules test pins it.
- The 20 ms row period is the one change to step 2/3 behaviour; it is there so the detector keeps the configuration its on-air results came from, and Task 1 re-checks the step 2 and 3 tests under it.
- Classification stays out: no `detect` call, no labels beyond what the record's own geometry supports. "Unknown" is the honest headline until step 6.
