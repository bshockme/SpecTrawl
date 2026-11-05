# Signal discovery: what other projects do, and what SpecTrawl took from them

Status: 2026-10-13, updated 2026-11-04. A survey of the blind detection and identification
techniques in open-source discovery tools, read from their source, and the
refinements made to SpecTrawl's detectors on the strength of it, checked
on the air here with the record store. Follows
`classification-review.md`.

## 1. The survey

Sources were read in code, not from descriptions. Thresholds are the
tools' own.

**SigDigger / Suscan / sigutils** (`sigutils/src/sigutils/detect.c`,
`suscan/analyzer/estimators/{fac,nonlinear}.c`,
`analyzer/spectsrcs/*.c`). Channel discovery: an exponentially averaged
PSD (10 s window), the noise floor as the mean of the bins whose running
min/max bracket the previous floor, a channel = bins over 2 × N0 (+3 dB),
valid only if its SNR is ≥ 6 dB and it is wider than 10 Hz; centre by
phasor centroid, bandwidth as power-equivalent width. Emitter persistence:
a channel's centre and width move with weight 1/(age+1), it is dropped
when `age > 2 × present`. Baud: two estimators in parallel, the first
valley of the autocorrelation (FAC) and the spectral line of |Δx|²
("nonlinear"); the line must stand **10 dB over the mean of the nearest
local minima either side**, else no rate ("we prefer to fail"). Carrier:
Costas loops for BPSK/QPSK/8PSK, Gardner timing. The "exp-2/4/8" spectrum
sources show the PSD of (x/|x|)^M, which is the PSK-order test; there is
**no automatic modulation classifier** (ORDER_ESTIMATION is an unused
enum).

**gr-inspector** (`lib/signal_detector_cvf_impl.cc`, `lib/ofdm_zkf_c_impl.cc`).
Threshold from the largest jump in the sorted averaged PSD (a two-cluster
split), signals as contiguous bins above it, bandwidth quantised. OFDM by
Zivkovic–Kattenbach: the autocorrelation peak at the useful-symbol lag,
rounded to typical FFT sizes, then the cyclic prefix from the spectrum of
the time-variant autocorrelation at that lag. Its ML classifier is dead.

**Universal Radio Hacker** (`src/urh/ainterpretation/AutoInterpretation.py`).
Noise from the quietest 1 % chunks of the envelope; OOK when the envelope
holds exact zeros or every variance is tiny; ASK when the envelope's
wavelet variance is 1.5× that of the normalised signal; PSK when it is 10×
that of its median-filtered version; else FSK if a second FFT peak sits
≥ 10 bins from the first. Bit length by the histogram of plateau lengths
and a divisor count with 20 % tolerance.

**rtl_433** (`src/pulse_detect.c`, `pulse_detect_fsk.c`, `pulse_analyzer.c`).
OOK slicer at the midpoint of a slow idle-only noise tracker and a fast
in-pulse high tracker, ±12 % hysteresis; FSK only inside an OOK envelope,
by a two-frequency tracker. The `-A` analyser histograms pulse, gap and
period widths at 20 % tolerance and names PWM / PPM / Manchester / NRZ
from how many bins each has.

**SDRTrunk, OP25, DSD / DSD-FME.** All sync-driven, none blind. OP25
needs an exact sync match to acquire a protocol and allows ≤ 2 errors only
once locked, dropping the lock when syncs stop arriving on the frame
count. SDRTrunk's DMR soft sync: a dot product of the last 24 soft symbols
with each sync, candidates ≥ 60/216, confirmed ≥ 95/216 after a timing
search, and the mode switches only when one sync type leads by more than
10. DSD compares dibit strings exactly.

**radiosonde_auto_rx** (`autorx/scan.py`, `scan/dft_detect.c`). A
`rtl_power` sweep, floor = median, peaks ≥ floor + 10 dB, snapped to a
10 kHz grid, blacklist and whitelist, each candidate confirmed by a
matched-filter correlation against the sonde's header (0.6–0.8
normalised, plus an allowed header-bit-error count).

