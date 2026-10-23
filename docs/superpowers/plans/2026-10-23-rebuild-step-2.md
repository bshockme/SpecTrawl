# SpecTrawl Rebuild, Step 2: FFT, Channelizer and Bus Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One shared overlap-save FFT per source feeding spectrum rows and a fast-convolution channelizer, with a `ChannelBus` that runs consumers on a pool, back-fills channels from the ring, measures lag and sheds under saturation; plus the items parked by the step 0–1 reviews.

**Architecture:** `dsp` gets the pure maths (no threads): the shared forward FFT with a 7-tap frequency-domain Blackman-Harris for rows, the per-channel design and extraction (bin gather × filter response → small inverse FFT, with the block-phase and residual-offset corrections that make back-filled and live output identical), and `Ddc`/`Demod` moved over from `detect`. `engine` gets one `Stage` thread per source (reads the ring, runs the FFT, fills a short `SpectrumCache` of recent blocks and a `RowHistory`), a `ChannelBus` whose worker pool computes each subscription's channel from the cache (or recomputes from the ring when back-filling), and a thin `Engine` that composes keeper, bus and sources. Nothing else in `engine` creates a thread, a down-converter or a clock.

**Tech Stack:** Rust 2024 workspace; `rustfft` 6.4 and `num-complex` 0.4 (already in `dsp`); `parking_lot`, `arc-swap`, `log` (already in `engine`); `backend-sim` for every test.

**Spec:** `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` (§4.1–§4.3, §6 step 2, §7, §9, §10). Parked items from the step 0–1 ledger are listed in the Review Focus and in Tasks 1–2.

## Global Constraints

