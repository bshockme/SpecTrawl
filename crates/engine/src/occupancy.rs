//! Known wideband occupants (FM broadcast, TV, LTE/NR) found by the spectral
//! identifier on averaged rows, kept as a persistent list so the UI can
//! label them and squelch them out of the way.

use std::time::SystemTime;

use detect::spectral::{Kind, Occupant, Spectrum, floor_estimate, identify};

use crate::{SpectrumRow, Tuning};

/// An occupant and when it was seen.
#[derive(Debug, Clone)]
pub struct KnownOccupant {
    pub occupant: Occupant,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    /// Looks in a row (since the last sighting) where it should have been
    /// visible and wasn't.
    misses: u32,
    /// What the cell monitor found on a cellular block (LTE sync, MIB), and
    /// when it last looked.
    pub cell: Option<detect::lte::LteCell>,
    pub(crate) cell_checked: Option<std::time::Instant>,
}

impl KnownOccupant {
    /// The occupant's label, with the cell identity when one was decoded:
    /// "LTE 10 MHz PCI 217 (2 ports)" instead of "LTE/NR 10 MHz".
    pub fn label(&self) -> String {
        match &self.cell {
            Some(c) if c.sync.is_some() => c.label(),
            Some(c) => format!("{} · {}", self.occupant.kind.label(), c.label()),
            None => self.occupant.kind.label(),
        }
    }
}

/// Looks in view without finding an occupant before it's forgotten.
const MAX_MISSES: u32 = 3;

pub(crate) struct OccupancyTracker {
    /// Rows averaged per identification.
    every: u32,
    sum: Vec<f64>,
    count: u32,
    tuning: Option<Tuning>,
    pub(crate) known: Vec<KnownOccupant>,
}

impl OccupancyTracker {
    pub(crate) fn new(every: u32) -> Self {
        Self { every: every.max(1), sum: Vec::new(), count: 0, tuning: None, known: Vec::new() }
    }

    /// Adds a row; every `every` rows identifies occupants on the average and
    /// merges them into `known`. Returns true when `known` was updated.
    pub(crate) fn push(&mut self, row: &SpectrumRow) -> bool {
        if self.tuning != Some(row.tuning) || self.sum.len() != row.db.len() {
            self.tuning = Some(row.tuning);
            self.sum = vec![0.0; row.db.len()];
            self.count = 0;
        }
        for (s, v) in self.sum.iter_mut().zip(&row.db) {
            *s += 10f64.powf(*v as f64 / 10.0);
        }
        self.count += 1;
        if self.count < self.every {
            return false;
        }
        let n = self.sum.len();
        let db: Vec<f32> = self.sum.iter().map(|s| (10.0 * (s / self.count as f64).max(1e-30).log10()) as f32).collect();
        self.sum.iter_mut().for_each(|s| *s = 0.0);
        self.count = 0;
        let t = row.tuning;
        let bin = t.span_hz / n as f64;
        // Floor windows: a twentieth of the span, 1-8 MHz (wide enough that a
        // TV channel or LTE carrier doesn't lift its own floor much).
        let window = ((t.span_hz / 20.0).clamp(1e6, 8e6) / bin) as usize;
        let floor = floor_estimate(&db, window);
        let f0 = t.center_hz - t.span_hz / 2.0;
        let found = identify(&Spectrum { db: &db, floor: &floor, f0_hz: f0, bin_hz: bin });
        self.merge(found, f0, f0 + t.span_hz, row.time);
        true
    }