**Wideband scanners.** `rtl_power` crops 20–50 % of every tuning window;
`hackrf_sweep` keeps only two quarter-band pieces of each 20 MHz FFT, so
the DC centre and both edges are always discarded; SDR++'s scanner is a
fixed level on the max bin; gr-mesa is a fixed squelch on a max-hold
spectrum with a duty-cycle gate and a hold-up time. Nothing surveyed
baselines LO-relative spurs or images automatically.

**Textbook blind features** (Azzouz–Nandi; Swami–Sadler cumulants).
γmax (the spectral peak of the normalised envelope, threshold ≈ 2–4)
splits constant-envelope FM/FSK from ASK/PSK/QAM; PSK order from the
nonlinear phase; cumulants C42 = −2 (BPSK), −1 (QPSK), −0.68 (16-QAM)
need symbol-spaced, carrier-locked samples, which a discovery tool
doesn't have. **ML** (TorchSig, RadioML): datasets and research blocks;
no open discovery tool ships one.

## 2. What SpecTrawl took

The run before (`base.db`) and after (`after1.db`, `after2.db`) were made
with `trawl_db` on the same eleven tunings (98, 112, 127, 155, 240, 460,
855.5, 930, 915, 152, 165 MHz, 45–120 s each) on the evening of
2026-10-13, with the second R2 surveying 24–1800 MHz and then capturing
channels for an offline corpus (`capture_channel`; FM stations at 93.7,
96.5, 97.7, 99.1; the P25 site; FLEX at 929.6125 / 931.3585; a 1 MHz
stretch of 902–928; `scratchpad/onair/corpus`, not kept).

### 2.1 The IF filter's edge was an emitter at every tuning

Every tuning grew a 250–360 kHz "Wideband" emitter at the tuner ± 4.35 MHz
(102.34 at 98, 107.64 and 116.35 at 112, 131.32 at 127, 150.63 and 159.33
at 155, 464.37 at 460, 934.38 at 930 …), envelope CV 0.7, no window SNR.
A 20 s mean spectrum at 280 MHz shows the Airspy's IF filter falling 14 dB
within 200 kHz past +4.45 MHz, and 6 dB past −4.6 MHz. The detector's
noise floor is the 20th percentile of 1024-bin blocks; the last block
spans the cliff, so its percentile sits far below the real floor and the
last 100 kHz inside the usable edge reads as a signal. This is the
`rtl_power` / `hackrf_sweep` lesson (crop the edges) applied to the floor
estimate rather than the search: bins outside the usable fraction are
now left out of every block's percentile (`BurstDetector::update_floor`).
After: no edge emitters at any tuning.

### 2.2 FM broadcast by its pilot

Stations and their fragments read "Wideband", "Unknown", "FSK 2400 Bd"
(a 136 kHz fragment at 14.6 dB, nine times) and "Analog FM". The DDC cut
of a record narrower than the station lets the envelope ripple, so the
constant-envelope test missed most of them, and the squared derivative of
a discriminator carrying a 19 kHz pilot has lines the rate rules read as
FSK. `blind::fm_pilot` looks for the pilot in the discriminator (a line
within ±25 Hz of 19 kHz, 15 dB over the 17–21 kHz median in both halves
of the window) and RDS at 57 kHz; it runs first for any record ≥ 30 kHz
wide. On the corpus every window of all four stations reads "FM broadcast
(stereo)", 97.7 with RDS; on air at 98 MHz: 19 windows "FM broadcast" and
no "FSK 2400 Bd" on stations ≥ 50 kHz wide (one 34 kHz fragment still
did, which is why the bar went from 50 to 30 kHz for `after2`).

### 2.3 Blind family estimators (`detect::blind`)

The sigutils / gr-inspector techniques, with sigutils' significance rule:
a line counts only when it stands 15 dB over the periodogram's median in
both halves of the window at the same frequency (the peak of a flat
periodogram over N bins is already ln N over its mean).

- `envelope_rate`: the cyclic line of |x|² (symbol or pulse rate of any
  shaped linear modulation; constant-envelope FSK has none).
