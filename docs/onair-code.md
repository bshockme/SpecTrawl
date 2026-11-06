# OnAir code in SpecTrawl

The owner decided on 2026-11-06 that SpecTrawl may include OnAir's code where it helps, for example a decoder, a table or a demodulator worth taking rather than rewriting. Every inclusion is listed here, so it can be replaced if SpecTrawl ever needs to be distributed under its own MIT licence again (for a distribution's packaging, say).

**Status: nothing is included yet.** `detect::acars` was written from the protocol's description, not from OnAir.

## What including it means

- **OnAir's licence:** OnAir (github.com/adswill/OnAir) is **GPL-3.0-or-later** (its `LICENSE` and `REUSE.toml`).
  - Any SpecTrawl crate that contains OnAir code is a combined work under the GPL. So is every binary that links it.
  - The rest of the tree stays MIT. MIT code may be combined into GPL work, but not the other way round.
  - Such a crate's `Cargo.toml` says `license = "GPL-3.0-or-later"` instead of `license.workspace = true`.
  - While any binary links such a crate, releases of it are GPL: the source is offered and OnAir's notices are kept.
- **Upstream origins:** much of OnAir is itself ported from other projects. Its `THIRD_PARTY.md` lists them, for example HD Radio from nrsc5, the HF digital modes from WSJT-X, and DAB service information from etisnoop. A few have provenance it marks as unknown (`LicenseRef-Unknown`, in its `docs/PROVENANCE.md`). An entry here records the upstream origin as well as OnAir's file. An inclusion whose origin OnAir doesn't know is not taken.
- **Name and logo:** OnAir's name and logo are all rights reserved (not under the GPL). They are not used.
- **Where it lives:** the local clone is `~/SpecTrawl/onair/OnAir`, git-ignored and kept as a reference. Its commit at the time of writing is `25034214fa91` (2026-11-04).

## How an inclusion is marked

- **In the code:** a header comment on the file or the item:

  ```rust
  // From OnAir core/src/<path> at 25034214fa91 (GPL-3.0-or-later; upstream: <project, licence> or "OnAir's own").
  // Listed in docs/onair-code.md.
  ```

- **In this ledger:** one row per inclusion.
  - **What:** a function, a table, a whole file. Ported (translated from C++) counts the same as copied.
  - **From:** OnAir's path and commit, and the upstream project and licence if OnAir names one.
  - **Where:** the SpecTrawl file and item.
  - **Why:** what it gave that writing it from the specification would not.
  - **Replace by:** what a clean reimplementation needs (the specification, test vectors, a committed capture that checks it).

## Inclusions

| What | From (OnAir path @ commit; upstream) | Where in SpecTrawl | Why | Replace by |
|---|---|---|---|---|
| (none yet) | | | | |
