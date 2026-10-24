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
#[derive(Debug, Clone)]
pub struct Row {
    pub first_sample: u64,
    pub end_sample: u64,
    pub blocks: u32,
    pub segment: Segment,
    /// DC-centred (negative frequencies first) linear power, 1.0 = 0 dBFS.
    pub power: Vec<f32>,
}
