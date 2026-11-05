# SpecTrawl Rebuild, Step 5: Replay

> Tasks run in order, one at a time, on branch `rebuild-step-5` (worktree `.worktrees/step-5`).

**Goal.** A record can be heard, or written out as audio, whether or not its IQ is still in the ring. The spec's step 5 is: "Replay. Aviation acceptance test on a recorded capture: click a 4 s AM burst, hear it, see its claims." No acceptance capture exists yet (decision 4), so the test runs on the simulator, which gains AM. The on-air check uses the RFOne on the air band.

**Architecture.**
- `engine::audio` is the audio queue (§4.7). A `Mixer` holds voices; each voice has a gain and reads a growing buffer of 48 kHz mono audio. One output pulls from the mixer: cpal's default device; a paced discard for headless runs; or nothing, when a test pulls the mixer itself.
- `engine::replay::Player` takes a record (or a region of a source) and fills a voice's buffer, choosing the first path that works:
  - **The ring.** A bus subscription (priority `Listen`, the bus's highest) back-filled from 50 ms before the record. Its channel is the record's centre at the mode's channel width. The record's samples go through `dsp::demod::Demod` on the bus worker, with the squelch open. The subscription stops at the record's end, plus the filter's group delay. For a record still open, it follows the live edge until the record closes.
  - **The record's slice.** When the ring no longer holds the start, or the source is gone, the slice file is read and demodulated on the player's own thread. A thread is allowed since 2026-11-05 (spec §7).
  - Otherwise, the play is refused with the reason.
- With `PlayTo::Wav(path)`, the finished audio is written as a 16-bit WAV by the player's thread instead of going to the mixer.
- The snapshot carries an `AudioView`: the output in use or why there is none, each play's state, position and length, and the underrun count.
- In the app:
  - The records list's detail panel gets **Play** (with a mode menu: Auto, AM, NFM, WFM), **Stop**, and the play's progress.
  - Clicking a waterfall box already selects its record, which shows the record's claims.

**Spec:** §4.7 (replay), §4.8 (slice tier), §4.9 (contract), §6 step 5, §7 (rules, rewritten 2026-11-05).

## Global constraints

- Only the source holds the device; replay reads the ring through the bus, or a slice from disk. No `Ddc::new(` and no `SystemTime::now(` in `crates/engine/src`. The rules test stays as it is.
- The engine owns two new threads, `replay` (slice reads and WAV writes) and the paced output. It joins both on drop. cpal runs its own callback thread.
- The audio callback never waits on the engine. The mixer's lock is held only to copy samples.
- The app keeps its rules: no thread, no lock, no clock, `api` types only. Play and Stop are `Command`s; progress comes from the snapshot.
- fmt, clippy (the CI package list) and `cargo test --workspace` stay clean. No test needs a sound card: tests use `AudioConfig::Manual` or `Paced`.

## Builder choices (spec §9), fixed by this plan

- **Mode.** `Command::Play` takes `mode: Option<AudioMode>`. With `None`, the engine chooses from the record's centre: 108–137 MHz and 225–400 MHz are AM, 88–108 MHz is WFM, everything else is NFM. This rule is the stand-in for the step 6 detector bank, and it is labelled so in the snapshot.
- **Channel.** The centre is the record's. The width is the larger of 1.25 × the record's width and the mode's width (AM 10 kHz, NFM 14 kHz, WFM 200 kHz). The channelizer chooses the rate. `Demod` runs at that rate with `squelch = 0`, so the whole record is heard.
- **Pre-roll** is 50 ms, as for slices. Plays are capped at 120 s of audio (about 23 MB).
- **Jitter buffer.** A voice starts once it holds 100 ms of audio (or its buffer is complete). If it catches up with an unfinished buffer, it waits silently until it holds 100 ms again, and that counts as one underrun.
- **Output.** cpal's default output device, preferring 48 kHz f32. Any other rate is resampled linearly, and mono is copied to every channel. With no device, the view says why and plays still run, so a WAV can still be written.

## Review focus

1. **Wrong span.** Replay starts at the record's start minus the pre-roll and ends at its end plus the group delay. A 4 s burst gives 4 s ± 0.2 s of audio, from the ring and from the slice alike.
2. **Wrong frequency, or a mirrored one.** The AM tone the simulator puts on the burst is what comes out. A 1 kHz tone dominates the audio by more than 20 dB over 300–3000 Hz.
3. **Ring and slice disagree.** The same record played both ways gives audio of the same length (±5%), with the same tone.
4. **No IQ left.** A record with no slice whose start has left the ring is refused with a reason. A refusal is not a silent play.
5. **Stop and source loss.** `StopPlay` ends a play at once (it unsubscribes or cancels). A source removed mid-play ends that play with `SourceDetached`, and the play reports "ended early".
6. **The UI never blocks.** The headless app test opens the simulator, clicks Play on a record and sees the play reach `Playing` and then `Finished` under the paced output, with every frame well under 100 ms.

## File map

| File | What |
|---|---|
| `crates/backend-sim/src/lib.rs` | `Modulation::AmTone { tone_hz, depth }` |
| `crates/api/src/audio.rs` (new) | `PlayId`, `AudioMode`, `PlayTarget`, `PlayTo`, `PlayState`, `PlayView`, `AudioView` |
| `crates/api/src/command.rs` | `Command::Play`, `Command::StopPlay`, `Outcome::Playing(PlayId)` |
| `crates/api/src/snapshot.rs` | `Snapshot::audio` |
| `crates/engine/src/audio.rs` (new) | `Mixer`, `AudioBuf`, `AudioConfig`, outputs (cpal, paced, manual), WAV writer |
| `crates/engine/src/replay.rs` (new) | `Player`: ring path (bus consumer), slice path (thread), refusal reasons, the view |
| `crates/engine/src/engine.rs` | `EngineConfig::audio`; the engine owns the player |
| `crates/engine/src/snapshot/*` | applies Play/StopPlay, publishes `audio`, updates open records' ends |
| `crates/engine/tests/replay_sim.rs` (new) | acceptance: the AM burst from the ring and from its slice, the refusal, stop, source loss |
| `crates/app/src/records_pane.rs` | Play / mode / Stop / progress |
| `crates/app/tests/headless_sim.rs` | click Play, see the play run |

## Tasks

1. **AM in the simulator.** Add `AmTone`, with a unit test that the envelope carries the tone at the given depth.
2. **api types.** Add the audio types, the commands and `Snapshot::audio`; `Snapshot::empty` gets an empty view. A replay subscribes at `Priority::Listen`, so no new priority is needed.
3. **`engine::audio`.** Mixer, buffers, jitter buffer and gains; the outputs; the WAV writer. Unit tests:
   - two voices mix with their gains;
   - the jitter buffer waits and counts an underrun;
   - a finished voice is removed;
   - the paced output advances at real time, within 10%.
4. **`engine::replay`.** The player, both paths, refusals, stop, and source loss. Unit tests: the mode rule and the channel width.
5. **The publisher.** Applies Play and StopPlay, publishes the view, and passes ends of records that are still open.
6. **The app.** Play, the mode menu, Stop, and progress in the records detail panel; the headless test.
7. **Acceptance on the simulator.** `replay_sim.rs` covers review items 1–5.
8. **On air, docs and BACKLOG.** On the RFOne at 118–137 MHz:
   - play a real airband record from the ring and from its slice, and write both as WAVs;
   - check their length against the record and their energy in the voice band;
   - record the choices in spec §9, and close or re-tag the BACKLOG lines for step 5.