    fn merge(&mut self, found: Vec<Occupant>, lo: f64, hi: f64, now: SystemTime) {
        let mut seen = vec![false; self.known.len()];
        for o in found {
            match self.known.iter().position(|k| same(&k.occupant, &o)) {
                Some(i) => {
                    let k = &mut self.known[i];
                    k.occupant = o;
                    k.last_seen = now;
                    k.misses = 0;
                    seen[i] = true;
                }
                None => self.known.push(KnownOccupant {
                    occupant: o,
                    first_seen: now,
                    last_seen: now,
                    misses: 0,
                    cell: None,
                    cell_checked: None,
                }),
            }
        }
        for (i, k) in self.known.iter_mut().enumerate() {
            if i < seen.len() && !seen[i] && k.occupant.lo_hz >= lo && k.occupant.hi_hz <= hi {
                k.misses += 1;
            }
        }
        self.known.retain(|k| k.misses < MAX_MISSES);
        self.known.sort_by(|a, b| a.occupant.lo_hz.total_cmp(&b.occupant.lo_hz));
        // Cellular blocks are cut differently from pass to pass: entries that
        // now overlap are one block; keep the newer cut.
        let mut i = 0;
        while i + 1 < self.known.len() {
            let (a, b) = (&self.known[i], &self.known[i + 1]);
            if same(&a.occupant, &b.occupant) {
                let keep = if a.last_seen >= b.last_seen { i } else { i + 1 };
                let first = a.first_seen.min(b.first_seen);
                let gone = self.known.remove(if keep == i { i + 1 } else { i });
                self.known[i].first_seen = first;
                // The cell identity belongs to the block, whichever cut stays.
                let synced = |c: &Option<detect::lte::LteCell>| c.as_ref().is_some_and(|c| c.sync.is_some());
                if self.known[i].cell.is_none() || (synced(&gone.cell) && !synced(&self.known[i].cell)) {
                    if gone.cell.is_some() {
                        self.known[i].cell = gone.cell;
                        self.known[i].cell_checked = gone.cell_checked;
                    }
                }
            } else {
                i += 1;
            }
        }
    }
}

impl OccupancyTracker {
    /// Records a cell monitor's look at the cellular block covering `hz`:
    /// the result (None: nothing recognisable) and when it was made.
    pub(crate) fn note_cell(&mut self, hz: f64, cell: Option<detect::lte::LteCell>) {
        if let Some(k) = self
            .known
            .iter_mut()
            .find(|k| matches!(k.occupant.kind, Kind::Cellular { .. }) && (k.occupant.lo_hz..=k.occupant.hi_hz).contains(&hz))
        {
            // Keep a synchronised identity over a later look that lost it
            // (a weak moment); replace anything else.
            let keep_old = cell.as_ref().is_none_or(|c| c.sync.is_none()) && k.cell.as_ref().is_some_and(|c| c.sync.is_some());
            if !keep_old {
                k.cell = cell;
            }
            k.cell_checked = Some(std::time::Instant::now());
        }
    }
}

/// The same occupant seen again: same kind of thing at (nearly) the same place.
fn same(a: &Occupant, b: &Occupant) -> bool {
    match (a.kind, b.kind) {
        (Kind::FmBroadcast { .. }, Kind::FmBroadcast { .. }) => (a.center_hz - b.center_hz).abs() < 50e3,
        (Kind::Tv { channel: x, .. }, Kind::Tv { channel: y, .. }) => x == y,
        (Kind::Cellular { .. }, Kind::Cellular { .. }) => a.lo_hz < b.hi_hz && b.lo_hz < a.hi_hz,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ(kind: Kind, c: f64, half: f64) -> Occupant {
        Occupant { kind, center_hz: c, lo_hz: c - half, hi_hz: c + half, snr_db: 20.0 }
    }

    #[test]
    fn merges_and_forgets_after_misses() {
        let mut t = OccupancyTracker::new(1);
        let now = SystemTime::now();
        let fm = occ(Kind::FmBroadcast { hd: false }, 102.1e6, 100e3);
        t.merge(vec![fm.clone()], 100e6, 105e6, now);
        // Seen again with HD this time: same station, updated.
        t.merge(vec![occ(Kind::FmBroadcast { hd: true }, 102.1e6, 200e3)], 100e6, 105e6, now);
        assert_eq!(t.known.len(), 1);
        assert_eq!(t.known[0].occupant.kind, Kind::FmBroadcast { hd: true });
        // Out of view: kept however long.
        for _ in 0..10 {
            t.merge(vec![], 200e6, 210e6, now);
        }
        assert_eq!(t.known.len(), 1);
        // In view but missing: forgotten after MAX_MISSES looks.
        for _ in 0..MAX_MISSES {
            t.merge(vec![], 100e6, 105e6, now);
        }
        assert!(t.known.is_empty());
    }
}
