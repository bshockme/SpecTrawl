# Rebuild steps 0 and 1: repository hygiene, then the ring, segments and clock

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the repository in a state where CI proves things (step 0), then build the lock-free block ring with its segment table, timeline and memory budget as the first crate of the new engine (step 1), with the simulator and the Airspy backend emitting every stream event the ring depends on.

**Architecture:** The old `engine` and `app` crates are renamed `engine-old` and `app-old` and keep working on air. A fresh `engine` crate holds the ring: a chain of fixed 64 MiB blocks reached through atomics, written by the device callback without a mutex, read by any thread by absolute sample index, and housekept by one process-wide keeper thread that provisions blocks ahead, offers aged blocks to the OS, retires blocks past each source's ceiling and recomputes the per-source budget. A `SegmentTable` records every retune and gap, a `Timeline` turns sample indices into wall time from refreshed anchors, and a wideband SigMF export is the first consumer that reads through segments.

**Tech Stack:** Rust 2024 edition (stable, 1.99 on the owner's machine), `parking_lot`, `arc-swap` (new), `windows-sys` 0.60 (`Win32_System_Memory`, `Win32_System_SystemInformation`) on Windows, `libc` on Unix, `serde_json` for SigMF metadata. Tests run on `backend-sim`. CI on GitHub Actions, Windows and Linux.

**Spec:** `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` (sections 3, 4.1, 4.8 slice note, 6 steps 0 and 1, 7, 8, 9, 10). Supporting reference: `docs/superpowers/specs/2026-10-22-design-audit.md`.

## Global Constraints

- SpecTrawl stays MIT. No OnAir code, header or binding is ever compiled into any crate (spec §3). Every new dependency must be MIT, Apache-2.0, BSD or Zlib licensed.
- The `detect`, `sdr-core`, `backend-airspy`, `backend-sim` and `catalog` crates are kept as libraries; changes to `detect` are bug fixes only (spec §3). This plan does not touch `detect` beyond its tests.
- Steps 0 to 5 are done serially on one branch, named `rebuild` (spec §7). No agent worktrees for this plan.
- No feature creates a thread, a down-converter or a clock (spec §7). The one thread this plan creates is the ring keeper in `engine::ring::keeper`; the device watchdog runs on it, not on its own thread.
- The ring stores i16 interleaved IQ; float sources are converted once in the callback (spec §4.1).
- Ring sizing values (spec §4.1): default block 64 MiB; guaranteed window 30 s per live source; opportunistic ceiling half of physical RAM by default; sweep sources hold a fixed ~1 s; the ring is never pinned (`VirtualLock`/`mlock` are not called); offered memory priority falls with age; user-facing unit is seconds.
- The device callback writes samples without taking a mutex (spec §4.1). It may take the segment-table mutex on a `Retune` or `Gap` event only.
- A read of a range that spans two segments is refused (spec §4.1).
- Real-capture tests fail when the capture they name is absent; `SPECTRAWL_SKIP_CAPTURES=1` skips them locally; CI never sets it (spec §6). Committed captures stay under 50 MB in total; wideband captures are release assets fetched by CI into `testdata/captures/`.
- BACKLOG gains a line for every DESIGN.md requirement not yet built (spec §6 step 0), with a reason the same day anything is cut (spec §7).
- Code style: `rustfmt.toml` (max_width 140, small heuristics Max); `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` on the foundation crates are CI gates from step 0 on.
- Commit messages are one imperative sentence summarising the change, as in the existing history ("Pagers on air: FLEX text, joining and sync fixes"), followed by the attribution trailers the executing session's system reminder requires.

## Review Focus

1. **A driver block that overlaps or precedes the ring head** (libairspy replays a transfer, or a simulator restart) must be ignored where already held and never move `head` backwards. Test: `ring::tests::out_of_order_block_is_ignored` in Task 10.
2. **A gap larger than the provisioned run of blocks** (device stalls for seconds) must advance `head` and report every sample it could not store as starved; it must never write into a block that belongs to another index. Test: `ring::tests::large_gap_starves_until_provisioned` in Task 10.
3. **A reader racing the keeper across a block boundary** must get either the true samples or `Err(Overwritten)`, never a torn copy. Test: `ring::tests::concurrent_reader_sees_ramp_or_overwritten` in Task 10.
4. **A sample-rate change after start** (`Retune` carrying a new `sample_rate`) must change the budget, the held-seconds figure and `time_of()` from that sample on. Tests: `budget::tests::rate_update_changes_allowance` in Task 13 and `timeline::tests::rate_change_applies_from_its_sample` in Task 12.
5. **An export whose range ends after `head` or starts before `oldest`** must fail with a named error before any file is written, not produce a short file that looks complete. Test: `export_errors_before_writing` in Task 15.

---

## File structure

Step 0 (hygiene):

- `LICENSE` (new, MIT), `Cargo.toml` (workspace license), every `crates/*/Cargo.toml` (`license.workspace = true`), `rust-toolchain.toml` (new).
- `crates/engine-old/` and `crates/app-old/` (renamed directories and packages; old app binary stays `spectrawl`).
- `crates/detect/tests/common/mod.rs` (new: the fail-not-skip capture loader), `crates/detect/tests/real_captures.rs`, `crates/detect/tests/real_spectra.rs` (use it).
- `testdata/captures/MANIFEST.toml` (new), `tools/fetch_captures.ps1`, `tools/fetch_captures.sh` (new), `tools/cut_sigmf.py` (new), `testdata/small/` (new, committed slices), `.gitignore` (exception for `testdata/small`).
- `.github/workflows/ci.yml` (new).
- `BACKLOG.md` (new section from the audit), `docs/superpowers/specs/2026-10-22-design-audit.md` (already written; commit it).

Step 1 (ring):

- `crates/sdr-core/src/lib.rs`: event contract documented (no type changes).
- `crates/backend-sim/src/lib.rs`: `SimConfig`, `SimControl`, anchors, injected events, unpaced mode.
- `crates/backend-airspy/src/lib.rs`: retune landing sample, anchor refresh, overload detection; `crates/backend-airspy/tests/on_air.rs` (ignored hardware test).
- `crates/engine/` (new crate, package `engine`):
  - `Cargo.toml`
  - `src/lib.rs` re-exports
  - `src/ring/mod.rs` `IqRing`, `RingConfig`, `ReadError`, `RingStats`, `WriteOutcome`
  - `src/ring/block.rs` `Block`, `BlockState`
  - `src/ring/volatile/mod.rs` `VolatileMemory`, `Region`, `OfferPriority`, `system()`
  - `src/ring/volatile/windows.rs`, `linux.rs`, `fallback.rs`, `fake.rs`
  - `src/ring/keeper.rs` `Keeper`, `Housekept`
  - `src/budget.rs` `RingBudget`, `BudgetConfig`, `Allowance`, `SourceId`, `Mode`
  - `src/segments.rs` `Segment`, `SegmentId`, `Boundary`, `SegmentTable`
  - `src/timeline.rs` `Timeline`
  - `src/source.rs` `Source`, `SourceConfig`, `SourceStats`, `DeviceSnapshot`, `ReadError`
  - `src/sigmf.rs` `rfc3339`, `write_meta`
  - `src/export.rs` `export_wideband`, `ExportResult`, `ExportError`
  - `tests/common/mod.rs`, `tests/source_sim.rs`, `tests/budget_two_sources.rs`, `tests/export_retune.rs`

---

## Step 0: repository hygiene

### Task 1: Licence, toolchain pin and the `rebuild` branch

**Files:**
- Create: `LICENSE`, `rust-toolchain.toml`
- Modify: `Cargo.toml` (workspace `license`), `crates/app/Cargo.toml`, `crates/backend-airspy/Cargo.toml`, `crates/backend-sim/Cargo.toml`, `crates/dsp/Cargo.toml`, `crates/engine/Cargo.toml`, `crates/sdr-core/Cargo.toml` (add `license.workspace = true`; `catalog` and `detect` already have it)

**Interfaces:**
- Produces: workspace `license = "MIT"` inherited by every crate; `rust-toolchain.toml` with `channel = "stable"`.

- [ ] **Step 1: Create the branch**

Run: `git -C D:\SpecTrawl checkout -b rebuild` (from `master` at f2a69ef or later). The untracked items (`crates/engine/examples/capture_wideband.rs`, `docs/superpowers/`, `tools/`) come along as untracked files; they are committed in Tasks 3 and 6.

- [ ] **Step 2: Write `LICENSE`**

The standard MIT text with the line `Copyright (c) 2026 auggiesoft` (the GitHub owner of the repository; the owner may change the name on review).

- [ ] **Step 3: Set the licence in the manifests**

In `Cargo.toml` change `license = "UNLICENSED"` to `license = "MIT"`. Add `license.workspace = true` under `[package]` in the six crate manifests listed above.

- [ ] **Step 4: Write `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 5: Verify every package reports MIT**

Run: `cargo metadata --no-deps --format-version 1 | python -c "import json,sys; print({p['name']: p['license'] for p in json.load(sys.stdin)['packages']})"`
Expected: every value is `MIT`.

- [ ] **Step 6: Commit**

```bash
git add LICENSE rust-toolchain.toml Cargo.toml crates/*/Cargo.toml
git commit -m "Licence the workspace MIT and pin the stable toolchain"
```

### Task 2: Formatting baseline

**Files:**
- Modify: whatever `cargo fmt --all` touches (107 hunks on 2026-10-23).

- [ ] **Step 1: Confirm the baseline is dirty**

Run: `cargo fmt --all --check | grep -c "^Diff in"`
Expected: a non-zero count.

- [ ] **Step 2: Format**

Run: `cargo fmt --all`

- [ ] **Step 3: Verify the build still passes**

Run: `cargo build --workspace`
Expected: success, no new warnings.

- [ ] **Step 4: Verify the check is clean**

Run: `cargo fmt --all --check`
Expected: exit 0, no output.

- [ ] **Step 5: Commit (formatting only, nothing else in this commit)**

```bash
git add -A crates
git commit -m "Apply rustfmt to the whole workspace so CI can check formatting"
```

### Task 3: Rename the old engine and app crates

**Files:**
- Rename: `crates/engine/` → `crates/engine-old/`; `crates/app/` → `crates/app-old/`
- Modify: `crates/engine-old/Cargo.toml` (package `engine-old`, `[lib] name = "engine_old"`), `crates/app-old/Cargo.toml` (package `spectrawl-old`, `[[bin]] name = "spectrawl"`, dependency `engine-old`), every `engine::` path in `crates/app-old/src/*.rs`, `crates/engine-old/examples/*.rs`, `crates/engine-old/tests/*.rs` (→ `engine_old::`), doc comments that say `cargo run -p engine` (→ `-p engine-old`), `tools/acceptance_captures.ps1` comments, spec §10 item 4 (`crates/engine-old/examples/capture_wideband`).

**Interfaces:**
- Produces: packages `engine-old` (lib `engine_old`) and `spectrawl-old` (binary still named `spectrawl`); the directory `crates/engine` is free for Task 9. The new app, when it arrives in step 3, is package `spectrawl-next` with binary `spectrawl-next` until step 8 swaps the names (decision recorded here; spec §6 leaves the binary name open).

- [ ] **Step 1: Rename with git so history follows**

```bash
git mv crates/engine crates/engine-old
git mv crates/app crates/app-old
git add crates/engine-old/examples/capture_wideband.rs
```

- [ ] **Step 2: Edit the two manifests**

`crates/engine-old/Cargo.toml`: `name = "engine-old"`, add
```toml
[lib]
name = "engine_old"
```
`crates/app-old/Cargo.toml`: `name = "spectrawl-old"`, dependency line `engine-old = { version = "0.1.0", path = "../engine-old" }` replacing the `engine` one, and
```toml
[[bin]]
name = "spectrawl"
path = "src/main.rs"
```

- [ ] **Step 3: Rewrite the crate paths**

Run (Git Bash): `grep -rl "engine::" crates/app-old/src crates/engine-old/examples crates/engine-old/tests | xargs sed -i 's/\bengine::/engine_old::/g; s/^use engine;/use engine_old;/'` then `grep -rl "cargo run.*-p engine " crates/engine-old | xargs sed -i 's/-p engine /-p engine-old /g'`. Check with `grep -rn "\bengine::" crates/app-old crates/engine-old | grep -v engine_old`. Expected: no matches.

- [ ] **Step 4: Update the two comments outside the crates**

`tools/acceptance_captures.ps1`: `$exe` still points at `target/release/examples/capture_wideband.exe`, which is unchanged; only the comment naming the crate changes. Spec §10 item 4: `crates/engine-old/examples/capture_wideband`.

- [ ] **Step 5: Verify everything builds, the old tests pass and the binary name is unchanged**

Run: `cargo build --workspace --all-targets && cargo test -p engine-old -p spectrawl-old && ls target/debug/spectrawl.exe`
Expected: build ok; `engine-old` tests pass (sigmf_export, record_all); the binary exists.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "Rename engine and app to engine-old and app-old; the old app still builds as spectrawl"
```

### Task 4: Real-capture tests fail instead of skipping; manifest and fetch scripts

**Files:**
- Create: `crates/detect/tests/common/mod.rs`, `testdata/captures/MANIFEST.toml`, `tools/fetch_captures.ps1`, `tools/fetch_captures.sh`
- Modify: `crates/detect/tests/real_captures.rs` (lines 9–26 and every `else { eprintln!("skipping…"); return; }`), `crates/detect/tests/real_spectra.rs` (lines 29, 38, 50), `BACKLOG.md`

**Interfaces:**
- Produces, in `crates/detect/tests/common/mod.rs`:
  - `pub fn capture(name: &str) -> Option<(Vec<Complex32>, f64)>`: loads `testdata/captures/<name>.sigmf-{data,meta}` (cf32), or `testdata/small/<name>` if that exists. If the file is absent and `SPECTRAWL_SKIP_CAPTURES` is set, prints `skipping: <name> not present (SPECTRAWL_SKIP_CAPTURES set)` and returns `None`. If absent and the variable is unset, panics with `capture <name> missing: run tools/fetch_captures (see testdata/captures/MANIFEST.toml) or set SPECTRAWL_SKIP_CAPTURES=1 to skip locally`.
  - `pub fn spectrum(name: &str) -> Option<String>` (the JSON text) with the same policy for `testdata/spectra/<name>.json`.
  - `pub fn capture_with(name: &str, skip: bool) -> Option<(Vec<Complex32>, f64)>`: the policy with the skip flag passed explicitly; `capture` reads the variable and calls it.
- `MANIFEST.toml` schema:
  ```toml
  release = "captures-v1"           # GitHub release tag that holds the assets
  [[capture]]
  name = "wideband_20261023_r1_p25_30s"   # stem; assets are <name>.sigmf-data and <name>.sigmf-meta
  sha256 = "…"                     # of the .sigmf-data
  bytes = 1200000000
  used_by = ["engine/tests/…"]
  ```
- Fetch scripts download every `[[capture]]` from the release into `testdata/captures/` with `gh release download <release> -R auggiesoft/SpecTrawl -p "<name>.*" -D testdata/captures --clobber`, verify the sha256, and exit non-zero on any missing asset or mismatch. With an empty capture list they print `manifest lists no captures` and exit 0. Assets already present with the right hash are not downloaded again.

- [ ] **Step 1: Write the failing tests for the loader policy** (test module inside `common/mod.rs`):

```rust
#[test]
#[should_panic(expected = "capture no_such_capture missing")]
fn missing_capture_panics_when_not_skipping() {
    let _ = super::capture_with("no_such_capture", false);
}

#[test]
fn missing_capture_skips_when_asked() {
    assert!(super::capture_with("no_such_capture", true).is_none());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p detect --test real_captures missing_capture`
Expected: compile error, `common` module not found.

- [ ] **Step 3: Implement `common/mod.rs`** with `capture`, `capture_with` and `spectrum` as specified, moving the existing `load` body from `real_captures.rs:9-21` into `capture_with`. `real_spectra.rs` keeps its JSON parsing but takes the text through `spectrum`.

- [ ] **Step 4: Replace every skip in the two test files**

Each `let Some(x) = load(name) else { eprintln!("skipping…"); return; };` becomes `let Some(x) = common::capture(name) else { return };` (the loader prints the skip line).

- [ ] **Step 5: Mark the tests whose captures no longer exist anywhere**

The seven stems `769.5782MHz_P25_Phase_1`, `771.6443MHz_P25_Phase_1`, `855.5882MHz_DMR_BS_data`, `460.5813MHz_NXDN_NXDN48`, `P25_site_770.3244`, `P25P2_site_769.1885`, `P25P2_site_771.2067` were recorded on another development machine and are not here. Put `#[ignore = "capture being re-recorded: see Task 5"]` on each test that names one for now, so they show as ignored rather than passing vacuously; Task 5 re-records them on the local systems, re-pins the expectations and removes the attribute. Add to `BACKLOG.md` a new heading `## Rebuild` (top of the file, before `## UI`) with the line `- **Golden captures re-recorded** (2026-10-23): the original P25/DMR/NXDN slices stayed on another machine; new ones live under testdata/small and the tests were re-pinned to the local systems.`

- [ ] **Step 6: Write the manifest and the two fetch scripts**

`MANIFEST.toml` starts with `release = "captures-v1"` and no `[[capture]]` entries (the first assets are added in step 5 of the build order, after the acceptance windows are cut). `fetch_captures.ps1` and `fetch_captures.sh` parse the manifest (PowerShell: a regex loop over `[[capture]]` tables; sh: `python3 -c` with `tomllib`), download, verify, and exit as specified in Interfaces.

- [ ] **Step 7: Run the detect tests three ways**

Run: `cargo test -p detect --test real_captures --test real_spectra`
Expected: the seven marked tests report `ignored`; the real_spectra tests pass (their files are committed); the two policy tests pass.
Run: `SPECTRAWL_SKIP_CAPTURES=1 cargo test -p detect --test real_captures -- --ignored`
Expected: the ignored tests run, print the `skipping:` line, and pass.
Run: `cargo test -p detect --test real_captures -- --ignored`
Expected: the seven fail with the `capture … missing` message.

- [ ] **Step 8: Commit**

```bash
git add crates/detect/tests testdata/captures/MANIFEST.toml tools/fetch_captures.ps1 tools/fetch_captures.sh BACKLOG.md
git commit -m "Real-capture tests fail when their capture is missing; add the captures manifest and fetch scripts"
```

### Task 5: Committed small captures: a FLEX slice, and the golden captures re-recorded on air

**Files:**
- Create: `tools/cut_sigmf.py`, `testdata/small/flex_929.6127_10s.sigmf-{data,meta}`, `testdata/small/<stem>.sigmf-{data,meta}` for each re-recorded golden capture (at most 10 s each, 4 MB at 50 kS/s cf32; the whole directory stays under 50 MB)
- Modify: `.gitignore` (add `!/testdata/small/` and `!/testdata/small/*.sigmf-data` after the `*.sigmf-data` line), `crates/detect/tests/real_captures.rs` (one new FLEX test; the seven ignored tests re-pinned and un-ignored)

**On-air recording (the Airspy serial `735863DC2F650AE3` is free except 08:46–08:50 on 2026-10-23, when the scheduled round-2 capture runs; if `open` fails, wait a minute and retry):** `cargo run --release -p engine-old --example capture_identified -- 735863DC2F650AE3 <MHz> 40 testdata/captures/golden` once per band: 855.4 (the local P25 Phase 1 control and traffic channels, NACs 0x340/0x343/0x344/0x34D, and the DMR data channel near 855.59), 770.3 (the 769–772 MHz P25 site the old tests named, if it exists here), 460.6 (NXDN, if anything is on air). The example writes a cf32 slice per identified emitter, named `<MHz>MHz_<label>`. Pick one slice per original test (P25 control with TSDUs, P25 traffic with TDULC, DMR CSBK, NXDN RCCH, a P25 Phase 2 traffic pair with the control channel for `P25P2_site_*`), cut each to its best 10 s with `cut_sigmf.py`, and commit them under `testdata/small` with the original stems replaced by the new frequencies. A test whose protocol is not on air here (NXDN is the likely one) keeps its `#[ignore]` with the reason `not on air in the owner's area` and a BACKLOG line under `## Rebuild`.

**Interfaces:**
- `tools/cut_sigmf.py <in-stem> <out-stem> <start_s> <seconds>`: copies the sample range of a SigMF pair, keeping datatype, rate and frequency, and writes `core:description = "cut from <in-stem> at <start_s> s"`. Supports `cf32_le` and `ci16_le`.
- The committed slice: 10 s from 30 s into `testdata/captures/flex_20261022_929.6127` (cf32, 50 kS/s, 4,000,000 bytes).

- [ ] **Step 1: Write the failing golden test**

```rust
#[test]
fn flex_pages_decode_from_the_committed_slice() {
    let Some((x, fs)) = common::capture("flex_929.6127_10s") else { return };
    let r = detect::flex::decode(&x, fs);
    assert!(r.pages.len() >= 1, "no pages: {r:?}");
}
```
Use the real names of `detect::flex`'s entry point and its page collection (read `crates/detect/src/flex.rs`; the old pager monitor at `crates/engine-old/src/monitor.rs` shows the call). After the first green run, replace `>= 1` with the exact count that run produced and add an assertion on the first page's capcode, so the test pins the decode.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p detect --test real_captures flex_pages`
Expected: FAIL with `capture flex_929.6127_10s missing`.

- [ ] **Step 3: Write `tools/cut_sigmf.py` and cut the slice**

Run: `python tools/cut_sigmf.py testdata/captures/flex_20261022_929.6127 testdata/small/flex_929.6127_10s 30 10`
Expected: `testdata/small/flex_929.6127_10s.sigmf-data` is exactly 4,000,000 bytes.

- [ ] **Step 4: Un-ignore the slice in `.gitignore`** and confirm with `git check-ignore -v testdata/small/flex_929.6127_10s.sigmf-data` (expected: no output, exit 1).

- [ ] **Step 5: Run the test, pin the counts, run again**

Run: `cargo test -p detect --test real_captures flex_pages`
Expected: PASS; then edit the assertions to the exact page count and first capcode and run again: PASS.

- [ ] **Step 6: Record the golden captures on air and cut them** as described above. Expected: at least the P25 control, P25 traffic and DMR slices exist under `testdata/small`, each `.sigmf-data` at most 4,000,000 bytes, `du -sh testdata/small` under 50 MB.

- [ ] **Step 7: Re-pin the seven tests**

For each test that now has a slice: change the stem to the new one, remove `#[ignore]`, run it once with the assertions loosened to `protocol` only, then pin `id` (the NAC or colour code the run reports), `kind` and `min_decoded` (two thirds of the decoded count the run produced, rounded down) and run again. `p25_control_channel_messages` and the Phase 2 tests pin whatever system/site/talkgroup values the run shows, by the same two-run procedure.

Run: `cargo test -p detect --test real_captures`
Expected: every un-ignored test passes on the committed slices; the remaining ignored ones carry the `not on air` reason.

- [ ] **Step 8: Commit**

```bash
git add tools/cut_sigmf.py testdata/small .gitignore crates/detect/tests/real_captures.rs BACKLOG.md
git commit -m "Commit 10 s golden slices under testdata/small (FLEX, P25, DMR) recorded on the local systems, with the tests re-pinned"
```

### Task 6: CI for Windows and Linux, and the BACKLOG audit section

**Files:**
- Create: `.github/workflows/ci.yml`
- Modify: `crates/backend-sim/src/lib.rs` (3 clippy warnings), `BACKLOG.md` (`## Rebuild` section), add `docs/superpowers/specs/2026-10-22-design-audit.md`, `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` and `docs/superpowers/plans/2026-10-23-rebuild-steps-0-1.md` to git.

**Interfaces:**
- CI gates from here on: `cargo fmt --all --check`; `cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp --all-targets -- -D warnings` (Task 9 adds `-p engine`); `tools/fetch_captures`; `cargo test --workspace` with `SPECTRAWL_SKIP_CAPTURES` unset. `engine-old`, `spectrawl-old`, `detect` and `catalog` are built and tested but not clippy-gated (51, 78, 80 and 33 warnings on 2026-10-23; they are replaced or fixed later and gating them now would be churn).

- [ ] **Step 1: Fix the three clippy warnings in `backend-sim`**

Run: `cargo clippy -p backend-sim --all-targets -- -D warnings`
Expected after the fix: exit 0.

- [ ] **Step 2: Write the workflow**

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [windows-latest, ubuntu-latest]
    runs-on: ${{ matrix.os }}
    env:
      GH_TOKEN: ${{ github.token }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: "rustfmt, clippy" }
      - uses: Swatinem/rust-cache@v2
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libgtk-3-dev libasound2-dev libxkbcommon-dev libwayland-dev libudev-dev pkg-config
      - run: cargo fmt --all --check
      - run: cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp --all-targets -- -D warnings
      - if: runner.os == 'Windows'
        run: pwsh tools/fetch_captures.ps1
      - if: runner.os == 'Linux'
        run: sh tools/fetch_captures.sh
      - run: cargo test --workspace
```
(`libgtk-3-dev` is for `rfd`, `libasound2-dev` for `cpal`, the rest for `eframe` on Wayland/X11. If the Linux build reports another missing system library, add it here; that is the only expected iteration.)

- [ ] **Step 3: Write the `## Rebuild` BACKLOG section from the audit**

Under the `## Rebuild` heading added in Task 4, add one line per row of `docs/superpowers/specs/2026-10-22-design-audit.md` whose status is PARTIAL, CONTRADICTED, SILENTLY DROPPED or MISSING (81 rows), in the audit's order, in this form: `- DESIGN §<section> L<line>: <requirement in at most twelve words> — <status> — <rebuild step that addresses it, or "not scheduled">`. The step comes from spec §6 (ring: 1; FFT/channelizer: 2; api/UI contract: 3; records, claims, store, tags, notes: 4; sweep: 4a; replay and scrubbing: 5; detector bank and classification throughput: 6; OnAir modes: 7; presentation slots: 8; explore/scan/RDS/cell/record-all/voice: 9). Anything the spec does not schedule (baselines, map pane, ML classifier, native sweep on other hardware, ppm/dBm calibration, role assigner, timeline pane, Wi-Fi/BLE signatures, disk tier for the raw ring) is `not scheduled`.

- [ ] **Step 4: Run the gates locally exactly as CI will**

Run: `cargo fmt --all --check && cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp --all-targets -- -D warnings && pwsh tools/fetch_captures.ps1 && cargo test --workspace`
Expected: all exit 0; `fetch_captures` prints `manifest lists no captures`.

- [ ] **Step 5: Commit**

```bash
git add .github BACKLOG.md crates/backend-sim docs/superpowers
git commit -m "Add CI for Windows and Linux, and the BACKLOG section listing every unbuilt DESIGN.md requirement"
```

- [ ] **Step 6: Push and watch the first run (outward action: the owner confirms the push)**

Run: `git push -u origin rebuild` then `gh run watch --repo auggiesoft/SpecTrawl` (or `gh run list --branch rebuild --limit 1`).
Expected: both matrix jobs green. If Linux fails on a system library, add it to the apt line, commit as "CI: add <lib> for <crate> on Linux", push again. If a simulator-timed test (`engine-old` `sigmf_export`, `record_all`) is flaky on the hosted runner, do not loosen it here; note it in BACKLOG under `## Rebuild` and move on. Step 0 is done when the run is green.

---

## Step 1: ring, segments and clock

### Task 7: Simulator emits the full event contract and can be scripted

**Files:**
- Modify: `crates/sdr-core/src/lib.rs` (doc comments on `StreamEvent` only), `crates/backend-sim/src/lib.rs`
- Test: `crates/backend-sim/src/lib.rs` (unit tests at the bottom)

**Interfaces:**
- `sdr-core` contract, written as doc comments on `StreamEvent` (no type change): `TimeAnchor` at stream start and at least once per second of samples, `at_sample` being the sample the `host_time` corresponds to; `Retune` stamped at the first sample that carries the new tuning, as closely as the backend can determine; `Gap` with `at_sample` the index of the first lost sample, and the next `on_samples` starts at `at_sample + lost_samples`; `Overload` at most once per second while input clips; `DeviceLost` once, after which no more samples arrive.
- `backend-sim` additions:
  ```rust
  pub struct SimConfig { pub emitters: Vec<Emitter>, pub rate: f64, pub center_hz: f64, pub paced: bool, pub serial: String }
  impl Default for SimConfig   // default_scenario(), 10e6, 855.25e6, paced: true, "SIM-0001"
  impl SimDevice {
      pub fn with_config(cfg: SimConfig) -> (SimDevice, SimControl);
  }
  #[derive(Clone)] pub struct SimControl(Arc<SimShared>);
  impl SimControl {
      pub fn inject_gap(&self, lost_samples: u64);   // next block: Gap{at_sample: n, lost_samples}, then n += lost
      pub fn inject_overload(&self);                 // next block: Overload{at_sample: n}
      pub fn drop_device(&self);                     // next block: DeviceLost, generator exits, is_streaming() becomes false
      pub fn starts(&self) -> u32;                   // how many times Device::start ran
      pub fn samples_sent(&self) -> u64;
  }
  ```
  `SimDevice::new(emitters)` is kept (app-old uses it) and equals `with_config(SimConfig { emitters, ..Default::default() }).0`. `paced: false` runs the generator as fast as the CPU allows (no sleep, no `Gap` from falling behind). When several injections are pending at one block boundary they are applied in the order gap, overload, samples, device loss, so an overload injected just before `drop_device` is still emitted. `TimeAnchor { at_sample: n, host_time: now }` is emitted at `n == 0` and whenever `n - last_anchor >= rate`. `is_streaming()` returns false once the generator has exited after `drop_device`.

- [ ] **Step 1: Write the failing tests** (unit tests in `backend-sim`, using a `Recorder: StreamSink` that stores `(first_sample, len)` per block and every event behind an `Arc<Mutex<..>>`, and a `wait_until(|| …)` that polls every 5 ms with a 10 s deadline):

```rust
#[test]
fn anchors_once_per_second_and_retune_at_block_start() {
    let (mut d, _c) = SimDevice::with_config(SimConfig { emitters: vec![], paced: false, ..Default::default() });
    let log = Recorder::new();
    d.start(Box::new(log.clone())).unwrap();
    wait_until(|| log.samples() >= 25_000_000);
    d.set_center(860e6).unwrap();
    wait_until(|| log.samples() >= 30_000_000);
    d.stop().unwrap();
    let at: Vec<u64> = log.events(|e| matches!(e, StreamEvent::TimeAnchor { .. })).iter().map(at_sample).collect();
    assert!(at.len() >= 3, "{at:?}");
    assert_eq!(at[0], 0);
    assert!(at.windows(2).all(|w| w[1] - w[0] >= 10_000_000 && w[1] - w[0] < 10_000_000 + 65_536), "{at:?}");
    let retunes = log.events(|e| matches!(e, StreamEvent::Retune { center_hz, .. } if *center_hz == 860e6));
    assert_eq!(retunes.len(), 1);
    assert_eq!(at_sample(&retunes[0]) % 65_536, 0);
}

#[test]
fn injected_gap_overload_and_loss_arrive_in_order() {
    let (mut d, c) = SimDevice::with_config(SimConfig { emitters: vec![], paced: false, ..Default::default() });
    let log = Recorder::new();
    d.start(Box::new(log.clone())).unwrap();
    wait_until(|| log.samples() >= 1_000_000);
    c.inject_gap(123_456);
    c.inject_overload();
    wait_until(|| log.has(|e| matches!(e, StreamEvent::Overload { .. })));
    c.drop_device();
    wait_until(|| !d.is_streaming());
    let StreamEvent::Gap { at_sample, lost_samples } = log.events(|e| matches!(e, StreamEvent::Gap { .. }))[0].clone() else { unreachable!() };
    assert_eq!(lost_samples, 123_456);
    assert_eq!(log.first_sample_after(at_sample), at_sample + 123_456);
    assert_eq!(log.events(|e| matches!(e, StreamEvent::DeviceLost)).len(), 1);
    assert_eq!(c.starts(), 1);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p backend-sim`
Expected: compile errors (`SimConfig`, `with_config`, `SimControl` undefined).

- [ ] **Step 3: Implement** `SimConfig`, `SimControl` (pending-gap `AtomicU64`, overload and drop `AtomicBool`s, `starts` and `samples_sent` counters in a shared struct the generator polls at each block), the anchor emission, the unpaced loop, and the doc comments in `sdr-core`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p backend-sim && cargo clippy -p backend-sim -p sdr-core --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core crates/backend-sim
git commit -m "Simulator emits anchors every second and can inject gaps, overloads and device loss; document the stream event contract"
```

### Task 8: Airspy backend stamps retunes where they land, refreshes anchors, reports overload

**Files:**
- Modify: `crates/backend-airspy/src/lib.rs` (`CbCtx`, `rx_callback`, `set_center`, `start`)
- Create: `crates/backend-airspy/tests/on_air.rs`

**Interfaces:**
- Pure function, unit-testable: `pub(crate) fn landing_sample(block_first: u64, block_len: usize, rate: f64, block_end: Instant, set_at: Instant, lands_after: Duration) -> u64`: the sample index inside `[block_first, block_first + block_len]` where a retune requested at `set_at` took effect, assuming the block's last sample arrived at `block_end` and the hardware applies a tune `lands_after` (2 ms, the midpoint of the measured 1.4–2.4 ms) after the request. Clamped to the block.
- `pub(crate) fn clipped_fraction(s: &[Complex32]) -> f32`: fraction of samples with `|re| >= 0.999 || |im| >= 0.999`.
- Behaviour: `set_center` stores the centre bits and the request `Instant` (as nanoseconds since a process-wide `OnceLock<Instant>` epoch, in an `AtomicU64`); the callback emits `Retune` at `landing_sample(...)` when it sees a new centre; emits `TimeAnchor { at_sample: n + count, host_time: SystemTime::now() }` whenever `n + count - last_anchor >= rate`; emits `Overload { at_sample: n }` when `clipped_fraction >= 0.01` and at least one second of samples has passed since the last one.
- The engine, not the backend, raises `DeviceLost` for the Airspy (Task 14's watchdog), because libairspy gives no callback on unplug; `Device::is_streaming` already returns false then.

- [ ] **Step 1: Write the failing unit tests** in `backend-airspy`:

```rust
#[test]
fn landing_sample_is_inside_the_block_and_follows_the_request_time() {
    let t0 = Instant::now();
    let rate = 10e6;
    let block_end = t0 + Duration::from_micros(6554);        // 65536 samples at 10 MS/s
    // Requested 1 ms into the block: lands 3 ms in = sample 30000.
    let s = landing_sample(1_000_000, 65_536, rate, block_end, t0 + Duration::from_millis(1), Duration::from_millis(2));
    assert!((29_000..31_000).contains(&(s - 1_000_000)), "{s}");
    // Requested long before the block: clamps to its first sample.
    assert_eq!(landing_sample(1_000_000, 65_536, rate, block_end, t0 - Duration::from_secs(1), Duration::from_millis(2)), 1_000_000);
    // Requested after the block ended: clamps to its last sample.
    assert_eq!(landing_sample(1_000_000, 65_536, rate, block_end, block_end, Duration::from_millis(2)), 1_000_000 + 65_536);
}

#[test]
fn clipped_fraction_counts_full_scale_samples() {
    let mut v = vec![Complex32::new(0.1, 0.1); 100];
    v[3] = Complex32::new(1.0, 0.0);
    v[7] = Complex32::new(0.0, -0.9995);
    assert!((clipped_fraction(&v) - 0.02).abs() < 1e-6);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p backend-airspy`
Expected: compile error, functions undefined.

- [ ] **Step 3: Implement** the two functions and wire them into `CbCtx`/`rx_callback`/`set_center`/`start` as described in Interfaces. Keep the existing `Retune` on `start` at sample 0.

- [ ] **Step 4: Write the ignored on-air test** `crates/backend-airspy/tests/on_air.rs`:

```rust
#[test]
#[ignore = "needs an Airspy: SPECTRAWL_AIRSPY_SERIAL=<serial> cargo test -p backend-airspy --test on_air -- --ignored"]
fn retunes_and_anchors_on_hardware() {
    // open by serial, start with a Recorder, retune 5 times 200 ms apart, stop; assert 5 Retune events with strictly
    // increasing at_sample, each within one transfer (65536) of the sample count reached when the retune was requested;
    // at least one TimeAnchor per second of samples; no Overload at default gain.
}
```

- [ ] **Step 5: Run unit tests and clippy; run the on-air test if the Airspy is free**

Run: `cargo test -p backend-airspy && cargo clippy -p backend-airspy --all-targets -- -D warnings`
Expected: PASS, clean. On hardware: `SPECTRAWL_AIRSPY_SERIAL=735863DC2F650AE3 cargo test -p backend-airspy --test on_air -- --ignored` passes (the device is busy during the 08:00–10:00 capture window on 2026-10-23 and whenever the old app is open).

- [ ] **Step 6: Commit**

```bash
git add crates/backend-airspy
git commit -m "Airspy stamps retunes at the sample they land on, refreshes the time anchor every second and reports overload"
```

### Task 9: New `engine` crate with the volatile-memory backends

**Files:**
- Create: `crates/engine/Cargo.toml`, `crates/engine/src/lib.rs`, `crates/engine/src/ring/mod.rs` (module list only for now), `crates/engine/src/ring/volatile/{mod,windows,linux,fallback,fake}.rs`
- Modify: `.github/workflows/ci.yml` (add `-p engine` to the clippy line)

**Interfaces:**
- `Cargo.toml`: package `engine`, `license.workspace = true`; deps `sdr-core`, `parking_lot = "0.12"`, `arc-swap = "1.7"`, `log = "0.4"`, `serde_json = "1"`; `[target.'cfg(windows)'.dependencies] windows-sys = { version = "0.60", features = ["Win32_Foundation", "Win32_System_Memory", "Win32_System_SystemInformation"] }`; `[target.'cfg(unix)'.dependencies] libc = "0.2"`; dev-deps `backend-sim`, `serde_json`.
- `ring/volatile/mod.rs`:
  ```rust
  pub const PAGE: usize = 4096;
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum OfferPriority { VeryLow, Low, BelowNormal, Normal }
  pub struct Region { ptr: NonNull<u8>, bytes: usize }   // page aligned; unsafe impl Send + Sync; accessors ptr(), bytes()
  pub trait VolatileMemory: Send + Sync {
      fn alloc(&self, bytes: usize) -> Option<Region>;   // bytes multiple of PAGE; zero-filled; committed lazily by the OS
      fn offer(&self, r: &Region, priority: OfferPriority);
      fn reclaim(&self, r: &Region) -> bool;              // true: contents intact; false: discarded, region writable again
      fn free(&self, r: Region);
  }
  pub fn system() -> Arc<dyn VolatileMemory>;             // windows / linux / fallback by cfg
  ```
- `windows.rs`: `VirtualAlloc(MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)`, `OfferVirtualMemory` with `VmOfferPriorityVeryLow..Normal`, `ReclaimVirtualMemory` (0 → intact, `ERROR_BUSY` (170) → discarded), `VirtualFree(MEM_RELEASE)`. Offered memory must not be touched until reclaimed; the ring (Task 11) guarantees that.
- `linux.rs`: `mmap(MAP_PRIVATE | MAP_ANONYMOUS)`; `offer` records a 64-bit FxHash-style hash of every 4 KiB page in a side table (`Mutex<HashMap<usize /*addr*/, Vec<u64>>>`), then `madvise(MADV_FREE)`; `reclaim` walks the pages: a page that now reads all zero while its recorded hash is not the all-zero page's hash is lost; every page is then written back to itself (one volatile byte write per page) to cancel the lazy free; returns false if any page was lost; `free` is `munmap`. Priority is ignored (`MADV_FREE` has none).
- `fallback.rs`: `alloc` via `std::alloc::alloc_zeroed` with page alignment, `offer` no-op, `reclaim` always true.
- `fake.rs`: `FakeVolatile` for tests: `alloc` as fallback; `offer` records `(addr, priority)` in order; `reclaim` returns false for regions in `discarded`; `pub fn discard_oldest_offered(&self, n: usize)` moves the n oldest still-offered regions into `discarded` and zero-fills them; `pub fn offered(&self) -> Vec<(usize, OfferPriority)>`.

- [ ] **Step 1: Write the failing tests** (`ring/volatile/mod.rs` test module):

```rust
#[test]
fn system_backend_round_trips_on_an_idle_machine() {
    let m = system();
    let r = m.alloc(8 * PAGE).unwrap();
    unsafe { std::ptr::write_bytes(r.ptr().as_ptr(), 0xA5, r.bytes()) };
    m.offer(&r, OfferPriority::Low);
    assert!(m.reclaim(&r), "offered memory was discarded on an idle machine");
    assert_eq!(unsafe { *r.ptr().as_ptr().add(5 * PAGE + 17) }, 0xA5);
    m.free(r);
}

#[test]
fn fake_backend_discards_oldest_offered_first() {
    let m = FakeVolatile::default();
    let a = m.alloc(PAGE).unwrap(); let b = m.alloc(PAGE).unwrap(); let c = m.alloc(PAGE).unwrap();
    unsafe { *a.ptr().as_ptr() = 1; *b.ptr().as_ptr() = 2; *c.ptr().as_ptr() = 3 };
    m.offer(&a, OfferPriority::VeryLow); m.offer(&b, OfferPriority::Low); m.offer(&c, OfferPriority::Normal);
    m.discard_oldest_offered(2);
    assert!(!m.reclaim(&a)); assert!(!m.reclaim(&b)); assert!(m.reclaim(&c));
    assert_eq!(unsafe { *a.ptr().as_ptr() }, 0);
    assert_eq!(unsafe { *c.ptr().as_ptr() }, 3);
    assert_eq!(m.offered().iter().map(|o| o.1).collect::<Vec<_>>(), vec![OfferPriority::VeryLow, OfferPriority::Low, OfferPriority::Normal]);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine`
Expected: the crate does not exist yet; after creating `Cargo.toml` and an empty `lib.rs`, compile errors for the missing items.

- [ ] **Step 3: Implement** the crate manifest, `lib.rs` (`pub mod ring;`), the trait, and the four backends. On Windows, `system()` returns the Windows backend; on Linux the Linux one; elsewhere the fallback.

- [ ] **Step 4: Run tests and clippy; add `engine` to CI's clippy line**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/engine .github/workflows/ci.yml Cargo.lock
git commit -m "New engine crate: volatile memory backends (Windows offer/reclaim, Linux MADV_FREE with page hashes, fallback, test fake)"
```

### Task 10: The block ring: writer, readers, provisioning

**Files:**
- Create: `crates/engine/src/ring/block.rs`
- Modify: `crates/engine/src/ring/mod.rs`

**Interfaces:**
```rust
pub const DEFAULT_BLOCK_BYTES: usize = 64 << 20;
pub const MAX_SLOTS: usize = 4096;                       // 256 GiB at the default block size
#[derive(Clone, Debug)] pub struct RingConfig { pub block_bytes: usize }   // multiple of 4096; Default = DEFAULT_BLOCK_BYTES
#[derive(Debug, PartialEq, Eq, Clone, Copy)] pub enum ReadError { Overwritten, Lost, Future }
#[derive(Debug, Default, Clone, Copy)] pub struct WriteOutcome { pub starved: u64 }
#[derive(Debug, Clone, Copy, Default)] pub struct RingStats {
    pub head: u64, pub oldest: u64, pub held_samples: u64, pub block_samples: u64,
    pub ceiling_blocks: u32, pub guaranteed_samples: u64,
    pub blocks_live: u32, pub blocks_offered: u32, pub blocks_lost_total: u64, pub starved_total: u64,
}
pub struct IqRing { /* cfg, mem: Arc<dyn VolatileMemory>, slots: Box<[ArcSwapOption<Block>]>, head: AtomicU64, oldest: AtomicU64, wanted_block: AtomicU64, ceiling_blocks: AtomicU32, guaranteed_samples: AtomicU64, counters */ }
impl IqRing {
    pub fn new(cfg: RingConfig, mem: Arc<dyn VolatileMemory>) -> Arc<IqRing>;
    pub fn block_samples(&self) -> u64;                    // block_bytes / 4
    pub fn head(&self) -> u64;                             // Acquire
    pub fn oldest(&self) -> u64;                           // Acquire; oldest sample that might still be readable
    pub fn write(&self, first_sample: u64, samples: Samples<'_>) -> WriteOutcome;   // single writer
    pub fn read_raw(&self, start: u64, out: &mut [[i16; 2]]) -> Result<(), ReadError>;
    pub fn read(&self, start: u64, out: &mut [Complex32]) -> Result<(), ReadError>;  // scaled by 1/32768
    pub fn provision(&self, ahead: u32) -> u32;            // keeper: ensure blocks for wanted_block..=wanted_block+ahead exist; returns blocks allocated
    pub fn set_ceiling_blocks(&self, n: u32); pub fn set_guaranteed_samples(&self, n: u64);
    pub fn stats(&self) -> RingStats;
}
```
- `block.rs`: `pub struct Block { pub index: u64, state: AtomicU8, readers: AtomicU32, region: Region, mem: Arc<dyn VolatileMemory> }` with `BlockState { Live = 0, Offered = 1, Lost = 2 }`, `fn samples(&self) -> *mut [i16; 2]`, and `impl Drop` that frees the region through `mem`.
- Writer protocol: the block for sample `s` is index `b = s / block_samples`, slot `b % MAX_SLOTS`. If the slot holds a block whose `index == b`, write there; otherwise count the samples as starved, set `wanted_block = b`, and skip them (`head` still advances past them so the stream stays numbered). `first_sample < head` → the overlap is dropped, the rest written. `first_sample > head` → `head` jumps (no zero fill); the segment table (Task 12) records the gap. `head` is stored with `Release` after the copy. Samples are converted exactly as `engine-old/src/ring.rs:78-88` does (Ci8 shifted, Cf32 scaled and clamped).
- Reader protocol: load `head` (Acquire) and `oldest`; `end > head` → `Future`; `start < oldest` → `Overwritten`; for each block in the range: `slots[b % MAX_SLOTS].load()`; a missing block or `index != b` → `Overwritten`; `readers += 1`; if state is `Offered`, CAS to `Live` and `mem.reclaim()`; `false` → set `Lost`, return `Lost`; if state is `Lost` → `Lost`; copy; `readers -= 1`; the `ArcSwap` guard keeps a retired block's memory alive until the copy ends, so a block that was retired mid-read still returns the true samples; after the copy, re-check `oldest <= start` and return `Overwritten` if it moved past `start` (the samples were valid but the caller must know the range is gone).
- `provision(ahead)`: for `b` in `wanted_block ..= wanted_block + ahead`, if the slot's block index is not `b`: allocate a block (the keeper calls this; the writer never allocates). Retirement and offering are Task 11; in this task `provision` only adds blocks, and `oldest` only moves when a slot is overwritten by a newer index or on starvation.
- Starvation breaks continuity: after a write skips samples because their block was not provisioned, `oldest` is set to the new `head` (the skipped samples were never stored, so nothing older is part of the held range any more; the keeper retires the orphaned older blocks on its next pass, and the Source records the skip as a `Gap` segment). This is why `large_gap_starves_until_provisioned` expects `Overwritten` at sample 10,050.
- Blocks provisioned ahead of `head` (index greater than the head's block) are not counted in `RingStats::blocks_live` and do not count against the ceiling; the ceiling and the stats cover blocks that hold written samples (index at or below the head's block).

- [ ] **Step 1: Write the failing tests** (`ring/mod.rs` test module, `RingConfig { block_bytes: 4096 }` so a block is 1024 samples, `FakeVolatile`):

```rust
fn ramp(start: u64, n: usize) -> Vec<[i16; 2]> { (0..n).map(|i| [((start + i as u64) % 30000) as i16, 7]).collect() }

#[test]
fn writes_across_blocks_and_reads_back_by_absolute_index() {
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
    assert_eq!(r.provision(3), 4);                       // blocks 0..=3
    assert_eq!(r.write(0, Samples::Ci16(&ramp(0, 1500))).starved, 0);
    assert_eq!(r.write(1500, Samples::Ci16(&ramp(1500, 1500))).starved, 0);
    assert_eq!(r.head(), 3000);
    let mut out = vec![[0i16; 2]; 2048];
    r.read_raw(900, &mut out).unwrap();                  // spans blocks 0, 1, 2
    assert_eq!(out[0], [900, 7]); assert_eq!(out[2047], [2947, 7]);
    assert_eq!(r.read_raw(2999, &mut out[..2]), Err(ReadError::Future));
    let mut f = vec![Complex32::default(); 4];
    r.read(100, &mut f).unwrap();
    assert!((f[0].re - 100.0 / 32768.0).abs() < 1e-7);
}

#[test]
fn out_of_order_block_is_ignored() {
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
    r.provision(2);
    r.write(0, Samples::Ci16(&ramp(0, 1000)));
    r.write(500, Samples::Ci16(&vec![[-1, -1]; 700]));   // overlaps 500..1000, adds 1000..1200
    assert_eq!(r.head(), 1200);
    let mut out = vec![[0i16; 2]; 2];
    r.read_raw(999, &mut out).unwrap();
    assert_eq!(out, [[999, 7], [-1, -1]]);
}

#[test]
fn large_gap_starves_until_provisioned() {
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
    r.provision(1);                                      // blocks 0, 1
    r.write(0, Samples::Ci16(&ramp(0, 1000)));
    let w = r.write(10_000, Samples::Ci16(&ramp(10_000, 100)));  // block 9 does not exist
    assert_eq!(w.starved, 100);
    assert_eq!(r.head(), 10_100);
    assert_eq!(r.stats().starved_total, 100);
    r.provision(1);                                      // keeper catches up: blocks 9, 10
    assert_eq!(r.write(10_100, Samples::Ci16(&ramp(10_100, 100))).starved, 0);
    let mut out = vec![[0i16; 2]; 1];
    assert_eq!(r.read_raw(10_050, &mut out), Err(ReadError::Overwritten));   // never stored
    r.read_raw(10_150, &mut out).unwrap();
}

#[test]
fn concurrent_reader_sees_ramp_or_overwritten() {
    // writer thread: 200 blocks of 1024 samples, calling provision(2) itself before each block (test-only use);
    // reader thread: 2000 random reads of 1500 samples within [oldest, head); each Ok read must be a perfect ramp.
    // Assert: no torn read; at least one Ok; at least one Overwritten.
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine ring::`
Expected: compile errors.

- [ ] **Step 3: Implement `block.rs` and `IqRing`** per the protocols above. The slot array is `Box<[ArcSwapOption<Block>]>` of `MAX_SLOTS`. Slot overwrite in `provision` (a newer index landing on a slot that still holds an older block) bumps `oldest` to `(old.index + 1) * block_samples`.

- [ ] **Step 4: Run tests, including the stress test several times**

Run: `cargo test -p engine ring:: && for i in 1 2 3 4 5; do cargo test -p engine concurrent_reader -- --quiet || exit 1; done`
Expected: PASS every time.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "Block ring: lock-free writer, absolute-index readers through ArcSwap slots, keeper-side provisioning"
```

### Task 11: Housekeeping: offer by age, retire past the ceiling, lose honestly

**Files:**
- Modify: `crates/engine/src/ring/mod.rs`

**Interfaces:**
```rust
impl IqRing {
    /// One keeper pass: provision 2 blocks ahead, retire blocks beyond the ceiling (oldest first, never the writer's block),
    /// offer every Live block older than the guaranteed window with a priority by age.
    pub fn housekeep(&self) -> Housekeeping;
}
#[derive(Debug, Default, Clone, Copy)] pub struct Housekeeping { pub provisioned: u32, pub retired: u32, pub offered: u32 }
```
- Offer schedule (builder default, spec §9 leaves it open): split the opportunistic window (blocks older than the guaranteed window, up to the ceiling) into four equal age bands from newest to oldest: `Normal`, `BelowNormal`, `Low`, `VeryLow`. A block is offered when it first falls outside the guaranteed window; it is re-offered at a lower priority when it crosses into an older band, only if it is `Live` and has no readers (`readers == 0`), using CAS `Live → Offered` before the syscall. A reclaimed block (a reader found it intact) is `Live` again and gets offered again on the next pass.
- Retirement: while `blocks_live + blocks_offered > ceiling_blocks`, drop the oldest block's slot (`store(None)`) and advance `oldest`. Blocks inside the guaranteed window are never retired or offered even if the ceiling is smaller than the guaranteed window (the ring defends itself; the budget in Task 13 never produces that case).
- Loss accounting: a reader that finds a discarded block marks it `Lost` and bumps `blocks_lost_total`; the keeper retires `Lost` blocks on its next pass and advances `oldest` past them if they are the oldest; a `Lost` block in the middle leaves a hole that reads return `Lost` for, until retirement reaches it.

- [ ] **Step 1: Write the failing tests** (block 4096 bytes = 1024 samples, fake memory):

```rust
#[test]
fn oldest_offered_blocks_are_lost_first_and_the_guaranteed_window_never_is() {
    let mem = Arc::new(FakeVolatile::default());
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
    r.set_ceiling_blocks(12);
    r.set_guaranteed_samples(3 * 1024);
    for b in 0..12u64 { r.housekeep(); r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024))); }
    r.housekeep();
    let s = r.stats();
    assert_eq!(s.blocks_live + s.blocks_offered, 12);
    assert!(s.blocks_offered >= 8, "{s:?}");               // 12 held, 3 guaranteed, writer's block live
    let pri: Vec<_> = mem.offered().iter().map(|o| o.1).collect();
    assert_eq!(pri[0], OfferPriority::VeryLow);              // oldest offered lowest
    assert_eq!(*pri.last().unwrap(), OfferPriority::Normal);
    mem.discard_oldest_offered(2);                            // the OS took blocks 0 and 1
    let mut out = vec![[0i16; 2]; 10];
    assert_eq!(r.read_raw(100, &mut out), Err(ReadError::Lost));
    assert_eq!(r.read_raw(1100, &mut out), Err(ReadError::Lost));
    r.read_raw(2100, &mut out).unwrap();                      // block 2 reclaimed intact
    r.read_raw(11 * 1024 + 5, &mut out[..1]).unwrap();        // newest, never offered
    r.housekeep();
    assert_eq!(r.stats().blocks_lost_total, 2);
    assert_eq!(r.oldest(), 2 * 1024);
    assert_eq!(r.read_raw(100, &mut out), Err(ReadError::Overwritten));
}

