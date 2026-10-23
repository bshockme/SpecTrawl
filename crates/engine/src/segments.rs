//! Which tuning and sample rate each stretch of the sample stream was recorded with.
//!
//! Every retune and every gap is a segment boundary. A read of a sample range is tagged
//! with its segment, and a range spanning two segments is refused.
//!
//! A `Gap` segment starts at the first lost sample: its first `lost_samples` samples
//! (`Segment::hole`) were never delivered, and the device's samples resume at
//! `Segment::data_from`. A range that intersects a hole is refused too.

use parking_lot::Mutex;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// A range that crosses a segment boundary: the first and last segments it touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpansSegments {
    pub first: SegmentId,
    pub last: SegmentId,
}

/// Why [`SegmentTable::covering`] refused a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverError {
    /// Part of the range lies in the hole of this `Gap` segment.
    Gap(SegmentId),
    /// The range crosses a segment boundary.
    Spans(SpansSegments),
}

/// Segments in ascending `from_sample` order, never empty. The lock is taken on
/// `Retune`/`Gap` events and by readers resolving a range, never per sample block.
#[derive(Debug)]
pub struct SegmentTable {
    inner: Mutex<Vec<Segment>>,
    next_id: AtomicU64,
}

impl SegmentTable {
    pub fn new(from_sample: u64, center_hz: f64, sample_rate: f64) -> Self {
        let first = Segment { id: SegmentId(0), from_sample, center_hz, sample_rate, boundary: Boundary::Start };
        Self { inner: Mutex::new(vec![first]), next_id: AtomicU64::new(1) }
    }

    /// Append a segment starting at `from_sample`. `from_sample` must be at least the last
    /// segment's; a lower value trips a `debug_assert!` and is clamped to the last
    /// segment's `from_sample` in release builds.
    pub fn push(&self, from_sample: u64, center_hz: f64, sample_rate: f64, boundary: Boundary) -> SegmentId {
        let id = SegmentId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut v = self.inner.lock();
        let last = v.last().map_or(0, |s| s.from_sample);
        debug_assert!(from_sample >= last, "segment pushed out of order");
        v.push(Segment { id, from_sample: from_sample.max(last), center_hz, sample_rate, boundary });
        id
    }

    fn index_of(v: &[Segment], sample: u64) -> usize {
        v.partition_point(|s| s.from_sample <= sample).saturating_sub(1)
    }

    /// The segment containing `sample`; a sample before the first segment gets the first.
    pub fn at(&self, sample: u64) -> Segment {
        let v = self.inner.lock();
        v[Self::index_of(&v, sample)].clone()
    }

    /// The segment holding all of `[start, end)` (half-open). Refused with
    /// `CoverError::Gap` if the range intersects any segment's hole (checked
    /// first), else with `CoverError::Spans` if it touches more than one
    /// segment. An empty range is covered by `at(start)`.
    pub fn covering(&self, start: u64, end: u64) -> Result<Segment, CoverError> {
        let v = self.inner.lock();
        let first = Self::index_of(&v, start);
        if end <= start {
            return Ok(v[first].clone());
        }
        let last = Self::index_of(&v, end - 1);
        if let Some(s) = v[first..=last].iter().find(|s| s.hole().is_some_and(|h| h.start < end && start < h.end)) {
            return Err(CoverError::Gap(s.id));
        }
        if first == last { Ok(v[first].clone()) } else { Err(CoverError::Spans(SpansSegments { first: v[first].id, last: v[last].id })) }
    }

    /// Drop segments that end before `oldest`; the one containing `oldest` is kept.
    pub fn prune_before(&self, oldest: u64) {
        let mut v = self.inner.lock();
        let keep = Self::index_of(&v, oldest);
        v.drain(..keep);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Always false: the table holds at least one segment.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn current(&self) -> Segment {
        self.inner.lock().last().expect("never empty").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covering_refuses_a_range_across_a_retune_and_prune_keeps_the_segment_holding_oldest() {
        let t = SegmentTable::new(0, 855e6, 10e6);
        let s1 = t.push(1_000, 860e6, 10e6, Boundary::Retune);
        let s2 = t.push(5_000, 860e6, 10e6, Boundary::Gap { lost_samples: 300 });
        assert_eq!(t.at(999).center_hz, 855e6);
        assert_eq!(t.at(1_000).id, s1);
        assert_eq!(t.covering(1_000, 5_000).unwrap().id, s1);
        assert_eq!(t.covering(900, 1_100), Err(CoverError::Spans(SpansSegments { first: t.at(900).id, last: s1 })));
        assert_eq!(t.covering(4_000, 5_000).unwrap().id, s1);
        // 5_000 .. 5_300 is s2's hole: refused alone, with data either side, or across s1.
        assert_eq!(t.covering(4_000, 6_000), Err(CoverError::Gap(s2)));
        assert_eq!(t.covering(5_100, 5_200), Err(CoverError::Gap(s2)));
        assert_eq!(t.covering(5_200, 5_400), Err(CoverError::Gap(s2)));
        assert_eq!(t.covering(5_300, 6_000).unwrap().id, s2);
        assert_eq!(t.at(5_000).hole(), Some(5_000..5_300));
        assert_eq!(t.at(5_000).data_from(), 5_300);
        assert_eq!(t.at(1_000).hole(), None);
        t.prune_before(3_000);
        assert_eq!(t.len(), 2);
        assert_eq!(t.at(3_000).id, s1);
    }
}
