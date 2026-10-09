# Backlog

Ideas and known rough edges that aren't scheduled yet. Newest at the top of each section.

## UI

- **Show the span click-to-tune will land on.** When hovering the survey (or the focus pane), draw the bracket the focus device would cover if clicked (centre ± usable bandwidth), not just a centre-frequency cursor.
- **Backfill the focus waterfall from survey history after a click-to-tune.** Copy the survey rows covering the new focus span into the focus waterfall so it doesn't start blank.
  - Survey rows are coarse (~54 kHz bins over 24–1800 MHz) and ~2.6 s apart. Stretch each one over the matching stretch of focus rows (≈130 rows at 50 rows/s) and resample the bins.
  - Draw the backfilled part dimmed or hatched so it doesn't pass for real focus data.

## Sweep / survey

- **Survey detection is dense.** Every persistent emitter (TV, LTE, paging) becomes a long record, so the survey shows many boxes (now drawn faintly). Needs grouping by emitter and a "new since baseline" view rather than boxes for everything.
- **Hop profile is estimated live** (median across hops, smoothed across passes). It removes passband ripple and fixed-offset spurs. Consider saving it per serial and gain so a fresh sweep starts clean.
- **Sweep is now one hop per USB transfer** on an Airspy (retune as a transfer arrives, 3 ms latency, R820T lock flag for slow VCO searches): about 2x the old 9.5 ms guard. What's left:
  - About 6 real VCO searches per full pass (~13 ms each, at the divider boundaries near 28, 55, 110, 218, 442 and 883 MHz). Hop centres could be placed so these land on the wrap-around, or a 4th FFT skipped there.
  - Busy hops whose carrier level moves between FFTs (LTE 700) used to trip the settle check; now only a first-FFT outlier counts. Watch for broadband stripes coming back.
  - The band-power probe (`slow_hops`) remains for devices without a lock flag; untested on anything but the Airspy.
  - HydraSDR RFOne should have the same R82x lock flag; check when one is on hand.
- **Survey waterfall fills slowly** (one row per pass). Consider updating the newest row hop by hop as the pass progresses.

## Detection

- **Decoding next steps.** P25 NID (NAC/DUID, BCH-corrected), DMR slot type (colour code/data type, Golay-corrected) and NXDN LICH are decoded and checked against real captures (`detect/tests/real_captures.rs`). Next: P25 TSBK (control channel messages: system/site IDs, grants → follow voice), DMR CSBK, DMR EMB (colour code on voice bursts), NXDN SACCH/RAN, P25 Phase 2. Also confirm the NXDN direction bit against a known transmitter.
- **Per-protocol re-slicing.** The prefilter cutoff is chosen before the protocol is known (4–6.5 kHz). Weak DMR improved further at 4 kHz (22 → ~50 slot types), so re-slice with a protocol-specific cutoff once identified.
- **Features are weak on real signals.** On hardware, FSK level counts and symbol rates often come out blank even where the sync bank confirms a protocol (NXDN48 at 460 MHz with ×8–18 syncs, DMR at 855.59 with ×15–33). Deviation also reads high (noise, discriminator clicks). Once a sync is found, use its timing to slice symbols properly (levels set per sync) and measure levels/deviation from that, instead of a raw histogram.
- **TDMA single-slot traffic fragments into many short records** (e.g. 47 records in 45 s at 856.3375), each too short to hold two syncs, so they stay unidentified. Group records by channel/emitter and classify the group, or lengthen the hangover for slotted signals.
- **Unverified labels:** ProVoice (binary sync, 2-bit tolerance) seen ×3 at 854.71 MHz, and the X2-TDMA patterns. Treat as provisional until checked against a known transmitter.

- **Bandwidth of FM broadcast/voice varies with modulation**, so a record's extent is the union over its lifetime. Fine for now; a per-row occupied-bandwidth average would be steadier.
- **Wideband signals that fill a floor block** (e.g. an LTE carrier filling most of a 10 MHz focus span) raise the floor estimate and can go undetected. Needs a floor that also looks at neighbouring blocks or history.

## Data sources

- **RadioReference database import** (as SDRTrunk does): download a trunked system's sites, frequencies, band plans and talkgroups by system ID. RadioReference's database web service (SOAP) needs the user's **Premium** subscription credentials plus an **application key** the software author requests from RadioReference. Plan: a settings page for the RR login (stored with the OS credential store, not plain text), "Import from RadioReference…" by system ID or by matching a decoded WACN/SYSID, merged into the same catalog as the DSD+/trunk-recorder imports. Check RR's terms on caching and redistribution before storing the data.

## Hardware

- **Manual Airspy gain stages** (LNA/Mixer/VGA individually). The UI has the linearity/sensitivity presets and AGC.
- **tinySA Ultra support** (on hand). A swept spectrum analyzer, not an SDR: no IQ, so no look-back or decoding. Three uses:
  - **Calibration reference:** reads in dBm, so measuring the same signal on both gives per-device dBFS→dBm offsets for the Airspys.
  - **Test source:** its signal-generator output gives known tones to measure Airspy gain tables and passband shape, and to test detection repeatably.
  - **Survey outside the R2's range:** about 100 kHz–6 GHz (HF below 24 MHz; 1.8–6 GHz for cellular, Wi-Fi, drone links).
  - Needs a second kind of source, a *power-sweep device* producing spectrum rows directly (USB serial text protocol). The Survey role and burst detector already work on rows. Check its COM port and firmware commands before writing it.
- **R2s hear each other's local oscillator.** Each R2 radiates a little LO energy out of its antenna port (strongest at the top edge of its span, centre + 5 MHz, plus weaker spurs); the other R2 picks it up even 10 ft away. Confirmed by swapping roles. The UI hides survey detections inside the Focus span (the focus is the better witness there). Better: model each device's known spur offsets and mask them wherever the *other* device sweeps, and suggest a few dB of attenuation or a filter on the survey antenna.
- **Two R2s on one USB bus interfere.** With both R2s sharing a hub, the sweeping one put a dense comb of narrow carriers into the other (856.6–859.5 MHz). On separate buses it's gone. Worth a note in docs and maybe a warning when two Airspys share a hub.
- **A second Airspy sometimes can't be opened while the first streams** (`AIRSPY_ERROR_NOT_FOUND`), seen in the `classify_sim` example but not in the app. libairspy opens each device in turn to read serials; the exact trigger isn't known yet.
- **Recover when a device is unplugged** (the `DeviceLost` event is defined but nothing reacts to it).
