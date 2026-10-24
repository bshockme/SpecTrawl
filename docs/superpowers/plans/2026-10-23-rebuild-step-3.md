# SpecTrawl Rebuild, Step 3: `api` and the Minimal App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An `api` crate holding the only types the app and engine share, a 20 Hz snapshot publisher in the engine that also owns device opening and command handling, and a new egui app that shows each receiver's waterfall, spectrum trace and one-line status from snapshots alone; plus the items parked by the step 2 reviews.

**Architecture:** `api` defines `Snapshot` (immutable, versioned, published into an `ArcSwap`), `Command` (an enum sent over a channel, acknowledged by sequence number in a later snapshot) and the data types both sides read (`Row`, `Segment`, ids, priorities), which move out of `engine` and are re-exported there. `engine::snapshot` is one thread per process: every 50 ms it drains the command queue, applies each command to the `Engine` (open/close a device through the backends it was given, tune, gain, mode, budget), composes a status sentence per receiver from a pure function, and stores a new `Snapshot`. The app holds one `Arc<Snapshot>` per frame, keeps a CPU-side history of max-reduced dB rows per receiver, uploads one row per new `Row` into a GPU ring texture, and sends commands; it takes no lock, creates no thread and reads no clock.

**Tech Stack:** Rust 2024 workspace; `arc-swap` 1.7 (api, engine); `eframe`/`egui` 0.36.2 with the `wgpu` renderer, `egui_dock` 0.21, `serde`/`serde_json`, `env_logger` 0.11 (app); `backend-sim` for every test, `backend-airspy` for the real device.

**Spec:** `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` (§4.1 "Shown in seconds", §4.9, §5, §6 step 3, §7, §9). `DESIGN.md` §9 (waterfall rules) and §10 (UI never blocks) stand. `docs/ui-design.md` §1 and §3.1 (the receiver letter, the header toolbar) are the reference for the pane; the rest of that document is step 8.

## Global Constraints

