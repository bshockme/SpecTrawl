# Algorithm review — 2026-10-11

Reviewed revision: `30a37dc`, `master`. No production source changes were made.

The broad DSP architecture is reasonable, but several implementations violate
their assumptions. The highest-priority problems are the floating-point CIC's
long-run instability, loss of sample-indexed tuning boundaries, and AM squelch
thresholds calibrated for the wrong input spectrum. Changing the entire DSP
architecture is not necessary to address these.

This is a targeted correctness review of the shared DSP, detection,
classification, and stream integration, with sampled inspection of protocol
decoders. It is not a bit-exact certification of every protocol, FEC table,
vocoder, or supported propagation condition.

## Findings

### 1. P1 — Floating-point CIC becomes unstable during continuous reception

Location: `crates/detect/src/ddc.rs:28`, especially lines 47–60.

The two integrators accumulate indefinitely in `f64`. The comb subtracts large,
nearly equal quantities. Finite precision eventually destroys the small
differences that represent the wanted signal. Using double precision delays
this failure; it does not make the recurrence safe for an indefinitely running
receiver.

**Reproduction:** compiled the unchanged `ddc.rs` with an appended standalone
driver. Fed constant complex `1 + 0j` at 10 MS/s, offset 0, cutoff 8 kHz,
target output 48 kS/s, in 100,000-sample chunks. The entire DDC, including its
final FIR, produced:

| Simulated input interval | Minimum real output | Maximum real output | RMS error from 1 |
|---|---:|---:|---:|
| 0–100 s | 0.999236 | 1.000215 | 0.000002 |
| 100–200 s | 0.990268 | 1.010147 | 0.000282 |
| 200–300 s | 0.965831 | 1.033209 | 0.000311 |
| 300–400 s | 0.635771 | 1.440355 | 0.239950 |
| 400–500 s | -0.566986 | 3.292441 | 1.161650 |
| 500–600 s | -0.566986 | 3.260308 | 1.374405 |

The output should remain 1 after startup. The final filter does not prevent
the failure. A nonzero-mean channel, such as a centered carrier, exercises this
path; actual failure time depends on signal statistics and decimation. Short
classification windows are less exposed than persistent listening chains.

