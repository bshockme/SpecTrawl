# Classification review

Status: 2026-10-12. Why the labels in the emitter table and the exploration
log were wrong so often, what the record store showed when the classifier
was run on air with its evidence kept, and what was changed. The numbers
come from `testdata`-free runs on this machine's two Airspy R2s with
`cargo run --release -p engine-old --example trawl_db`; the stores are
`run1.db` (before) and `run3.db` (after) in the session scratch directory
(not kept),
and the same tables are now written by the app to
`Documents/SpecTrawl/spectrawl.db`.

## 1. How a label is made (the path under review)

1. The burst detector turns rows into **records** (`engine::bursts`): bins
   above floor + 7 dB, peak ≥ 10 dB, extent trimmed 26 dB below the peak,
   grouped into **emitters** by centre and width (`engine::emitters`).
2. The classifier thread (`engine::classify`) takes each record's IQ out of
   the ring, down-converts it to a channel (DDC cutoff 0.65 × width + 1 kHz,
   2.5 × width sample rate), and runs `detect::classify`:
   FLEX header → sync bank (DMR, P25, NXDN, POCSAG, …) → LoRa → GSM →
   feature labels (Carrier / nFSK / AM-OOK / Analog FM / Wideband / Unknown).
3. `detect::context::refine` renames by position: DC offset, the 5 MHz spur
   grid, the USB transfer tone, a close look at bare lines
   (`detect::carrier`), then the band plan ("Aviation AM", "Marine FM", …).
4. The emitter tallies every label; `Emitter::label()` is a sticky
   position label if any, else the most frequent protocol label seen twice,
   else the most frequent feature label.
5. The explorer (`engine::explore`) turns the focus side's emitter label
   into a **verdict** for a survey emitter: during a visit after a 2 s hold,
   or **in place** the moment a survey emitter inside a focus span has any
   label at all.

## 2. What the evidence showed

### 2.1 The old exploration log (2026-10-11)

`Documents/SpecTrawl/captures/exploration_log.csv`, 170 rows, all made
before the algorithm-review fixes. Read against the band:

| Band | Rows | What was logged | What it was |
|---|---|---|---|
| 88–108 MHz | 20 | "FSK 2400 Bd", "AM / OOK", "Wideband FM", "Internal spur" (703 kHz wide), "Carrier" (54 kHz wide) | FM broadcast stations and their fragments; a spur-adjacent blob |
| 108–137 MHz | 6 | "AM / OOK" 315–1509 kHz wide | humps of raised floor in the VOR/ILS band |
| 225–270 MHz | 16 | "Military air AM" at 33 kHz to 1.4 MHz wide, "Unknown" | AM voice channels are ≤ 25 kHz; the wide ones were floor |
| 274–280 MHz | 2 | "Internal spur" 1.9 MHz and 15.7 MHz wide | a narrow focus-side spur under a wide survey blob |
| 851–861 MHz | 60 | 36 "P25 Phase 1" with NACs; also "AM / OOK" ×6, "FSK 4800 Bd" ×3, "Analog FM", "P25 Phase 2" on channels later identified as Phase 1 | idle or weak looks at P25 channels, logged as if final |
| 800.003, 850.002, 949.999 MHz | 4 | "AM / OOK", "Carrier" | the radio's 5 MHz spur grid |
| 929–932 MHz | 6 | FLEX identified; "Carrier" 168 kHz wide | pagers; a carrier label on a wide record |

Every "classified" in-place row has dwell 0.0: the first label a focus
emitter ever had became the verdict, and only an *identified* verdict could
replace it.

### 2.2 The record store, before the fixes (run 1, 2026-10-12)

One Airspy stepped through nine tunings (60–90 s each) while the other
surveyed 24–1800 MHz. 1460 classification jobs. Per label, the **window
SNR** (the record's power density against the floor beside it, measured on
the wideband spectrum of the very IQ the classifier saw):