- SpecTrawl stays MIT; no OnAir code is linked or copied (spec §3).
- `detect`, `sdr-core`, `backend-airspy`, `backend-sim`, `catalog` are kept crates: no changes in this plan.
- "No feature creates a thread, a down-converter or a clock" (spec §7). After this plan the thread creators in `crates/engine/src` are `ring/keeper.rs`, `stage.rs`, `channels/sched.rs` and `snapshot/mod.rs`; `Ddc::new(` and `SystemTime::now(` appear nowhere in `crates/engine/src`. The app creates no thread, takes no lock (`Mutex`, `RwLock`, `parking_lot`, `.lock()` absent from `crates/app/src`) and reads no clock (`SystemTime::now(` and `Instant::now(` absent from `crates/app/src`; frame pacing is egui's). Tasks 4 and 5 pin these with rules tests.
- The app depends on `engine` for exactly one call, `engine::snapshot::spawn`, in `crates/app/src/lib.rs`; every other file in the app uses `api` types only (spec §4.9 "never touches engine state"; Task 5's rules test).
- Nothing is ported by moving pane code from `app-old` (spec §6). `app-old/src/theme.rs` (colour constants, `apply`, frames, `waterfall_lut`) is a design asset, not pane code, and may be copied verbatim minus `class_color`. The old `spectrum_view.rs` is reference only.
- Builder choices (spec §9) fixed by this plan: snapshot cadence 20 Hz (50 ms); a snapshot carries each receiver's newest 16 finished rows, the last 64 acks and the last 200 log lines; waterfall history 1024 rows of 4096 max-reduced columns, texture 1024 rows by the displayed width, zoom no narrower than 24 history columns; receivers take letters `A`, `B`, … in order of opening, lowest free letter first (`S` is reserved for the sweep, step 4a); default budget 30 s guaranteed, ceiling half of physical RAM.
- `cargo fmt --all --check` and `cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p api -p engine -p spectrawl-app --all-targets -- -D warnings` stay clean (Task 7 adds the two crates to CI).
- Steps 0–5 are serial on one branch: work on branch `rebuild-step-3` in a worktree under `.worktrees/`, forked from `master` at 652d973 or later.
- Every commit message ends with the two attribution lines the session carries (`Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and the `Claude-Session:` line), whatever a subagent's own reminder says.

## Review Focus

Failure modes the spec implies that the step 3 tests must pin (each has its test in the task named):

1. **The engine does something slow while the UI runs.** Opening an Airspy or a stalled pool must not stall a frame: the app reads an `ArcSwap` and never waits. A backend whose `open` sleeps 700 ms stalls the snapshot thread, and the app's frames stay under 100 ms throughout. (Task 7.)
2. **The UI drops frames.** Rows must not be lost when the app misses snapshots for up to 0.8 s: each snapshot carries the newest 16 rows and the pane appends only rows newer than the last it has. Feeding every third snapshot gives a contiguous history. (Tasks 4 and 6.)
3. **A retune while the waterfall runs.** Rows of the new tuning must not be drawn under the old frequency axis: a centre or rate change clears the history and the log says so. (Task 6.)
4. **A device lost or closed while its pane is open.** The pane says "Device lost" with the reason and stays until the user closes it; `Close` removes the receiver from the snapshot and the tab from the dock. (Tasks 4 and 7.)
5. **A command that fails.** Opening a device twice or tuning outside the device's range is acknowledged with the error, which the log shows, and the pane shows the snapshot's tuning, not the request. (Tasks 4 and 5.)

Parked from the step 2 reviews and placed here (Task 1): `Start::At` designed against the head's segment; `Engine::bus().detach` leaving `Engine::sources` stale; the recompute LRU never trimmed or counted; `Stage::bins(0)` panicking; the `end_reason` doc; the fall-behind test comment; `windowed_sinc` and `design_channel` input guards; `Engine::remove_source` detaching a source it does not own; `take_next`'s unreachable branch. Left in BACKLOG with reasons: back-filling subscriptions shed as laggards, ended-entry scans, the sub-side jump landing at `oldest/L + 2` (a back-filling subscription wants every readable block, so the asymmetry with the stage is intended).

---

## File map

| Path | Responsibility |
|---|---|
| `crates/api/src/lib.rs` | module list, re-exports of `sdr_core::{Capabilities, DeviceInfo, GainSetting}`, `SourceId`, `Mode`, `ChannelId`, `Priority`, `ConsumerState` |
| `crates/api/src/timeline.rs` | `SegmentId`, `Boundary`, `Segment`, `Row` (moved from `engine`) |
| `crates/api/src/snapshot.rs` | `Snapshot` and its views |
| `crates/api/src/command.rs` | `Command`, `Outcome`, `Ack`, `CommandSender`, `CommandReceiver`, `command_channel` |
| `crates/api/src/handle.rs` | `Handle` |
| `crates/api/src/fmt.rs` | `mhz`, `rate`, `secs`, `count` |
| `crates/engine/src/snapshot/mod.rs` | `PublisherConfig`, `spawn`, the "snapshot" thread loop |
| `crates/engine/src/snapshot/devices.rs` | `Registry`: backends, enumerated devices, open receivers and their letters |
| `crates/engine/src/snapshot/build.rs` | `Snapshot` assembly from engine state |
| `crates/engine/src/snapshot/status.rs` | `StatusInput`, `status_line` |
| `crates/app/src/lib.rs` | `run()`: engine config, backends, `engine::snapshot::spawn`, eframe |
| `crates/app/src/main.rs` | logger, `spectrawl_app::run()` |
| `crates/app/src/app.rs` | `App`: snapshot per frame, dock, panes, command collection, persistence |
| `crates/app/src/tabs.rs` | `Tab` |
| `crates/app/src/theme.rs` | copied from `app-old` |
| `crates/app/src/waterfall.rs` | pure: `History`, `View`, `Lut`, `reduce_db`, `colorize`, ring index, quads, Hz↔x |
| `crates/app/src/trace.rs` | pure: `Trace`, `auto_range`, `nice_step` |
| `crates/app/src/receiver_pane.rs` | header, status line, spectrum plot, waterfall painting, interaction |
| `crates/app/src/devices_pane.rs` | device list, open/close, budget |
| `crates/app/src/log_pane.rs` | the snapshot's log |
| `crates/engine/tests/snapshot_sim.rs`, `crates/app/tests/{rules,headless_sim}.rs` | integration tests |

---

### Task 1: Step 2 parked items

**Files:**
- Modify: `crates/engine/src/channels/mod.rs` (`subscribe` ~281, `detach` ~261, `ConsumerStats::end_reason` doc ~155), `crates/engine/src/channels/sub.rs`, `crates/engine/src/channels/sched.rs` (`take_next`), `crates/engine/src/stage.rs` (`Recompute`, `bins` ~169, `StageStats`), `crates/engine/src/engine.rs` (`remove_source`), `crates/dsp/src/fir.rs` (`windowed_sinc`), `crates/dsp/src/channelizer.rs` (`design_channel` ~46), `crates/engine/tests/stage_sim.rs` (fall-behind test comment), `crates/engine/tests/channel_backfill.rs`, `BACKLOG.md`
- Test: `crates/engine/tests/channel_backfill.rs`, unit tests in `stage.rs`, `channelizer.rs`, `fir.rs`

**Interfaces:**
- Consumes: everything as it is on master 652d973.
- Produces: `StageStats::recompute_held: usize`; `ChannelBus::detach` is `pub(crate)`; `Engine::config(&self) -> &EngineConfig`.

- [ ] **Step 1: Write the failing back-fill test**

In `channel_backfill.rs` (use its existing helpers: a paced sim at 10 MS/s centred 855.25 MHz, `Collector`, `wait_until`):

```rust
#[test]
fn a_backfill_started_before_a_retune_is_designed_for_its_own_segment() {
    // 859.0 MHz is inside the usable span at 855.25 MHz and outside it at 851.25 MHz.
    // ... open engine + sim, wait 1.0 s of samples, note `before = src.head()`, retune to 851.25e6, wait 0.5 s ...
    let c = Collector::new();
    // 0.6 s before the retune: about 183 old-segment blocks of 256 output samples (78125 S/s).
    let id = bus.subscribe(spec(src.id(), 859.0e6), Start::At(before - 6_000_000), Priority::Classify, Box::new(c.clone())).unwrap();
    wait_until(|| c.samples() > 0 && c.gaps() > 0);
    let stats = bus.stats().consumers.into_iter().find(|s| s.channel.id == id).unwrap();
    assert!(c.samples() >= 40_000, "old-segment blocks were delivered: {}", c.samples());
    assert!(c.gap_reasons().contains(&GapReason::OutOfSpan));
    assert_eq!(stats.state, ConsumerState::OutOfSpan);
}
```

(`Collector::gap_reasons()` may need adding to `tests/common/mod.rs`; keep it to a `Vec<GapReason>` behind the collector's mutex.)

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p engine --test channel_backfill a_backfill_started_before_a_retune`
Expected: FAIL at `subscribe(...).unwrap()` with `Design(OutsideSpan)` (the channel is designed against `segment_at(head)`).

- [ ] **Step 3: Design `Start::At` subscriptions against their own segment**

In `subscribe`: the segment used for the usable-span check and the first design is `source.segment_at(s)` for `Start::At(s)` and `segment_at(head)` for `Start::Live`. The subscription already redesigns when the segment changes; nothing else moves.

- [ ] **Step 4: Run it to verify it passes, then the whole back-fill file**

Run: `cargo test -p engine --test channel_backfill`
Expected: all PASS.

- [ ] **Step 5: The remaining parked items, each with its unit test where one is named**

- `ChannelBus::detach` becomes `pub(crate)`; `Engine::remove_source` is the only path (update any integration test that called `bus.detach` to `engine.remove_source`). `Engine::remove_source` checks its map first and returns `false` without detaching a source it does not own. Add `pub fn config(&self) -> &EngineConfig` to `Engine` (Task 4 reads the budget from it).
- `Stage::bins(0)` returns `Err(ReadError::Overwritten)` (block 0's window starts before sample 0; document it on `bins`). Unit test: `bins_of_block_zero_is_an_error`.
- Recompute LRU: each entry carries the `Instant` of its last hit; `Stage::trim_recompute(&self, idle: Duration)` drops entries unused for `idle`, and the stage thread calls it with 1 s each time it publishes a row. `StageStats::recompute_held: usize` reports the entries held (set under the stats lock by `bins` and `trim_recompute`). Unit test with a stage on an unpaced sim: after a `bins` miss `recompute_held == 1`; after 1.2 s of rows `recompute_held == 0`.
- `ConsumerStats::end_reason` doc: "`Some(Panicked)` with state `Shed` when the consumer panicked in its `Shed` delivery".
- `sched.rs` `take_next`: the `sub == None` branch sets `turn = Idle`.
- `fir::windowed_sinc`: `debug_assert!(taps >= 3 && taps % 2 == 1, "...")`. `design_channel`: `fs`, `usable_fraction` must satisfy `fs > 0.0 && usable_fraction > 0.0 && usable_fraction <= 1.0`, else `BadSpec`; unit test `a_nan_or_zero_rate_is_a_bad_spec` (`fs = f64::NAN` and `fs = 0.0` both `Err(BadSpec)`).
- `stage_sim.rs` fall-behind test: the comment says the stage falls a few blocks behind on a 100 MS/s source and the test checks the jump lands within 0.1 s of the head, not further.
- `BACKLOG.md`: remove the `Start::At` and `Engine::bus().detach` lines; the recompute LRU line is not needed (fixed before it was written); keep the back-filling-laggard and ended-entry-scan lines and add one: "Sub-side Overwritten/Lost jump lands at `oldest/L + 2` while the stage jumps to the live edge: intended, a back-filling subscription wants every readable block."

- [ ] **Step 6: Run the engine and dsp suites, fmt and clippy**

Run: `cargo test -p engine -p dsp && cargo fmt --all --check && cargo clippy -p dsp -p engine --all-targets -- -D warnings`
Expected: all PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add -A crates/engine crates/dsp BACKLOG.md
git commit -m "Engine, dsp: step 2 parked items (Start::At segment, detach path, LRU trim, guards)"
```

---

### Task 2: The `api` crate: shared types moved out of `engine`

**Files:**
- Create: `crates/api/Cargo.toml` (`[package] name = "api"`, workspace version/edition/license; `[dependencies] sdr-core = { path = "../sdr-core" }`, `arc-swap = "1.7"`, `serde = { version = "1", features = ["derive"], optional = true }`; `[features] serde = ["dep:serde"]`, which puts `Serialize, Deserialize` on `SourceId` and `ChannelId`), `crates/api/src/{lib,timeline,snapshot,command,handle,fmt}.rs`
- Modify: `crates/engine/Cargo.toml` (add `api = { path = "../api" }`), `crates/engine/src/segments.rs`, `crates/engine/src/rows.rs`, `crates/engine/src/budget.rs`, `crates/engine/src/channels/mod.rs`, `crates/engine/src/lib.rs`

**Interfaces:**
- Produces (all `pub`, all with `Debug, Clone` and the derives their engine originals had; `Health`, `Status`, `Outcome`, `Ack`, `LogEntry`, `DeviceEntry` also derive `PartialEq`; `Health` and `Outcome` are `Copy`):

```rust
// lib.rs
pub use sdr_core::{Capabilities, DeviceInfo, GainSetting};
pub struct SourceId(pub u32);                 // moved from engine::budget, same derives
pub enum Mode { Live, Sweep }                 // moved from engine::budget
pub struct ChannelId(pub u64);                // moved from engine::channels
pub enum Priority { Recorder, PagerMonitor, Classify, EmitterDecoder, TrunkControl, Listen }   // moved
pub enum ConsumerState { Running, OutOfSpan, Shed, Ended }                                    // moved
// timeline.rs: SegmentId, Boundary, Segment (with hole() and data_from()), Row — moved verbatim from engine::segments / engine::rows
// snapshot.rs
pub struct Snapshot { pub version: u64, pub receivers: Vec<Receiver>, pub devices: Vec<DeviceEntry>, pub bus: BusView, pub acks: Vec<Ack>, pub log: Vec<Arc<LogEntry>> }
impl Snapshot { pub fn empty() -> Snapshot }  // version 0, everything empty
pub struct DeviceEntry { pub info: DeviceInfo, pub open_as: Option<SourceId> }
pub struct Receiver {
    pub id: SourceId, pub letter: char, pub device: DeviceInfo, pub caps: Arc<Capabilities>,
    pub mode: Mode, pub center_hz: f64, pub sample_rate: f64, pub segment: SegmentId,
    pub gain: Option<GainSetting>, pub status: Status, pub ring: RingView, pub stage: StageView,
    pub head: u64, pub head_time: Option<SystemTime>,
    pub rows: Vec<Arc<Row>>,          // the newest finished rows, oldest first (16 at most)
    pub consumers: Vec<ConsumerView>,
}
pub enum Health { Ok, Warning, Lost }
pub struct Status { pub health: Health, pub text: String, pub reason: Option<String> }
pub struct RingView { pub held_secs: f64, pub guaranteed_secs: f64, pub ceiling_secs: f64, pub device_gaps: u64, pub starved_samples: u64, pub overloads: u64, pub device_lost: bool }
pub struct StageView { pub blocks: u64, pub skipped_blocks: u64, pub block_micros: f64, pub behind_secs: f64 }
pub struct ConsumerView { pub id: ChannelId, pub center_hz: f64, pub bandwidth_hz: f64, pub priority: Priority, pub lag_secs: f64, pub state: ConsumerState }
pub struct BusView { pub workers: usize, pub saturated: bool, pub shed_total: u64, pub panicked_total: u64 }
pub struct LogEntry { pub n: u64, pub receiver: Option<char>, pub text: String }
// command.rs
pub enum Command {
    Enumerate,
    Open { backend: String, serial: String },
    Close(SourceId),
    Tune { source: SourceId, center_hz: f64 },
    SetGain { source: SourceId, gain: GainSetting },
    SetMode { source: SourceId, mode: Mode },
    SetBudget { guaranteed_secs: f64, ceiling_bytes: u64 },
}
pub enum Outcome { Done, Opened(SourceId) }
pub struct Ack { pub seq: u64, pub result: Result<Outcome, String> }
pub struct CommandSender { /* mpsc::Sender<(u64, Command)>, Arc<AtomicU64> */ }   // Clone
impl CommandSender { pub fn send(&self, c: Command) -> u64 }                        // the sequence number, from 1
pub struct CommandReceiver { /* mpsc::Receiver<(u64, Command)> */ }
impl CommandReceiver { pub fn drain(&self) -> Option<Vec<(u64, Command)>> }         // None once every sender is gone
pub fn command_channel() -> (CommandSender, CommandReceiver)
// handle.rs
pub struct Handle { /* Arc<ArcSwap<Snapshot>>, CommandSender */ }                   // Clone
impl Handle {
    pub fn new(snapshot: Arc<ArcSwap<Snapshot>>, commands: CommandSender) -> Handle;
    pub fn latest(&self) -> Arc<Snapshot>;      // load_full
    pub fn send(&self, c: Command) -> u64;
}
// fmt.rs
pub fn mhz(hz: f64) -> String      // "855.250 MHz"
pub fn rate(sps: f64) -> String    // "10 MS/s", "2.5 MS/s", "768 kS/s"
pub fn secs(s: f64) -> String      // "2 m 40 s" (≥ 60), "30 s" (10..60, rounded), "0.5 s" (< 10, one decimal); negatives read as 0
pub fn count(n: usize, noun: &str) -> String   // "1 channel", "3 channels"
```

- `engine` re-exports every moved type from its old path (`pub use api::{SourceId, Mode}` in `budget.rs`, `pub use api::{Boundary, Segment, SegmentId}` in `segments.rs`, `pub use api::Row` in `rows.rs`, `pub use api::{ChannelId, Priority, ConsumerState}` in `channels/mod.rs`) so nothing downstream changes. `RowHistory` gains `pub fn newest(&self, n: usize) -> Vec<Arc<Row>>` (oldest first).

- [ ] **Step 1: Write the failing `fmt` tests in `crates/api/src/fmt.rs`**

```rust
#[test]
fn formats_read_like_the_spec() {
    assert_eq!(mhz(855.25e6), "855.250 MHz");
    assert_eq!(mhz(124.85e6), "124.850 MHz");
    assert_eq!(rate(10e6), "10 MS/s");
    assert_eq!(rate(2.5e6), "2.5 MS/s");
    assert_eq!(rate(768e3), "768 kS/s");
    assert_eq!(secs(160.0), "2 m 40 s");
    assert_eq!(secs(30.0), "30 s");
    assert_eq!(secs(0.5), "0.5 s");
    assert_eq!(secs(-1.0), "0 s");
    assert_eq!(count(1, "channel"), "1 channel");
    assert_eq!(count(3, "channel"), "3 channels");
}
```

And in `command.rs`:

```rust
#[test]
fn sequence_numbers_count_from_one_and_drain_reports_disconnection() {
    let (tx, rx) = command_channel();
    assert_eq!(tx.send(Command::Enumerate), 1);
    assert_eq!(tx.send(Command::Enumerate), 2);
    assert_eq!(rx.drain().unwrap().len(), 2);
    assert_eq!(rx.drain().unwrap().len(), 0);
    drop(tx);
    assert!(rx.drain().is_none());
}
```

- [ ] **Step 2: Create the crate and run the tests to see them fail to compile**

Run: `cargo test -p api`
Expected: compile errors for the missing functions.

- [ ] **Step 3: Implement `api`, move the types, re-export them from `engine`**

Move the definitions (not copies) with their doc comments and derives; `Segment::hole`/`data_from` move with `Segment`. `Handle` and `CommandSender` are `Clone`.

- [ ] **Step 4: Run the whole workspace**

Run: `cargo test --workspace && cargo fmt --all --check && cargo clippy -p api -p engine --all-targets -- -D warnings`
Expected: all PASS with no engine test changed (the re-exports keep every path).

- [ ] **Step 5: Commit**

```bash
git add -A crates/api crates/engine
git commit -m "api: the engine/UI contract types; Row, Segment and ids move out of engine"
```

---

### Task 3: `engine::snapshot::status`: the status sentence

**Files:**
- Create: `crates/engine/src/snapshot/mod.rs` (for now: `pub mod status;` only), `crates/engine/src/snapshot/status.rs`
- Modify: `crates/engine/src/lib.rs` (`pub mod snapshot;`)

**Interfaces:**
- Produces:

```rust
pub struct StatusInput {
    pub mode: Mode, pub center_hz: f64, pub sample_rate: f64,
    pub held_secs: f64, pub guaranteed_secs: f64,
    pub channels: usize, pub worst_lag_secs: f64, pub shed: usize,
    pub device_lost: bool,
    /// Seconds since the head last advanced.
    pub silent_secs: f64,
    pub stage_behind_secs: f64,
}
pub fn status_line(i: &StatusInput) -> api::Status
```

Rules, first match wins; `live` is `format!("Live at {}, {}: holding {} (guaranteed {}), {}", fmt::mhz(center), fmt::rate(rate), fmt::secs(held), fmt::secs(guaranteed), fmt::count(channels, "channel"))` followed by `", worst lag {}"` (`fmt::secs`) when `worst_lag_secs >= 0.1`:

| Condition | health | text | reason |
|---|---|---|---|
| `device_lost` | Lost | `Device lost` | `no samples for {secs(silent)}` |
| `silent_secs >= 1.0` | Warning | `Waiting for samples at {mhz}` | `none for {secs(silent)}` |
| `mode == Sweep` | Warning | `Sweep mode` | `not re-homed on the new engine yet (step 4a)` |
| `shed > 0` | Warning | live | `{count(shed, "channel")} shed: the pool is saturated` |
| `stage_behind_secs >= 0.5` | Warning | live | `the FFT is {secs(behind)} behind the device` |
| otherwise | Ok | live | none |

- [ ] **Step 1: Write the failing tests in `status.rs`**

```rust
fn base() -> StatusInput { StatusInput { mode: Mode::Live, center_hz: 855.25e6, sample_rate: 10e6, held_secs: 160.0, guaranteed_secs: 30.0, channels: 3, worst_lag_secs: 0.0, shed: 0, device_lost: false, silent_secs: 0.0, stage_behind_secs: 0.0 } }

#[test]
fn a_healthy_receiver_says_what_it_holds() {
    let s = status_line(&base());
    assert_eq!(s.health, Health::Ok);
    assert_eq!(s.text, "Live at 855.250 MHz, 10 MS/s: holding 2 m 40 s (guaranteed 30 s), 3 channels");
    assert_eq!(s.reason, None);
}
#[test]
fn lag_shed_and_behind_add_their_reasons() {
    let s = status_line(&StatusInput { worst_lag_secs: 0.3, ..base() });
    assert!(s.text.ends_with(", worst lag 0.3 s"));
    let s = status_line(&StatusInput { shed: 2, ..base() });
    assert_eq!((s.health, s.reason.as_deref()), (Health::Warning, Some("2 channels shed: the pool is saturated")));
    let s = status_line(&StatusInput { stage_behind_secs: 0.7, ..base() });
    assert_eq!(s.reason.as_deref(), Some("the FFT is 0.7 s behind the device"));
}
#[test]
fn silence_and_loss_come_first() {
    let s = status_line(&StatusInput { silent_secs: 1.3, ..base() });
    assert_eq!((s.health, s.text.as_str(), s.reason.as_deref()), (Health::Warning, "Waiting for samples at 855.250 MHz", Some("none for 1.3 s")));
    let s = status_line(&StatusInput { device_lost: true, silent_secs: 2.0, shed: 1, ..base() });
    assert_eq!((s.health, s.text.as_str(), s.reason.as_deref()), (Health::Lost, "Device lost", Some("no samples for 2.0 s")));
}
#[test]
fn sweep_mode_says_it_is_pending() {
    let s = status_line(&StatusInput { mode: Mode::Sweep, ..base() });
    assert_eq!((s.text.as_str(), s.reason.as_deref()), ("Sweep mode", Some("not re-homed on the new engine yet (step 4a)")));
}
```

(`Health` needs `PartialEq`; add it in `api`.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine snapshot::status`
Expected: FAIL to compile (no `status_line`).

- [ ] **Step 3: Implement `status_line`**

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p engine snapshot::status`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A crates/engine/src/snapshot crates/engine/src/lib.rs crates/api
git commit -m "Engine: the per-receiver status sentence"
```

---

### Task 4: `engine::snapshot`: the publisher thread, devices and commands

**Files:**
- Modify: `crates/engine/src/snapshot/mod.rs`, `crates/engine/Cargo.toml` (`arc-swap` is already a dependency)
- Create: `crates/engine/src/snapshot/devices.rs`, `crates/engine/src/snapshot/build.rs`
- Modify: `crates/engine/tests/rules.rs` (allow `snapshot/mod.rs` to create a thread), `crates/engine/src/stage.rs` (nothing unless `RowHistory::newest` needs a hook)
- Test: `crates/engine/tests/snapshot_sim.rs`

**Interfaces:**
- Consumes: `Engine` (`add_source`, `remove_source`, `source`, `sources`, `bus`, `keeper`, `config`), `Source` (`snapshot`, `stats`, `head`, `segment_at`, `time_of`, `set_center`, `set_gain`, `set_mode`, `mode`), `ChannelBus::{stage, stats}`, `Stage::{stats, rows}`, `RowHistory::newest`, `RingBudget::set_config`, `sdr_core::Backend`, Task 3's `status_line`, Task 2's `api` types.
- Produces:

```rust
pub struct PublisherConfig { pub period: Duration, pub rows_per_snapshot: usize, pub acks_kept: usize, pub log_kept: usize }
impl Default for PublisherConfig   // 50 ms, 16, 64, 200
/// Starts thread "snapshot"; the engine lives on it and is dropped when the last `Handle` is.
pub fn spawn(engine: Engine, backends: Vec<Box<dyn Backend>>, cfg: PublisherConfig) -> api::Handle
```

Behaviour the tests pin:

- The loop: `next = now + period`; `drain` (None ⇒ break: drop the registry's sources through the engine and return); apply each command in order, pushing an `Ack` (and a log line `command {seq} failed: {err}` for an `Err`); build the snapshot (`version + 1`); `store`; `thread::park_timeout(until next)`. The first snapshot is stored before `spawn` returns (version 1, devices enumerated).
- `Registry` (`devices.rs`): `backends`, `entries: Vec<DeviceEntry>`, `open: BTreeMap<SourceId, Opened { letter, info, caps: Arc<Capabilities>, gain: Option<GainSetting> }>`. `enumerate()` calls every backend's `enumerate` (an `Err` becomes a log line `{backend}: enumeration failed: {err}`) and keeps an open device in `entries` even when it is no longer enumerated. Letters: the lowest of `A..=Z` not in use.
- Commands → `Result<Outcome, String>`: `Enumerate` re-enumerates; `Open` fails with `no backend named {backend}` or `already open as {letter}`, else `backend.open(serial)` then `engine.add_source(dev, Mode::Live)` (errors as `to_string()`), logs `opened {label} as {letter}`, returns `Opened(id)`; `Close` fails with `no receiver {id}`, else `engine.remove_source`, frees the letter, logs `{letter} closed`; `Tune` → `source.set_center`, logs `{letter} tuned to {mhz}`; `SetGain` → `source.set_gain`, remembered, logs `{letter} gain set`; `SetMode` → `source.set_mode`, logs `{letter} mode {Live|Sweep}`; `SetBudget` → `keeper.budget().set_config(BudgetConfig { guaranteed_secs, ceiling_bytes, sweep_secs: engine.config().budget.sweep_secs })`, logs `budget: guaranteed {secs}, ceiling {n} MB`.
- `build.rs`: one `bus.stats()` per tick; per open receiver in letter order: `DeviceSnapshot` for tuning, `stats()` for the ring view (`held_secs`, `guaranteed_secs`, `ceiling_secs`, `device_gaps`, `starved_samples`, `overloads`, `device_lost`), `stage.stats()` for the stage view, `stage.rows().newest(rows_per_snapshot)`, consumers filtered by `channel.source` (`lag_secs` as reported; `worst_lag_secs` over `Running` ones; `shed` = those in state `Shed`), `head`, `head_time = source.time_of(head)`, `segment = segment_at(head).id`, and the status from `status_line` with `silent_secs` from a per-receiver `(last_head, Instant)` the publisher keeps (`Instant`, never `SystemTime`). `DeviceEntry::open_as` from the registry. A device lost (`stats.device_lost`) logs `{letter}: device lost` once.
- `rules.rs`: the thread allowlist gains `"snapshot/mod.rs"`.

- [ ] **Step 1: Write the failing integration tests in `tests/snapshot_sim.rs`**

Helpers: `fn spawn_sim() -> Handle` (an `Engine` with `BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }`, `BusConfig::default()`, `SourceConfig::default()`, backends `vec![Box::new(backend_sim::SimBackend)]`, `PublisherConfig::default()`); `fn wait_for(h: &Handle, timeout: Duration, f: impl Fn(&Snapshot) -> bool) -> Arc<Snapshot>` polling `latest()` every 2 ms and panicking with the last snapshot's `{:?}` on timeout; `fn ack(h: &Handle, seq: u64) -> Ack` (waits up to 2 s for `acks` to contain `seq`).

```rust
#[test]
fn snapshots_arrive_at_twenty_hertz_with_contiguous_rows() {
    let h = spawn_sim();
    let s = wait_for(&h, Duration::from_millis(500), |s| s.devices.iter().any(|d| d.info.serial == "SIM-0001"));
    assert!(s.version >= 1);
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(id)) = ack(&h, seq).result else { panic!() };
    wait_for(&h, Duration::from_secs(1), |s| s.receivers.iter().any(|r| r.id == id && !r.rows.is_empty()));
    let t0 = Instant::now();
    let (mut versions, mut rows): (Vec<u64>, BTreeMap<u64, Arc<Row>>) = (vec![], BTreeMap::new());
    while t0.elapsed() < Duration::from_secs(2) {
        let s = h.latest();
        if versions.last() != Some(&s.version) {
            versions.push(s.version);
            let r = s.receivers.iter().find(|r| r.id == id).unwrap();
            for row in &r.rows { rows.entry(row.first_sample).or_insert_with(|| row.clone()); }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(versions.windows(2).all(|w| w[1] > w[0]));
    assert!((34..=46).contains(&versions.len()), "{} snapshots in 2 s", versions.len());
    assert!(rows.len() >= 30, "{} rows", rows.len());
    assert!(rows.values().zip(rows.values().skip(1)).all(|(a, b)| a.end_sample == b.first_sample), "rows are not contiguous");
    let r = h.latest().receivers.iter().find(|r| r.id == id).cloned().unwrap();
    assert_eq!(r.letter, 'A');
    assert_eq!(r.status.health, Health::Ok);
    assert!(r.status.text.starts_with("Live at 855.250 MHz, 10 MS/s: holding "), "{}", r.status.text);
    assert!(r.rows.len() <= 16);
}

#[test]
fn a_tune_is_acknowledged_and_shows_in_the_snapshot_and_the_log() {
    let h = spawn_sim();
    let seq = h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(id)) = ack(&h, seq).result else { panic!() };
    let seq = h.send(Command::Tune { source: id, center_hz: 851.25e6 });
    assert!(matches!(ack(&h, seq).result, Ok(Outcome::Done)));
    let s = wait_for(&h, Duration::from_millis(800), |s| {
        s.receivers.iter().any(|r| r.id == id && r.center_hz == 851.25e6 && r.rows.last().is_some_and(|row| row.segment.center_hz == 851.25e6))
    });
    let r = s.receivers.iter().find(|r| r.id == id).unwrap();
    assert!(r.status.text.contains("851.250 MHz"));
    assert!(s.log.iter().any(|l| l.receiver == Some('A') && l.text == "A tuned to 851.250 MHz"), "{:?}", s.log);
    // Out of the device's range: acknowledged with the error, tuning unchanged.
    let seq = h.send(Command::Tune { source: id, center_hz: 10e9 });
    let a = ack(&h, seq);
    assert!(a.result.is_err(), "{a:?}");
    assert_eq!(h.latest().receivers[0].center_hz, 851.25e6);
    assert!(h.latest().log.iter().any(|l| l.text.starts_with(&format!("command {seq} failed: "))));
}

#[test]
fn a_lost_device_is_said_so_with_the_reason() {
    let engine = Engine::new(/* as spawn_sim */);
    let (d, control) = backend_sim::SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() });
    let src = engine.add_source(Box::new(d), Mode::Live).unwrap();
    let h = engine::snapshot::spawn(engine, vec![], PublisherConfig::default());
    wait_for(&h, Duration::from_secs(1), |s| s.receivers.iter().any(|r| r.id == src.id() && !r.rows.is_empty()));
    control.drop_device();
    let s = wait_for(&h, Duration::from_millis(3500), |s| s.receivers[0].status.health == Health::Lost);
    assert_eq!(s.receivers[0].status.text, "Device lost");
    assert!(s.receivers[0].status.reason.as_deref().unwrap().starts_with("no samples for "));
    assert!(s.log.iter().any(|l| l.text.ends_with("device lost")));
}
```

(A source added before `spawn` has no letter from an `Open`: the registry adopts every source already in `engine.sources()` at start, lowest letters first, with `caps` from `source.snapshot()`.)

```rust
#[test]
fn closing_frees_the_letter_and_opening_twice_is_refused() {
    let h = spawn_sim();
    let open = || h.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() });
    let Ok(Outcome::Opened(a)) = ack(&h, open()).result else { panic!() };
    assert_eq!(ack(&h, open()).result, Err("already open as A".to_string()));
    let seq = h.send(Command::Close(a));
    assert!(matches!(ack(&h, seq).result, Ok(Outcome::Done)));
    let s = wait_for(&h, Duration::from_millis(500), |s| s.receivers.is_empty());
    assert_eq!(s.devices.iter().find(|d| d.info.serial == "SIM-0001").unwrap().open_as, None);
    let Ok(Outcome::Opened(b)) = ack(&h, open()).result else { panic!() };
    assert_ne!(a, b);
    let s = wait_for(&h, Duration::from_millis(500), |s| s.receivers.len() == 1);
    assert_eq!(s.receivers[0].letter, 'A');
    assert_eq!(ack(&h, h.send(Command::Open { backend: "none".into(), serial: "x".into() })).result, Err("no backend named none".to_string()));
}

#[test]
fn dropping_the_handle_stops_the_engine() {
    let engine = Engine::new(/* as spawn_sim */);
    let (d, control) = backend_sim::SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() });
    engine.add_source(Box::new(d), Mode::Live).unwrap();
    let h = engine::snapshot::spawn(engine, vec![], PublisherConfig::default());
    wait_for(&h, Duration::from_secs(1), |s| !s.receivers.is_empty());
    drop(h);
    std::thread::sleep(Duration::from_millis(300));
    let sent = control.samples_sent();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(control.samples_sent(), sent, "the device is still streaming after the handle was dropped");
}
```

- [ ] **Step 2: Run to verify they fail to compile**

Run: `cargo test -p engine --test snapshot_sim`
Expected: compile errors (no `spawn`, `PublisherConfig`).

- [ ] **Step 3: Implement `spawn`, `Registry` and `build`**

One thread, named `snapshot`, created with `thread::Builder::new().name(...)`. Acks live in a `VecDeque<Ack>` capped at `acks_kept`, the log in a `VecDeque<Arc<LogEntry>>` capped at `log_kept` with `n` counting from 1; both are copied into every snapshot oldest first.

- [ ] **Step 4: Run the new tests, then the rules test and the suite**

Run: `cargo test -p engine --test snapshot_sim && cargo test -p engine --test rules && cargo test -p engine`
Expected: all PASS (the rules test only after `snapshot/mod.rs` is in its allowlist; `SystemTime::now(` must still be absent).

- [ ] **Step 5: Commit**

```bash
git add -A crates/engine
git commit -m "Engine: the snapshot publisher, device registry and command handling"
```

---

### Task 5: The `app` crate: eframe shell, dock, devices and log panes, receiver header and status

**Files:**
- Create: `crates/app/Cargo.toml`, `crates/app/src/{lib,main,app,tabs,theme,devices_pane,log_pane,receiver_pane}.rs`
- Test: `crates/app/tests/rules.rs`

`crates/app/Cargo.toml`:

```toml
[package]
name = "spectrawl-app"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "spectrawl_app"
path = "src/lib.rs"

[[bin]]
name = "spectrawl-next"
path = "src/main.rs"

[dependencies]
api = { path = "../api" }
backend-airspy = { path = "../backend-airspy" }
backend-sim = { path = "../backend-sim" }
eframe = { version = "0.36.2", default-features = false, features = ["wgpu", "default_fonts", "wayland", "x11", "persistence"] }
egui = "0.36.2"
egui_dock = { version = "0.21", features = ["serde"] }
engine = { path = "../engine" }
env_logger = "0.11"
log = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

**Interfaces:**
- Consumes: `api::{Handle, Snapshot, Receiver, Command, GainSetting, Health, fmt}`; `engine::snapshot::spawn`, `engine::{Engine, EngineConfig, BudgetConfig, BusConfig, SourceConfig, RingBudget}` in `lib.rs` only.
- Produces:

```rust
// lib.rs
pub mod app; pub mod devices_pane; pub mod log_pane; pub mod receiver_pane; pub mod tabs; pub mod theme; pub mod trace; pub mod waterfall;
pub fn run() -> eframe::Result   // Engine: guaranteed 30 s, ceiling RingBudget::physical_ram_bytes() / 2, sweep 1 s, BusConfig/SourceConfig default; backends [AirspyBackend::new(), SimBackend]; eframe NativeOptions { Wgpu, 1600×960, title "SpecTrawl" }
// tabs.rs
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)] pub enum Tab { Receiver(SourceId), Devices, Log }   // SourceId gains serde derives in api (serde optional feature "serde", on in the app)
// app.rs
pub struct App { /* handle, snap: Arc<Snapshot>, seen_version, dock: DockState<Tab>, panes: BTreeMap<SourceId, ReceiverPane>, budget_draft, db_ranges: HashMap<String, (f32, f32)> */ }
impl App {
    pub fn new(handle: Handle) -> App;                               // dock: Devices | receivers (empty) over Log
    pub fn restore(&mut self, storage: &dyn eframe::Storage);         // db_ranges by serial, key "spectrawl-next"
    pub fn frame(&mut self, ui: &mut egui::Ui);                       // the whole frame body
    pub fn handle(&self) -> &Handle;
    pub fn pane(&self, id: SourceId) -> Option<&ReceiverPane>;
}
impl eframe::App for App { fn ui(..) { self.frame(ui) } fn save(..) }
// receiver_pane.rs
pub struct ReceiverPane { /* history, trace, view, paused, tex, tex_w, write_row, drag, last_status: Status, last_tuning: Option<(f64, f64)> */ }
impl ReceiverPane {
    pub fn new() -> ReceiverPane;
    pub fn ingest(&mut self, r: &Receiver);                           // Task 6 fills it; this task stores the status and tuning
    pub fn show(&mut self, ui: &mut egui::Ui, r: &Receiver, out: &mut Vec<Command>);
    pub fn last_status(&self) -> &Status;
    pub fn history_len(&self) -> usize;
}
// devices_pane.rs
pub struct BudgetDraft { pub guaranteed_secs: f64, pub ceiling_mb: u64 }
pub fn show(ui: &mut egui::Ui, snap: &Snapshot, draft: &mut BudgetDraft, out: &mut Vec<Command>);
// log_pane.rs
pub fn show(ui: &mut egui::Ui, snap: &Snapshot);
```

Behaviour:

- `frame`: `snap = handle.latest()`; if `snap.version != seen_version`: for each receiver, create its pane and a `Tab::Receiver` (added to the dock node holding receivers, or as a new right split when there is none) if new, then `pane.ingest(r)`; remove panes and tabs whose receiver is gone; `seen_version = snap.version`. Draw the dock (`DockArea::show_inside(ui, self)`; receiver tabs not closable by `×`, Devices and Log closable and reopened from a `View ▾` menu in a top bar). Send every command collected into `out` after drawing. End with `ui.ctx().request_repaint_after(Duration::from_millis(16))` — no sleep.
- Receiver header, left to right (ui-design §3.1): letter chip (`theme::chip`), device label, mode; centre as an `egui::DragValue` in MHz with 3 decimals, speed 0.001, bound to a local copy that is reset to the snapshot's `center_hz` whenever the value is not being edited, and sends `Tune` (rounded to `caps.tuning_step_hz`) when editing ends; `Gain ▾` popover with Linearity and Sensitivity sliders (0–1), an Auto button and a Manual section (three `u8` drag values 0–15 for the stages named in `caps.gain_stages`), each change sending `SetGain`; `dB range ▾` popover (floor, top, Auto; Auto calls `trace::auto_range` in Task 6); right end `⋯` menu with `Close device` → `Command::Close(id)`.
- Status line under the header: `r.status.text`, then `· {reason}` muted when present; coloured `theme::TEXT2` for Ok, `theme::SELECT` for Warning, `theme::ALERT` for Lost. A second muted line: `holding {secs(held)}, guaranteed {secs(guaranteed)}` and, when `ring.overloads > 0`, `· clipping ({n})`.
- Body (this task): a placeholder frame with the text "waterfall (Task 6)"; Task 6 replaces it.
- Devices pane: `Refresh` → `Enumerate`; a row per `DeviceEntry`: label, backend, serial, then `Open` (sends `Open`) or `open as {letter}` with `Close`; below, "Ring budget": guaranteed seconds and ceiling MB drag values and `Apply` → `SetBudget { guaranteed_secs, ceiling_bytes: ceiling_mb << 20 }`; the bus line `workers {n} · shed {n} · panicked {n}` and `saturated` when so.
- Log pane: `snap.log` oldest first, `[{letter}] text` or `text`, auto-scrolled to the end (`egui::ScrollArea::vertical().stick_to_bottom(true)`).
- Persistence: `save` writes `db_ranges` (serial → range) as JSON under key `spectrawl-next`; `restore` reads it; a new pane for a serial with a saved range starts with it. The dock layout is not persisted in this step (receiver ids are per process; workspaces are step 8).

- [ ] **Step 1: Write the failing rules test in `crates/app/tests/rules.rs`**

Reuse the `sources()`/`test_module_start` helpers from `crates/engine/tests/rules.rs` (copy them; the two crates are not allowed to share test code).

```rust
#[test]
fn the_app_creates_no_thread_takes_no_lock_and_reads_no_clock() {
    for (p, s) in sources() {
        for banned in ["thread::spawn(", "thread::Builder::new(", "Mutex", "RwLock", "parking_lot", ".lock()", "SystemTime::now(", "Instant::now(", "thread::sleep("] {
            assert!(!s.contains(banned), "{}: contains `{banned}`", p.display());
        }
    }
}

#[test]
fn the_app_reaches_the_engine_only_to_spawn_it() {
    for (p, s) in sources() {
        let is_lib = p.file_name().is_some_and(|f| f == "lib.rs");
        assert!(is_lib || !s.contains("engine::"), "{}: uses engine types; the app reads api only", p.display());
    }
}
```

- [ ] **Step 2: Create the crate; run the rules test to see it fail**

Run: `cargo test -p spectrawl-app --test rules`
Expected: FAIL (`no sources found`, or compile errors) until the sources exist.

- [ ] **Step 3: Implement the shell**

`theme.rs` copied from `app-old` minus `class_color` (and its `detect` import). `main.rs` is `env_logger::init(); spectrawl_app::run()`.

- [ ] **Step 4: Run the rules test, build the binary, run it once by hand against the simulator**

Run: `cargo test -p spectrawl-app --test rules && cargo build -p spectrawl-app && cargo clippy -p spectrawl-app --all-targets -- -D warnings`
Expected: PASS, builds, no warnings. Run `cargo run -p spectrawl-app` for a few seconds: the Devices pane lists "Simulated SDR"; `Open` makes an `A` receiver tab whose status reads `Live at 855.250 MHz, 10 MS/s: holding …`; `Close device` removes it. Record what you saw in the report (the headless test in Task 7 pins it).

- [ ] **Step 5: Commit**

```bash
git add -A crates/app crates/api
git commit -m "app: eframe shell, dock, devices and log panes, receiver header and status line"
```

---

### Task 6: Waterfall and spectrum trace

**Files:**
- Create: `crates/app/src/waterfall.rs`, `crates/app/src/trace.rs`
- Modify: `crates/app/src/receiver_pane.rs` (`ingest`, body)

**Interfaces:**
- Consumes: `api::Row` (`power`: 65536 DC-centred linear bins, 1.0 = 0 dBFS; `first_sample`, `end_sample`, `segment.center_hz`, `segment.sample_rate`), `theme::waterfall_lut`, `theme::PLOT_BG`.
- Produces (`waterfall.rs`, all pure, no egui except `Color32`):

```rust
pub const ROWS: usize = 1024;        // history depth and texture height
pub const HIST_W: usize = 4096;      // history columns
pub const MIN_ZOOM_COLS: f64 = 24.0;
pub const GAP: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);
pub struct HistRow { pub db: Vec<f32> /* HIST_W, NaN = no data */, pub first_sample: u64, pub end_sample: u64 }
pub struct History { pub center_hz: f64, pub sample_rate: f64, rows: VecDeque<HistRow> /* newest first */ }
impl History {
    pub fn new() -> History;
    /// Pushes `row` if newer than the newest; a change of centre or rate clears everything first and returns `Cleared`;
    /// a hole before `row` (first_sample > newest.end_sample) pushes one all-NaN row first and returns `Hole`.
    pub fn push(&mut self, row: &Row) -> Pushed;      // enum Pushed { Skipped, Appended, Hole, Cleared }
    pub fn len(&self) -> usize; pub fn newest(&self) -> Option<&HistRow>; pub fn iter(&self) -> impl Iterator<Item = &HistRow>;
    pub fn span(&self) -> Option<(f64, f64)>;          // centre ± rate/2
    pub fn secs_per_row(&self) -> f64;                 // (end − first) / rate of the newest row, 0.05 when empty
}
pub fn reduce_db(power: &[f32], out: &mut [f32]);     // max per column in the linear domain, then 10·log10(max(p, 1e-30))
pub struct View { pub db_min: f32, pub db_max: f32, pub zoom: Option<(f64, f64)> }
impl View {
    pub fn window(&self, span: (f64, f64)) -> (f64, f64);                         // zoom clamped inside span, else span
    pub fn cols(&self, span: (f64, f64), n: usize) -> (usize, usize);              // history columns [c0, c1) of the window: c0 = floor, c1 = ceil, both clamped to 0..=n, c1 − c0 ≥ 1
    pub fn zoom_about(&mut self, span: (f64, f64), hz: f64, factor: f64);          // factor > 1 narrows; never below MIN_ZOOM_COLS/HIST_W of the span; never outside span
    pub fn pan(&mut self, span: (f64, f64), dhz: f64);
    pub fn hz_to_x(&self, span: (f64, f64), rect: Rect, hz: f64) -> f32;
    pub fn x_to_hz(&self, span: (f64, f64), rect: Rect, x: f32) -> f64;
}
pub struct Lut(pub [Color32; 256]);
pub fn colorize(db: &[f32], view: &View, lut: &Lut, out: &mut Vec<Color32>);      // NaN → GAP; clamped to the range
pub fn next_write_row(write_row: usize) -> usize;                                  // (write_row + ROWS − 1) % ROWS
/// The two textured quads that draw a ring texture whose newest row is `write_row`, newest at the top:
/// (uv rect, fraction of the screen rect top..bottom) each; the second is absent when write_row == 0.
pub fn quads(write_row: usize) -> [Option<(Rect, (f32, f32))>; 2];
```

(`trace.rs`):

```rust
pub struct Trace { pub smooth: Vec<f32>, pub hold: Vec<f32>, pub alpha: f32 /* 0.6 */, pub decay_db: f32 /* 0.05 per row */ }
impl Trace { pub fn new() -> Trace; pub fn update(&mut self, db: &[f32]); pub fn reset(&mut self); }
pub fn auto_range(db: &[f32]) -> (f32, f32);   // (10th percentile − 5, 99.5th percentile + 10), ignoring NaN; (−110, −45) when empty
pub fn nice_step(span: f64, n: f64) -> f64;    // 1/2/5 × 10^k so that span/step ≈ n
pub fn decimals(step: f64) -> usize;
```

Pane behaviour:

- `ingest`: for each `r.rows` row in order, `history.push(row)`; on `Appended`/`Hole` reduce the row into the texture (`push_tex_row`), on `Cleared` reset the trace and rebuild the texture; the trace updates from the newest row's reduced dB (`reduce_db` into `HIST_W`, then `trace.update`). Paused panes still ingest into history; they stop advancing `write_row` and uploading (the texture rebuilds from history when unpaused).
- Body: spectrum plot of fixed height 160 px (trace in `theme::ACCENT`, hold in `theme::MUTED`, dB grid lines every `nice_step(range, 6)`, frequency ticks every `nice_step(window, 8)` labelled in MHz with `decimals`), then the waterfall filling the rest. The waterfall is one `TextureHandle` named `waterfall-{letter}` of `[tex_w, ROWS]` rebuilt from history when `tex_w` (the rect width in pixels, at most `HIST_W`), the range or the zoom changes; otherwise one `set_partial([0, write_row], ColorImage [tex_w, 1])` per new row (`TextureOptions::LINEAR`). Drawn with `quads` as two `Mesh` quads. Time marks at the left edge every 10 s (`secs_per_row`), labelled `−10 s`, `−20 s`, ….
- Interaction: wheel over the body → `zoom_about(cursor Hz, 1.25^notches)`; primary drag on the waterfall → `pan`; primary drag on the spectrum → tune: the trace shifts with the cursor while dragging and on release the pane sends `Tune { center_hz: round_to(center − Δhz, caps.tuning_step_hz) }`; double-click → `zoom = None`; `Pause` button (and Space while the pane is hovered) toggles `paused`.

- [ ] **Step 1: Write the failing unit tests in `waterfall.rs` and `trace.rs`**

```rust
fn row(first: u64, end: u64, center: f64, hot_bin: usize) -> Row { /* power = vec![1e-10; 65536] with power[hot_bin] = 1.0, segment at center, rate 10e6 */ }

#[test]
fn a_single_hot_bin_survives_reduction() {
    let mut out = vec![0.0; HIST_W];
    let mut p = vec![1e-10f32; 65536]; p[12_345] = 1.0;
    reduce_db(&p, &mut out);
    let hot = 12_345 * HIST_W / 65536;
    assert!((out[hot] - 0.0).abs() < 1e-3, "{}", out[hot]);
    assert!(out.iter().enumerate().filter(|(i, _)| *i != hot).all(|(_, &v)| (v + 100.0).abs() < 1e-3));
}
#[test]
fn history_appends_contiguous_rows_marks_holes_and_clears_on_retune() {
    let mut h = History::new();
    assert_eq!(h.push(&row(0, 32_768, 855.25e6, 0)), Pushed::Appended);
    assert_eq!(h.push(&row(32_768, 65_536, 855.25e6, 0)), Pushed::Appended);
    assert_eq!(h.push(&row(32_768, 65_536, 855.25e6, 0)), Pushed::Skipped);   // seen already
    assert_eq!(h.push(&row(98_304, 131_072, 855.25e6, 0)), Pushed::Hole);
    assert_eq!(h.len(), 4);
    assert!(h.iter().nth(1).unwrap().db.iter().all(|v| v.is_nan()));
    assert_eq!(h.push(&row(131_072, 163_840, 851.25e6, 0)), Pushed::Cleared);
    assert_eq!((h.len(), h.center_hz), (1, 851.25e6));
    for k in 0..(ROWS as u64 + 10) { h.push(&row(200_000 + k * 32_768, 200_000 + (k + 1) * 32_768, 851.25e6, 0)); }
    assert_eq!(h.len(), ROWS);
}
#[test]
fn feeding_every_third_snapshot_loses_no_rows() {
    // 16 rows per snapshot, 1 new row per snapshot: skipping two of every three snapshots still gives a contiguous history.
    let rows: Vec<Row> = (0..60u64).map(|k| row(k * 32_768, (k + 1) * 32_768, 855.25e6, 0)).collect();
    let mut h = History::new();
    for v in (0..60).step_by(3) {
        let newest = &rows[v.saturating_sub(15)..=v];
        for r in newest { h.push(r); }
    }
    assert_eq!(h.len(), 58);   // rows 0..=57 (the last snapshot taken is v = 57)
    assert!(h.iter().all(|r| !r.db[0].is_nan()));
}
#[test]
fn zoom_stays_inside_the_span_and_above_the_minimum() {
    let span = (850.25e6, 860.25e6);
    let mut v = View { db_min: -110.0, db_max: -45.0, zoom: None };
    v.zoom_about(span, 855.0e6, 2.0);
    assert_eq!(v.window(span), (852.5e6, 857.5e6));
    for _ in 0..40 { v.zoom_about(span, 855.0e6, 2.0); }
    let (lo, hi) = v.window(span);
    assert!((hi - lo - 10e6 * MIN_ZOOM_COLS / HIST_W as f64).abs() < 1.0);
    v.pan(span, -100e6);
    assert_eq!(v.window(span).0, span.0);
    v.zoom_about(span, 855.0e6, 0.01);
    assert_eq!(v.zoom, None);
}
#[test]
fn hz_and_x_round_trip_and_columns_cover_the_window() {
    let span = (850.25e6, 860.25e6);
    let v = View { db_min: -110.0, db_max: -45.0, zoom: Some((852.25e6, 853.25e6)) };
    let rect = Rect::from_min_size(pos2(10.0, 0.0), vec2(1000.0, 100.0));
    assert!((v.x_to_hz(span, rect, v.hz_to_x(span, rect, 852.75e6)) - 852.75e6).abs() < 1.0);
    assert_eq!(v.cols(span, HIST_W), (819, 1229));
    let full = View { zoom: None, ..v };
    assert_eq!(full.cols(span, HIST_W), (0, HIST_W));
}
#[test]
fn colorize_clamps_and_marks_gaps() {
    let lut = Lut(theme::waterfall_lut());
    let v = View { db_min: -100.0, db_max: -50.0, zoom: None };
    let mut out = Vec::new();
    colorize(&[-200.0, -100.0, -75.0, -50.0, 0.0, f32::NAN], &v, &lut, &mut out);
    assert_eq!((out[0], out[1], out[3], out[4], out[5]), (lut.0[0], lut.0[0], lut.0[255], lut.0[255], GAP));
    assert_eq!(out[2], lut.0[127]);
}
#[test]
fn the_ring_quads_cover_the_texture_once() {
    assert_eq!(next_write_row(0), ROWS - 1);
    assert_eq!(next_write_row(5), 4);
    let [a, b] = quads(0);
    assert!(b.is_none());
    assert_eq!(a.unwrap().1, (0.0, 1.0));
    let [a, b] = quads(256);
    let (ua, fa) = a.unwrap(); let (ub, fb) = b.unwrap();
    assert_eq!((fa, fb), ((0.0, 0.75), (0.75, 1.0)));                       // rows 256..1024 on top, then rows 0..256
    assert_eq!((ua.min.y, ua.max.y, ub.min.y, ub.max.y), (0.25, 1.0, 0.0, 0.25));
}
// trace.rs
#[test]
fn trace_smooths_and_holds() {
    let mut t = Trace::new();
    t.update(&[-100.0, -50.0]);
    assert_eq!(t.hold, vec![-100.0, -50.0]);
    t.update(&[-100.0, -100.0]);
    assert!((t.smooth[1] - (0.6 * -50.0 + 0.4 * -100.0)).abs() < 1e-4);
    assert!((t.hold[1] - -50.05).abs() < 1e-4);
}
#[test]
fn auto_range_and_nice_steps() {
    let db: Vec<f32> = (0..1000).map(|i| -100.0 + i as f32 * 0.05).collect();   // −100 .. −50.05
    let (lo, hi) = auto_range(&db);
    assert!((lo - (-95.0 - 5.0)).abs() < 0.2 && (hi - (-50.3 + 10.0)).abs() < 0.3, "{lo} {hi}");
    assert_eq!(auto_range(&[]), (-110.0, -45.0));
    assert_eq!(nice_step(10e6, 8.0), 1e6);
    assert_eq!(nice_step(1e6, 8.0), 100e3);
    assert_eq!(decimals(100e3), 1);   // 852.1
    assert_eq!(decimals(1e6), 0);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p spectrawl-app --lib`
Expected: compile errors.

- [ ] **Step 3: Implement `waterfall.rs` and `trace.rs`, then the pane body**

`History::push` computes `db` with `reduce_db` into `HIST_W` columns. `quads`: with the newest row at texture row `write_row`, screen top..bottom shows texture rows `write_row..ROWS` then `0..write_row`; uv `y` is `row / ROWS`.

- [ ] **Step 4: Run the unit tests, clippy, and look at it**

Run: `cargo test -p spectrawl-app && cargo clippy -p spectrawl-app --all-targets -- -D warnings`
Expected: PASS. Run `cargo run -p spectrawl-app`, open the simulator: the default scenario's emitters show as lines on the waterfall at their frequencies, the trace follows, wheel zooms about the cursor, a drag on the spectrum retunes (the status line changes), `Pause` freezes the waterfall. Record what you saw.

- [ ] **Step 5: Commit**

```bash
git add -A crates/app
git commit -m "app: waterfall ring texture, spectrum trace, zoom, pan and drag-to-tune"
```

---

### Task 7: Headless acceptance test, CI, BACKLOG and docs

**Files:**
- Create: `crates/app/tests/headless_sim.rs`
- Modify: `.github/workflows/ci.yml` (clippy list), `BACKLOG.md`, `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` §9 ("Chosen in step 3"), `crates/api/src/lib.rs` (crate doc: the contract in prose)

**Interfaces:**
- Consumes: `spectrawl_app::app::App` (`new`, `frame`, `handle`, `pane`), `ReceiverPane::{last_status, history_len}`, `engine::snapshot::spawn`, `backend_sim::SimBackend`.

- [ ] **Step 1: Write the failing headless test**

```rust
/// A backend whose `open` takes 700 ms: the snapshot thread stalls, the UI must not.
struct SlowBackend;
impl Backend for SlowBackend {
    fn name(&self) -> &str { "slow" }
    fn enumerate(&self) -> sdr_core::Result<Vec<DeviceInfo>> { Ok(vec![DeviceInfo { backend: "slow".into(), serial: "SLOW-1".into(), label: "Slow SDR".into() }]) }
    fn open(&self, _serial: &str) -> sdr_core::Result<Box<dyn Device>> { std::thread::sleep(Duration::from_millis(700)); Ok(Box::new(SimDevice::new(vec![]))) }
}

#[test]
fn the_app_shows_the_simulator_and_never_blocks_on_the_engine() {
    let engine = Engine::new(EngineConfig { budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }, bus: BusConfig::default(), source: SourceConfig::default() });
    let handle = engine::snapshot::spawn(engine, vec![Box::new(SimBackend), Box::new(SlowBackend)], PublisherConfig::default());
    let ctx = egui::Context::default();
    let mut app = App::new(handle.clone());
    let mut frame_ms: Vec<f64> = Vec::new();
    let mut opened: Option<SourceId> = None;
    let input = |k: usize| egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 900.0))), time: Some(k as f64 / 60.0), ..Default::default() };
    for k in 0..300 {
        if k == 10 { handle.send(Command::Open { backend: "sim".into(), serial: "SIM-0001".into() }); }
        if k == 60 { handle.send(Command::Open { backend: "slow".into(), serial: "SLOW-1".into() }); }
        if k == 240 { handle.send(Command::Close(opened.unwrap())); }
        let t = Instant::now();
        let _ = ctx.run(input(k), |ctx| { egui::CentralPanel::default().show(ctx, |ui| app.frame(ui)); });
        frame_ms.push(t.elapsed().as_secs_f64() * 1e3);
        if opened.is_none() { opened = handle.latest().receivers.iter().find(|r| r.letter == 'A').map(|r| r.id); }
        if k == 230 {
            let id = opened.expect("the simulator opened as A");
            let pane = app.pane(id).expect("a pane for A");
            assert!(pane.history_len() >= 40, "{} rows after ~3.7 s", pane.history_len());
            assert!(pane.last_status().text.starts_with("Live at 855.250 MHz, 10 MS/s: holding "), "{}", pane.last_status().text);
            assert!(handle.latest().receivers.iter().any(|r| r.letter == 'B'), "the slow device opened as B");
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(app.pane(opened.unwrap()).is_none(), "the closed receiver's pane is gone");
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let (p50, max) = (frame_ms[frame_ms.len() / 2], *frame_ms.last().unwrap());
    println!("headless frames: p50 {p50:.2} ms, max {max:.2} ms");
    // A 700 ms open on the snapshot thread must not reach the UI; 100 ms leaves room for a 2-vCPU CI runner's scheduling.
    assert!(max < 100.0, "a frame took {max:.1} ms while the engine ran (slow open at frame 60)");
    assert!(p50 < 10.0, "median frame {p50:.1} ms");
}
```

(The test also proves the Devices and Log panes draw headless: `App::new`'s dock contains them.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p spectrawl-app --test headless_sim`
Expected: FAIL (compile errors if any accessor is missing, otherwise the pane/history assertions).

- [ ] **Step 3: Make it pass without changing the pane's production behaviour**

Missing accessors go in; anything else the test reveals is a finding for the report.

- [ ] **Step 4: CI, BACKLOG, spec §9, api crate doc**

- `ci.yml` clippy: `-p sdr-core -p backend-sim -p backend-airspy -p dsp -p api -p engine -p spectrawl-app`.
- `BACKLOG.md` Rebuild section: mark `DESIGN §10 L254: UI never blocks on the engine — DONE (done 2026-10-23, step 3): the app holds one Arc<Snapshot> per frame from an ArcSwap and sends commands over a channel`; `DESIGN §2 L43: Custom wgpu waterfall with GPU colour map — PARTIAL — the ring texture is a GPU texture updated one row per FFT row; the colour map is applied on the CPU when a row is uploaded (step 8 or later for a shader map)`; add lines: "Sample-rate command not in `Command` (`Source` has no `set_rate`; the Airspy's 2.5 MS/s is unreachable from the new app)"; "Waterfall zoom resolution is 4096 columns: zooming past 24 columns is blocked; native-bin zoom like the old survey view is step 8"; "Ring user cap in seconds ("lower wins") not wired: `SetBudget` takes bytes only"; "Opening an Airspy blocks the snapshot thread for the duration of `open` (no snapshot for ~0.5 s); move opens to a helper thread if it shows"; "A retune clears the waterfall history; re-mapping rows that overlap the new span is not done"; "Dock layout, pop-outs, workspaces and the command palette are not in the new app yet (step 8 carries them over)"; the step 1 line "Ring user cap and per-source pin/weight UI not wired" is replaced by the seconds-cap line (bytes and guaranteed seconds are wired).
- Spec §9 gains a paragraph "Chosen in step 3 (2026-10-23):" with the Global Constraints' builder choices (cadence, rows per snapshot, acks and log depth, history and texture sizes, letters, default budget).
- `crates/api/src/lib.rs` crate doc: three paragraphs — what a `Snapshot` is and when it is published; how a `Command` is acknowledged; what the app may and may not do (one `Arc<Snapshot>` per frame, no engine type).

- [ ] **Step 5: Run everything as CI does**

Run: `cargo fmt --all --check && cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p api -p engine -p spectrawl-app --all-targets -- -D warnings && cargo test --workspace`
Expected: clean, all PASS.

- [ ] **Step 6: Commit**

```bash
git add -A crates/app .github BACKLOG.md docs crates/api
git commit -m "app: headless acceptance test; CI covers api and the app; BACKLOG and spec notes for step 3"
```

---

## Self-review notes

- **Spec coverage.** §4.9: `Snapshot` (per-receiver state, lag per consumer, newest rows, status with reason) and `Command` acknowledged in a later snapshot (Tasks 2, 4); records, emitters, telemetry, systems, calls and pages are later steps and the struct grows then. §5 health slot "holding 2 m 40 s, guaranteed 30 s" (Task 5). §6 step 3 "waterfall, rows and status lines from snapshots only" (Tasks 5–7). §4.1 "Shown in seconds" (Task 5; bytes appear only in the budget ceiling field, which the spec allows as a user cap). DESIGN §9 waterfall rules: ring texture, one row per FFT row, max per pixel, FFT rate independent of the frame rate, drag-to-tune, wheel zoom about the cursor (Task 6); peak-hold decay (Task 6); look-back scrub and record boxes are steps 4–5. DESIGN §10 UI never blocks (Task 7).
- **Type consistency.** `Row`, `Segment`, `SourceId`, `ChannelId`, `Priority`, `ConsumerState`, `Mode` have one definition (api) after Task 2; `Status`/`Health` are defined in Task 2 and produced in Task 3; `Handle::{latest, send}` are used by Tasks 5–7 as declared in Task 2; `App::{new, frame, handle, pane}` and `ReceiverPane::{last_status, history_len}` are declared in Task 5 and used in Task 7.
- **Review Focus.** 1 → Task 7 (`SlowBackend`, max frame < 50 ms); 2 → Task 4 (`rows.len() <= 16`, contiguity) and Task 6 (`feeding_every_third_snapshot_loses_no_rows`); 3 → Task 6 (`Cleared`); 4 → Task 4 (lost, close) and Task 7 (pane gone); 5 → Task 4 (already open, out-of-range tune, log line).
- **Proportion.** The plan is about the length of the step 2 plan for a smaller engine change and a new UI crate; the code blocks are tests and signatures. The pane's drawing code is described, not transcribed, because the old `spectrum_view.rs` is the reference for every drawing decision the plan does not make.
- **Things deliberately not in this step.** Sweep mode in the new app (4a), records and boxes (4), replay and scrub (5), decoders and telemetry (6–7), the inspector, catalogue, dock persistence, pop-outs, workspaces and the command palette (8), a sample-rate command, a shader colour map, native-resolution zoom. Each has a BACKLOG line after Task 7.