**Fix direction:** use correctly sized wrapping fixed-point CIC arithmetic, or
a bounded-state FIR/moving-sum implementation with controlled numerical error.
Do not simply reset live filter state periodically and introduce discontinuities.
Require a long-duration constant-input and carrier-plus-weak-signal regression.
The classic CIC overflow argument depends on two's-complement arithmetic;
it does not justify unbounded floating-point accumulation.
[Donadio, CIC Filter Introduction, §§4–5](https://dspguru.com/files/cic.pdf).

### 2. P1 — DSP and exports lose the frequency/time boundaries of the IQ

Locations: `crates/engine/src/lib.rs:226`, `:498`, `:660`;
`crates/engine/src/listen.rs:374`; `crates/engine/src/sigmf.rs:99`.

`RingSink::on_event` discards `at_sample` for Retune and Gap events, retaining
only the newest tuning and aggregate gap counters. The IQ ring retains samples
from before and after a retune without the corresponding tuning history.

Consequences established by the call paths:

- `live_worker` can average FFTs across a retune, then label the entire row with
  the latest center frequency. Resetting the burst detector when the row's
  tuning changes occurs after this contamination.
- A rebuilt listening chain uses the new tuner center but starts 50 ms
  (analog) or 500 ms (P25) behind the current head. After a hardware retune,
  this can demodulate old-frequency IQ as if it belonged to the new tuning.
- Export captures the current device center, even for an older requested
  region. A historical wideband export receives incorrect frequency metadata;
  a historical channel export mixes using the wrong frequency offset.
- Gap positions cannot be communicated to individual decoders. Zero filling
  preserves sample count but is not a replacement for resetting or invalidating
  signal-processing state at a discontinuity.

**Fix direction:** preserve sample-indexed stream epochs and gap intervals with
the ring; split reads, FFT accumulation, and decoder state at those boundaries.
Constrain retune look-back to the correct epoch. Export separate SigMF capture
segments or reject requests spanning incompatible tunings. Include the hardware
settling interval when defining the first usable samples of a new epoch.

Evidence: static source/call-path review, not a hardware reproduction.

### 3. P1 — AM noise squelch opens on the filtered noise it actually receives

Locations: `crates/detect/src/demod.rs:38`, `:206`;
`crates/engine/src/listen.rs:378`.

The AM channel filter cuts off at 5 kHz. The squelch estimates envelope noise
above 6 kHz and assumes a noise-only ratio near 0.2, opening below 0.06.
The preceding channel filter changes that noise spectrum enough to make
noise alone satisfy the carrier criterion. The existing test supplies
unfiltered Gaussian samples directly to `Demod`, bypassing this interaction.

**Reproduction using the compiled Rust crates:** 0.6 seconds of Gaussian IQ
noise at 10 MS/s through `Ddc::new(fs, 0, 5000, 48000)` and `Demod::new(Am, ...)`
leaves squelch open and outputs audio RMS about **0.216**. An independent
transcription measured the ratio near **0.0457**, below the opening threshold,
with squelch open for every sample in the second half of the run.

**Fix direction:** derive/calibrate the noise statistic for the actual channel
bandwidth, or use an independent carrier-to-noise measurement. Test through
the DDC at all supported input rates, including weak carriers and strongly
modulated AM. Envelope AM demodulation itself is an appropriate choice.

### 4. P2 — POCSAG correction accepts ambiguous three-bit errors

Location: `crates/detect/src/pocsag.rs:64`.

The syndrome table corrects up to two errors in the 31 BCH bits, then the
decoder unconditionally repairs overall parity. It can therefore accept a
candidate three bits away from the received word. That defeats the extended
code's ability to reject an ambiguous three-bit error.

**Reproduction using the compiled Rust crate:** the valid all-zero codeword,
corrupted at bit positions 1, 2, and 5, becomes `0x00000026`.
`correct(0x26)` returns `Some((0x00900027, 3))`: a different valid codeword,
with different address data. This can manufacture incorrect pager identities
or message content.

**Fix direction:** enforce a total correction distance of at most two across
all 32 bits, including parity. Test all single/double errors and rejection of
triple errors around known valid words. The implemented generator and parity
format match the standard; the defect is the acceptance policy after correction.
[ITU-R M.584-2, Annex 1 §1.4](https://www.itu.int/dms_pubrec/itu-r/rec/m/r-rec-m.584-2-199711-i!!pdf-e.pdf).

### 5. P2 — Fixed chirp-analysis windows exclude advertised LoRa modes

Location: `crates/detect/src/chirp.rs:81`, `:98`, `:117`.

The analyzer uses 0.25 ms windows and requires the frequency to sweep over
10% of the observed bandwidth inside each window. Slow chirps cannot satisfy
that criterion. Fast chirps can wrap inside most windows and fail the linear
fit instead. Although `lora_params` searches SF7–12, the preceding detector
has already rejected much of that range.

**Reproduction using the compiled Rust crate:** noiseless 125 kHz/SF12 IQ
produces `ramp_fraction = 0`, `level_fraction ≈ 0.993`, and no LoRa result.
The signal is treated as locally constant rather than chirped.

An independent sweep of the same regression equations found zero accepted
ramp windows for 125 kHz/SF9–12, 250 kHz/SF10–12, and 500 kHz/SF11–12.
500 kHz/SF7 also fell below the required 50% ramp fraction.

**Fix direction:** use several window durations tied to candidate symbol
durations, or candidate dechirping/FFT detection with preamble repetition checks.
A linear ramp fit is useful as a generic strong-chirp feature, but this fixed
window is insufficient for the stated LoRa parameter range. Include all
bandwidth/SF combinations, offsets, and noise levels in the regression matrix.
[CSS/LoRa tutorial](https://arxiv.org/abs/2310.10503).

### 6. P2 — Two arbitrary sync hits are promoted to a confirmed protocol

Locations: `crates/detect/src/sync.rs:195`;
`crates/detect/src/lib.rs:374`.

`best` requires two distinct occurrences, but never checks their spacing.
There is no frame-period field in `SyncEntry`. Classification then sets
`protocol` and confidence even when subsequent frame decoding validates
nothing. This omits the timing confirmation specified in DESIGN.md §7.1.

**Reproduction using the compiled Rust crate:** two DMR BS-voice hits at symbol
indices 100 and 201 return a confirmed DMR result at confidence 0.8. That
101-symbol spacing does not fit the DMR burst grid. DMR voice sync placement
is constrained by slots and the 360 ms voice superframe; arbitrary spacing is
not evidence of that structure.
[ETSI TS 102 361-1, §5.1.2.1](https://www.etsi.org/deliver/etsi_TS/102300_102399/10236101/02.02.01_60/ts_10236101v020201p.pdf).

**Fix direction:** implement protocol-specific permitted cadence, allowing
missed frames, slot distinctions, and sampling-clock tolerance. Use validated
FEC/CRC or protocol fields as additional evidence where available. Preserve
uncertain candidates separately from confirmed protocols. The probe establishes
the missing validation, not a measured false-positive rate on real RF.

### 7. P2 — The DC-spur classifier ignores the receiver's IF architecture

Locations: `crates/detect/src/context.rs:40`;
`crates/engine/src/classify.rs:314`;
`crates/backend-airspy/src/lib.rs:249`.

Every unidentified signal narrower than 5 kHz and within 1.5 kHz of the tuner
center is labeled `DC offset` with confidence 0.9. The context passed to this
rule omits `Capabilities.if_arch`, even though Airspy advertises `RealIf` and
the design explicitly distinguishes real-IF from zero-IF receivers.

Tuning an Airspy onto a real narrow carrier therefore makes the heuristic
declare it an artifact based only on its location. The separate 5 MHz-grid
rule likewise treats frequency coincidence as proof of an internal spur.

**Fix direction:** gate DC-offset handling on the actual IF architecture and
measured artifact behavior. Use retune-relative movement or another independent
observation to distinguish hardware artifacts from real signals. Frequency
coincidence alone should yield a suspicion, not a confident exclusion.

Evidence: deterministic source branch and capability propagation review.

### 8. P2 — Track merging inflates mean SNR

Location: `crates/engine/src/bursts.rs:401`.

When overlapping tracks merge, their `snr_sum` values are added but their
`rows` counts are combined with `max`. The numerator counts observations that
the denominator deliberately discards. The final mean can exceed every SNR
ever observed.

**Reproduction using the compiled Rust crate:** two tracks, each observed for
two rows at 20 dB SNR, joined by one 20 dB bridge row, produce
`mean_snr_db = 33.333332` while `max_snr_db = 20`.

**Fix direction:** define how simultaneous segments contribute to one row's
SNR, and combine numerator/denominator consistently. Retain enough per-row
information if the intended statistic is one peak per distinct row.

## Algorithm suitability and remaining limits

| Job | Selection | Assessment |
|---|---|---|
| Spectrum display and tone levels | Periodic four-term Blackman–Harris FFT, linear-power averaging, coherent-gain normalization | Appropriate. The normalization is for sinusoidal amplitude/power per bin, not a calibrated noise PSD; use ENBW and sample-rate normalization for a future PSD display. Max-per-bucket reduction is appropriate for keeping display peaks, not for integrated power. |
| Extract a few arbitrary channels | Complex mixer, CIC decimation, windowed-sinc FIR | Appropriate architecture; fix finding 1. Stopband specifications need to be explicit. Independent coefficient checks found about 54 dB suppression for a WFM-path blocker near the first CIC alias region; adequacy depends on blocker strength. An FIR after aliasing cannot remove a blocker already folded into the wanted band. |
| Detect sparse narrowband energy | Local low-percentile floor, threshold runs, frequency/time association | Reasonable heuristic, but not calibrated CFAR. Noise statistics differ between averaged live FFTs and max-reduced survey bins. Fix finding 8. |
| Detect a carrier filling the visible span | Same local percentile floor | Insufficient without an independent noise reference. A flat occupied span becomes its own floor; smoothing only delays this. A Rust probe demonstrates the miss. This limitation is already in BACKLOG.md; survey hints mitigate known occupants, not arbitrary wideband detection. Adjacent clean spectrum, historical baseline, calibrated floor, or structure-based detectors are needed. |
| Blind modulation features | Envelope CV, instantaneous-frequency histogram, spectral line of squared frequency differences | Useful candidate features, not definitive classifiers. Gate-based removal of samples before the symbol-rate FFT (`detect/lib.rs:301`) changes the time axis and creates joins; compute timing features on contiguous runs or preserve original indices. Histogram/statistical features may legitimately use gated samples. |
| Narrowband FSK identification | All-phase sign sync search, per-sync level estimation, adaptive slicing | Sensible for the supported FSK waveforms; fix finding 6. Validate pulse-shaping and clock drift independently. This does not establish suitability for every PSK/CQPSK/simulcast channel. |
| Analog audio | Conjugate-product FM discriminator, AM envelope detector, IIR audio filtering, WFM de-emphasis | Core choices are reasonable. Fix AM squelch integration (finding 3). Audio resampling/filter rejection needs an explicit quality target; a passing 1 kHz tone test is not a stopband test. |
| LoRa/CSS | Frequency-ramp regression | Suitable as a strong-signal feature, not as implemented for the full advertised SF/BW matrix; finding 5. |
| POCSAG | BCH syndrome lookup, parity, batch assembly | Correct code family and basic framing; acceptance defect in finding 4. |
| LTE identification | CP autocorrelation, PSS/SSS correlation, PBCH soft decoding with tail-biting Viterbi and CRC | Sensible structure. Inspected Viterbi explicitly enforces matching initial/final states; MIB combining is limited to the candidate 40 ms period. Current hardcoded timing is a limited LTE model, not proof of all duplex/CP modes. External vectors and multipath/rate-error coverage remain needed. |
| RDS and GSM identification | Pilot PLL/biphase/differential/syndrome chain; FCCH/SCH and checked coding | Appropriate families. Reviewed pipeline structure and acceptance checks, not every standards field or operating condition. |
| Digital voice | Protocol-specific FEC and IMBE/AMBE parameter/speech synthesis | Inspected interfaces and existing tests. Independent bit-exact vocoder certification was not performed. Synthetic round trips can share encoder/decoder mistakes. |
| Sweep correction | Median across hop-relative bins | Useful when real signals do not occupy the same relative bins in most measured hops. Not a calibration reference. Dense/repeating allocations can contaminate the learned profile. |

Decimation must control energy that folds into the retained band before it is
discarded. This underlies the DDC stopband recommendation, rather than a blanket
claim that every rate conversion is currently wrong.
[MathWorks multirate filter overview](https://www.mathworks.com/help/dsp/ug/overview-of-multirate-filters.html).

## Validation and reproducibility

- `cargo test --workspace --offline`: passed. Cargo reported 139 passing tests,
  but 11 real-IQ capture tests return early because `testdata/captures` is absent.
  The three checked-in spectrum-fixture tests did execute. This is 128 tests
  exercising their checks, not 139 independent validations of behavior.
- Six standalone Rust probes against compiled local libraries: all six assertions
  failed as documented. Five exercise implementation/acceptance defects; the
  sixth demonstrates the known full-span noise-floor limitation.
- The long-duration probe compiles a copy of the exact, unmodified `ddc.rs` with
  an appended `main`; it reproduces finding 1 without relying on a translation.
- Independent Python/C calculations additionally checked chirp parameter
  coverage, filter response, AM noise statistics, and CIC arithmetic.
- No on-air tests, hardware retunes, or complete protocol conformance suite were
  run. Finding 2's stream integration consequences were traced statically.

The review probes are outside Cargo's normal test discovery and deliberately
assert the desired behavior. They are kept separately so this review does not
change production code or the normal test suite.

```sh
cargo test --workspace --offline
python3 docs/review/run-probes.py
# Also process 600 seconds of synthetic IQ through the exact DDC source:
python3 docs/review/run-probes.py --long-ddc
```

The probe runner should exit nonzero on this revision. Its long-DDC section
prints measurements; those should approach a constant output of 1 after a fix.
It expects the default `target/debug/deps` location. Probe source:
[algorithm-probes.rs](review/algorithm-probes.rs),
[run-probes.py](review/run-probes.py).

Suggested repair order: preserve stream epochs; stabilize the DDC; correct AM
squelch; enforce POCSAG correction distance and sync cadence; repair chirp
coverage and artifact classification; correct track statistics. Add independent
fixtures and make missing required capture fixtures visible in validation.

## Status — 2026-10-11, after the fixes

All eight findings were addressed on the working tree the same day, with
regression tests beside each fix. The probe runner now passes five of its
six probes; the sixth (a flat occupied span becoming its own noise floor)
is the known limitation already in BACKLOG.md and was left alone. The
long-DDC run holds 0.999999 steady through all 600 s. The workspace's own
suite went from 139 to 146 tests, all passing.

| # | Fix | Where |
|---|---|---|
| 1 | CIC integrators in wrapping two's-complement i64, scale chosen from the decimation so the output always fits (Hogenauer). Exact after thousands of integrator wraps. | `detect::ddc::Cic`, test `cic_output_is_exact_after_integrator_wraps` |
| 2 | The engine keeps a sample-indexed log of tuning epochs and holes (lost samples; ±10 ms settling around each retune), pruned to the ring. FFT rows average IQ of one epoch only and carry that epoch's tuning; the listener, pager/P25 monitors, RDS, record-all and the cell monitor stop at every hole and retune, reset their DDC and never look back across one; the classifier mixes with the tuner centre its IQ was taken at; wideband SigMF exports write one capture segment per tuning and annotate holes, channel exports use the tuning in force at the end of the stretch. | `engine::StreamState` (`run_from`, `continuous_from`, `segments`), `live_worker`, `listen`, `monitor`, `rds_monitor`, `record_all`, `cell_monitor`, `classify`, `sigmf`; test `stream_log_splits_at_retunes_and_losses` |
| 3 | AM squelch replaced by a phase-coherence carrier detector: mean unit phasor of the phase advance over a lag of 1/(2·cutoff), which is ~0 for noise however it was filtered and →1 for a carrier whatever its envelope. Calibrated through the real DDC at 10 and 2.5 MS/s: noise 0.14–0.20, carrier 0 dB 0.56–0.62, 6 dB 0.88–0.90, 100 % tone AM 0.92+. Opens above 0.5, closes below 0.35. | `detect::demod`, test `am_squelch_through_the_channel_filter` |
| 4 | `correct` refuses any repair of more than two bits over all 32 (the extended code only detects three). | `detect::pocsag`, test `rejects_three_errors_instead_of_guessing` |
| 5 | LoRa identification by dechirping: each (BW, SF) candidate is multiplied by its conjugate reference chirp and judged on symbol-length FFT frames (power in the top eight bins), so every mode is seen at its own time scale. Tested on all 18 modes at the classifier's channel rate, off centre, at 10 dB SNR; FSK, noise and carriers rejected. | `detect::chirp::lora`, tests `finds_every_lora_mode_with_noise_and_offset`, `fsk_is_not_a_chirp` |
| 6 | `SyncEntry.period` (DMR 144, P25 Phase 1 36, Phase 2 180, NXDN/dPMR 192, YSF 480, POCSAG 544 symbols; 0 for the unverified X2-TDMA and ProVoice entries); a second sync confirms only at a whole number of periods (±3 symbols + 0.5 %), missed frames allowed; the count is the largest grid-consistent set. | `detect::sync::best`, test `syncs_off_the_frame_grid_dont_confirm` |
| 7 | `Context.dc_spike` (from `Capabilities.if_arch`): the DC-offset label applies to zero-IF radios only. A spur-grid frequency is "Internal spur" at 0.9 only when the line also measures as a pure steady one, at 0.6 when merely unmodulated, and a modulated transmitter there keeps its modulation label. | `detect::context`, `engine::classify::Radio`, test `dc_and_grid_spurs` |
| 8 | Merged tracks keep the larger row count and rescale `snr_sum` to it (row-weighted mean of the two means), so the mean can't exceed the maximum. | `engine::bursts::associate` |

Also: `docs/review/run-probes.py` takes its rlibs from `cargo build
--message-format=json` instead of the newest file in `target/debug/deps`
(two feature sets of `num_complex` had been mixed), and the LoRa probe
calls `chirp::lora`.

Not done, and worth knowing:

- Nothing here was checked on air. The AM squelch thresholds, the retune
  guard (10 ms either side of the reported boundary) and the sync grids'
  tolerance are reasoned and tested on synthetic signals only.
- P25 Phase 1's 36-symbol grid assumes every frame is a multiple of 72
  bits (true of HDU, TDU, LDU, TDULC and TSDU); PDUs were not checked.
- The DDC stopband, the full-span noise floor, calibrated CFAR thresholds
  and the other items in "Algorithm suitability" above remain as written.
