# SpecTrawl UI: receivers, docking, inspector

Status: design, 2026-10-10; all of §8 is implemented (commits 1496c78 onward), plus the Diagnostics tab; there is no live reflow at width breakpoints, by decision (see §4). Supersedes the pane list in `DESIGN.md` §9. The
current UI (fixed left column of cards, two spectrum views, a tab strip at
the bottom) is the starting point; this is where it goes and in what order.

## 0. House guide (adopted 2026-11-08, rebuild step 8)

The new app (`crates/app`) follows the owner's house guide, `~/auggiesoft-ui-guide` (its `rules/` and
`looks/instrument.md`). Where this document and the guide disagree, this document wins, and the
disagreement is listed under "Deviations" below with its reason. The old app (`app-old`) is not
restyled.

- **Look:** Instrument, on every pane. No Plain page yet.
- **Type:** Barlow 500 for the UI, Barlow Semi Condensed 600 (uppercase, 0.08 em tracking) for
  labels, section titles and column headers, Auggie Mono for every frequency, level, count, time
  and identifier. TTFs decompressed from the guide's woff2 live in `crates/app/assets/fonts/` with
  their OFL licences; egui's own fonts are the fallbacks. Units are drawn quieter than values
  (`theme::value_unit`).
- **The six roles** (`crates/app/src/theme.rs`):

  | Role | SpecTrawl meaning |
  |---|---|
  | accent `#5aa9f0` | the selected record, row or box; a selected toggle (Live/Sweep) |
  | caution `#f0a830` | a receiver's Warning status; the bus saturated |
  | alarm `#ff4d4d` | a receiver Lost; losing samples; a replay ended early; an error |
  | ok `#3ccf6e` | a decoder Locked |
  | live `#ff5cc8` | audio playing out; recording (none drawn yet) |
  | info `#9fd3ff` | a decoder Syncing |

  Searching lamps are ink-2 rings. A healthy screen shows no caution, alarm or live.
- **Identity colours:** the Instrument set (no red, amber, green or magenta). Known wideband
  occupants take violet, always with their name. The receiver letter is a neutral chip: it is an
  identifier, not a state.
