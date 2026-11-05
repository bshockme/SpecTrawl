# SpecTrawl Rebuild: the RFOne's Board Sweep

> Done on branch `rebuild-native-sweep` (off `rebuild-step-4a`), after the RFOne got sweep firmware (`docs/hydrasdr-sweep-mode.md`, ~/hydrasdr `experiments/2026-11-04-sweep-fw`).

**Goal:** a device that sweeps by itself is the row producer for its source in Sweep mode, through the same ring, segment table, stitched rows, record book and snapshot as the retune sweep (spec §4.6a). DESIGN §4: "native sweep, else retune-and-dwell".

**Contract (`sdr-core`).** `Device::start_native_sweep(NativeSweep { first_center_hz, step_hz, steps, dwell_blocks, .. })` returns the grid granted, with `block_samples`; until `stop_native_sweep` the stream carries only the board's hops, each a `Retune` at its first sample followed by `dwell_blocks` blocks, all captured after lock; `TimeAnchor`s come from the device clock every ~25 ms; `set_center` is refused and `set_gain` pauses the sweep. Default methods refuse, so other backends are untouched.

**Backend (`backend-airspy::native`).**
- Detects support from the firmware version (`sweep`) when libhydrasdr drives the board.
- Start: restart the stream raw (sample type 5, packing off); call `hydrasdr_sweep_start` with the 16-byte parameters (lock check 2 reads, timeout 200 polls). (Superseded the same day: the first version opened a second USB handle and sent vendor request 0x70 itself.)
- Stop: send SWEEP_STOP (0x71); restart the stream as float IQ at the live centre.
- The callback parses each 16 KB block:
  - It drops torn and untagged blocks, and any hop whose lock timed out or which doesn't start at block 0.
  - It counts skipped steps and dropped buffers.
  - It converts each block to IQ by multiplying by j^n, which shifts the 5 MHz IF to DC and undoes the inversion. A 95-tap Kaiser half-band filter and decimation by 2 follow, at libhydrasdr's float scale.
- The delivered-vs-clock check is off while sweeping.
- Fixed on the way: libhydrasdr's `dropped_samples` is cumulative.

**Engine (`engine::sweep`).**
- `run_plan` tries `run_native` first when the device offers it and `native_dwell > 0`.
- It sweeps the measured hops' range. Hops are read off `Source::segments_from` as they complete:
  - A hop cut short by the next one is skipped (counted as redone).
  - A segment off the grid is ignored.
  - A wrap publishes the pass.
- If the board is silent for 500 ms, its sweep is started again (counted in `unseen_retunes`).
- The grid, blanking and pass stitching (hop profile, max-decimation, the stitched segment) are shared with the retune sweep (`Grid`, `blank_hop`, `Stitcher`). The board's profile has its own key (`-board`).
- The status sentence says "on the board".

## Tests

- `backend-airspy` unit tests:
  - tag parsing, and torn and untagged blocks;
  - the parameter block's bytes;
  - a tone at its RF offset, with its mirror more than 50 dB down;
  - libhydrasdr's amplitude scale.
- `backend-sim` simulates a board sweep: 4088-sample blocks, one segment per hop, 1.23 ms of wall time per hop.
- `engine/tests/sweep_sim.rs::a_board_sweep_is_read_off_the_segment_table_and_stitched_the_same_way`:
  - on board, no lock waits, settle retries or slow hops;
  - every hop a segment, and one stitched segment;
  - both carriers 30 dB over the floor;
  - back to Live restores the centre.
- On air: `backend-airspy/examples/native_sweep` (backend only: hop rate, hop sizes, anchors, a gain change mid-sweep, return to live). The engine comparison is in BACKLOG ("Board sweep on the RFOne").
