//! Sample index to wall-clock time, from the device's periodically refreshed anchors.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

#[derive(Debug)]
struct Inner {
    anchors: VecDeque<(u64, SystemTime)>,
    /// `(from_sample, rate)` ascending; never empty.
    rates: Vec<(u64, f64)>,
}

impl Inner {
    /// Seconds spanned by samples `[lo, hi)` at the rates in force.
    fn seconds_between(&self, lo: u64, hi: u64) -> f64 {
        let mut secs = 0.0;
        let mut pos = lo;
        let mut i = self.rates.partition_point(|r| r.0 <= lo).saturating_sub(1);
        while pos < hi {
            let next = self.rates.get(i + 1).map_or(hi, |r| r.0.min(hi));
            let rate = self.rates[i].1;
            if rate > 0.0 {
                secs += (next - pos) as f64 / rate;
            }
            pos = next;
            i += 1;
        }
        secs
    }
}

#[derive(Debug)]
pub struct Timeline {
    inner: Mutex<Inner>,
}

impl Timeline {
    pub const MAX_ANCHORS: usize = 64;

    pub fn new(rate: f64) -> Self {
        Self { inner: Mutex::new(Inner { anchors: VecDeque::new(), rates: vec![(0, rate)] }) }
    }

    /// Record that `at_sample` was captured at `host_time`; keeps the newest `MAX_ANCHORS`.
    pub fn anchor(&self, at_sample: u64, host_time: SystemTime) {
        let mut g = self.inner.lock();
        g.anchors.push_back((at_sample, host_time));
        while g.anchors.len() > Self::MAX_ANCHORS {
            g.anchors.pop_front();
        }
    }

    /// From `from_sample` on the stream runs at `rate`. History is kept so older samples
    /// still resolve at the rate they were recorded with.
    pub fn set_rate(&self, from_sample: u64, rate: f64) {
        let mut g = self.inner.lock();
        let last = g.rates.last().map_or(0, |r| r.0);
        if from_sample <= last {
            // Out of order or a repeat: replace the latest entry rather than reorder.
            if let Some(l) = g.rates.last_mut() {
                l.1 = rate;
            }
        } else {
            g.rates.push((from_sample, rate));
        }
    }

    /// Wall time of `sample`, from the newest anchor at or before it (else the oldest
    /// anchor). `None` if there are no anchors.
    pub fn time_of(&self, sample: u64) -> Option<SystemTime> {
        let g = self.inner.lock();
        let idx = g.anchors.partition_point(|a| a.0 <= sample);
        let (a_sample, a_time) = if idx > 0 { g.anchors[idx - 1] } else { *g.anchors.front()? };
        if sample >= a_sample {
            Some(a_time + Duration::from_secs_f64(g.seconds_between(a_sample, sample)))
        } else {
            Some(a_time - Duration::from_secs_f64(g.seconds_between(sample, a_sample)))
        }
    }

    /// Drop anchors older than the newest one at or before `oldest` (that one is kept),
    /// and rate changes that no kept anchor can still need.
    pub fn prune_before(&self, oldest: u64) {
        let mut g = self.inner.lock();
        let keep = g.anchors.partition_point(|a| a.0 <= oldest).saturating_sub(1);
        g.anchors.drain(..keep);
        if let Some(&(first, _)) = g.anchors.front() {
            let r = g.rates.partition_point(|r| r.0 <= first).saturating_sub(1);
            g.rates.drain(..r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_of_interpolates_from_the_nearest_earlier_anchor() {
        let tl = Timeline::new(10e6);
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        tl.anchor(0, t0);
        tl.anchor(10_000_000, t0 + Duration::from_millis(1_003)); // the device runs 0.3% slow
        assert_eq!(tl.time_of(5_000_000), Some(t0 + Duration::from_millis(500)));
        assert_eq!(tl.time_of(15_000_000), Some(t0 + Duration::from_millis(1_503)));
        assert_eq!(Timeline::new(10e6).time_of(5), None);
    }

    #[test]
    fn rate_change_applies_from_its_sample() {
        let tl = Timeline::new(10e6);
        let t0 = SystemTime::UNIX_EPOCH;
        tl.anchor(0, t0);
        tl.set_rate(10_000_000, 2.5e6); // 1 s at 10 MS/s, then 2.5 MS/s
        assert_eq!(tl.time_of(12_500_000), Some(t0 + Duration::from_secs(2)));
    }
}