- **Data:** the spectrum trace is the trace cyan, never the accent; the peak hold is ink-2. The
  waterfall uses inferno at every tenth (`theme::CMAP`; the Instrument look's five stops are among them, but interpolating those five alone lifts the noise floor to a bright purple), whose floor and ceiling are the
  receiver's dB range. Plot wells are flat black.
- **Plots:** the band plan is a 20 px strip between the spectrum and the waterfall (O14), its
  fills in identity colours at 15 % with stronger edges, names cut to fit, two rows for overlaps,
  every allocation at the pointer on hover. The waterfall's time sits in the left gutter on chips,
  marks at least 48 px apart, with the top row's clock time (O2, O13). The spectrum's smoothed
  trace carries the peak hold (D3).
- **Gold thread:** the wordmark top left, and (with the page bar, step 8 Task 8) the 2 px underline
  under the active workspace. Nowhere else.
- **Project voice:** none.

### Deviations from the guide

- **I4, 44 px targets:** SpecTrawl is a desktop, mouse-driven app with dense tables; rows stay
  18 px and controls are egui-sized. It is the guide's Compact density without the touch floor.
  There is no touch page.
- **L8, presets rather than free docking:** free docking stays (§4, the owner's earlier choice).
  Presets and Reset layout give the escape hatch the rule asks for.
- **R1, a redesign behind a setting:** the new app is not yet the default, and `app-old` is the way
  back until step 8 switches over, so the new app carries one look only.
- **O13, a hairline at each retune:** a retune clears the waterfall's history, so no rows span
  one and there is nothing for the line to separate. The time gutter's chips and the clock time
  at the top follow O13 otherwise.
- **Page bar (signature §3, L3):** SpecTrawl's pages are its workspaces, because the dock lets the
  owner rearrange panes. The page bar lists the workspaces, and the gold underline marks the active
  one, not a dock tab (several dock tabs are active at once).

## 1. Principles

1. **One pane per receiver.** Each SDR is a self-contained pane: spectrum,
   waterfall, and a header toolbar with the controls that only make sense
   for that receiver. A third or fourth receiver is just another pane;
   nothing else in the UI has to know.
2. **The receiver letter is the universal handle.** `S` for the sweeper,
   `A`, `B`, `C`… for live receivers. It appears in the pane header, the
   `Src` column of every table, the inspector, the transport bar, and the
   survey pane's tune-target chip.
3. **One contextual inspector** replaces the card stack. It renders
   whatever is selected and never grows.
4. **Audio is a transport bar**, one persistent line at the bottom, not a
   card.
5. **Results and their controls live together.** Call following's
   priority/lockout state is visible in Calls; trunking identity and name
   imports live in Trunking.
6. **Rarely used things go behind an icon rail**: devices, name imports,
   follow lists, workspaces, settings.
7. **Docking, pop-out windows and named workspaces** make screen size and
   receiver count the user's choice, not the layout's.

Fixed sizes: table row height (18 px) and the spectrum trace height. Extra
pixels go to waterfall history and more table rows, never to chrome.

## 2. Foundation changes (no visible change, done first)

### 2.1 Receiver identity

Today the app has two slots, `Role::Survey` and `Role::Focus`, and
everything is keyed on them (`pane(Role::Focus)` from listen, selection,
P25, emitters, exports; "Focus A" is a literal in four places).

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ReceiverId(pub char);          // 'S', 'A', 'B', ...

pub enum ReceiverMode { Sweep, Live }

struct Receiver {                          // was Pane
    id: ReceiverId,
    mode: ReceiverMode,
    source: Source,
    view: SpectrumView,
    records: ...,
    gain: ...,
}
```

- `S` is the sweeper. Live receivers take the next free letter from `A`.
  A letter is a **slot**: a receiver pane with no device shows a device
  picker in place of the spectrum, and reopening a workspace restores the
  slot even if its device is unplugged.
- `App.tune_target: ReceiverId` is where survey clicks and table
  double-clicks tune. Keys `1`/`2`/`3` pick it; the survey pane shows it
  as a chip ("tunes → A"). With one live receiver it is implicit.
- The engine does not change: one `Source` per device already.
- CSV exports replace `source=survey|focus` with the letter and mode.

### 2.2 Selection

Today: `selected: Option<u64>` (a record id with no receiver) and
`selected_emitter: Option<(Role, u64)>`; the selection card only looks in
the focus pane.

```rust
pub enum Selection {
    None,
    Receiver(ReceiverId),
    Record(ReceiverId, u64),
    Emitter(ReceiverId, u64),
    Talkgroup { system: SystemKey, tg: u32 },
    Radio     { system: SystemKey, id: u32 },
    Capcode   { protocol: String, capcode: u64 },
}
```

Clicking a spectrum box, a table row, a pane header or a talkgroup cell
sets it. The inspector, the listen target and the export buttons all read
it. `Talkgroup`, `Radio` and `Capcode` are new.

### 2.3 Trunking systems, not P25 systems

The engine exposes `p25_systems() -> Vec<(nac, SystemInfo)>` and the Calls
tab resolves names against `systems.first()`. Trunking must cover any
system we can identify, so the app sees one shape:

```rust
pub enum SystemKind { P25, DmrCapPlus, DmrConPlus, DmrTier3, Nxdn, Ltr, Edacs, Mpt1327 }

/// Stable identity for lookups, names and selection.
pub enum SystemKey {
    P25   { wacn: Option<u32>, system: Option<u16>, nac: u16 },
    Dmr   { network: Option<u32>, colour_code: u8, control_hz: f64 },   // Cap+/Con+/Tier III
    Nxdn  { ran: u8, control_hz: f64 },
    Other { kind: SystemKind, key: String },
}

pub struct TrunkSystem {
    pub key: SystemKey,
    pub kind: SystemKind,
    pub receiver: ReceiverId,            // which receiver's monitor learned it
    pub identity: Vec<(&'static str, String)>,   // "NAC", "WACN", "SYS", "RFSS/SITE", "Rest ch", ...
    pub control_hz: Vec<f64>,            // P25: one; Cap+: rest channel moves
    pub site_name: Option<String>,       // imported
    pub network_name: Option<String>,    // imported
    pub adjacent: Vec<(String, Option<f64>)>,
    pub grants: usize,
    pub last_activity: SystemTime,
}
```

- `Source::systems() -> Vec<TrunkSystem>` wraps `p25_systems()` today and
  grows a Cap+ entry when `dmr::CapPlusStatus` is tracked by a monitor.
  NXDN, LTR and EDACS entries come with their decoders.
- `CallEvent` gains `system: Option<SystemKey>` (it already carries a
  system string from `CallLog::granted`); the Calls tab resolves names by
  the call's own system, which also makes two receivers on two sites
  correct.
- The catalog gains `name_for(&SystemKey, tg)` / `radio_name(&SystemKey,
  id)` / `site_name(&SystemKey)` so imports from DSD+ (which keys DMR
  groups by network) and trunk-recorder both resolve through one call.

## 3. Panes

Every pane is a `Tab` in a dock. Tabs are a plain enum, not trait objects,
so the dock tree serialises as identifiers and the pane bodies stay the
`App` methods they are now (`App` implements `egui_dock::TabViewer`).

```rust
#[derive(Serialize, Deserialize, PartialEq)]
enum Tab {
    Receiver(ReceiverId),
    Inspector,
    Emitters, Records, Trunking, Calls, Pages, Diagnostics,
}
```

### 3.1 Receiver pane

Header toolbar, left to right:

- Letter chip (`S` teal, `A`/`B` light), device label, mode.
- **Sweep**: `lo – hi MHz` drag values and Apply. **Live**: centre drag
  value.
- `Gain ▾` popover: Linearity / Sensitivity / AGC, step slider.
- `dB range ▾` popover: top, floor, Auto.
- **Live**: `Decoders ▾` popover: the monitors running on this span
  (control channels, pagers) with on/off later; today read-only.
- **Sweep**: `tunes → A` chip (the tune target; click to cycle).
- Right end: `⋯` menu: Rename letter, Pop out, Close device.

Body: trace and waterfall as now; the sweep pane keeps the focus bracket
labelled with each live receiver's letter. Clicking the header selects
`Selection::Receiver(id)`.

The SURVEY, FOCUS A and DISPLAY cards go away. Their statistics move to the
inspector's diagnostics disclosure and the Diagnostics tab.

### 3.2 Inspector

One pane, re-rendered from `Selection`:

| Selection | Content |
|---|---|
| None | "Click a box, a row or a receiver header." plus the look-back export button for the tune target |
| Receiver | letter, device, mode, centre/range, gain, dB range (same controls as the header, full size), `▸ Diagnostics` collapsed (rate, FFT, look-back, gaps, classifier load; sweep: hops, rate, revisit, re-measured, lock waits) |
| Record | id, class, MHz, bandwidth, duration, SNR, peak; decoded fields; **Listen**; Export channel / Full band |
| Emitter | id, label, activity, tallies, first/last seen, duty, bursts; latest call; **Listen**; Export channel (look-back) |
| Talkgroup | number, imported name, system, last calls (5), ★ Priority / ⊘ Lock out toggles, Follow this talkgroup only (later) |
| Radio | id, imported name, system, last calls (5) |
| Capcode | number, protocol, channel, last pages (5), filter Pages to it |

Export progress and the "saves to" path stay at the bottom of Record and
Emitter views.

### 3.3 Transport bar

A 26 px persistent strip at the bottom, replacing the LISTEN card and the
current status line:

```
▶  A · 772.800 MHz · P25 · Following   TG 1042 Dispatch ← R 50113   ● rec     mode ▾   vol ──●──   stop  │  2 rx · rings 1.2 GB · drops 0 · 60 fps
```

- Left: state (`▶` live, `○` squelched, `■` stopped, `♪ file.wav 3.2 s`
  while a recording plays), receiver letter, frequency, mode, call summary,
  encrypted/rec markers. Click the call summary to select the talkgroup.
- `mode ▾` popover: Auto / NFM / AM / WFM / P25, Phase 2 checkbox, Follow
  calls toggle, Listen here (frequency entry), Recordings folder.
- Volume slider, Stop.
- Right: the diagnostics summary that is on the status line today.

Listen targets the receiver the selection belongs to, else the tune
target. One audio output; starting listen on B stops A.

### 3.4 Bottom tabs

- **Emitters**, **Records**: as now, `Src` is the letter.
- **Trunking**: one row per `TrunkSystem` across all receivers: kind,
  identity, site/network name, control channel(s), adjacent sites,
  grants, last activity, `Src`. A header strip holds `Import names ▾`
  (DSD+ folder, trunk-recorder config, CSV, clear) and the import
  summary ("names: DSD+ 1,204 TG · 318 radios"). Clicking a row selects
  nothing yet; double-click tunes the tune target to its control channel.
- **Calls**: as now, plus a `★/⊘` column, the follow-list summary and the
  current follow state in the filter bar, and a `Calls | Grants` toggle
  (Grants is today's P25 table, for every system). Talkgroup and radio
  cells are clickable (select). Right-click keeps the priority/lockout
  menu.
- **Pages**: as now; capcode cells select; `Import names ▾` shared with
  Trunking for capcode aliases later.
- **Diagnostics**: per-receiver statistics in full (the former card
  stats), sweep hop timing (a strip showing each hop's age), classifier
  load, audio queue state, and the window/layout itself. `--tab
  diagnostics` opens it in front.

### 3.5 Icon rail

A 36 px vertical rail on the far left: Devices, Names, Follow lists,
Workspaces, Settings. Each opens a flyout (an `egui::Window` anchored to the
rail, closed on click-away). Devices holds today's SOURCES list with
"open as S / A / B / next free". Follow lists edits priority/lockout
directly. Settings holds the catalog path, recordings folder, SigMF folder
and, later, RadioReference credentials.

## 4. Docking

**egui_dock** (MIT; builds against our egui 0.36; floating windows; serde).
Not egui_tiles: no floating windows, so pop-out would be viewport-only.

- `App.dock: DockState<Tab>`; taken out of `App` for the frame so
  `DockArea::show_inside(ui, self)` can borrow `App` as the viewer.
- Tabs may float inside the main window (drag off the strip); drag back to
  re-dock. `Receiver` tabs may not be closed by `×` (closing a device is in
  the header menu); table tabs may, and come back from the rail's
  Workspaces flyout or `View ▾`.
- `Tab::Receiver(id)` for an unassigned slot draws the device picker.
- The first run opens the window maximized and builds the layout for the
  monitor's width in points (`ViewportInfo::monitor_size`), so a 1080p
  and a 4K screen start differently. After that the window's position,
  size and maximized state are part of the workspace and come back on
  the next start. **Reset layout** and the presets rebuild for the
  window's current width. There is no live reflow: arrangements the
  user made are never rebuilt because the window changed size.
  - **< 1600**: receivers stacked; tables below; a wider left column.
  - **1600–2559**: receivers stacked centre; inspector docked left;
    tables below.
  - **≥ 2560** (1440p, or 4K at 150%): tables docked right of the
    receivers; inspector left.

## 5. Pop-out windows

Dragging a tab out of the OS window is not possible (winit cannot carry a
drag across windows; egui loses the pointer). So pop-out is a button:
`⧉` on the tab's context menu, on a floating window's title bar, and
`Ctrl+Shift+P` for the focused tab.

- Each popped-out window is an **immediate viewport**
  (`ctx.show_viewport_immediate`) that hosts **its own `DockState<Tab>`**.
  A window therefore holds any number of tabs, split and tabbed like the
  main dock: "Calls | Pages" on a second monitor is one window. Immediate
  viewports keep `App` as plain `&mut self`; deferred ones would need the
  whole app behind `Arc<Mutex>`.
- `Move to window N` / `Move to main` on a tab's context menu replaces
  cross-window drag. Closing a window returns its tabs to the main dock.
- Textures are per `egui::Context`, so a receiver pane in a popped-out
  window draws the same waterfall with no re-upload.
- Each extra window repaints with the app (we repaint continuously), so a
  few are fine; cost grows per window.
- On backends without multi-viewport (Wayland, web) egui falls back to an
  embedded window with no code change.

```rust
struct PopOut { id: ViewportId, title: String, dock: DockState<Tab>, rect: Option<Rect> }
App { dock: DockState<Tab>, popouts: Vec<PopOut>, .. }
```

## 6. Workspaces

`Documents/SpecTrawl/workspaces/<name>.json`:

```json
{ "main": <DockState<Tab>>, "popouts": [{ "title", "dock", "rect" }], "tune_target": "A" }
```

- `last.json` is written on exit and on every layout change (debounced);
  loaded on start. Named workspaces are saved/loaded/deleted from the
  rail's Workspaces flyout and the top bar's `Workspace: name ▾`.
- Ship three named defaults: **survey**, **trunking**, **paging**.
- Receiver slots are restored by letter; a slot whose device is absent
  shows the picker.
- `--tab` and the other command-line flags keep working: they apply after
  the workspace loads.

## 7. Command palette and keys

After docking. `Ctrl+K` opens a centred window with a text box and a
filtered list; commands are a `Vec<Command { name, keywords, run }>` built
each frame from the app state:

- `tune A 772.8`, `open <device> as B`, `listen 155.475 NFM`, `follow A`,
  `stop`, `export emitters`, `workspace trunking`, `pop out calls`,
  `priority 1042`, `lockout 1042`, `reset layout`.

Keys: `1`/`2`/`3` tune target, `Space` pause, `M` mute, `Ctrl+K` palette,
`Ctrl+Shift+P` pop out, `Esc` clear selection.

## 8. Migration order

1. Foundation: `ReceiverId`, `Selection`, `TrunkSystem`/`SystemKey`, names
   by system. No visible change; all tests pass.
2. Transport bar and inspector. LISTEN and SELECTION cards go.
3. Receiver pane headers. SURVEY, FOCUS A, DISPLAY cards go; the left
   column is now only Devices and the inspector.
4. egui_dock: `Tab` enum, default layouts by width, `last.json`.
5. Pop-out windows with their own docks; `Move to window`.
6. Trunking tab, Calls view toggle and `★/⊘` column, icon rail, named
   workspaces.
7. Command palette and keys.

Each step is a commit that leaves the app usable.

## 9. Open questions

- Should `S` be a mode rather than a letter (any receiver can sweep)? Kept
  as a letter for now: two sweepers is unlikely and `S` reads at a glance.
- Floating-window title bar `⧉` needs egui_dock's window-frame hook; if
  the version we pin lacks it, the tab context menu is enough.
- Per-monitor-configuration layouts (the write-up's "persist per monitor
  configuration"): deferred; `last.json` plus named workspaces covers it.
- DMR Cap+/Con+ identity: the rest channel moves, so `control_hz` is a
  list and the key uses the network id when the CSBK carries one.