- `psk_order`: the carrier line in x², x⁴, x⁸ (BPSK / QPSK / 8PSK); not
  when x itself has a line there in both halves (a carrier, AM, FSK
  tones), nor when the power spectrum holds a partner line within 6 dB
  (FSK raised to a power leaves a pair at M × (offset ± deviation)).
- `ofdm`: the cyclic-prefix autocorrelation peak at the useful-symbol
  lag (20 µs–6 ms), isolated from the short-lag correlation every
  band-limited signal has.
- `burst_period`: the first envelope-autocorrelation peak after its first
  dip (slots, frames), needing three repeats in the window.

Wired into the fallback chain after the FSK rules: PSK → OFDM → AM / OOK
→ "Bursts every n ms" → Unknown. On air these gave few labels (a
synthetic-tested path; nothing local is plain PSK), and the first cut gave
wrong ones: "Bursts every 6 ms" on 0.1 s hop bursts (a shoulder of the
envelope's own correlation, fixed by requiring the dip and three
periods), "BPSK" on FLEX 1600 and on hop bursts at 10–14 dB (fixed by the
partner-line rule and by asking both halves of x for its own line).

### 2.4 Positive AM evidence was still too easy

Idle pager channels read "AM" (16 windows at 930 MHz, 9 dB): a keyed FSK
transmitter near the edge of the record has a strong line and envelope
content in the voice band. AM voice now also needs the line near the
channel centre (within 8 % of the channel rate), holding ≥ 50 % of the
on-signal power (a sine at full modulation gives two thirds; the pager's
strongest line held 24 %), and a steady instantaneous frequency while the
envelope is up (90th percentile of its smoothed swing < 400 Hz; FSK, FM
and keyed data move by kilohertz). Corpus: the three pager captures went
from 23 / 36 / 11 "AM" windows to none, FLEX identifications unchanged.

### 2.5 Emitter labels weighted by analysed time, modulation first

Votes were unweighted: a 0.17 s look counted as much as a 1.5 s one. The
emitter now keeps seconds analysed per label (`Emitter::weight`), picks
the heaviest, and lets a label that describes a modulation beat "Carrier"
/ "Unknown" / "Wideband" once it holds a quarter of the time (an idle
transmitter is a carrier most of the time; it is the talking that says
what it is). 99.09 MHz [FM broadcast ×4, FSK 2400 ×3] now reads FM.
461.9632 MHz stays "Carrier": its four "Analog FM" windows total 0.69 s
against 5.7 s of carrier, and 13 "Weak"; whether it is a voice channel
with an idle carrier or something else needs a listen.

### 2.6 Combs need persistent members; line labels lapse on wide emitters

Spur-comb labels (sticky) landed on brief narrow blips at the P25 site
("Spur comb 25 kHz (8 lines)": the channel grid) and on the sidelines of
the 902–928 hoppers ("Spur comb 24 kHz (14 lines)"), and a FLEX channel
whose first record was 3 kHz wide kept "Spur comb 325 kHz" after growing
to 15 kHz. Comb members must now have ≥ 6 records (the radio's 750 kHz
comb lines had 12–40 in a minute, the false ones 1–5), and a line label
stops applying once the emitter is wider than 1.5 × a line. Since
2026-11-03 members must also be on the air at least half the time: busy
P25 site channels passed the record count and still made a "comb" of the
channel raster.

### 2.7 Frequency hoppers as one thing (`HopCluster`)

**Removed 2026-11-03.** A two-receiver run (both R2s on one antenna through
the multicoupler, same stops at the same time) showed the rule firing far
outside the meters: conventional NFM at 461.963 MHz, the P25/Cap+ site at
851–860 MHz, FLEX at 929–931 MHz ("187 channels") and 240 MHz were named
"FHSS hop", and which receiver named an emitter so was chance. Nothing in
the rule checked that anything hopped (members on one at a time, in quick
succession), and an emitter whose only record was still open averaged 0 s
(`on_time_s` grows only on close), so every long transmission briefly
qualified ("0 ms bursts"). The meters go back to separate emitters. The
original design follows for the record.