#[test]
fn ceiling_retires_oldest_and_shrinking_takes_effect_on_the_next_pass() {
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()));
    r.set_ceiling_blocks(6); r.set_guaranteed_samples(1024);
    for b in 0..10u64 { r.housekeep(); r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024))); }
    r.housekeep();
    assert_eq!(r.oldest(), 4 * 1024);
    assert_eq!(r.stats().held_samples, 6 * 1024);
    r.set_ceiling_blocks(3);
    let h = r.housekeep();
    assert_eq!(h.retired, 3);
    assert_eq!(r.oldest(), 7 * 1024);
}

#[test]
fn guaranteed_window_is_never_offered_even_under_a_tiny_ceiling() {
    let mem = Arc::new(FakeVolatile::default());
    let r = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
    r.set_ceiling_blocks(2); r.set_guaranteed_samples(4 * 1024);
    for b in 0..6u64 { r.housekeep(); r.write(b * 1024, Samples::Ci16(&ramp(b * 1024, 1024))); }
    r.housekeep();
    assert!(mem.offered().is_empty());
    assert!(r.stats().held_samples >= 4 * 1024);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine ring::`
Expected: compile error (`housekeep` undefined).

- [ ] **Step 3: Implement `housekeep`** per Interfaces.

- [ ] **Step 4: Run the ring tests and the stress test again**

Run: `cargo test -p engine ring::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "Ring housekeeping: offer aged blocks to the OS by priority, retire past the ceiling, report lost blocks"
```

### Task 12: Segment table and timeline

**Files:**
- Create: `crates/engine/src/segments.rs`, `crates/engine/src/timeline.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
```rust
// segments.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct SegmentId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq)] pub enum Boundary { Start, Retune, Gap { lost_samples: u64 } }
#[derive(Debug, Clone, PartialEq)] pub struct Segment { pub id: SegmentId, pub from_sample: u64, pub center_hz: f64, pub sample_rate: f64, pub boundary: Boundary }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct SpansSegments { pub first: SegmentId, pub last: SegmentId }
pub struct SegmentTable { inner: parking_lot::Mutex<Vec<Segment>>, next_id: AtomicU64 }
impl SegmentTable {
    pub fn new(from_sample: u64, center_hz: f64, sample_rate: f64) -> Self;          // one Start segment
    pub fn push(&self, from_sample: u64, center_hz: f64, sample_rate: f64, boundary: Boundary) -> SegmentId;  // from_sample >= the last one's
    pub fn at(&self, sample: u64) -> Segment;                                         // the segment containing `sample`
    pub fn covering(&self, start: u64, end: u64) -> Result<Segment, SpansSegments>;  // [start, end) inside one segment
    pub fn prune_before(&self, oldest: u64);                                          // drop segments that end before `oldest`, keep the one containing it
    pub fn len(&self) -> usize; pub fn current(&self) -> Segment;
}
// timeline.rs
pub struct Timeline { anchors: parking_lot::Mutex<VecDeque<(u64, SystemTime)>>, rate_bits: AtomicU64, rate_from: AtomicU64 }
impl Timeline {
    pub const MAX_ANCHORS: usize = 64;
    pub fn new(rate: f64) -> Self;
    pub fn anchor(&self, at_sample: u64, host_time: SystemTime);   // keeps the newest MAX_ANCHORS
    pub fn set_rate(&self, from_sample: u64, rate: f64);           // a Retune with a new rate
    pub fn time_of(&self, sample: u64) -> Option<SystemTime>;      // None before the first anchor
    pub fn prune_before(&self, oldest: u64);                       // keeps one anchor at or before `oldest`
}
```
`time_of`: use the newest anchor with `at_sample <= sample` (else the oldest anchor), and the rate in force between the anchor and `sample` (a rate change at `rate_from` splits the interval in two).

- [ ] **Step 1: Write the failing tests**

```rust
// segments.rs
#[test]
fn covering_refuses_a_range_across_a_retune_and_prune_keeps_the_segment_holding_oldest() {
    let t = SegmentTable::new(0, 855e6, 10e6);
    let s1 = t.push(1_000, 860e6, 10e6, Boundary::Retune);
    let s2 = t.push(5_000, 860e6, 10e6, Boundary::Gap { lost_samples: 300 });
    assert_eq!(t.at(999).center_hz, 855e6);
    assert_eq!(t.at(1_000).id, s1);
    assert_eq!(t.covering(1_000, 5_000).unwrap().id, s1);
    assert_eq!(t.covering(900, 1_100), Err(SpansSegments { first: t.at(900).id, last: s1 }));
    assert_eq!(t.covering(4_000, 6_000), Err(SpansSegments { first: s1, last: s2 }));
    t.prune_before(3_000);
    assert_eq!(t.len(), 2);
    assert_eq!(t.at(3_000).id, s1);
}

// timeline.rs
#[test]
fn time_of_interpolates_from_the_nearest_earlier_anchor() {
    let tl = Timeline::new(10e6);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    tl.anchor(0, t0);
    tl.anchor(10_000_000, t0 + Duration::from_millis(1_003));   // the device runs 0.3% slow
    assert_eq!(tl.time_of(5_000_000), Some(t0 + Duration::from_millis(500)));
    assert_eq!(tl.time_of(15_000_000), Some(t0 + Duration::from_millis(1_503)));
    assert_eq!(Timeline::new(10e6).time_of(5), None);
}

#[test]
fn rate_change_applies_from_its_sample() {
    let tl = Timeline::new(10e6);
    let t0 = SystemTime::UNIX_EPOCH;
    tl.anchor(0, t0);
    tl.set_rate(10_000_000, 2.5e6);                                // 1 s at 10 MS/s, then 2.5 MS/s
    assert_eq!(tl.time_of(12_500_000), Some(t0 + Duration::from_secs(2)));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine segments:: timeline::`
Expected: compile errors.

- [ ] **Step 3: Implement both modules** and `pub mod segments; pub mod timeline;` in `lib.rs`.

- [ ] **Step 4: Run**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "Segment table for retunes and gaps, and a timeline that turns sample indices into wall time from refreshed anchors"
```

### Task 13: The ring budget across sources

**Files:**
- Create: `crates/engine/src/budget.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct SourceId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Mode { Live, Sweep }
#[derive(Debug, Clone)] pub struct BudgetConfig { pub guaranteed_secs: f64, pub ceiling_bytes: u64, pub sweep_secs: f64 }
impl Default for BudgetConfig { /* 30.0, physical_ram_bytes() / 2, 1.0 */ }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct Allowance { pub ceiling_blocks: u32, pub guaranteed_samples: u64 }
pub struct RingBudget { cfg: Mutex<BudgetConfig>, members: Mutex<Vec<Member>> }
impl RingBudget {
    pub fn new(cfg: BudgetConfig) -> Arc<Self>;
    pub fn physical_ram_bytes() -> u64;       // Windows GlobalMemoryStatusEx.ullTotalPhys; Unix sysconf(_SC_PHYS_PAGES) * sysconf(_SC_PAGESIZE); else 8 GiB
    pub fn set_config(&self, cfg: BudgetConfig);
    pub fn join(&self, id: SourceId, rate: f64, mode: Mode, weight: f32, block_bytes: usize);
    pub fn update(&self, id: SourceId, rate: f64, mode: Mode);
    pub fn set_weight(&self, id: SourceId, weight: f32);
    pub fn leave(&self, id: SourceId);
    pub fn allowances(&self) -> Vec<(SourceId, Allowance)>;
}
```
Allowance rule (spec §4.1): a Sweep member gets `guaranteed_samples = sweep_secs * rate` and `ceiling_blocks = max(2, ceil(sweep_secs * rate * 4 / block_bytes) + 1)`. Every Live member first gets its guaranteed window: `guaranteed_samples = guaranteed_secs * rate`, `g_i = ceil(guaranteed_samples * 4 / block_bytes)` blocks. The remainder `R = ceiling_bytes − Σ(sweep ceilings in bytes) − Σ(g_i × block_bytes)`, if positive, is shared so that opportunistic seconds are equal per unit weight: `S = R / Σ_i (weight_i × rate_i × 4)`, and member i gets `floor(weight_i × S × rate_i × 4 / block_bytes)` extra blocks. `ceiling_blocks = g_i + extra + 1` (the +1 is the writer's current block). A weight of 0 means "guaranteed only".

- [ ] **Step 1: Write the failing tests**

```rust
const MB: usize = 1 << 20;

#[test]
fn live_sources_get_equal_seconds_not_equal_bytes() {
    let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 1000 * MB as u64, sweep_secs: 1.0 });
    b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB);     // 40 MB/s
    b.join(SourceId(2), 2.5e6, Mode::Live, 1.0, MB);    // 10 MB/s
    let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
    let secs = |id, rate: f64| a[&id].ceiling_blocks as f64 * MB as f64 / (rate * 4.0);
    assert!((secs(SourceId(1), 10e6) - secs(SourceId(2), 2.5e6)).abs() < 0.2, "{a:?}");   // within one block of each other
    let expect = 1000.0 * 1048576.0 / 50e6;   // 1000 MiB shared at 50 MB/s combined: 20.97 s each
    assert!((secs(SourceId(1), 10e6) - expect).abs() < 0.5);
    assert_eq!(a[&SourceId(1)].guaranteed_samples, 10_000_000);
}

