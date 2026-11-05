# SpecTrawl Rebuild, Step 4a: The Sweep Re-homed

> Done serially on branch `rebuild-step-4a` (worktree `.worktrees/step-4a`), after step 4.

**Goal (spec §4.6a, §6 step 4a):** the retune-and-dwell sweep is the row producer for a source in Sweep mode on the new engine: it writes into the same ring, every hop is a segment in the segment table, the shared FFT, channelizer and bus are off for that source, and its stitched rows go out through the same history, row sink and snapshot as live rows. Survey records and emitters go through the step 4 record book (`Role::Survey`). **Test:** a survey pass on the simulator produces the same row as the old sweep.

**Architecture.** `engine::sweep` is `engine-old::sweep` re-homed: the plan (span, segments, FFT size and count, spur grid), the hop grid over the usable fraction, tuner-lock waits, the long-wait probe for slow hops on devices without a lock flag, the settle check (first FFT's floor), re-measuring a hop whose samples were refused, the hop profile (median across hops, smoothed, saved per device/FFT/bins/gain under `<Documents>/SpecTrawl/profiles`, the same files `engine-old` uses), spur-grid and LO-mask blanking, and max-decimation of the stitched row. It runs on the source's stage thread: the stage's loop checks the source's mode each block, and in Sweep mode hands the thread to `sweep::run` until the mode changes back (the stage then retunes to the last live centre and resumes FFT blocks from the head). No new thread.

What changed from `engine-old`:

- **Timing from the segment table.** `engine-old` retuned just as a USB block arrived and counted its guard from that block. The new backends stamp each retune at the sample it lands on (step 1), so the sweep retunes, waits until the source's newest segment is the new tuning, and measures from that segment's first sample plus a settle guard (`SETTLE_S` = 1 ms, or the plan's guard, or the device's latency less the 2 ms landing; ×3 on big jumps). A lock wait counts from the landing. A retune that never shows within 500 ms is counted (`unseen_retunes`, a status warning) and the hop is measured from the head.
- **Refused reads redo the hop.** `Source::read` refuses a range that crosses a segment boundary, touches a hole or has been overwritten; the hop is measured again (`redone_hops`), as `engine-old` did on a gap.
- **Rows are api rows.** A pass is one `Row`: linear power (NaN where a hop was not measured), `first_sample`/`end_sample` the ring's head at the pass's start and end, `blocks` the hops measured, and a synthetic segment (`STITCHED` bit + the plan's generation; centre = the grid's middle, rate = the grid's width) that stays the same from pass to pass, so the app's waterfall keeps its history and the detector its tracks; a new plan gets a new generation.
- **Survey records.** The record book recognises stitched rows by the `STITCHED` bit: it detects them with `DetectorConfig::sweep()` (as `engine-old` did), marks the records `Role::Survey`, makes no position claims (the "tuner" is the span's middle; the sweep blanks the radio's spur grid itself) and asks for no slice.
- **LO masks are the engine's.** Each snapshot tick the publisher gives every sweeping receiver the other live Airspys' LO leaks (centre + 5 MHz, ±15 kHz) to blank, as `app-old` did (spec §4.9: LO masking is an engine state machine).

Commands and views: `Command::SetSweep { source, start_hz, stop_hz, segments }` (clamped to the device's range, picked up at the end of the pass in progress); `SetMode` to Sweep gives the receiver the letter `S` if free (spec §9, reserved since step 3) and a free letter back on return to Live; `Tune` on a sweeping receiver is refused. `Receiver::sweep: Option<SweepView>` carries the progress; the status sentence reads "Sweeping 24.0–1797.0 MHz: pass 1.3 s, 152 hops/s (1368 MHz/s)".

## Tests

- `engine/src/sweep` unit tests carried over: settle check, segment normalisation, hop mask, range and spur blanking, hop profile.
- `engine/tests/sweep_sim.rs`: four hops over 845–875 MHz on the simulator; stitched passes with one segment id, the axis and bin count, every hop measured, both carriers 30 dB over the floor, every hop a segment of its own in the source's table, no unseen retunes; back to Live restores the earlier centre and live rows. Through the snapshot: `SetSweep` (and refusing a span outside the device), `SetMode`, `Tune` refused, the letter `S`, the status sentence, survey records at both carriers with no claims and no slice.
- `engine-old/tests/sweep_parity.rs` (the spec's test): the same plan and simulated carriers through `engine-old` and the new engine; the rows have the same length, peaks at the same bins (±1) and levels (±1.5 dB), floors (±1.5 dB) and quiet-bin means (±1 dB). It lives in `engine-old` (with `engine` as a dev-dependency) so the engine's CI lint run does not pull `detect` in, and goes with `engine-old` in step 8.

## Left

- **No partial rows.** `engine-old` published the pass in progress every 100 ms for clock-driven views; the new sweep publishes completed passes only (a full 24–1800 MHz pass is ~1.3 s on an R2).

## Done after the engine part

- The receiver header's Live/Sweep switch and the sweep span (from–to MHz, sent as `SetSweep` when an edit settles); a sweeping receiver's spectrum drag does not tune. Headless test: switched to Sweep over 845–875 MHz, the receiver takes `S` and its waterfall fills with passes over the hop grid.
- On air (R2, 24–1797 MHz, 40 s each): 127 hops/s on both engines, the same floor to 0.02 dB per hop, the FM stations within 2.5 dB; fewer lock waits and redone hops on the new one. The 870–879 MHz hop varies by 40 dB from run to run on both (BACKLOG).