The 902–928 MHz meters hop: ~90 ms bursts of 15–40 kHz every 20 s per
transmitter, landing on a different emitter each time (195 emitters at
915 MHz in a minute, nearly all "Unknown"). No surveyed tool groups them;
rtl_433's width-histogram idea applied at the emitter layer does: emitters
whose records average ≤ 0.3 s, 5–80 kHz wide, unidentified and heard
within 3 minutes are chained when within 400 kHz of each other; a chain of
≥ 8 with consistent widths is a cluster, and each member's label becomes
"FHSS hop (912.4–918.4 MHz, 23 channels, 90 ms bursts)" ahead of its
feature votes. Recomputed at most every 2 s on record close. A trunked
site's slotted channels look alike until their syncs name them (seconds),
and two or four of them never reach eight.

### 2.8 Symbol-rate line significance

`features::symbol_rate` now also applies sigutils' rule: the line must be
10 dB over the mean of its own nearest local minima (a broad hump has
minima far down its sides but not far below its top).

### 2.9 Before and after, in numbers

Same eleven tunings, 2026-10-13 21:44 (`base`) and 22:15 (`after2`), the
classifier's window labels (protocol identifications vary with traffic:
P25 215 → 184 windows, FLEX 44 → 35, DMR 3 → 6, nothing lost to the
changes). The after run was made before §2.3's last fix (the period label
now needs a 0.3 s window) and the hop clusters were formed on it.

| Window label | Before | After |
|---|---|---|
| "Wideband" (FM stations, edge blobs) | 129 | 0 |
| Edge blobs at tuner ± 4.35 MHz, > 200 kHz wide | 126 | 1 |
| "FM broadcast (stereo)" / "(stereo, RDS)" | 0 | 31 / 8 |
| "AM" | 19 | 0 |
| "Analog FM" | 7 | 10 (+5 "ISM 915 Analog FM") |
| "Unknown" | 123 | 109 |
| "Carrier" / "ISM 915 Carrier" | 73 / 52 | 82 / 78 |
| "Weak" (gated, not tallied) | 620 | 542 |
| "Bursts every n ms" (hop bursts, since gated by window length) | 0 | 10 |

Emitters at the 915 MHz stop: 195 confirmed before, nearly all "Unknown"
or "ISM 915 Carrier"; after, 29 of them read "FHSS hop (910.8–912.0 MHz,
13 channels, 79 ms bursts)" and its sister clusters, the rest being the
sidelines and lines that are not bursts.

## 3. Not taken, and why

- **sigutils' floor** (mean of bins bracketing the previous floor):
  SpecTrawl's percentile-of-blocks with the edge fix behaves; revisit if
  humps of raised floor keep splitting.
- **URH's wavelet-variance modulation split**: its OOK branch is a noise
  trap (every variance small → OOK), which is exactly the failure the
  window-SNR gate was added to stop.
- **Cumulants / ML**: need symbol timing and carrier lock, or a model and
  training set; the sync bank plus the lines above cover what is on the
  air here.
- **OP25-style sync tolerance** (exact to acquire, 2 errors once locked):
  SpecTrawl's two-sync-on-the-frame-grid rule is already stricter for
  acquisition.

## 4. Open

- The "Bursts every n ms" and PSK/OFDM labels have only synthetic
  positives; a known PSK or OFDM transmitter in view would settle their
  thresholds. HD Radio sidebands (OFDM, 363 Hz subcarriers) are the local
  candidate once the DDC takes a 70 kHz sideband on its own.
- 461.9632 MHz: resolved 2026-11-03 once one CRC-checked frame was enough
  to name a protocol: it is DMR, colour code 9.
- 463.3755 MHz read "Analog FM" then "DMR BS data CC 1" (three CSBKs
  passing CRC at 8–9 dB on a 6.7 kHz record): probably a shared business
  channel with both analog and DMR users; unconfirmed.
- The 902–928 MHz meters are separate emitters again (2.7). Grouping them
  needs proof of hopping (members on one at a time, in quick succession)
  checked on two receivers.