- SpecTrawl stays MIT; no OnAir code is linked or copied (spec §3).
- `detect`, `sdr-core`, `backend-airspy`, `backend-sim`, `catalog` are kept crates: changes are bug fixes or the move of `Ddc`/`Demod` into `dsp` that spec §4.2 names (`detect` keeps a thin dependency on `dsp`).
- "No feature creates a thread, a down-converter or a clock" (spec §7). After this plan the only thread creators in `crates/engine/src` outside test modules are `ring/keeper.rs`, `stage.rs` and `channels/sched.rs`; `Ddc::new` and `SystemTime::now` appear nowhere in `crates/engine/src`. Task 10's rules test pins this.
- One forward FFT per source; the waterfall rows decimate from it, never run their own (spec §4.2).
- Builder choices (spec §9) fixed by this plan: FFT size N = 65536; overlap-save with hop L = N/2 = 32768 (50 % overlap); a block `b` covers input samples `[b·L, (b+1)·L)` and is computed from ring samples `[(b−1)·L, (b+1)·L)`; a channel's inverse FFT size K is the smallest power of two ≥ 64 with `fs·K/N ≥ max(min_rate, 1.6·bandwidth_hz)`, and K ≤ N/4; a channel's output rate is `fs·K/N` (a power-of-two fraction of the input rate, at least the requested minimum); the channel filter is a Blackman-windowed sinc of `K/2 + 1` taps at the output rate with cutoff `bandwidth_hz/2`; the spectrum cache holds 32 blocks; rows are published every 50 ms; the pool has `available_parallelism − 2` workers (at least 1).
- `cargo fmt --all --check` and `cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p engine --all-targets -- -D warnings` stay clean (CI runs them).
- Steps 0–5 are serial on one branch: work on branch `rebuild-step-2` in a worktree under `.worktrees/`, forked from `master` at 130ab59 or later.
- Every commit message ends with the two attribution lines the session carries (`Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and the `Claude-Session:` line).

## Review Focus

Failure modes the spec implies that the step 2 tests must pin (each has its test in the task named):

1. **A retune while a channel is live or back-filling.** The block straddling the segment boundary is delivered as a `Gap`, the channel continues with a design for the new tuning, and no delivery ever mixes two tunings. If the new tuning puts the channel outside the usable span the consumer stops hearing samples and the stats say `out_of_span`. (Task 8.)
2. **A consumer callback that panics.** The pool worker survives, other consumers are unaffected, the panicking consumer is ended with `EndReason::Panicked` and counted in `BusStats`. (Task 7.)
3. **Back-fill from history that is retired mid-way.** A subscription started near `oldest` on a small ring gets `Gap { reason: Unreadable(Overwritten) }` and continues from the next readable block instead of failing block by block. (Task 8.)
4. **A source removed while consumers are live.** Each consumer gets `Delivery::Ended { reason: SourceDetached }` exactly once, the stage thread exits, and later subscribes to that source fail with `BusError::NoSuchSource`. (Task 10.)
5. **The FFT thread falling behind the device.** The stage skips to the live edge, counts the skipped blocks, and keeps publishing rather than reading ever-older samples until they are overwritten one by one. (Task 6.)

Parked from the step 0–1 reviews and placed here: flaky stress-test coverage assertions (Task 1), persistent offer refusals retried every tick (Task 1), silent look-back shrinkage on hole eviction (Task 1), `IqRing::new` visibility, `ExportError` traits, keeper ownership and `Segment::hole` as a hint (Task 2), the sample waker (declined, see Task 6).

---

## File map

| Path | Responsibility |
|---|---|
| `crates/dsp/src/lib.rs` | module list, keeps `blackman_harris`, `SpectrumAnalyzer`, `decimate_max` (used by `app-old`) |
| `crates/dsp/src/fir.rs` | `windowed_sinc` low-pass design shared by `Ddc` and the channelizer |
| `crates/dsp/src/ddc.rs` | moved from `detect` unchanged except the tap design call |
| `crates/dsp/src/demod.rs` | moved from `detect` unchanged |
| `crates/dsp/src/spectrum.rs` | `SharedFft` (unwindowed forward FFT) and `bh_power` (windowed power without a second FFT) |
| `crates/dsp/src/channelizer.rs` | `ChannelDesign`, `design_channel`, `Extractor` |
| `crates/engine/src/stage.rs` | per-source FFT thread, `StageConfig`, `StageStats`, `BlockSink` |
| `crates/engine/src/cache.rs` | `SpectrumCache` of recent forward FFTs |
| `crates/engine/src/rows.rs` | `Row`, `RowHistory` |
| `crates/engine/src/channels/mod.rs` | public bus API: `ChannelBus`, `ChannelSpec`, `Priority`, `Delivery`, `Consumer`, stats, errors |
| `crates/engine/src/channels/sub.rs` | one subscription: design per segment, cache-or-recompute, gap coalescing, delivery |
| `crates/engine/src/channels/sched.rs` | the worker pool and round-robin-by-priority scheduler, saturation and shedding |
| `crates/engine/src/engine.rs` | `Engine`: keeper + bus + sources |
| `crates/engine/tests/{stage_sim,channel_live,channel_backfill,bus_shedding,channels_200,engine_sim,rules}.rs` | integration tests |

---

### Task 1: Ring parked items (stress-test coverage, hole-eviction stat, offer back-off)

**Files:**
- Modify: `crates/engine/src/ring/mod.rs` (stress tests ~860–1100, `record_hole` ~282, `offer_aged` ~555, `RingStats` ~95, `IqRing` fields ~121)
- Modify: `crates/engine/src/source.rs` (`SourceStats` ~57, `stats()` ~386)

**Interfaces:**
- Consumes: `IqRing`, `FakeVolatile`, `VolatileMemory::offer -> bool` as they are.
- Produces: `RingStats::holes_evicted: u64`, `SourceStats::holes_evicted: u64`, `IqRing::offer_backoff_ticks(&self) -> u32` (`pub(crate)`, for the test).

- [ ] **Step 1: Make the two stress tests retry until the race is observed**

Both `concurrent_reader_sees_ramp_or_overwritten` and `concurrent_offers_and_reads_never_copy_offered_memory` end with a coverage assertion (`over > 0`; `mem.offered().len() > 1`) that fails about once in forty runs although the correctness assertions passed. Wrap each test body in a function `fn run_once() -> bool` that returns whether the race was observed, and have the `#[test]` call it up to 8 times:

```rust
#[test]
fn concurrent_reader_sees_ramp_or_overwritten() {
    // Correctness (no torn read) is asserted inside every attempt; the race
    // itself is only likely per attempt, so coverage gets eight tries.
    assert!((0..8).any(|_| concurrent_reader_once()), "no read was overwritten in 8 attempts");
}
```

Same shape for the offers test (`concurrent_offers_once() -> bool` returns `mem.offered().len() > 1`).

- [ ] **Step 2: Run them 20 times each to see no failure**

Run: `cargo test -p engine --lib concurrent_ -- --test-threads=1` repeated 20 times (a shell loop); Expected: 20 × `test result: ok`.

- [ ] **Step 3: Write the failing test for the hole-eviction stat**

In `ring/mod.rs` tests, next to the existing `HOLES + 1` writes test (~line 802):

```rust
#[test]
fn evicting_a_hole_is_counted() {
    let (r, mut w) = IqRing::with_slots(RingConfig { block_bytes: 4096 }, Arc::new(FakeVolatile::default()), 512);
    // HOLES + 2 writes each starting 10 samples past the head make HOLES + 1 holes.
    r.provision(300);
    let mut s = 0u64;
    for _ in 0..=HOLES {
        s += 10;
        w.write(s, Samples::Ci16(&ramp(s, 100)));
        s += 100;
    }
    assert_eq!(r.stats().holes_evicted, 1);
    assert_eq!(r.oldest(), 10, "oldest moved past the evicted hole [0, 10)");
}
```

- [ ] **Step 4: Run it to see it fail** — Run: `cargo test -p engine --lib evicting_a_hole_is_counted`; Expected: FAIL (no field `holes_evicted`).

- [ ] **Step 5: Implement the stat**

`IqRing` gains `holes_evicted: AtomicU64`; `record_hole` increments it (Relaxed) in the `k >= HOLES` branch and logs at `debug` with the evicted range; `RingStats` and `SourceStats` gain `pub holes_evicted: u64` and `stats()` copies it through.

- [ ] **Step 6: Run it to see it pass** — Run: `cargo test -p engine --lib evicting_a_hole_is_counted`; Expected: PASS.

- [ ] **Step 7: Write the failing test for offer back-off**

```rust
/// Refuses every offer and counts the attempts.
struct Refusing(FakeVolatile, AtomicU64);
// impl VolatileMemory: alloc/free/reclaim delegate; offer() increments self.1 and returns false.

#[test]
fn a_refused_offer_backs_off_exponentially_and_an_accepted_one_resets() {
    let mem = Arc::new(Refusing(FakeVolatile::default(), AtomicU64::new(0)));
    let (r, mut w) = IqRing::new(RingConfig { block_bytes: 4096 }, mem.clone());
    r.set_guaranteed_samples(3072);                         // only block 0 is past the window
    r.provision(3);
    w.write(0, Samples::Ci16(&ramp(0, 4096)));           // blocks 0..3 written (1024 samples each)
    r.housekeep();                                          // pass 1: one refused offer attempt
    assert_eq!(mem.1.load(Ordering::Relaxed), 1);
    assert_eq!(r.offer_backoff_ticks(), 1);
    r.housekeep();                                          // pass 2: skipped, backing off
    assert_eq!(mem.1.load(Ordering::Relaxed), 1);
    r.housekeep();                                          // pass 3: retried, refused again, back-off doubles
    assert_eq!(mem.1.load(Ordering::Relaxed), 2);
    assert_eq!(r.offer_backoff_ticks(), 2);
    for _ in 0..20 { r.housekeep(); }                       // retries at passes 6, 11 and 20
    assert_eq!(mem.1.load(Ordering::Relaxed), 5);
    assert_eq!(r.offer_backoff_ticks(), 16);
}
```

Add a second assertion path with `FakeVolatile` (accepting): after one accepting `housekeep`, `offer_backoff_ticks() == 0`.

- [ ] **Step 8: Run it to see it fail** — Expected: FAIL (`offer_backoff_ticks` missing; attempts grow every pass).

- [ ] **Step 9: Implement the back-off**

`IqRing` gains `offer_backoff: AtomicU32` (ticks to skip) and `offer_skip: AtomicU32` (ticks skipped so far). In `offer_aged`: if `offer_skip < offer_backoff`, increment `offer_skip` and return 0 without offering. Otherwise offer as now; if the pass attempted at least one offer and every attempt was refused, set `offer_backoff = min(max(1, 2·offer_backoff), 3000)` (3000 ticks = 60 s) and `offer_skip = 0`; if any offer was accepted, set both to 0. `pub(crate) fn offer_backoff_ticks(&self) -> u32` returns `offer_backoff`. Log the first refusal of a back-off run at `warn`, later ones at `debug`.

- [ ] **Step 10: Run the ring tests** — Run: `cargo test -p engine`; Expected: all pass, including `a_refused_offer_leaves_the_block_live_and_the_next_pass_retries` (it must still see a retry: it calls `housekeep` twice, so adjust it to call `housekeep` three times with a comment, since the second pass is now the back-off skip).

- [ ] **Step 11: Commit** — `git commit -m "Ring: retry stress-test coverage, count evicted holes, back off after refused offers"`.

---

### Task 2: Engine API hygiene from the final review

**Files:**
- Modify: `crates/engine/src/ring/mod.rs` (`IqRing::new` ~163, `RingWriter` ~145)
- Modify: `crates/engine/src/export.rs` (`ExportError` ~14)
- Modify: `crates/engine/src/source.rs` (`Source` ~252, `start` ~261)
- Modify: `crates/engine/src/segments.rs` (`Segment::hole` doc ~36)
- Test: `crates/engine/tests/source_sim.rs`

**Interfaces:**
- Produces: `Source::start(device, cfg, mode, keeper: &Arc<Keeper>) -> Result<Source>` unchanged in signature but the `Source` now holds its own `Arc<Keeper>` and exposes `pub fn keeper(&self) -> &Arc<Keeper>`; `ExportError: Display + std::error::Error`; `IqRing::new` and `IqRing::with_slots` are `pub(crate)`; `RingWriter` stays `pub` (named in `Source`'s sink) but cannot be constructed outside the crate.

- [ ] **Step 1: Write the failing tests** (append to `tests/source_sim.rs`)

```rust
#[test]
fn a_source_keeps_its_keeper_alive() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(2.5e6, true);
    let src = Source::start(d, cfg(), Mode::Live, &keeper).unwrap();
    drop(keeper);                                    // the caller lets go; the source still has one
    wait_until(|| src.stats().held_secs >= 0.5);     // still housekept (blocks keep being provisioned)
    assert_eq!(src.stats().starved_samples, 0);
    assert_eq!(src.keeper().tick_panics(), 0);
}

#[test]
fn export_error_is_a_std_error() {
    fn is_error<E: std::error::Error + Send + Sync + 'static>() {}
    is_error::<ExportError>();
    let e = ExportError::Ring(engine::ring::ReadError::Gap);
    assert_eq!(e.to_string(), "the range touches a gap: samples the device never delivered");
}
```

- [ ] **Step 2: Run them to see them fail** — `cargo test -p engine --test source_sim`; Expected: compile errors (`keeper()` missing; `Display` not implemented).

- [ ] **Step 3: Implement**

- `Source` gains `keeper: Arc<Keeper>` (cloned from the argument) and `pub fn keeper(&self) -> &Arc<Keeper>`.
- `impl Display for ExportError` with these exact messages: `Spans(s)` → `"the range spans segments {first} to {last}"` (the `SegmentId` numbers); `Ring(ReadError::Gap)` → `"the range touches a gap: samples the device never delivered"`; `Ring(Overwritten)` → `"the range is no longer held"`; `Ring(Lost)` → `"the range was discarded by the OS under memory pressure"`; `Ring(Future)` → `"the range has not been written yet"`; `Io(e)` → `"writing the export failed: {e}"`. `impl std::error::Error for ExportError` with `source()` returning the io error.
- `IqRing::new` and `with_slots` become `pub(crate)`; ring unit tests are in-crate and keep compiling.
- `Segment::hole` doc: "A hint for consumers that want to skip a hole in one step. Reads are the authority: a range that intersects a hole is refused by `Source::read` whether or not it is visible here (the ring remembers the newest 64 holes itself)."

- [ ] **Step 4: Run the whole engine suite** — `cargo test -p engine`; Expected: all pass.

- [ ] **Step 5: Commit** — `git commit -m "Engine: Source owns its keeper, ExportError is a std error, IqRing::new is crate-private"`.

---

### Task 3: `dsp` takes over `Ddc` and `Demod` from `detect`

**Files:**
- Create: `crates/dsp/src/fir.rs`
- Move: `crates/detect/src/ddc.rs` → `crates/dsp/src/ddc.rs`; `crates/detect/src/demod.rs` → `crates/dsp/src/demod.rs` (`git mv`, so history follows)
- Modify: `crates/dsp/src/lib.rs` (module list and re-exports), `crates/detect/src/lib.rs:10-12,35` (remove `mod ddc; mod demod;`, add `pub use dsp::Ddc; pub use dsp::demod;`), `crates/detect/Cargo.toml` (add `dsp = { version = "0.1.0", path = "../dsp" }`)

**Interfaces:**
- Produces: `dsp::fir::windowed_sinc(taps: usize, cutoff_hz: f64, fs: f64) -> Vec<f32>` (Blackman window, taps normalised to unit DC gain, `taps` odd); `dsp::Ddc` with the same public API (`Ddc::new(fs, offset_hz, cutoff_hz, target_fs)`, `process`, `fs_out`), `dsp::demod::{Demod, Mode, AUDIO_RATE}` unchanged. `detect::Ddc` and `detect::demod` keep working as re-exports, so `engine-old`, `app-old` and the `detect` examples compile untouched.

- [ ] **Step 1: Write the failing test for `windowed_sinc`** (`crates/dsp/src/fir.rs` tests)

```rust
#[test]
fn windowed_sinc_has_unit_dc_gain_and_rejects_the_stopband() {
    let taps = windowed_sinc(65, 6_250.0, 78_125.0);
    assert_eq!(taps.len(), 65);
    let dc: f32 = taps.iter().sum();
    assert!((dc - 1.0).abs() < 1e-6);
    // Response at 25 kHz (adjacent channel): below −60 dB.
    let resp = |f: f64| -> f32 {
        let w = 2.0 * std::f64::consts::PI * f / 78_125.0;
        taps.iter().enumerate().map(|(i, &t)| t as f64 * (w * i as f64).cos()).sum::<f64>().abs() as f32
    };
    assert!(20.0 * resp(25_000.0).log10() < -60.0);
    assert!((resp(1_000.0) - 1.0).abs() < 0.01);
}
```

- [ ] **Step 2: Run it to see it fail** — `cargo test -p dsp windowed_sinc`; Expected: FAIL (module missing).

- [ ] **Step 3: Implement `windowed_sinc`** — the tap formula now inline in `Ddc::new` (sinc × Blackman, normalised); `Ddc::new` calls it.

- [ ] **Step 4: Move the files and wire the re-exports** — `git mv` both files; `dsp/src/lib.rs` adds `pub mod channelizer;` later (Task 5), now `pub mod ddc; pub mod demod; pub mod fir; pub use ddc::Ddc;`. `detect/src/lib.rs` drops its two modules and re-exports as above. `demod.rs` has no in-crate imports; `ddc.rs` only `num_complex`.

- [ ] **Step 5: Run the workspace tests** — `cargo test --workspace`; Expected: all pass (the moved unit tests run under `dsp`; `detect`'s examples and `engine-old` compile through the re-exports). Then `cargo clippy -p dsp --all-targets -- -D warnings`.

- [ ] **Step 6: Commit** — `git commit -m "dsp takes over Ddc and Demod from detect; windowed-sinc design shared"`.

---

### Task 4: `dsp::spectrum`: the shared forward FFT and windowed power from it

**Files:**
- Create: `crates/dsp/src/spectrum.rs`
- Modify: `crates/dsp/src/lib.rs` (`pub mod spectrum;`)

**Interfaces:**
- Produces:
  - `pub struct SharedFft` with `pub fn new(n: usize) -> SharedFft`, `pub fn size(&self) -> usize`, `pub fn forward(&mut self, input: &[Complex32], bins: &mut [Complex32])` (both exactly `n` long; `bins` in natural FFT order, DC at index 0, unwindowed, unnormalised).
  - `pub fn bh_power(bins: &[Complex32], out: &mut [f32])`: linear power of the periodic 4-term Blackman-Harris-windowed spectrum, DC-centred (negative frequencies first, like `SpectrumAnalyzer::take_power`), normalised so a full-scale complex sinusoid reads 1.0.
  - `pub const BH: [f64; 4] = [0.35875, 0.48829, 0.14128, 0.01168]` (the coefficients `blackman_harris` already uses).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn bh_power_matches_a_windowed_fft_bin_for_bin() {
    let n = 4096;
    let mut rng = 0x1234_5678_9abc_def0u64;
    let noise = |rng: &mut u64| { *rng ^= *rng << 13; *rng ^= *rng >> 7; *rng ^= *rng << 17; (*rng >> 11) as f32 / (1u64 << 53) as f32 - 0.5 };
    let input: Vec<Complex32> = (0..n).map(|i| {
        let t = i as f32 / n as f32;
        Complex32::from_polar(0.5, 2.0 * std::f32::consts::PI * 100.0 * t)
            + Complex32::from_polar(0.1, 2.0 * std::f32::consts::PI * (-700.3) * t)
            + Complex32::new(noise(&mut rng), noise(&mut rng)) * 1e-3
    }).collect();
    let mut reference = SpectrumAnalyzer::new(n);
    reference.push(&input);
    let mut want = Vec::new();
    reference.take_power(&mut want);
    let mut fft = SharedFft::new(n);
    let mut bins = vec![Complex32::default(); n];
    fft.forward(&input, &mut bins);
    let mut got = vec![0.0f32; n];
    bh_power(&bins, &mut got);
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        if *w > 1e-12 { assert!(((g - w) / w).abs() < 1e-3, "bin {i}: {g} vs {w}"); }
    }
    assert!((got[n / 2 + 100] - 0.25).abs() < 0.01, "0.5 amplitude tone reads 0.25 power");
}
```

- [ ] **Step 2: Run it to see it fail** — `cargo test -p dsp bh_power`; Expected: FAIL (module missing).

- [ ] **Step 3: Implement**

`SharedFft` plans a forward FFT once (`FftPlanner`), copies `input` into `bins` and runs `process_with_scratch`. `bh_power` uses the identity for the periodic window `w[n] = a0 − a1·cos(2πn/N) + a2·cos(4πn/N) − a3·cos(6πn/N)`: multiplying by `cos(2πmn/N)` shifts the spectrum by ±m bins, so

```text
Xw[k] = a0·X[k] − (a1/2)·(X[k−1] + X[k+1]) + (a2/2)·(X[k−2] + X[k+2]) − (a3/2)·(X[k−3] + X[k+3])   (indices mod N)
out[i] = |Xw[(i + N/2) mod N]|² / (N·a0)²
```

(`N·a0` is the window's coherent gain, the sum of `blackman_harris(N)`.)

- [ ] **Step 4: Run it to see it pass** — Expected: PASS. Also `cargo clippy -p dsp --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit** — `git commit -m "dsp: shared forward FFT and Blackman-Harris power from the same bins"`.

---

### Task 5: `dsp::channelizer`: channel design and extraction

**Files:**
- Create: `crates/dsp/src/channelizer.rs`
- Modify: `crates/dsp/src/lib.rs` (`pub mod channelizer;`)

**Interfaces:**
- Produces:

```rust
pub const FFT_SIZE: usize = 65_536;            // N; the engine uses this
pub const HOP: usize = FFT_SIZE / 2;            // L
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignError { TooWide, OutsideSpan, BadSpec }
#[derive(Debug, Clone)]
pub struct ChannelDesign {
    pub n: usize, pub k: usize,
    pub center_bin: i64,        // c = round(offset_hz / bin_hz)
    pub residual_hz: f64,       // offset_hz − c·bin_hz
    pub out_rate: f64,          // fs·K/N
    pub out_per_block: usize,   // K/2
    pub response: Vec<Complex32>, // H: K-point FFT of the zero-padded taps
}
pub fn design_channel(n: usize, fs: f64, usable_fraction: f64, offset_hz: f64, bandwidth_hz: f64, min_rate: f64) -> Result<ChannelDesign, DesignError>;
pub struct Extractor { /* design + inverse FFT plan + scratch */ }
impl Extractor {
    pub fn new(design: ChannelDesign) -> Extractor;
    pub fn design(&self) -> &ChannelDesign;
    /// `bins`: the N-point forward FFT (natural order) of input samples `n0 .. n0 + N`;
    /// writes the K/2 output samples for input positions `n0 + L .. n0 + N` into `out`.
    pub fn extract(&mut self, bins: &[Complex32], n0: u64, out: &mut [Complex32]);
}
```

Rules `design_channel` enforces: `BadSpec` if `bandwidth_hz <= 0`, `min_rate <= 0`, or `n` is not a power of two; K = smallest power of two ≥ 64 with `fs·K/n ≥ max(min_rate, 1.6·bandwidth_hz)`; `TooWide` if that K > n/4; `OutsideSpan` if `|offset_hz| + out_rate/2 > usable_fraction·fs/2`. Taps: `fir::windowed_sinc(K/2 + 1, bandwidth_hz / 2, out_rate)`, zero-padded to K, forward-FFT'd (unnormalised) → `response`.

`extract` (fast convolution, overlap-save in the decimated domain):

```text
for j in 0..K:  m = if j < K/2 { j } else { j − K }
                Y[j] = X[(c + m) mod N] · H[j]
y = IFFT_K(Y) / N                       (rustfft's inverse is unnormalised; 1/N gives unit gain)
keep y[K/2 .. K]  (the first K/2 are the circular wrap, discarded)
rot = exp(−i·2π·((c mod N)·(n0 mod N) mod N)/N)     (the mixer's phase at the block start; with L = N/2 this is ±1)
for i in 0..K/2:  out[i] = y[K/2 + i] · rot · exp(−i·2π·residual_hz·(n0 + L + D·i)/fs)    with D = N/K
```

The last factor is computed from the absolute input position, so output is identical whether a block is produced live or back-filled, and consecutive blocks join without a phase step.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn design_picks_the_rate_and_refuses_bad_specs() {
    let d = design_channel(65_536, 10e6, 0.9, 1.0e6, 12_500.0, 48_000.0).unwrap();
    assert_eq!((d.k, d.out_per_block), (512, 256));
    assert_eq!(d.out_rate, 78_125.0);
    assert_eq!(d.center_bin, 6554);                     // 1.0e6 / 152.587890625 = 6553.6
    assert!((d.residual_hz - (1.0e6 - 6554.0 * 10e6 / 65_536.0)).abs() < 1e-6);
    let narrow = design_channel(65_536, 10e6, 0.9, -2.0e6, 6_250.0, 8_000.0).unwrap();
    assert_eq!((narrow.k, narrow.out_rate), (128, 19_531.25));
    assert_eq!(design_channel(65_536, 10e6, 0.9, 0.0, 3.0e6, 48e3), Err(DesignError::TooWide));
    assert_eq!(design_channel(65_536, 10e6, 0.9, 4.49e6, 12_500.0, 48e3), Err(DesignError::OutsideSpan));
    assert_eq!(design_channel(65_536, 10e6, 0.9, 0.0, 0.0, 48e3), Err(DesignError::BadSpec));
}

/// Twelve consecutive hops of a tone exactly on the channel centre, plus a
/// neighbour 25 kHz up at the same level: the output is a steady DC with no
/// step at block joins and the neighbour gone.
fn steady_output_for(center_bin: i64) {
    let (n, fs) = (65_536usize, 10e6);
    let bin = fs / n as f64;
    let offset = center_bin as f64 * bin + 50.0;         // residual 50 Hz
    let d = design_channel(n, fs, 0.9, offset, 12_500.0, 48_000.0).unwrap();
    assert_eq!(d.center_bin, center_bin);
    let l = n / 2;
    let tone = |f: f64, i: u64| Complex32::from_polar(0.5, (2.0 * std::f64::consts::PI * f * i as f64 / fs) as f32);
    let input: Vec<Complex32> = (0..(14 * l) as u64).map(|i| tone(offset, i) + tone(offset + 25e3, i)).collect();
    let mut fft = SharedFft::new(n);
    let mut bins = vec![Complex32::default(); n];
    let mut ex = Extractor::new(d.clone());
    let mut out = Vec::new();
    for b in 1..13u64 {
        let n0 = (b - 1) * l as u64;
        fft.forward(&input[n0 as usize..n0 as usize + n], &mut bins);
        let mut block = vec![Complex32::default(); d.out_per_block];
        ex.extract(&bins, n0, &mut block);
        if b >= 3 { out.extend(block); }            // skip filter settling
    }
    let mean = out.iter().sum::<Complex32>() / out.len() as f32;
    assert!((mean.norm() - 0.5).abs() < 0.005, "gain {}", mean.norm());
    let ripple = out.iter().map(|y| (y - mean).norm()).fold(0.0f32, f32::max);
    assert!(ripple < 0.01, "ripple {ripple}: a block-phase step or a leaking neighbour");
}

#[test] fn even_centre_bin_is_steady_across_blocks() { steady_output_for(6554); }
#[test] fn odd_centre_bin_is_steady_across_blocks() { steady_output_for(6555); }
#[test] fn negative_odd_centre_bin_is_steady_across_blocks() { steady_output_for(-13_107); }
```

- [ ] **Step 2: Run them to see them fail** — `cargo test -p dsp channelizer`; Expected: FAIL (module missing).

- [ ] **Step 3: Implement `design_channel`, `Extractor::new`, `Extractor::extract`** per the formulas above. Allocate the K-point inverse FFT plan and scratch in `new`; `extract` allocates nothing.

- [ ] **Step 4: Run them to see them pass** — Expected: PASS for all four. If the odd-bin test fails with a ripple near 1.0 while the even one passes, the `rot` factor is missing or has the wrong sign.

- [ ] **Step 5: Run clippy and fmt** — `cargo clippy -p dsp --all-targets -- -D warnings && cargo fmt --all --check`.

- [ ] **Step 6: Commit** — `git commit -m "dsp: fast-convolution channelizer with block-phase and residual corrections"`.

---

### Task 6: `engine::stage`: the per-source FFT thread, spectrum cache and rows

**Files:**
- Create: `crates/engine/src/stage.rs`, `crates/engine/src/cache.rs`, `crates/engine/src/rows.rs`
- Modify: `crates/engine/src/lib.rs` (modules and re-exports), `crates/engine/Cargo.toml` (add `dsp = { version = "0.1.0", path = "../dsp" }`)
- Test: `crates/engine/tests/stage_sim.rs`

**Interfaces:**
- Consumes: `Source::{read, head, oldest, segment_at, id}`, `dsp::spectrum::{SharedFft, bh_power}`, `dsp::channelizer::{FFT_SIZE, HOP}`.
- Produces:

```rust
pub struct StageConfig { pub fft_size: usize /* 65536 */, pub cache_blocks: usize /* 32 */, pub row_period_secs: f64 /* 0.05 */, pub row_history: usize /* 64 */ }   // Default = those values
pub trait BlockSink: Send + Sync { fn on_block(&self, source: SourceId, block: u64); }
pub struct Stage;
impl Stage {
    /// Starts thread "fft-<source id>". `sink.on_block` is called after each block is cached.
    pub fn start(source: Arc<Source>, cfg: StageConfig, sink: Arc<dyn BlockSink>) -> Arc<Stage>;
    pub fn source(&self) -> &Arc<Source>;
    pub fn cache(&self) -> &Arc<SpectrumCache>;
    pub fn rows(&self) -> &Arc<RowHistory>;
    pub fn hop(&self) -> u64;                       // L
    pub fn fft_size(&self) -> usize;
    pub fn latest_block(&self) -> Option<u64>;
    pub fn stats(&self) -> StageStats;
    pub fn stop(&self);                             // sets the flag and joins; Drop calls it
}
#[derive(Debug, Clone, Default)]
pub struct StageStats { pub blocks: u64, pub skipped_blocks: u64, pub block_micros: f64 /* EMA, α = 0.05, of FFT + rows time */, pub latest_block: Option<u64> }
pub struct SpectrumCache;   // cache.rs
impl SpectrumCache {
    pub fn new(depth: usize) -> SpectrumCache;
    pub fn get(&self, block: u64) -> Option<(Segment, Arc<[Complex32]>)>;
    pub fn latest(&self) -> Option<u64>;
    pub(crate) fn insert(&self, block: u64, segment: Segment, bins: Arc<[Complex32]>);   // evicts the oldest past `depth`
}
pub struct Row { pub first_sample: u64, pub end_sample: u64, pub blocks: u32, pub segment: Segment, pub power: Vec<f32> /* DC-centred linear, 1.0 = 0 dBFS */ }
pub struct RowHistory;      // rows.rs
impl RowHistory {
    pub fn new(depth: usize) -> RowHistory;
    pub fn latest(&self) -> Option<Arc<Row>>;
    pub fn since(&self, end_sample_after: u64) -> Vec<Arc<Row>>;   // rows with end_sample > the argument, oldest first
    pub fn len(&self) -> usize;
    pub(crate) fn push(&self, row: Row);
}
```

Stage loop: `b` starts at `max(1, oldest / L + 1)`. Each iteration: if stopped → exit; if `head < (b+1)·L` → `park_timeout(1 ms)` and retry; else `source.read((b−1)·L, buf[0..N])`: `Ok(seg)` → forward FFT into a new `Arc<[Complex32]>`, `cache.insert(b, seg, bins)`, accumulate `bh_power` into the row in progress (publish the partial row first if `seg.id` differs from the row's), publish the row when it holds `ceil(row_period·fs/L)` blocks, `sink.on_block(id, b)`, `b += 1`; `Err(Future)` → park 1 ms; `Err(Overwritten | Lost)` → `b = oldest / L + 2`, `skipped_blocks += 1`; `Err(Gap | SpansSegments)` → `b += 1`, `skipped_blocks += 1`. Rows are averaged linear power over their blocks. The sample waker recommended by the step 1 review is not built: the stage is the only ring poller, 1 ms parking costs nothing measurable at 3.3 ms per block, and the write path stays lock-free.

- [ ] **Step 1: Write the failing tests** (`tests/stage_sim.rs`, using `tests/common/mod.rs` helpers plus a `cw_sim(rate, freq_hz, level_dbfs)` helper that builds a `SimConfig` with one `Cw` emitter, `paced: true`, `report_lag_gaps: false`)

```rust
struct Count(AtomicU64);
impl BlockSink for Count { fn on_block(&self, _: SourceId, _: u64) { self.0.fetch_add(1, Ordering::Relaxed); } }

#[test]
fn rows_show_the_carrier_in_the_right_bin_and_blocks_reach_the_sink() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }));
    let (d, _c) = cw_sim(10e6, 855.0e6, -30.0);                      // centre 855.25 MHz: offset −250 kHz
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let sink = Arc::new(Count(AtomicU64::new(0)));
    let stage = Stage::start(src.clone(), StageConfig::default(), sink.clone());
    wait_until(|| stage.rows().len() >= 10);
    let row = stage.rows().latest().unwrap();
    assert_eq!(row.power.len(), 65_536);
    let peak = row.power.iter().enumerate().fold((0, f32::MIN), |m, (i, &p)| if p > m.1 { (i, p) } else { m }).0;
    let expected = 65_536 / 2 - 1638;                                  // −250e3 / 152.5879 = −1638.4
    assert!((peak as i64 - expected as i64).abs() <= 2, "peak at {peak}, expected {expected}");
    let mut sorted = row.power.clone(); sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    assert!(row.power[peak] > median * 1e2, "carrier not 20 dB above the floor");
    assert!(row.blocks >= 15 && row.blocks <= 16, "{}", row.blocks);   // 50 ms at 305 blocks/s
    let s = stage.stats();
    assert_eq!(sink.0.load(Ordering::Relaxed), s.blocks);
    assert_eq!(s.skipped_blocks, 0);
    assert!(stage.cache().get(s.latest_block.unwrap()).is_some());
    assert!(stage.cache().get(s.latest_block.unwrap() - 40).is_none(), "cache deeper than configured");
    assert!(s.block_micros > 0.0 && s.block_micros < 3000.0, "{} µs per block", s.block_micros);
    stage.stop();
}

