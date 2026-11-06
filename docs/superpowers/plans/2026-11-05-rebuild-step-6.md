# SpecTrawl Rebuild, Step 6: The detector bank

> Tasks run in order, on branch `rebuild-step-6` (worktree `.worktrees/step-6`).

**Goal.** Every record with IQ gets at least one look by a bank of detectors. Each detector returns claims with their own evidence; nothing returns a bare label. A P25 control channel gets a native decoder on the bus. Its system (NAC, WACN, system, RFSS, site) appears as a row built from the same claims the record and its emitter show, so the three views cannot disagree.

The spec's step 6 is: "Detector bank with `detect` wrapped and the pool. P25 acceptance test on the owner's 851–860 MHz capture: system row within 10 s, no contradiction between views." The wideband capture was never recorded (decision 4: the manifest lists none). So the acceptance test runs on the simulator. The simulator gains an emitter that plays a recorded IQ file, and it plays the committed real P25 control channel (`testdata/small/859.7677MHz_P25_Phase_1`, NAC 0x34D) inside the wideband 851–860 MHz stream. The on-air check uses a radio at 855.25 MHz on the local system.

**Spec:** §4.4 (records, emitters), §4.5 (bank), §4.6 (DecoderBank, native first), §5 (slots, rule 3), §6 step 6, §7, §8 (`detect` defects).

## Architecture

