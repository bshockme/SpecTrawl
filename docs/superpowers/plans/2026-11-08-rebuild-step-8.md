# Rebuild step 8: presentation slots, the inspector, the catalogue

**Spec:** `docs/superpowers/specs/2026-10-22-spectrawl-rebuild-design.md` §5 (slots and rules) and §6 step 8. `docs/ui-design.md` stays the reference for the panes, docking, pop-outs, workspaces and the palette (they are carried over, spec §5: "they were not the problem"). **New for this step:** the owner's house guide, `~/auggiesoft-ui-guide` (look: Instrument). The owner allows deviations from it where they are justified (2026-11-08); each one is listed in `docs/ui-design.md` §0 with its reason.

**Order** (the guide's ADOPTING §4: labels and bugs, then colour roles, then layout). Each task is a commit that leaves the app usable, with its tests.

## Task 1: Adopt the guide (look, type, colour roles)

- `docs/ui-design.md` gains §0 "House guide": the look (Instrument everywhere; no Plain page yet), how SpecTrawl's colours fill the six roles, identity colours, the colour map, project voice (none), and the deviations below.
- `theme.rs` takes the Instrument tokens: bg `#111214`, panel `#1c1d21`, header `#16171a`, plot well `#000000`, line `#2c2e33`, ink `#e6e7ea` / `#a9abb1` / `#6c6e74`, accent `#5aa9f0`, caution `#f0a830`, alarm `#ff4d4d`, ok `#3ccf6e`, live `#ff5cc8`, info `#9fd3ff`, trace `#5cc8e8`, graticule `#33363c`, house gold `#c9a84c`.
- Role fixes:
  - Selection (records, boxes) moves from amber to the accent (C5, C6). Amber is caution only.
  - Known wideband occupants, voice activity and "new" stop being green or violet state-like fills; they take identity colours (`--id-*`, no red/amber/green/magenta) with a word beside them (C1, C2).
  - The open-record and closed-record boxes stay ink outlines; the selected one is the accent.
  - "LOSING n% OF SAMPLES" and device lost are alarm, worded; nothing else in a healthy screen uses caution, alarm or live (C3).
- The waterfall colour map becomes the Instrument inferno (`#000004 → #420a68 → #932667 → #dd513a → #fcffa4`), with the floor and ceiling the dB-range controls already expose (C9).
- Type: Barlow (UI), Barlow Semi Condensed 600 uppercase with 0.08 em tracking (labels, column headers, section titles), Auggie Mono (every frequency, level, count, time and identifier). The guide ships woff2; egui needs TTF/OTF, so the fonts are decompressed with fonttools into `crates/app/assets/fonts/` with both OFL licences. egui's default fonts stay as fallbacks for glyphs the subsets lack (`⏷`, `▸`, `★`, `⊘`).
- Units are quieter than values: one helper (`theme::value_unit`) draws `851.0125 MHz` with the unit in ink-2, one step smaller, a thin space between.
- The gold thread: the wordmark "SpecTrawl" top-left in gold (Task 1), and a 2 px gold underline under the active page in the top bar's page bar (Task 8). Nowhere else.
- Test: a theme unit test checks the role constants against the guide's values and the contrast floors (T1: ink and ink-2 ≥ 4.5:1 on panel and bg); `readme_shots` re-renders the screenshots.

## Task 2: The frame (top bar, status bar, log)

- Top bar: wordmark, then the workspace/preset picker and `View ⏷`, settings on the right. It never scrolls away.
- Status bar under it (spec §5 "Status line", guide A1/A3): one traffic-light tile (OK / `!` / critical, with a word) summarising every receiver's `Status.health`, the pool (dropped, missed), bus shed, and lost samples; then the conditions as sentences, each saying what to change ("RFOne: losing 12 % of samples. Move it to a USB port on another controller."). A condition stays until it clears. Clicking the tile opens the Log filtered to conditions.
- Log slot: the engine narrates every retune, every consumer started or shed, and every claim that changed a record's or emitter's headline, with the reason (spec §5). Engine side: `LogEntry` gains a `kind` (Retune, Consumer, Headline, Condition, Other); the records module logs headline changes.
- Test: an app test with a snapshot carrying a Warning receiver shows the tile in caution with its sentence; an engine test sees a headline-change log line when a decoder's claim replaces a sync-bank headline.

## Task 3: Selection and the inspector

- `Selection { None, Receiver, Record, Emitter, System, Channel, Decoder }`, set by a waterfall box, a table row, a pane header, a system or channel row. Records, emitters, systems and channels highlight the selection with the accent (L7: views that share a selection share it).
- The inspector pane (spec §5): for a record, the headline claim, then every claim with its evidence kind, confidence and numbers (decoded solid, sync-bank with its counts, inferred muted with the word "inferred"); the features (bandwidth, SNR, duty, envelope CV, and the symbol rate and FSK levels once `Evidence::Features` carries them); the attached decoder's lamp, sentence and counts; replay, save, tags and notes (moved from the records pane's detail). For an emitter: its headline and how it was reached, its records, activity and duty. For a system or channel: its identity fields, control channel, records. For a receiver: device, mode, centre, gain, ring seconds, stage and pool load.
- **Why this name:** the record view says which claim won the headline and why the others lost (rank and confidence), because classification mistakes are the owner's main frustration: a wrong name should be explainable from the inspector alone.
- `Evidence::Features` gains `symbol_rate: Option<f64>` and `levels: Option<u8>` (serde default), filled where the modulation rules measured them.
- Test: the kittest harness selects a record in the records pane and finds its claims in the inspector; a record with a band-plan claim shows "inferred".

