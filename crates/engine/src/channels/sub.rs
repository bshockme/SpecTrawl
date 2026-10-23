//! One subscription: its channel's extractor, its consumer and its place in
//! the input. [`Sub::run_block`] does one scheduling turn's work.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use dsp::channelizer::{ChannelDesign, DesignError, Extractor, design_channel};
use dsp::spectrum::SharedFft;
use sdr_core::Complex32;

use super::{ChannelInfo, ChannelSpec, Consumer, ConsumerState, Delivery, EndReason, Flow, GapReason};
use crate::segments::{Segment, SegmentId};
use crate::source::ReadError;
use crate::stage::Stage;

/// What the scheduler and the stats see of a subscription.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Counters {
    pub next_block: u64,
    pub blocks: u64,
    pub recomputed: u64,
    pub gaps: u64,
    pub state: ConsumerState,
}

impl Counters {
    /// Still has a consumer to feed.
    pub fn live(&self) -> bool {
        matches!(self.state, ConsumerState::Running | ConsumerState::OutOfSpan)
    }
}

/// Input samples `from .. to` that produced no output yet reported.
struct PendingGap {
    from: u64,
    to: u64,
    reason: GapReason,
}

/// The scratch for recomputing a block from the ring after a cache miss;
/// dropped again on the next cache hit.
struct Recompute {
    fft: SharedFft,
    input: Vec<Complex32>,
    bins: Vec<Complex32>,
}

pub(crate) struct Sub {
    info: ChannelInfo,
    spec: ChannelSpec,
    usable_fraction: f64,
    stage: Arc<Stage>,
    /// `None` once the subscription has ended.
    consumer: Option<Box<dyn Consumer>>,
    /// The segment the extractor (or its absence: out of span) was designed for.
    segment: SegmentId,
    extractor: Option<Extractor>,
    recompute: Option<Recompute>,
    out: Vec<Complex32>,
    /// Gaps to deliver before the next `Samples`, coalesced by reason.
    pending: Vec<PendingGap>,
    /// Set when the consumer panicked during the current turn.
    panicked: bool,
    counters: Counters,
}