- **`detect` exposes the ladder's rungs as separate public detectors** (`detect::bank`):
  - the sync pass (both cutoffs, the protocol's preferred re-slice) with its frames and decoded fields;
  - FLEX headers, GSM SCH, LoRa chirps and sub-audible squelch, each as an identifier;
  - the modulation rules (the ladder's tail, behind the weak-signal gate) as a description.
  - `classify_with` keeps its behaviour for `engine-old`.
- **`engine::classify`, the pool.**
  - **When a record is looked at:** once it has been open 1.5 s, or when it closes, whichever comes first.
  - **The window:** a bus subscription at `Priority::Classify`, back-filled from the record's start minus 50 ms. Its centre is the record's centre, and its width is `max(1.25 × width, 12.5 kHz)`, capped at 1 MHz. Its rate is at least `max(48 kHz, 2.5 × width)`. It collects up to 1.5 s, then stops.
  - **The work:** the window goes to a bounded queue served by the pool's own threads, owned and joined by the engine. A slow look never holds a bus worker.
  - **What the snapshot counts:** looked, waiting, not looked because the IQ had left the ring, dropped from a full queue, and survey records (which have no IQ).
- **The bank** is a list of `Detector`s (`fn look(&Window) -> Vec<Claim>`):
  - sync bank → `SyncBank` (with syncs, frames tried and decoded);
  - identifiers → `Decoder` (FLEX, GSM, squelch code, and P25/DMR/NXDN when frames decoded);
  - modulation → `Features` (the label at its rule's confidence, never above 0.6).
  - Band plan claims are added by the book when a record is placed, so survey records get them too: `BandPlan`, inferred, never a rename.
- **Claims on records.**
  - The book keeps the bank's claims per record across the row-by-row rebuilds (today `place` drops them), next to the position and band plan claims.
  - A closed record whose claims change is sent to the store again.
  - The headline is per spec §4.5: a decoder claim, else a sync-bank claim, else none ("unknown", with the features).
- **Emitter labels** are voted from the emitter's records' claims (re-homing `engine-old::emitters`):
  - decoder claims first;
  - then a protocol seen in at least 2 records, or in one record with a strong sync (≥ 4 syncs, or half its frames decoded);
  - a feature label only as a muted description. No record borrows the emitter's label.
- **The decoder bank** (`engine::decoders`).
  - The `Decoder` trait of spec §4.6: mode, configure, feed, reset, telemetry, summary.
  - `P25Control` wraps `detect`: frames on 0.6 s blocks with 0.12 s overlap, then the NID's NAC by majority, then TSBK messages, then `SystemInfo`.
  - **Attaching:** a decoder attaches to an emitter whose records carry a P25 Phase 1 claim with TSDU frames. It's a bus consumer at `Priority::TrunkControl`, back-filled from the open record's start so the system appears without waiting for new traffic. At most 4 per source. A decoder detaches 30 s after its emitter has no open record.
  - **What it produces:** its telemetry is a `Decoder` claim on the emitter's open record, carrying an `Identity::P25 { nac, wacn, system, rfss, site }`, refreshed as fields arrive.
- **`Identity`** is a new api type carried by `Evidence::Decoder`. It's optional and serde-defaulted, so stores written by step 4 still read.
- **The systems view.** `Snapshot::systems` is built from the records' decoder claims with a P25 identity, grouped by system key, plus the decoder's counters (TSBKs, grants, last message). There is no other path, so a system row, the record's claim and the emitter's headline read the same `Identity`.
- **Decoders in the snapshot:** each decoder's lamp (`Searching | Syncing | Locked`), its one-line summary and its counters.
- **The app:**
  - the records detail lists every claim with its evidence kind (decoded solid, sync with its count, inferred muted);
  - a Systems panel lists the systems;
  - the status line names the decoders.
- **The simulator** gains `Modulation::Iq`: a recorded clip resampled into the stream (low-passed to its signal, then a polyphase interpolator), looped.

## Global constraints

- No `Ddc::new(` and no `SystemTime::now(` in `crates/engine/src`. The pool's threads and the decoders' work are engine-owned and joined on drop. The rules test stays as it is.
- The bank never renames: band plan and position stay their own claims.
- Every claim has evidence. "Unknown" (no claim from the bank) is an allowed answer and shows as such.
- fmt, clippy (the CI package list) and `cargo test --workspace` stay clean, and CI stays green.

## Builder choices (spec §9), fixed by this plan

- **Pool size:** `max(1, (available_parallelism − 2) / 4)` threads. The queue holds 64 windows; a full queue drops the newest window, and the drop is counted.
- **The look:** 1.5 s at most (`engine-old`'s `classify_record`). Records shorter than 20 ms get no look ("too short" is not a claim).
- **Confidence:**
  - sync bank: `detect`'s own, by sync count: 0.8, 0.9 or 0.97;
  - identifiers: 0.95;
  - P25 decoder with a NAC and a TSBK: 0.99;
  - modulation: the rule's own, capped at 0.6;
  - band plan: 0.2.
- **Sync confirmation (§7.1, §8):**
  - Measure first: today's rule (any whole number of periods ± 3 % + 3 symbols) against "the next sync within the protocol's longest frame + 3 %", on the real captures and on noise and random 4FSK.
  - Adopt the tighter rule only if the real captures still confirm.

## Review focus

1. **Contradiction.** For one signal, the record's headline, the emitter's headline and the system row show the same NAC, WACN, system and site, because they read one `Identity`.
2. **Honesty.**
   - Noise, a bare carrier and random 4FSK get no decoded claim.
   - A weak record gets no modulation claim.
   - Band plan claims never become a headline.
3. **Coverage.** Every record with IQ is looked at before its IQ leaves the ring, or is counted as not looked; the counters add up.
4. **Ownership.** No detector reads the radio. Windows come from the bus; decoders are bus consumers; the pool joins on drop.
5. **Claims survive.** A record's bank claims survive the row-by-row rebuild of an open record, its close, the store and a restart.

## Tasks

1. **`detect::bank`:** the public rungs. A test that `classify_with` gives the same labels on the real captures as before.
2. **Sync confirmation:** the measurement, then the rule if it holds; `detect`'s tests stay green.
3. **`api`:** `Identity`, `Evidence::Decoder.identity`, `SystemsView`, `ClassifyView`, `DecoderView`, and their snapshot fields.
4. **The book:** keeps bank claims, adds band plan claims, re-sends closed records whose claims changed, and votes emitter labels.
5. **The pool and the bank** (`engine::classify`), with unit tests on synthetic windows.
6. **`Modulation::Iq`** in the simulator, with a test that the clip's spectrum appears at its frequency with nothing beside it.
7. **The decoder bank:** the `Decoder` trait, `P25Control`, attach and detach, and the systems view.
8. **Acceptance on the simulator:** the real P25 control clip at 859.7677 MHz in the 851–860 MHz stream. The system row within 10 s, NAC 0x34D, and record, emitter and system in agreement. The simulator's sync-only emitters get sync-bank claims and no decoded claims.
9. **The app:** claims in the records detail, the Systems panel and decoder status; the headless test.
10. **On air, docs and BACKLOG:**
    - a radio at 855.25 MHz on the local system;
    - spec §9's choices;
    - the `detect` defects this step does not fix, with their reasons.