#[test]
fn sweep_sources_hold_one_second_and_do_not_share() {
    let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 1000 * MB as u64, sweep_secs: 1.0 });
    b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB);
    b.join(SourceId(2), 10e6, Mode::Sweep, 1.0, MB);
    let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
    assert_eq!(a[&SourceId(2)].ceiling_blocks, 40);                      // ceil(40e6 B / 1 MiB) = 39, plus the writer's block
    assert!(a[&SourceId(1)].ceiling_blocks >= 950, "{a:?}");             // nearly everything
    b.update(SourceId(2), 10e6, Mode::Live);
    let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
    assert!((a[&SourceId(1)].ceiling_blocks as i64 - a[&SourceId(2)].ceiling_blocks as i64).abs() <= 1);
}

#[test]
fn rate_update_changes_allowance() {
    let b = RingBudget::new(BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 100 * MB as u64, sweep_secs: 1.0 });
    b.join(SourceId(1), 10e6, Mode::Live, 1.0, MB);
    let before = b.allowances()[0].1;
    b.update(SourceId(1), 2.5e6, Mode::Live);
    let after = b.allowances()[0].1;
    assert_eq!(before.guaranteed_samples, 20_000_000);
    assert_eq!(after.guaranteed_samples, 5_000_000);
    assert_eq!(before.ceiling_blocks, after.ceiling_blocks);            // same bytes, more seconds
}

