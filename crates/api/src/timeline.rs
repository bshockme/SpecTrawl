//! Segments and waterfall rows, shared by the engine and the interfaces.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Boundary {
    Start,
    Retune,
    Gap { lost_samples: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub id: SegmentId,
    pub from_sample: u64,
    pub center_hz: f64,
    pub sample_rate: f64,
    pub boundary: Boundary,
}

impl Segment {
    /// The samples at the start of a `Gap` segment that the device never
    /// delivered: `from_sample .. from_sample + lost_samples`. `None` otherwise.
    ///
    /// A hint for consumers that want to skip a hole in one step. Reads are the
    /// authority: a range that intersects a hole is refused by `Source::read`
    /// whether or not it is visible here (the ring remembers the newest 64
    /// holes itself).
    pub fn hole(&self) -> Option<Range<u64>> {
        match self.boundary {
            Boundary::Gap { lost_samples } if lost_samples > 0 => Some(self.from_sample..self.from_sample + lost_samples),
            _ => None,
        }
    }

    /// The first sample of the segment that holds data (past its hole, if any).
    pub fn data_from(&self) -> u64 {
        self.hole().map_or(self.from_sample, |h| h.end)
    }
}

/// Linear power averaged over `blocks` FFT blocks covering
/// `first_sample .. end_sample`, all inside `segment`.
#[derive(Clone)]
pub struct Row {
    pub first_sample: u64,
    pub end_sample: u64,
    pub blocks: u32,
    pub segment: Segment,
    /// DC-centred (negative frequencies first) linear power, 1.0 = 0 dBFS.
    pub power: Vec<f32>,
}

/// The bins are elided (a snapshot's `Debug`, in a failing test's message say, would print 65 536 of them per row).
impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("first_sample", &self.first_sample)
            .field("end_sample", &self.end_sample)
            .field("blocks", &self.blocks)
            .field("segment", &self.segment)
            .field("power", &format_args!("[{} bins]", self.power.len()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rows_debug_counts_its_bins_instead_of_printing_them() {
        let segment = Segment { id: SegmentId(1), from_sample: 0, center_hz: 855.25e6, sample_rate: 10e6, boundary: Boundary::Start };
        let row = Row { first_sample: 0, end_sample: 32_768, blocks: 1, segment, power: vec![0.5; 65_536] };
        let text = format!("{row:?}");
        assert!(text.contains("power: [65536 bins]"), "{text}");
        assert!(!text.contains("0.5"), "{text}");
    }
}