| Label | Jobs | Mean window SNR | Envelope CV | Note |
|---|---|---|---|---|
| AM / OOK | 365 | 5.6 dB | 0.49 | 297 of them at the 855 MHz P25 site, on 5–8 kHz records between the P25 channels; none above 8.6 dB |
| Carrier | 395 | 5.2 dB | 0.5 | 2 kHz lines; at 240 MHz nine fixed spots at tuner ± 96, 296, 392 kHz; at 98 MHz six spots 750 kHz apart |
| CW line (birdie) | 176 | 4.7 dB | 0.48 | the same lines after `carrier::verdict` |
| Aviation AM | 22 | 3.4 dB | 0.60 | 28 kHz records of floor at 127 MHz |
| Military air AM | 29 | 5.5 dB | 0.55 | 6 kHz records of floor at 240 MHz |
| Analog FM | 48 | 7.6 dB | 0.37 | NFM voice at 461.96 and 463.69 MHz was real (deviation 3 kHz, CV 0.33) |
| GSM (FCCH) | 7 | 13 dB | 0.26 | FM broadcast stations at 93.6 and 96.5 MHz |
| P25 Phase 1 / 2 | 99 | 22–31 dB | | all right, with NACs |
| FLEX 3200/4 | 21 | 19 dB | | right |

Complex Gaussian noise has an envelope coefficient of variation of 0.52
(Rayleigh), a one-hump frequency histogram and a few kHz of apparent
deviation. The fallback chain labelled exactly that "AM / OOK" (CV > 0.35)
or "Analog FM" (deviation 1–7 kHz) or "Carrier" (narrow, no levels). The
weak-signal classifications were not describing signals at all.

The window SNR runs 5–8 dB below the detector's per-row peak SNR for the
same records: the detector reports the best 20 ms row, the classifier sees
the whole window.

## 3. Findings and fixes

| # | Finding | Fix | Where |
|---|---|---|---|
| 1 | **Noise got modulation labels.** No feature label checked that the window held a signal. | The classifier thread measures each window's SNR on the wideband spectrum (24 FFTs spread over the window, density over the record's bins against the 20th percentile of the bins 0.75–3 widths either side) and passes it in; below 7 dB `detect::classify` returns **"Weak"** with no modulation label. "Weak" is not tallied on the emitter and never becomes a verdict. The SNR is kept in `Features::snr_db`, shown in the inspector and filed in the store. | `engine::classify::window_snr`, `detect::classify_with`, `detect::WEAK_DB` |
| 2 | **"AM / OOK" needed no evidence of AM.** Any varying envelope qualified. | A varying envelope now has to show one of two shapes: a carrier line ≥ 10 dB over its surroundings with voice-band envelope content (**"AM"**), or an envelope at two levels 12 dB apart with ≥ 70 % of samples near one or the other and a plain carrier while on (**"OOK"**; a TDMA burst is keyed but carries FSK, so it is excluded by its on-state deviation). Otherwise "Unknown". | `detect::am_evidence` |
| 3 | **A timing line alone made an "FSK n Bd" label.** AM and pulses put a line at their modulation rate too; FM broadcast fragments came out "FSK 2400 Bd". | A rate without clear levels is FSK only with a steady envelope (CV < 0.3). | `detect::classify_with` |
| 4 | **Image spurs and spur combs were classified one line at a time**, hundreds of times an hour, as "Carrier" / "CW line (birdie)" / "Military air AM". | After classifying a line (< 5 kHz) the thread asks the emitter book about its company: a line at the mirror frequency about the tuner centre → **"Internal spur (image)"**; three or more lines at one spacing (20 kHz–2.5 MHz) → **"Spur comb 750 kHz (6 lines)"**. Both are sticky labels. Records of an emitter with three sticky labels are no longer re-classified. | `engine::emitters::spur_context`, `engine::classify::pick` |
| 5 | **Band names applied to any width.** "Aviation AM" on 28 kHz of floor, "Military air AM" on 1.4 MHz. | Voice-service names (aviation, military air, marine, FRS/GMRS, NOAA) apply only to emissions ≤ 30 kHz wide. | `detect::context::refine` |
| 6 | **In-place verdicts were instant and final.** One classification of one record became "classified"; a later, better feature label never replaced it. | A feature label needs ≥ 2 classifications behind it (`Observation::votes`); an occupant counts as many. A verdict is logged again when the label changes at the same kind. | `engine::explore::determined_in_place` |
| 7 | **The focus emitter chosen for a survey emitter ignored shape.** A 2 kHz spur under a 15.7 MHz survey blob supplied "Internal spur" for the blob, then stuck. | `explore::observe` only takes a focus emitter whose width can account for the survey's (beyond its bin quantisation); otherwise the survey emitter is only "active" and the store records why. | `engine::explore::comparable` |
| 8 | **GSM was named on FCCH tone bursts alone**, and the emitter believed it after two hits. FM broadcast stations passed the test. | The protocol is named only when an SCH decoded. | `detect::classify_with` |
| 9 | **Nothing was kept.** Each wrong label could only be argued about from the CSV. | The record store (`engine_old::store`): records, every classification job with its window, SNR, features, sync count and decoded fields, emitter snapshots with their tallies, verdicts with the evidence they rested on, notes for every retune. SQLite, WAL, one writer thread. The app opens `Documents/SpecTrawl/spectrawl.db` at start; `trawl_db` runs headless into any file; `docs/review/classify_report.sh` summarises one. | `engine_old::store`, `crates/engine-old/examples/trawl_db.rs` |