#[test]
fn weight_zero_means_guaranteed_only_and_everyone_gets_at_least_two_blocks() {
    let b = RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 10 * MB as u64, sweep_secs: 1.0 });
    b.join(SourceId(1), 10e6, Mode::Live, 0.0, MB);
    b.join(SourceId(2), 10e6, Mode::Live, 1.0, MB);
    let a: std::collections::HashMap<_, _> = b.allowances().into_iter().collect();
    assert_eq!(a[&SourceId(1)].ceiling_blocks, 40);                      // 39 guaranteed + 1, nothing extra
    assert!(a[&SourceId(2)].ceiling_blocks >= 40);
    assert!(RingBudget::physical_ram_bytes() >= 1 << 30);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine budget::`
Expected: compile errors.

- [ ] **Step 3: Implement `budget.rs`** and export it from `lib.rs`.

- [ ] **Step 4: Run**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "Ring budget: guaranteed window per live source, equal opportunistic seconds, fixed one second for sweep sources"
```

### Task 14: Keeper thread, `Source` and the device sink

**Files:**
- Create: `crates/engine/src/ring/keeper.rs`, `crates/engine/src/source.rs`, `crates/engine/tests/common/mod.rs`, `crates/engine/tests/source_sim.rs`, `crates/engine/tests/budget_two_sources.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
```rust
// ring/keeper.rs
pub trait Housekept: Send + Sync {
    fn id(&self) -> SourceId;
    fn tick(&self, now: Instant, allowance: Allowance);   // apply allowance, ring.housekeep(), prune, watchdog
}
pub struct Keeper { budget: Arc<RingBudget>, members: Mutex<Vec<Weak<dyn Housekept>>>, stop: AtomicBool, paused: AtomicBool, thread: Mutex<Option<JoinHandle<()>>> }
impl Keeper {
    pub const TICK: Duration = Duration::from_millis(20);
    pub fn start(budget: Arc<RingBudget>) -> Arc<Keeper>;    // spawns the thread "ring-keeper"
    pub fn budget(&self) -> &Arc<RingBudget>;
    pub fn register(&self, m: Weak<dyn Housekept>);
    pub fn pause(&self); pub fn resume(&self);                // tests only: a paused keeper skips ticks
    pub fn stop(&self);                                       // joins; also on Drop
}
// source.rs
#[derive(Debug, Clone)] pub struct SourceConfig { pub ring: RingConfig, pub weight: f32 }   // Default: 64 MiB blocks, 1.0
#[derive(Debug, Clone)] pub struct DeviceSnapshot { pub info: DeviceInfo, pub caps: Capabilities, pub center_hz: f64, pub sample_rate: f64 }
#[derive(Debug, Clone, Default)] pub struct SourceStats { pub ring: RingStats, pub held_secs: f64, pub guaranteed_secs: f64, pub ceiling_secs: f64, pub gaps: u64, pub lost_samples: u64, pub overloads: u64, pub device_lost: bool, pub segments: usize }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ReadError { Overwritten, Lost, Future, SpansSegments }   // ring::ReadError plus the segment check
pub struct Source { inner: Arc<SourceInner>, device: Arc<Mutex<Box<dyn Device>>>, id: SourceId }
impl Source {
    pub fn start(device: Box<dyn Device>, cfg: SourceConfig, mode: Mode, keeper: &Arc<Keeper>) -> sdr_core::Result<Source>;
    pub fn id(&self) -> SourceId;
    pub fn mode(&self) -> Mode;
    pub fn set_mode(&self, mode: Mode);                      // budget.update only; the device keeps streaming
    pub fn set_center(&self, hz: f64) -> sdr_core::Result<()>;
    pub fn set_gain(&self, g: GainSetting) -> sdr_core::Result<()>;
    pub fn snapshot(&self) -> DeviceSnapshot;
    pub fn head(&self) -> u64; pub fn oldest(&self) -> u64;
    pub fn segment_at(&self, sample: u64) -> Segment;
    pub fn read_raw(&self, start: u64, out: &mut [[i16; 2]]) -> Result<Segment, ReadError>;   // refuses ranges across segments
    pub fn read(&self, start: u64, out: &mut [Complex32]) -> Result<Segment, ReadError>;
    pub fn time_of(&self, sample: u64) -> Option<SystemTime>;
    pub fn stats(&self) -> SourceStats;
}
impl Drop for Source { /* budget.leave, device.stop */ }
```
- `SourceInner` (private): `ring: Arc<IqRing>`, `segments: SegmentTable`, `timeline: Timeline`, counters, `last_samples_at: AtomicU64` (nanos since a process epoch), `device: Weak<Mutex<Box<dyn Device>>>`, `budget: Arc<RingBudget>`. It implements `StreamSink` through a small `Sink { inner: Arc<SourceInner> }`: `on_samples` → `ring.write` (a non-zero `starved` adds to `lost_samples`, bumps `gaps`, and pushes a `Boundary::Gap { lost_samples: starved }` segment at the new head), stamp `last_samples_at`; `on_event`: `Retune` → `segments.push(.., Boundary::Retune)`, `timeline.set_rate` and `budget.update` if the rate changed; `Gap` → `segments.push(at_sample + lost, .., Boundary::Gap { lost })`, counters; `TimeAnchor` → `timeline.anchor`; `Overload` → counter; `DeviceLost` → flag; `GainChange` → ignored.
- `Housekept::tick`: `ring.set_ceiling_blocks/set_guaranteed_samples` from the allowance, `ring.housekeep()`, `segments.prune_before(ring.oldest())`, `timeline.prune_before(..)`, and the watchdog: if not already lost and no samples for 2 s and `device.try_lock()` succeeds with `is_streaming() == false` → set `device_lost`, log a warning. `try_lock` so the keeper never blocks on a UI retune.
- `SourceId` is allocated from a process-wide `AtomicU32`. `held_secs = ring.held_samples / rate`, `guaranteed_secs = guaranteed_samples / rate`, `ceiling_secs = ceiling_blocks × block_samples / rate`.
- `Source::read_raw`: `segments.covering(start, start + out.len())` first (→ `SpansSegments`), then `ring.read_raw`.
- `tests/common/mod.rs`: `sim(rate, paced) -> (Box<dyn Device>, SimControl)` with no emitters, `cfg() -> SourceConfig` with 1 MiB blocks (262,144 samples), `wait_until(f)` polling every 5 ms with a 20 s deadline that panics past it.

- [ ] **Step 1: Write the failing integration tests**

`crates/engine/tests/source_sim.rs`:
```rust
#[test]
fn streams_into_the_ring_and_reports_seconds() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    // The unpaced sim runs at ~280 MS/s, far beyond any real device, and starves the keeper's provisioning.
    let (d, _c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.stats().held_secs >= 1.0);
    let s = src.stats();
    assert!(s.held_secs <= 64.0 * 1048576.0 / 40e6 + 0.1, "{s:?}");     // never above the ceiling (1.6 s)
    assert!((s.guaranteed_secs - 0.5).abs() < 1e-9);
    assert!(s.ceiling_secs > 1.0);
    assert_eq!(s.gaps, 0);
    let mut out = vec![Complex32::default(); 1000];
    let seg = src.read(src.head() - 2000, &mut out).unwrap();
    assert_eq!(seg.center_hz, 855.25e6);
    assert!(src.time_of(src.head() - 1).is_some());
}