#[test]
fn a_stage_that_falls_behind_skips_to_the_live_edge() {
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 0.2, ceiling_bytes: 64 << 20, sweep_secs: 0.2 }));
    let (d, _c) = sim(10e6, false);                                   // unpaced: ~280 MS/s, far faster than the FFT
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let stage = Stage::start(src.clone(), StageConfig::default(), Arc::new(Count(AtomicU64::new(0))));
    wait_until(|| stage.stats().skipped_blocks > 0 && stage.stats().blocks > 50);
    let a = stage.stats().latest_block.unwrap();
    wait_until(|| stage.stats().latest_block.unwrap() > a + 20);        // still publishing after skipping
    stage.stop();
}

#[test]
fn a_row_never_spans_a_retune() {
    // start a stage, wait for rows, src.set_center(860e6), wait for a row whose segment.center_hz == 860e6,
    // then assert every row in rows().since(0) has first_sample and end_sample inside one segment
    // (src.segment_at(first_sample).id == src.segment_at(end_sample - 1).id).
}
```

- [ ] **Step 2: Run them to see them fail** — `cargo test -p engine --test stage_sim`; Expected: compile errors (modules missing).

- [ ] **Step 3: Implement `cache.rs`, `rows.rs`, `stage.rs`**; export `Stage, StageConfig, StageStats, BlockSink, SpectrumCache, Row, RowHistory` from `lib.rs`.

- [ ] **Step 4: Run them to see them pass** — Expected: PASS (three tests). Then `cargo clippy -p engine --all-targets -- -D warnings`.

- [ ] **Step 5: Commit** — `git commit -m "Engine: per-source FFT stage with a spectrum cache and waterfall rows"`.

---

### Task 7: `engine::channels`: the bus, live delivery, scheduling and lag

**Files:**
- Create: `crates/engine/src/channels/mod.rs`, `crates/engine/src/channels/sub.rs`, `crates/engine/src/channels/sched.rs`
- Modify: `crates/engine/src/lib.rs`
- Test: `crates/engine/tests/channel_live.rs`

**Interfaces:**
- Consumes: `Stage`, `SpectrumCache`, `dsp::channelizer::{design_channel, Extractor, DesignError}`, `Source`.
- Produces (all `pub`, re-exported from `engine`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct ChannelId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority { Recorder, PagerMonitor, Classify, EmitterDecoder, TrunkControl, Listen }   // lowest first; spec §4.3 order reversed
#[derive(Debug, Clone)]
pub struct ChannelSpec { pub source: SourceId, pub center_hz: f64, pub bandwidth_hz: f64, pub min_rate: f64, pub parent: Option<ChannelId> }
#[derive(Debug, Clone, Copy)] pub enum Start { Live, At(u64) }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum GapReason { Unreadable(ReadError), OutOfSpan }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum EndReason { SourceDetached, RateChanged, Panicked }
pub enum Delivery<'a> {
    Samples { block: u64, first_sample: u64, samples: &'a [Complex32] },   // first_sample = block·L, input domain
    Gap { from_sample: u64, to_sample: u64, reason: GapReason },           // coalesced; always precedes the next Samples
    Shed,
    Ended { reason: EndReason },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Flow { Continue, Stop }
pub trait Consumer: Send + 'static { fn deliver(&mut self, d: Delivery<'_>) -> Flow; }
impl<F: FnMut(Delivery<'_>) -> Flow + Send + 'static> Consumer for F { ... }
#[derive(Debug, Clone)]
pub struct ChannelInfo { pub id: ChannelId, pub source: SourceId, pub center_hz: f64, pub bandwidth_hz: f64, pub sample_rate: f64, pub samples_per_block: usize, pub hop: u64 }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ConsumerState { Running, OutOfSpan, Shed, Ended }
#[derive(Debug, Clone)]
pub struct ConsumerStats { pub channel: ChannelInfo, pub priority: Priority, pub next_block: u64, pub lag_blocks: u64, pub lag_secs: f64, pub blocks: u64, pub recomputed: u64, pub gaps: u64, pub state: ConsumerState }
#[derive(Debug, Clone)]
pub struct BusStats { pub workers: usize, pub saturated: bool, pub shed_total: u64, pub panicked_total: u64, pub consumers: Vec<ConsumerStats> }
#[derive(Debug, Clone, PartialEq)]
pub enum BusError { NoSuchSource, NestedChannelsNotSupported, Design(DesignError), NotHeld { oldest: u64 } }
#[derive(Debug, Clone)]
pub struct BusConfig { pub workers: usize /* available_parallelism − 2, min 1 */, pub max_lag_secs: f64 /* 2.0 */, pub stage: StageConfig }   // impl Default with those values
pub struct ChannelBus;
impl ChannelBus {
    pub fn new(cfg: BusConfig) -> Arc<ChannelBus>;          // starts threads "bus-worker-<i>"
    /// Starts the source's stage with a `BusSink(Weak<ChannelBus>)` as its BlockSink (a Weak, so bus and stage do not keep each other alive).
    pub fn attach(self: &Arc<Self>, source: Arc<Source>) -> Arc<Stage>;
    pub fn detach(&self, source: SourceId) -> bool;         // Task 10
    pub fn stage(&self, source: SourceId) -> Option<Arc<Stage>>;
    pub fn subscribe(&self, spec: ChannelSpec, start: Start, priority: Priority, consumer: Box<dyn Consumer>) -> Result<ChannelInfo, BusError>;
    pub fn unsubscribe(&self, id: ChannelId) -> bool;       // the consumer is dropped by the pool after any delivery in progress; no callback
    pub fn stats(&self) -> BusStats;
    pub fn stop(&self);                                     // stops stages and workers; Drop calls it
}
```

