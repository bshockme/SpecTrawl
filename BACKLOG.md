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
- **Tighter retune guard.** The 9.5 ms guard is conservative (the USB transfer size is the main uncertainty). Finding the actual retune edge in the data could roughly double the hop rate.
- **Survey waterfall fills slowly** (one row per pass). Consider updating the newest row hop by hop as the pass progresses.

## Detection

- **Bandwidth of FM broadcast/voice varies with modulation**, so a record's extent is the union over its lifetime. Fine for now; a per-row occupied-bandwidth average would be steadier.
- **Wideband signals that fill a floor block** (e.g. an LTE carrier filling most of a 10 MHz focus span) raise the floor estimate and can go undetected. Needs a floor that also looks at neighbouring blocks or history.

## Hardware

- **Manual Airspy gain stages** (LNA/Mixer/VGA individually). The UI has the linearity/sensitivity presets and AGC.
- **Recover when a device is unplugged** (the `DeviceLost` event is defined but nothing reacts to it).