#[test]
fn retune_and_gap_make_segments_and_reads_across_them_are_refused() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() >= 3_000_000);
    let before = src.head();
    src.set_center(860e6).unwrap();
    wait_until(|| src.segment_at(src.head() - 1).center_hz == 860e6);
    let seg = src.segment_at(src.head() - 1);
    assert_eq!(seg.boundary, Boundary::Retune);
    assert!(seg.from_sample >= before && seg.from_sample % 65_536 == 0);
    let mut out = vec![[0i16; 2]; 10];
    assert_eq!(src.read_raw(seg.from_sample - 5, &mut out), Err(ReadError::SpansSegments));
    c.inject_gap(70_000);
    wait_until(|| matches!(src.segment_at(src.head() - 1).boundary, Boundary::Gap { .. }));
    let g = src.segment_at(src.head() - 1);
    assert_eq!(g.boundary, Boundary::Gap { lost_samples: 70_000 });
    assert_eq!(src.stats().gaps, 1);
    assert_eq!(src.stats().lost_samples, 70_000);
    assert_eq!(src.stats().segments, 3);
}

#[test]
fn device_loss_is_flagged_without_a_restart() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig::default()));
    let (d, c) = sim(2.5e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() > 100_000);
    c.inject_overload();
    c.drop_device();
    wait_until(|| src.stats().device_lost);
    assert_eq!(src.stats().overloads, 1);
    assert_eq!(c.starts(), 1);
}
```

`crates/engine/tests/budget_two_sources.rs`:
```rust
#[test]
fn two_live_sources_hold_equal_seconds_and_a_mode_change_redistributes_without_restart() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.25, ceiling_bytes: 100 << 20, sweep_secs: 0.1 }));
    // The unpaced sim runs at ~280 MS/s, far beyond any real device, and starves the keeper's provisioning.
    let (d1, c1) = sim(10e6, true);
    let (d2, c2) = sim(2.5e6, true);
    let a = Source::start(d1, cfg(), Mode::Live, &keeper).unwrap();
    let b = Source::start(d2, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| a.stats().held_secs >= a.stats().ceiling_secs - 0.1 && b.stats().held_secs >= b.stats().ceiling_secs - 0.1);
    let (sa, sb) = (a.stats(), b.stats());
    assert!((sa.ceiling_secs - sb.ceiling_secs).abs() < 0.15, "{sa:?} {sb:?}");   // 80 and 21 blocks: 2.10 s and 2.20 s
    assert!((sa.held_secs - sb.held_secs).abs() < 0.3, "{sa:?} {sb:?}");
    b.set_mode(Mode::Sweep);
    wait_until(|| b.stats().ceiling_secs < 0.3 && a.stats().ceiling_secs > sa.ceiling_secs * 1.15);   // a gains b's share: 99 blocks, 2.60 s
    wait_until(|| a.stats().held_secs >= a.stats().ceiling_secs - 0.1);
    assert!(a.stats().held_secs > sa.held_secs * 1.15);
    assert!(b.stats().held_secs <= 0.3);
    b.set_mode(Mode::Live);
    wait_until(|| (a.stats().ceiling_secs - b.stats().ceiling_secs).abs() < 0.15);
    assert_eq!(c1.starts(), 1); assert_eq!(c2.starts(), 1);
    assert_eq!(a.stats().gaps + b.stats().gaps, 0);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine --test source_sim --test budget_two_sources`
Expected: compile errors.

- [ ] **Step 3: Implement `keeper.rs` and `source.rs`** per Interfaces; export `Keeper`, `Source`, `SourceConfig`, `SourceStats`, `DeviceSnapshot`, `ReadError`, `Mode`, `SourceId`, `RingBudget`, `BudgetConfig`, `Segment`, `Boundary`, `RingConfig`, `RingStats` from `lib.rs`.

- [ ] **Step 4: Run all engine tests, three times for the timed ones**

Run: `cargo test -p engine && for i in 1 2 3; do cargo test -p engine --test budget_two_sources --test source_sim -- --quiet || exit 1; done && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS each time, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "Source and keeper: devices stream into the block ring without a mutex, budgets redistribute on mode change, device loss is detected"
```

### Task 15: Wideband SigMF export through segments

**Files:**
- Create: `crates/engine/src/sigmf.rs`, `crates/engine/src/export.rs`, `crates/engine/tests/export_retune.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
```rust
// sigmf.rs
pub fn rfc3339(t: SystemTime) -> String;                       // "2026-10-23T12:00:00.123456Z"; same output as engine_old::sigmf::rfc3339
pub struct Meta { pub datatype: &'static str, pub sample_rate: f64, pub frequency: f64, pub datetime: Option<String>, pub hw: String, pub description: String, pub sample_count: u64 }
pub fn write_meta(path: &Path, m: &Meta) -> std::io::Result<()>;   // core:version "1.0.0", core:recorder "spectrawl", captures[0] with core:sample_start 0
// export.rs
#[derive(Debug)] pub enum ExportError { Spans(SpansSegments), Ring(ring::ReadError), Io(std::io::Error) }
#[derive(Debug, Clone)] pub struct ExportResult { pub data_path: PathBuf, pub meta_path: PathBuf, pub samples: u64, pub segment: Segment }
impl Source {
    /// Writes [start, end) as ci16_le SigMF named <dir>/<stem>.sigmf-{data,meta}. Checks the segment and the ring range
    /// before creating any file; reads in 262144-sample chunks; on a mid-way ring error deletes the partial files.
    pub fn export_wideband(&self, start: u64, end: u64, dir: &Path, stem: &str) -> Result<ExportResult, ExportError>;
}
```