Semantics:
- `subscribe`: `parent: Some(_)` → `NestedChannelsNotSupported` (field kept so the API does not change when the second stage arrives). The design is made for the source's current segment (`design_channel(N, seg.sample_rate, caps.usable_bandwidth_fraction, center_hz − seg.center_hz, bandwidth_hz, min_rate)`); `OutsideSpan` at subscribe time is an error, later it is a gap. `Start::Live` → `next_block = latest_block + 1` (or 1). `Start::At(s)` → `next_block = max(1, s / L)`; if `(next_block − 1)·L < source.oldest()` → `NotHeld { oldest }`.
- Scheduler (`sched.rs`): a `Mutex<Sched>` + `Condvar`; `Sched` holds the subscriptions, one `VecDeque<ChannelId>` per priority, `latest_block` per source, and the busy counters. `on_block(source, b)` marks every idle subscription of that source with `next_block ≤ b` ready and notifies. A worker pops from the highest non-empty queue, marks the subscription running, releases the lock, runs **one block** (`sub.run_block`), then re-queues it at the back of its queue if still behind, else marks it idle. One block per turn is the round-robin.
- `run_block(b)` (`sub.rs`): bins from `cache.get(b)`; on a miss, read `(b−1)·L .. (b+1)·L` from the ring with the subscription's own lazily created `SharedFft` (dropped again on the next cache hit) and count `recomputed`. A ring error extends the pending gap by `[b·L, (b+1)·L)` with `Unreadable(e)`; on `Overwritten | Lost` also jump `next_block = max(next_block + 1, oldest / L + 2)`. The segment from the cache or the read selects the extractor: redesigned when `segment.id` changes; `sample_rate` differing from `ChannelInfo.sample_rate` → `Ended { RateChanged }`; `OutsideSpan` → pending gap `OutOfSpan` and state `OutOfSpan`. Before any `Samples` the pending gap (if any) is delivered as one `Gap`. `Flow::Stop` ends the subscription silently. A panic inside `deliver` is caught (`catch_unwind`), the subscription is ended with `Ended { Panicked }` (delivered on the next turn if the consumer can still be called, otherwise just dropped), `panicked_total += 1`, and the worker continues.
- Lag: `lag_blocks = latest_block + 1 − next_block` (0 when caught up), `lag_secs = lag_blocks·L / sample_rate` of the source's current segment.