## Task 4: Events and occupants

- The Records table shows events by default (spec §5): records shorter than 60 s that are not part of a known wideband occupant. A toggle shows all.
- An Emitters tab lists occupants and emitters: headline, frequency, records, duty, first and last seen, with stale ones ghosted and their age (D5, D7).

## Task 5: Telemetry and health

- `DecoderView` gains the common block (spec §4.6): `snr_db`, `level_db`, `cfo_hz`, and keeps `blocks_ok`/`blocks_bad`.
- The health slot: three small plots per selected decoder or receiver (SNR, frames ok %, bus lag), each with its current value, unit and time window in words (D1, D7), kept in the app as a short history.

## Task 6: The thing per domain, and the catalogue

- PAGES for pager monitors (capcode, protocol, channel, time, text, damaged marker). The text is shown live in the app only; tests use the synthetic slice and screenshots never show the Pages tab with real traffic.
- The catalogue (spec §5): every mode the engine can name or decode, grouped by domain (trunking, land mobile, aviation, pagers, broadcast, cellular, other), each with a one-line blurb, whether it decodes or only names, and a count of records and emitters it named in this session; search. Engine side: a static registry in `engine::decoders` (and the sync bank's protocols), published once in the snapshot.

## Task 7: Plots (overlay rules)

- The spectrum's smoothed trace carries a peak envelope (D3).
- Waterfall time in the left gutter (relative ticks ≥ 48 px apart, clock time at the top) and a hairline with a `retune` tick at every retune (O13).
- The band plan as a 20 px strip on the frequency axis, labels elided to fit (O14), not a wash.
- Labels on boxes move to chips with a backing (O2); collisions collapse to `+N` (O5).

## Task 8: Workspaces, pop-outs, palette and keys (carried over)

- The top bar's page bar (guide L3, signature §2–3): the workspace presets in workflow order, Survey · Trunking · Paging, then the saved workspaces; the active one carries the 2 px gold underline.
- From `app-old` and `docs/ui-design.md` §5–7: named workspaces and `last.json`, three presets (survey, trunking, paging), Reset layout; pop-out windows with their own docks; Ctrl+K palette with shortcuts shown; keys `1`/`2`/`3`, `Space`, `M`, `Esc`.

## Task 9: Switch over

- The workspace default becomes the new app (binary `spectrawl`); `app-old` and `engine-old` are deleted in one commit (spec §6). Before that, the owner confirms, and BACKLOG lists what the old app does that the new one does not yet (live listen and call following, the Calls and Trunking tabs' imports, explore, scan, RDS, cell monitor: step 9), and `engine-old`'s examples still in use (`trawl_db`) are re-homed or replaced.

## Deviations from the guide (recorded in `docs/ui-design.md` §0)

- **I4 (44 px targets):** SpecTrawl is a desktop, mouse-driven app with dense tables; rows stay 18 px and controls egui-sized. It is the guide's Compact density without the touch floor. No touch page exists.
- **L8 (designed presets, not free docking):** free docking stays (ui-design.md §4, the owner's earlier decision); presets and Reset layout are added, which covers the escape hatch.
- **R1 (redesign behind a setting):** the new app is not yet the default; `app-old` is the way back until Task 9, so the new app does not carry a second look.
- **Page bar (signature §3, L3):** SpecTrawl's pages are its workspaces, not fixed screens, because the dock lets the owner rearrange them; the page bar lists the workspaces, and the gold underline marks the active one, not a dock tab (several dock tabs are active at once).