## 4. Before and after on air

Same nine tunings, same evening, about ten minutes apart (run 1 before any
of §3; run 3 with all of it). Feature labels are the non-protocol,
non-"Weak" modulation labels.

Classification jobs per tuning. *Feature* = modulation labels given
without a protocol (the ones that were mostly wrong); *spur* = image and
comb labels; *proto* = sync-bank, FLEX, GSM identifications.

| Tuning | Jobs before → after | Feature before → after | Weak after | Spur after | Proto before → after |
|---|---|---|---|---|---|
| 98.0 MHz (FM broadcast) | 261 → 167 | 190 → 6 | 48 | 29 | 7 → 0 (the 7 were "GSM (FCCH)" on FM stations) |
| 112.0 MHz (VOR/ILS) | 15 → 67 | 3 → 26 | 17 | 0 | 2 → 0 |
| 127.0 MHz (airband) | 44 → 36 | 33 → 3 | 22 | 0 | 0 → 0 |
| 155.0 MHz (VHF land mobile) | 31 → 19 | 9 → 0 | 1 | 0 | 0 → 0 |
| 240.0 MHz (military air) | 326 → 95 | 289 → 9 | 29 | 37 | 0 → 0 |
| 280.0 MHz | 8 → 8 | 0 → 0 | 0 | 0 | 0 → 0 |
| 460.0 MHz (UHF land mobile) | 86 → 54 | 59 → 24 | 12 | 0 | 1 → 3 |
| 855.5 MHz (P25 site) | 517 → 493 | 394 → 13 | 268 | 1 | 99 → 136 |
| 930.0 MHz (pagers, ISM) | 172 → 160 | 68 → 33 | 46 | 2 | 37 → 41 |
| **All** | 1460 → 1099 | **1045 → 114** | 443 | 69 | 146 → 180 |

No feature label was given below the 7 dB gate in the after run. The
protocol count rose because the classifier stopped spending its budget on
spur lines and floor: at the P25 site it made 136 P25 identifications in
90 s instead of 99.

What the user sees, the confirmed focus emitters' labels:

| Before (run 1) | n | After (run 3) | n |
|---|---|---|---|
| AM / OOK | 31 | (no label yet) | 35 |
| Carrier | 20 | Unknown | 25 |
| P25 Phase 1 | 19 | P25 Phase 1 | 21 |
| Wideband | 13 | Wideband | 17 |
| CW line (birdie) | 10 | Internal spur (image) | 11 |
| (no label) | 7 | Carrier | 9 |
| FLEX 3200/4 | 6 | FLEX 3200/4, FLEX 1600/4 | 7 |
| Unknown | 5 | CW line (birdie) | 4 |
| ISM 915 Wideband / FSK 512 Bd | 9 | Spur comb 750 kHz / 49 kHz | 5 |
| GSM (FCCH) | 4 | P25 Phase 2 | 2 |
| Aviation AM | 3 | Wideband FM | 1 |

The 112 MHz row went the other way (3 → 26 feature labels): 21 "Carrier"
at 8.8 dB with a steady envelope (CV 0.33) on 3 kHz records. Those are
consistent with real carriers in the VOR/ILS band and were under the
detector's notice before; they are not known to be wrong.

Still questionable in the after run, each worth a capture:

- "Military air AM" ×5 at 240 MHz: 16 kHz wide, 7.6 dB, envelope CV 0.65,
  passed the carrier-plus-voice-band test. Plausible, unverified.