- [ ] **Step 1: Write the failing tests** (`tests/channel_live.rs`). Shared helpers go in `tests/common/mod.rs` for Tasks 7–10: `keeper()` (a keeper with `guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2`), `cw_sim` (moved from Task 6's test), `two_tone_sim(rate, f1, f2, level_dbfs)`, and `Collector`: a cloneable `Consumer` over `Arc<Mutex<..>>` that stores samples (or only counts them, `counting_only()`), counts deliveries by kind (`samples()`, `gaps()`, `shed_deliveries()`, `end_deliveries()`, `ended() -> Option<EndReason>`, `first_delivery_first_sample()`, `first_samples_are_consecutive_by(hop)`, `tail(n)`), returns `Flow::Stop` once `with_limit(n)` is reached, and can sleep per delivery (`sleeping(d)`).

```rust
const TONE_OFFSET: f64 = 6555.0 * (10e6 / 65_536.0) + 50.0;   // odd centre bin, 50 Hz residual

#[test]
fn a_live_channel_delivers_the_tone_at_dc_in_order_with_small_lag() {
    let (d, _c) = cw_sim(10e6, 855.25e6 + TONE_OFFSET, -20.0);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 2, ..Default::default() });
    bus.attach(src.clone());
    let got = Collector::new();
    let info = bus.subscribe(ChannelSpec { source: src.id(), center_hz: 855.25e6 + TONE_OFFSET, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None }, Start::Live, Priority::Listen, Box::new(got.clone())).unwrap();
    assert_eq!((info.sample_rate, info.samples_per_block, info.hop), (78_125.0, 256, 32_768));
    wait_until(|| got.samples() >= 2 * 78_125);
    let s = bus.stats().consumers.into_iter().find(|c| c.channel.id == info.id).unwrap();
    assert_eq!(s.gaps, 0);
    assert!(s.lag_secs < 0.2, "{s:?}");
    assert_eq!(s.state, ConsumerState::Running);
    assert!(got.first_samples_are_consecutive_by(32_768));
    let tail = got.tail(78_125 / 2);                               // skip settling
    let mean = tail.iter().sum::<Complex32>() / tail.len() as f32;
    assert!((20.0 * mean.norm().log10() - (-20.0)).abs() < 0.5, "level {} dBFS", 20.0 * mean.norm().log10());
    assert!(tail.iter().map(|y| (y - mean).norm()).fold(0.0f32, f32::max) < 0.02 * mean.norm());
    assert!(bus.unsubscribe(info.id));
    let n = got.samples();
    std::thread::sleep(Duration::from_millis(200));
    assert!(got.samples() <= n + 256, "deliveries after unsubscribe");
}

#[test]
fn bad_subscriptions_are_refused() {
    // NestedChannelsNotSupported for parent: Some(ChannelId(1));
    // Design(OutsideSpan) for center 855.25e6 + 4.6e6; Design(TooWide) for bandwidth 3e6;
    // NotHeld: keeper with ceiling_bytes 64 << 20 (1.6 s at 10 MS/s), wait_until src.oldest() > 2·32768
    // (about 2 s), then Start::At(0) → Err(NotHeld { oldest }) with oldest == src.oldest() at that moment or later;
    // NoSuchSource for SourceId(99_999).
}

#[test]
fn a_panicking_consumer_is_ended_and_the_others_continue() {
    // two subscriptions on one source: one whose deliver panics on its 3rd Samples, one Collector.
    // wait_until the panicking one's state == Ended and bus.stats().panicked_total == 1;
    // the Collector keeps receiving (samples grow by ≥ 78_125 afterwards) and no worker died
    // (bus.stats().workers == 2 and lag stays < 0.5 s).
}

#[test]
fn consumers_are_scheduled_by_priority_then_round_robin() {
    // workers: 1; three subscriptions whose deliver records (ChannelId, Instant) into a shared log:
    // A and B at Priority::Classify, C at Priority::Listen. After 1 s: C's deliveries never wait behind
    // A or B for more than one block (C's max inter-delivery gap < 3·3.3 ms); A and B have delivery
    // counts within 10 % of each other.
}
```

- [ ] **Step 2: Run them to see them fail** — `cargo test -p engine --test channel_live`; Expected: compile errors.

- [ ] **Step 3: Implement `channels/mod.rs`, `channels/sub.rs`, `channels/sched.rs`** (ChannelBus implements `BlockSink` via `on_block`; `attach` calls `Stage::start(source, cfg.stage.clone(), self.clone())`). `detach` and shedding are stubs that return `false` / do nothing until Tasks 9–10.

- [ ] **Step 4: Run them to see them pass** — Expected: PASS (four tests). Then clippy.

- [ ] **Step 5: Commit** — `git commit -m "Engine: ChannelBus with a priority round-robin pool and lag per consumer"`.

---

### Task 8: Back-fill, gaps, retunes and out-of-span

**Files:**
- Modify: `crates/engine/src/channels/sub.rs`
- Test: `crates/engine/tests/channel_backfill.rs`

**Interfaces:**
- Consumes: Task 7's API; `dsp::Ddc` as the reference.
- Produces: no new API; `Start::At` and the gap paths now fully behave as Task 7 specifies.

- [ ] **Step 1: Write the failing tests**

```rust
/// Spec §6 step 2: a channel back-filled from 5 s ago matches a direct DDC to within filter tolerance.
#[test]
fn a_channel_backfilled_from_five_seconds_ago_matches_a_direct_ddc() {
    let fs = 10e6;
    let tone = 855.25e6 + TONE_OFFSET;
    let (d, _c) = two_tone_sim(fs, tone, tone + 25e3, -20.0);        // the neighbour is 25 kHz up
    let keeper = Keeper::start(RingBudget::new(BudgetConfig { guaranteed_secs: 8.0, ceiling_bytes: 512 << 20, sweep_secs: 0.2 }));
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 2, ..Default::default() });
    bus.attach(src.clone());
    wait_until(|| src.head() >= 60_000_000);                         // 6 s of history
    let start = ((src.head() - 50_000_000) / 32_768) * 32_768;        // 5 s ago, on a hop
    let want = 78_125usize;                                            // 1 s of channel output
    // Reference: the old-style DDC straight off the ring (same rate: 10e6 / 128 = 78125).
    let mut raw = vec![Complex32::default(); 1_200_000];
    src.read(start - 32_768, &mut raw).unwrap();
    let mut ddc = Ddc::new(fs, TONE_OFFSET, 6_250.0, 78_125.0);
    assert_eq!(ddc.fs_out, 78_125.0);
    let mut reference = Vec::new();
    ddc.process(&raw, &mut reference);
    // Back-filled channel.
    let got = Collector::with_limit(want + 4096);                     // returns Flow::Stop when full
    bus.subscribe(ChannelSpec { source: src.id(), center_hz: tone, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None }, Start::At(start), Priority::Listen, Box::new(got.clone())).unwrap();
    wait_until(|| got.samples() >= want);
    assert_eq!(got.gaps(), 0);
    assert!(got.first_delivery_first_sample() == start);
    let (a, b) = (got.tail(want / 2), reference[reference.len() / 2..reference.len() / 2 + want / 2].to_vec());
    let power = |x: &[Complex32]| 10.0 * (x.iter().map(|v| v.norm_sqr()).sum::<f32>() / x.len() as f32).log10();
    assert!((power(&a) - power(&b)).abs() < 0.5, "power {} vs {} dB", power(&a), power(&b));
    let freq = |x: &[Complex32]| x.windows(2).map(|w| (w[1] * w[0].conj()).arg()).sum::<f32>() / (x.len() - 1) as f32 * 78_125.0 / (2.0 * std::f32::consts::PI);
    assert!(freq(&a).abs() < 1.0 && freq(&b).abs() < 1.0, "tone not at DC: {} / {} Hz", freq(&a), freq(&b));
    let ripple = |x: &[Complex32]| { let m = x.iter().sum::<Complex32>() / x.len() as f32; x.iter().map(|v| (v - m).norm()).fold(0.0f32, f32::max) / m.norm() };
    assert!(ripple(&a) < 0.02 && ripple(&b) < 0.02, "neighbour leaks: {} / {}", ripple(&a), ripple(&b));
    let s = bus.stats().consumers[0].clone();
    assert!(s.recomputed > 100, "back-fill should have recomputed from the ring: {s:?}");
}

#[test]
fn backfill_over_retired_history_reports_one_gap_and_continues() {
    // ring ceiling 16 MiB (≈0.4 s at 10 MS/s, 1 MiB blocks), guaranteed 0.2 s; wait until oldest > 0;
    // subscribe Start::At(src.oldest() / 32_768 * 32_768 + 32_768) with a consumer that sleeps 50 ms per
    // delivery (so the ring retires blocks under it); wait until it has ≥ 5 Samples deliveries;
    // assert gaps ≥ 1, every Gap has reason Unreadable(Overwritten), from_sample < to_sample, and the
    // Samples after a Gap start at a block ≥ the gap's to_sample / 32_768.
}

#[test]
fn a_retune_delivers_a_gap_then_continues_on_the_new_tuning() {
    // live channel on the tone (as in channel_live); wait for 1 s of samples; src.set_center(860e6)
    // (the tone is now at −3.75 MHz, inside ±4.5 MHz); wait for 1 s more of samples.
    // assert exactly one Gap with reason Unreadable(ReadError::SpansSegments) (or Gap if the sim stamps one);
    // the level after the retune is within 0.5 dB of before and the tone is still at DC (< 1 Hz).
    // Then src.set_center(870e6): wait_until state == OutOfSpan; samples stop growing (≤ +512 over 300 ms).
    // Then src.set_center(855.25e6): wait_until state == Running; the next deliveries carry one Gap with
    // reason OutOfSpan whose range covers the out-of-span stretch, then Samples.
}
```

- [ ] **Step 2: Run them to see them fail** — `cargo test -p engine --test channel_backfill`; Expected: the first fails on `recomputed`/gap handling or the output, the others on missing behaviour.

- [ ] **Step 3: Implement the recompute path, gap coalescing, redesign on segment change, out-of-span** in `sub.rs` as Task 7 specifies.

- [ ] **Step 4: Run them to see them pass** — Expected: PASS (three tests; the first takes ~8 s).

- [ ] **Step 5: Commit** — `git commit -m "Engine: channels back-fill from the ring, coalesce gaps, follow retunes"`.

---

### Task 9: Shedding under saturation

**Files:**
- Modify: `crates/engine/src/channels/sched.rs`, `crates/engine/src/channels/mod.rs` (`BusStats::saturated`)
- Test: `crates/engine/tests/bus_shedding.rs`

**Interfaces:**
- Produces: the shedding rule below; `BusStats::saturated` is true when, over the last full second, at least 90 % of `on_block` calls found every worker running with a non-empty ready queue.

Rule (checked inside `on_block`, under the scheduler lock, at most once per second): if `saturated` and at least one subscription has `lag_secs > cfg.max_lag_secs`, shed the one with the lowest priority among those (ties: the largest lag). Shedding sets state `Shed`, queues it for one last turn in which the worker delivers `Delivery::Shed` and drops it, increments `shed_total`, and logs `warn!("channel bus: shed channel {id} ({priority:?}) lagging {lag:.1} s under a saturated pool")`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_saturated_pool_sheds_the_lowest_priority_laggard_and_the_rest_recover() {
    let (d, _c) = sim(10e6, true);
    let src = Arc::new(Source::start(d, cfg(), Mode::Live, &keeper()).unwrap());
    let bus = ChannelBus::new(BusConfig { workers: 1, max_lag_secs: 0.5, ..Default::default() });
    bus.attach(src.clone());
    let spec = |hz: f64| ChannelSpec { source: src.id(), center_hz: hz, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None };
    let light = Collector::new();
    let slow = Collector::sleeping(Duration::from_millis(15));          // 15 ms per 3.3 ms block: cannot keep up
    let a = bus.subscribe(spec(852.0e6), Start::Live, Priority::Listen, Box::new(light.clone())).unwrap();
    let b = bus.subscribe(spec(853.0e6), Start::Live, Priority::Recorder, Box::new(slow.clone())).unwrap();
    wait_until(|| bus.stats().shed_total == 1);
    let s = bus.stats();
    assert!(s.saturated);
    assert!(s.consumers.iter().all(|c| c.channel.id != b.id || c.state == ConsumerState::Shed));
    wait_until(|| slow.shed_deliveries() == 1);
    wait_until(|| bus.stats().consumers.iter().find(|c| c.channel.id == a.id).unwrap().lag_secs < 0.2);
    assert_eq!(bus.stats().shed_total, 1, "shed more than the one laggard");
}
```

- [ ] **Step 2: Run it to see it fail** — Expected: `wait_until` times out (`shed_total` stays 0).

- [ ] **Step 3: Implement** the busy counters, `saturated`, and the shedding rule.

- [ ] **Step 4: Run it to see it pass** — Expected: PASS. Run the whole engine suite too.

- [ ] **Step 5: Commit** — `git commit -m "Engine: the bus sheds the lowest-priority laggard when the pool is saturated"`.

---

### Task 10: `Engine` composition, detach, the 200-channel acceptance test, rules test and docs

**Files:**
- Create: `crates/engine/src/engine.rs`, `crates/engine/tests/engine_sim.rs`, `crates/engine/tests/channels_200.rs`, `crates/engine/tests/rules.rs`
- Modify: `crates/engine/src/channels/mod.rs` (`detach`), `crates/engine/src/lib.rs`, `BACKLOG.md` (`## Rebuild`), `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` (§9: a dated "Chosen in step 2" list)

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone)]
pub struct EngineConfig { pub budget: BudgetConfig, pub bus: BusConfig, pub source: SourceConfig }   // no Default: the budget is the caller's decision
pub struct Engine;
impl Engine {
    pub fn new(cfg: EngineConfig) -> Engine;                                           // keeper + bus
    pub fn add_source(&self, device: Box<dyn Device>, mode: Mode) -> sdr_core::Result<Arc<Source>>;   // Source::start, bus.attach
    pub fn remove_source(&self, id: SourceId) -> bool;                                 // bus.detach, then drop
    pub fn source(&self, id: SourceId) -> Option<Arc<Source>>;
    pub fn sources(&self) -> Vec<Arc<Source>>;
    pub fn bus(&self) -> &Arc<ChannelBus>;
    pub fn keeper(&self) -> &Arc<Keeper>;
}
```

`ChannelBus::detach(source)`: stops the stage, ends every subscription of that source with `Ended { SourceDetached }` (delivered by the pool, once each), removes the stage; later `subscribe` for that source → `NoSuchSource`. `Drop for Engine` detaches everything, stops the bus and the keeper.

- [ ] **Step 1: Write the failing tests**

`tests/engine_sim.rs`:

```rust
#[test]
fn removing_a_source_ends_its_consumers_once_and_stops_its_stage() {
    let engine = Engine::new(EngineConfig { budget: BudgetConfig { guaranteed_secs: 0.5, ceiling_bytes: 128 << 20, sweep_secs: 0.2 }, bus: BusConfig { workers: 2, ..Default::default() }, source: cfg() });
    let (d, _c) = sim(10e6, true);
    let src = engine.add_source(d, Mode::Live).unwrap();
    let got = Collector::new();
    engine.bus().subscribe(ChannelSpec { source: src.id(), center_hz: 852.0e6, bandwidth_hz: 12_500.0, min_rate: 48e3, parent: None }, Start::Live, Priority::Classify, Box::new(got.clone())).unwrap();
    wait_until(|| got.samples() > 78_125);
    assert!(engine.remove_source(src.id()));
    wait_until(|| got.ended() == Some(EndReason::SourceDetached));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(got.end_deliveries(), 1);
    assert!(engine.bus().stage(src.id()).is_none());
    assert!(matches!(engine.bus().subscribe(/* same spec */, Start::Live, Priority::Classify, Box::new(Collector::new())), Err(BusError::NoSuchSource)));
    assert_eq!(engine.keeper().tick_panics(), 0);
}
```

`tests/channels_200.rs` (spec §6 step 2: 200 simultaneous channels at 10 MS/s with measured lag):

```rust
#[test]
fn two_hundred_channels_at_ten_megasamples_keep_up() {
    let engine = Engine::new(EngineConfig { budget: BudgetConfig { guaranteed_secs: 1.0, ceiling_bytes: 256 << 20, sweep_secs: 0.2 }, bus: BusConfig::default(), source: cfg() });
    let (d, _c) = SimDevice::with_config(SimConfig { paced: true, report_lag_gaps: false, ..Default::default() });   // the default 800 MHz scenario
    let src = engine.add_source(Box::new(d), Mode::Live).unwrap();
    let collectors: Vec<Collector> = (0..200).map(|_| Collector::counting_only()).collect();
    for (i, c) in collectors.iter().enumerate() {
        engine.bus().subscribe(ChannelSpec { source: src.id(), center_hz: 851.3e6 + i as f64 * 12_500.0, bandwidth_hz: 12_500.0, min_rate: 48_000.0, parent: None }, Start::Live, Priority::Classify, Box::new(c.clone())).unwrap();
    }
    std::thread::sleep(Duration::from_secs(5));
    let s = engine.bus().stats();
    let stage = engine.bus().stage(src.id()).unwrap().stats();
    let lags: Vec<f64> = s.consumers.iter().map(|c| c.lag_secs).collect();
    let max_lag = lags.iter().cloned().fold(0.0, f64::max);
    println!("200 channels @ 10 MS/s: max lag {max_lag:.3} s, mean lag {:.3} s, stage {:.0} µs/block, recomputed {} blocks, workers {}",
        lags.iter().sum::<f64>() / 200.0, stage.block_micros, s.consumers.iter().map(|c| c.recomputed).sum::<u64>(), s.workers);
    assert_eq!(s.shed_total, 0, "{s:?}");
    assert!(max_lag < 1.0, "max lag {max_lag} s");
    assert!(s.consumers.iter().all(|c| c.gaps == 0 && c.state == ConsumerState::Running));
    assert!(collectors.iter().all(|c| c.samples() >= 3 * 78_125), "a channel received under 3 s of samples in 5 s");
    assert_eq!(src.stats().starved_samples, 0);
    assert_eq!(stage.skipped_blocks, 0);
}
```

`tests/rules.rs` (spec §7, enforced on `crates/engine/src`):

```rust
/// Only the part of each file before its first `#[cfg(test)]` counts.
fn sources() -> Vec<(PathBuf, String)> { /* walk CARGO_MANIFEST_DIR/src for *.rs */ }