- [ ] **Step 1: Write the failing tests** (`tests/export_retune.rs`):

```rust
#[test]
fn export_after_a_retune_carries_the_new_centre() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 2.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.head() >= 5_000_000);
    src.set_center(860e6).unwrap();
    wait_until(|| src.segment_at(src.head() - 1).center_hz == 860e6 && src.head() - src.segment_at(src.head() - 1).from_sample > 2_000_000);
    let seg = src.segment_at(src.head() - 1);
    let dir = std::env::temp_dir().join(format!("spectrawl-export-{}", std::process::id()));
    let start = seg.from_sample + 100_000;
    let r = src.export_wideband(start, start + 1_000_000, &dir, "after").unwrap();
    assert_eq!(std::fs::metadata(&r.data_path).unwrap().len(), 4_000_000);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&r.meta_path).unwrap()).unwrap();
    assert_eq!(meta["captures"][0]["core:frequency"], 860e6);
    assert_eq!(meta["global"]["core:datatype"], "ci16_le");
    assert_eq!(meta["global"]["core:sample_rate"], 10e6);
    assert!(meta["captures"][0]["core:datetime"].as_str().unwrap().ends_with('Z'));
    let spanning = src.export_wideband(seg.from_sample - 1_000, seg.from_sample + 1_000, &dir, "spanning");
    assert!(matches!(spanning, Err(ExportError::Spans(_))));
    assert!(!dir.join("spanning.sigmf-data").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn export_errors_before_writing() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 16 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, false);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    wait_until(|| src.oldest() > 1_000_000);
    let dir = std::env::temp_dir().join(format!("spectrawl-export-err-{}", std::process::id()));
    let head = src.head();
    assert!(matches!(src.export_wideband(head - 10, head + 10_000, &dir, "future"), Err(ExportError::Ring(ring::ReadError::Future))));
    assert!(matches!(src.export_wideband(0, 1000, &dir, "gone"), Err(ExportError::Ring(ring::ReadError::Overwritten))));
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p engine --test export_retune`
Expected: compile errors.