- "AM" ×7 at the P25 site (8.4 dB, 7.7 kHz) and ×5 at 930 MHz.
- A strong 9 kHz signal at 855.6375 MHz (33 dB) alternated between
  "Analog FM" (deviation 1.3–1.5 kHz) and "2FSK 2400 Bd" from one window
  to the next. It is an analog FM repeater (an industrial railroad
  channel; the repeaters there are known for
  a low background hum), so the FSK half was the error. A capture of it
  keyed up shows the idle carrier swinging ±1.2 kHz between two levels at
  134.4 bit/s with the pattern repeating every 23 bits: DCS (Digital Coded
  Squelch), which is what the hum is. Over 80 % of the discriminator's
  power sits below 150 Hz, the two-level swing gives a two-peak frequency
  histogram that the level counter reads as two FSK levels, and a
  spurious line in the squared derivative then named a rate. Fixed after
  run 3 in two steps: `Features::tone_hz` finds a single tone holding
  ≥ 50 % of the discriminator's power with no third harmonic, or a
  sub-audible band holding ≥ 70 %, and such a window is "Analog FM" before
  any FSK rule runs; then `detect::squelch` decodes the signalling itself.
  DCS: low-pass to 300 Hz, slice at the midpoint of the two levels, bit
  clock from the phase farthest from the threshold, ≥ 90 % agreement at a
  lag of 23 bits, majority-fold to one word, then the rotation that passes
  the Golay(23,12) check with the fixed 100 bits in place (the standard
  104-code rotation preferred). CTCSS: a standard tone holding half the
  50–300 Hz power and 13 dB over the band's median. The code is filed as
  the record's identity like a NAC. On the capture every window from 0.5
  to 1.5 s reads "Analog FM · DCS 205", voice windows included
  (`detect/tests/real_captures.rs`); on air (200 s at 855.5 MHz,
  2026-10-13) all 11 of its record and group windows did, where a first
  live run with the decode after the modulation rules had called four of
  eight "4FSK 2400 Bd" (the histogram sees four levels in DCS under
  voice). The published inversion pairs
  (023↔047, 205↔263, …) come out of the decoder, which pins the bit
  order and polynomial; which polarity radios call "normal" is taken as a
  one being positive deviation, unverified against a scanner. Not in the
  run 3 numbers.

## 5. What is still wrong or unknown

- **Weak signals get no label.** That is honest, but a persistent 5 dB
  emitter now shows "Weak"-less records and an empty label. The right next
  step is to classify the *emitter* over a longer window when it has been
  seen often (the group window already exists for bursty emitters; extend
  it to weak steady ones) rather than to lower the gate.
- **The window SNR is a band-average.** For a record wider than its signal
  (the detector's 26 dB trim on a strong neighbour's skirt) the SNR is
  diluted; for a record narrower than the signal it is fine. Records are
  more often too wide than too narrow, so the gate errs towards "Weak".
- **"Unknown" with a good SNR is the new residue.** Those are signals the
  feature set can't describe (digital modes without a sync entry, OFDM
  fragments, mixed windows). They are the candidates for the next detector
  work, and the store now says which they are.
- **Survey emitters still have no classification of their own.** The
  sweep's ring holds hops, not a continuous channel; everything the survey
  table shows comes from a focus receiver's verdicts. The in-place path is
  gated now, but it still depends on a focus receiver happening to cover
  the emitter.
- **Majority votes are unweighted.** A record classified on 0.25 s counts
  as much as one on 1.5 s. With "Weak" and "too short" excluded this
  matters less; weighting by analysed time would be the next refinement.
- **The 750 kHz comb at 88–108 MHz and the image pairs at ± 96 / 296 /
  392 kHz** are the radio's or the host's. They move with the tuner (images)
  or sit on absolute frequencies (comb); both are now labelled, neither is
  blanked. If they prove constant, blank them like the 5 MHz grid.
- **Captures from the earlier sessions are not on this machine**
  (`testdata/captures` is empty here), so `detect/tests/real_captures.rs`
  skips. The protocol path was checked on air only through the P25 and FLEX
  results above, which were unchanged.

## 6. Reproducing

```
# before/after runs (two Airspys, the second one surveying meanwhile):
cargo run --release -p engine-old --example trawl_db -- <serial> run.db \
  "98.0:60,112.0:45,127.0:45,155.0:60,240.0:60,280.0:45,460.0:60,855.5:90,930.0:60" "note"
cargo run --release -p engine-old --example trawl_db -- <serial2> run.db "24-1800:520" "survey"
docs/review/classify_report.sh run.db 1
```

Useful queries against any store:

```sql
-- labels given at low SNR (should only be Weak / protocol names)
select label, count(*), round(avg(snr_db),1) from classifications
 where protocol is null and snr_db < 7 group by 1 order by 2 desc;
-- why a verdict said what it said
select datetime(time,'unixepoch','localtime'), label, evidence from verdicts order by time desc;
-- an emitter's history
select datetime(time,'unixepoch','localtime'), kind, label, snr_db, analysed_s, sync_count
  from classifications where emitter_id = ? order by time;
```