#[test]
fn nothing_in_the_engine_constructs_a_down_converter_or_reads_the_wall_clock() {
    for (p, s) in sources() {
        assert!(!s.contains("Ddc::new("), "{}: the channelizer in dsp is the only down-converter", p.display());
        assert!(!s.contains("SystemTime::now("), "{}: time comes from the timeline", p.display());
    }
}

#[test]
fn threads_are_created_only_by_the_keeper_the_stage_and_the_pool() {
    let allowed = ["ring/keeper.rs", "stage.rs", "channels/sched.rs"];
    for (p, s) in sources() {
        let spawns = s.contains("thread::spawn(") || s.contains("thread::Builder::new(");
        let ok = allowed.iter().any(|a| p.to_string_lossy().replace('\\', "/").ends_with(a));
        assert!(!spawns || ok, "{}: creates a thread", p.display());
    }
}
```

- [ ] **Step 2: Run them to see them fail** — Expected: compile errors (`Engine` missing), `detach` returning false.

- [ ] **Step 3: Implement `engine.rs` and `detach`**; re-export `Engine, EngineConfig` from `lib.rs`.

- [ ] **Step 4: Run the three tests, then the whole workspace** — `cargo test --workspace`; Expected: all pass. Record the `println!` line of `channels_200` in the task report (it is the step's first benchmark number).

- [ ] **Step 5: Update the docs**

- `BACKLOG.md` `## Rebuild`: mark done with "(done 2026-10-23, step 2)": `§5 L149` one overlap-save FFT per source, `§11 L274` dsp holds FFT, filters, channelizer, demodulators, `§10 L252` per-source USB thread plus FFT/channelizer thread, `§4 L101` usable bandwidth fraction masks band edges; reword `§10 L259` benchmark early to "first numbers from `channels_200`: <the printed line>"; `§14 L310` milestone 3 → "channelizer done; symbol engine and synthetic tests remain (step 6)". Add lines: second-stage channelizer for narrow modes not built (`ChannelSpec::parent` is refused; build with the first narrow mode); channel output rates are `fs/2^k` (a per-channel rational resampler if a consumer needs an exact rate); each subscription computes its own channel (no sharing between consumers wanting the same channel; add if profiling shows it matters); the sample waker was declined (1 ms polling by the stage); the hole-eviction stat and offer back-off from Task 1 close their step 1 lines.
- Spec §9: append "Chosen in step 2 (2026-10-23):" with the Global Constraints' builder values (N, overlap, K rule, filter, cache depth, row period, pool size, shedding rule).