- [ ] **Step 3: Implement `sigmf.rs` and `export.rs`** per Interfaces (`rfc3339` may be transcribed from `engine_old::sigmf::rfc3339`; it is the only code taken from the old engine, and the plan records that here).

- [ ] **Step 4: Run the whole workspace exactly as CI does**

Run: `cargo fmt --all --check && cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p engine --all-targets -- -D warnings && cargo test --workspace`
Expected: all green; the old app still builds and its tests pass.

- [ ] **Step 5: Commit, push, confirm CI**

```bash
git add crates/engine
git commit -m "Wideband SigMF export reads through the segment table, so an export after a retune carries the right centre"
git push
```
Then `gh run watch`: both jobs green. Step 1 is done.

---

## Self-review notes

- Spec §6 step 0: CI (Task 6), fail-not-skip (Task 4), committed small captures (Task 5), BACKLOG lines (Task 6). Covered.
- Spec §6 step 1: events from `sdr-core`/backends (Tasks 7, 8, watchdog in 14), block ring with guaranteed and opportunistic windows (Tasks 10, 11), export after retune (Task 15), memory-pressure test (Task 11), equal seconds and Sweep→Live without restart (Task 14). Covered.
- Spec §4.1 "shown in seconds": `SourceStats` carries `held_secs`, `guaranteed_secs`, `ceiling_secs` (Task 14); the health slot that displays them is step 8.
- Spec §4.1 Linux page marker and offer schedule: pinned in Tasks 9 and 11 as builder defaults; spec §9 allows the builder to change them.
- Not in this plan, by the spec's order: the FFT, the channelizer, `api`, the new app, records, the slice tier (§4.8) and the acceptance-capture publishing (its windows are cut after the 2026-10-23 recordings exist and added to `MANIFEST.toml` when step 5 first needs them).
- Type consistency: `Mode` lives in `budget.rs` and is re-exported; `ReadError` exists twice by design (`ring::ReadError` without `SpansSegments`, `source::ReadError` with it) and `ExportError::Ring` carries the ring one; `Allowance` flows keeper → `Housekept::tick` → `IqRing::set_*`; `SimControl::starts()` is what the "no restart" assertions use.
