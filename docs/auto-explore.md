# Auto-explore: unattended visits to survey emitters

Status: design, 2026-10-10; nothing implemented. Decisions here came out of
a discussion on 2026-10-10; the open points are in §8.

## 1. What it is

The survey keeps finding emitters it cannot name (54 kHz bins, ~2.6 s
between passes). Auto-explore gives a focus receiver a work queue of those
emitters and has it visit them in turn: tune, dwell, classify and decode
until it reaches a verdict, write the verdict down, move on. The result is
a survey table that fills with names over time, and a log of what was
visited and concluded.

It is a **mode of a focus receiver**, set per receiver in its pane header:
*Manual* (today's behaviour), *Explore*, and later *Scanner* (the
super-scanner in `BACKLOG.md`, which is Explore with "stay until it ends"
and audio on). Two receivers may both explore; they share one queue.

## 2. The loop

```
while mode == Explore:
    e = queue.pop()                     # §4
    tune(e.centre, span to cover it)
    wait up to 1 s for e to appear in the focus span
    dwell:                              # §5
        run the classifier on e's channel
        if a decoder exists for the label, run it until it yields an ID
        stop on verdict, on 3 s of silence, or at the cap
    record verdict                      # §6
    (optional) save a channel capture   # §6
```

A click-to-tune on an exploring receiver, or any manual retune, drops it
back to *Manual*. Switching back to *Explore* resumes the queue.

## 3. Verdicts

Every visit ends in exactly one:

| Verdict | Meaning | Logged? |
|---|---|---|
| identified | a decoder produced an identity: P25 system/site/NAC, DMR colour code and Cap+ repeater, NXDN RAN, FLEX frame headers, LTE cell, RDS station name (once RDS exists) | yes |
| classified | a stable label without an identity (analog FM voice, 4FSK 4800 Bd, AM, unknown digital) | yes |
| unknown | present for the whole dwell, no stable label | yes |
| gone | not present during the dwell | no; bumps the emitter's retry count |

The emitter's label in the survey table becomes the latest verdict's label
and ID, as classification does today. *Gone* three times in a row drops the
emitter to the back of the queue with a long retry interval.

## 4. The queue

One queue per engine, shared by all exploring receivers, rebuilt whenever
the survey's emitter list changes. Candidates are confirmed emitters that
pass the filters; order is:

1. **Active now** (seen in the latest survey pass) before idle.
2. **Unlabelled or weakly labelled** before labelled.
3. **New since baseline** before old.
4. **Longest since last visit** first; never visited counts as longest.
5. Emitters **inside another focus receiver's span** are de-prioritised
   (moved behind everything else that is active), not excluded. See §7
   for why they usually never reach the explorer at all.

An emitter being visited by one explorer is skipped by the other.

### Filters (settings, saved in the workspace)

- **Ranges**: only emitters inside user-drawn ranges of the survey span
  (one or more segments), or anywhere if none are drawn. The same segments
  the segmented survey mode will use.
- **Band presets**: exclude, de-prioritise, or restrict to named presets:
  FM broadcast, TV, LTE, pagers, aviation, land mobile, amateur, ISM,
  satellite. A preset is a list of ranges plus, where the spectral
  identifier already names the occupant (FM, TV, LTE), that label. Default:
  exclude FM broadcast, TV and LTE.
- **Priorities**: a preset or range can also be *prioritised*, which puts
  its emitters at the front of the queue regardless of the ordering above.

## 5. Dwell

Adaptive: stop at the first verdict, leave after 3 s of silence, cap at
15 s. The cap exists for slotted or bursty traffic that never settles; the
silence rule is what makes a run over a mostly idle band quick. A fixed
dwell can be added as a setting if the adaptive one proves annoying.

While a receiver explores, its pane shows the explorer state above the
spectrum: the emitter being visited (frequency, bandwidth, why it was
chosen), dwell remaining, the running classification, and queue length.

## 6. Output

**Exploration log**, a new bottom tab, one row per visit with a verdict
other than *gone*: time, receiver letter, frequency, bandwidth, verdict,
label and ID, dwell length, a link to the capture if one was saved.
Clicking a row tunes a manual receiver to it. The log is kept in memory and
appended to a CSV next to the emitter exports.

**Capture** is off by default. When on, it saves a channel capture
(`sigmf`, the receiver's DDC at the emitter's bandwidth) for visits ending
in *unknown*; a second setting extends that to every visit. Captures use
the usual export path and naming.

## 7. Emitters already in view

If an emitter lies inside a focus receiver's span, in Manual or Explore,
the engine already has its IQ: the classifier runs on it there, without
any retune. So those emitters get their discovery done in place and are
written to the log and the emitter table as if visited. That keeps the
explorer from spending dwells on things another receiver is parked on, and
is the reason §4 only de-prioritises them rather than skipping them: by
the time one reaches the front of the queue it has usually been determined
already and no longer qualifies.

## 8. Open points

- Whether *classified* visits should be revisited later in the hope of an
  identity (a patrol schedule), or only on request.
- Audio: Explore is silent. Scanner is where audio and "stay on current
  until it ends" belong; its rule set (newest, most signals, stay) is in
  `BACKLOG.md`.
- Thumbnails: a small waterfall image per log row would help recognising
  signals later; cheap to keep in memory, needs a decision on persisting.

## 9. First cut

Explore mode on one or more receivers, the shared queue with range and
preset filters, adaptive dwell, verdicts, the log tab, in-view
determination (§7). No capture, no priorities, no patrol. Capture and
priorities are small additions once the loop works.