- [ ] **Step 6: fmt, clippy, commit** — `cargo fmt --all && cargo clippy -p sdr-core -p backend-sim -p backend-airspy -p dsp -p engine --all-targets -- -D warnings`; `git commit -m "Engine composes keeper, bus and sources; 200-channel acceptance test; step 2 rules and docs"`.

---

## Self-review notes

- **Spec coverage.** §4.2: one FFT per source (Task 6), bins at 153 Hz / 6.5 ms floor (N = 65536, Tasks 5–6), fast-convolution channels added and dropped at runtime (Task 7), back-fill (Task 8), `parent` carried but refused (Task 7, BACKLOG in Task 10), `dsp` takes over `Ddc`/`Demod`/filters (Task 3), "nothing else constructs a down-converter" (Task 10 rules test; `Ddc::new` stays `pub` in `dsp` because `detect::prepare`, the old engine and offline examples use it, and the engine-side rule is what the spec wants enforced). §4.3: consumers register (spec, start, callback) and get blocks in order with gap markers (Task 7–8), pool sized to the machine, round-robin, lag in the stats (Task 7), shedding that says so (Task 9), priorities in the spec's order (Task 7). §6 step 2 tests: 200 channels with measured lag (Task 10), back-fill vs direct DDC (Task 8). Rows for step 3's waterfall (Task 6). Parked step 1 items (Tasks 1–2, 6).
- **Type consistency.** `ChannelInfo.hop` = `Stage::hop()` = `dsp::channelizer::HOP` = 32768; `Delivery::Samples.first_sample = block·hop`; `ConsumerStats.lag_secs` uses the source's current segment rate; `GapReason::Unreadable(engine::ReadError)` is the `source::ReadError` (with `SpansSegments` and `Gap`), not the ring's.
- **Proportion.** The plan carries signatures, rules and tests; the only algorithm written out is the extraction formula (Task 5) and the window identity (Task 4), which the signatures do not determine.
