# Backlog

Ideas and known rough edges that aren't scheduled yet. Newest at the top of each section.

## UI

- **Show the span click-to-tune will land on.** When hovering the survey (or the focus pane), draw the bracket the focus device would cover if clicked (centre ± usable bandwidth), not just a centre-frequency cursor.
- **Backfill the focus waterfall from survey history after a click-to-tune.** Copy the survey rows covering the new focus span into the focus waterfall so it doesn't start blank.
  - Survey rows are coarse (~54 kHz bins over 24–1800 MHz) and ~2.6 s apart. Stretch each one over the matching stretch of focus rows (≈130 rows at 50 rows/s) and resample the bins.
  - Draw the backfilled part dimmed or hatched so it doesn't pass for real focus data.

## Sweep / survey

- **Per-device flatness correction.** Faint vertical striping at hop boundaries: the R2's response isn't flat across each 9 MHz slice. Measure the passband shape per serial and divide it out.
- **Tighter retune guard.** The 9.5 ms guard is conservative (the USB transfer size is the main uncertainty). Finding the actual retune edge in the data could roughly double the hop rate.
- **Survey waterfall fills slowly** (one row per pass). Consider updating the newest row hop by hop as the pass progresses.

## Hardware

- **Airspy gain control in the UI.** Currently fixed at linearity 13/21.
- **Recover when a device is unplugged** (the `DeviceLost` event is defined but nothing reacts to it).