impl Sub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        info: ChannelInfo,
        spec: ChannelSpec,
        usable_fraction: f64,
        stage: Arc<Stage>,
        segment: SegmentId,
        design: ChannelDesign,
        next_block: u64,
        consumer: Box<dyn Consumer>,
    ) -> Sub {
        let out = vec![Complex32::default(); design.out_per_block];
        Sub {
            info,
            spec,
            usable_fraction,
            stage,
            consumer: Some(consumer),
            segment,
            extractor: Some(Extractor::new(design)),
            recompute: None,
            out,
            pending: Vec::new(),
            panicked: false,
            counters: Counters { next_block, blocks: 0, recomputed: 0, gaps: 0, state: ConsumerState::Running },
        }
    }

    pub fn info(&self) -> &ChannelInfo {
        &self.info
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// One turn: deliver block `next_block` (preceded by any pending gap), or
    /// record why it cannot be. Returns true if the consumer panicked.
    pub fn run_block(&mut self) -> bool {
        self.panicked = false;
        if self.consumer.is_none() {
            return false;
        }
        let b = self.counters.next_block;
        let l = self.stage.hop();
        let cached = self.stage.cache().get(b);
        let (seg, cached_bins) = match cached {
            Some((seg, bins)) => {
                self.recompute = None;
                (seg, Some(bins))
            }
            None => match self.recompute_block(b) {
                Ok(seg) => (seg, None),
                Err(ReadError::Future) => return false, // not written yet: try again next turn
                Err(e) => {
                    let mut next = b + 1;
                    if matches!(e, ReadError::Overwritten | ReadError::Lost) {
                        // To the live edge, never backwards; the gap runs to where samples resume.
                        next = next.max(self.stage.source().oldest() / l + 2);
                    }
                    self.add_gap(b * l, next * l, GapReason::Unreadable(e));
                    self.counters.next_block = next;
                    return false;
                }
            },
        };
        if seg.id != self.segment {
            self.redesign(&seg);
            if self.consumer.is_none() {
                return self.panicked;
            }
        }
        let Some(extractor) = self.extractor.as_mut() else {
            self.add_gap(b * l, (b + 1) * l, GapReason::OutOfSpan);
            self.counters.next_block = b + 1;
            return false;
        };
        let bins: &[Complex32] = match &cached_bins {
            Some(bins) => bins,
            None => &self.recompute.as_ref().expect("recomputed just now").bins,
        };
        extractor.extract(bins, (b - 1) * l, &mut self.out);
        for g in std::mem::take(&mut self.pending) {
            self.counters.gaps += 1;
            if !self.deliver(Delivery::Gap { from_sample: g.from, to_sample: g.to, reason: g.reason }) {
                return self.panicked;
            }
        }
        let out = std::mem::take(&mut self.out);
        if self.deliver(Delivery::Samples { block: b, first_sample: b * l, samples: &out }) {
            self.out = out;
        }
        self.counters.blocks += 1;
        self.counters.next_block = b + 1;
        self.panicked
    }

    /// Something other than the consumer panicked during a turn: end the
    /// subscription without calling the consumer again.
    pub fn abandon(&mut self) {
        self.finish();
    }

    /// Block `b`'s bins from the ring, into the recompute scratch.
    fn recompute_block(&mut self, b: u64) -> Result<Segment, ReadError> {
        let (n, l) = (self.stage.fft_size(), self.stage.hop());
        let r = self.recompute.get_or_insert_with(|| Recompute {
            fft: SharedFft::new(n),
            input: vec![Complex32::default(); n],
            bins: vec![Complex32::default(); n],
        });
        let seg = self.stage.source().read((b - 1) * l, &mut r.input)?;
        r.fft.forward(&r.input, &mut r.bins);
        self.counters.recomputed += 1;
        Ok(seg)
    }

    /// A new segment: design the channel for its tuning and rate.
    fn redesign(&mut self, seg: &Segment) {
        self.segment = seg.id;
        let s = &self.spec;
        let design = design_channel(
            self.stage.fft_size(),
            seg.sample_rate,
            self.usable_fraction,
            s.center_hz - seg.center_hz,
            s.bandwidth_hz,
            s.min_rate,
        );
        match design {
            Ok(d) if d.out_rate == self.info.sample_rate && d.out_per_block == self.info.samples_per_block => {
                self.extractor = Some(Extractor::new(d));
                self.counters.state = ConsumerState::Running;
            }
            Err(DesignError::OutsideSpan) => {
                self.extractor = None;
                self.counters.state = ConsumerState::OutOfSpan;
            }
            Ok(_) | Err(_) => self.end(EndReason::RateChanged),
        }
    }

    /// Extend the last pending gap if it has the same reason and touches
    /// `from`, else queue a new one.
    fn add_gap(&mut self, from: u64, to: u64, reason: GapReason) {
        match self.pending.last_mut() {
            Some(g) if g.reason == reason && g.to >= from => g.to = g.to.max(to),
            _ => self.pending.push(PendingGap { from, to, reason }),
        }
    }

    /// Deliver `d`; false once the subscription has ended (the consumer
    /// returned `Flow::Stop` or panicked).
    fn deliver(&mut self, d: Delivery<'_>) -> bool {
        let Some(consumer) = self.consumer.as_mut() else { return false };
        match catch_unwind(AssertUnwindSafe(|| consumer.deliver(d))) {
            Ok(Flow::Continue) => true,
            Ok(Flow::Stop) => {
                self.finish();
                false
            }
            Err(_) => {
                self.panicked = true;
                self.finish();
                false
            }
        }
    }

    /// Tell the consumer why it ends, then end.
    fn end(&mut self, reason: EndReason) {
        self.deliver(Delivery::Ended { reason });
        self.finish();
    }

    /// Drop the consumer and the scratch; the counters stay for the stats.
    fn finish(&mut self) {
        if let Some(c) = self.consumer.take() {
            // A consumer that panicked may panic again in its Drop.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(c)));
        }
        self.counters.state = ConsumerState::Ended;
        self.extractor = None;
        self.recompute = None;
        self.pending = Vec::new();
        self.out = Vec::new();
    }
}
