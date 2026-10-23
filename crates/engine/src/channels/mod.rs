//! The channel bus (spec §4.3): consumers subscribe to a channel of a
//! source with a start point, a priority and a callback; a pool of workers
//! cuts each channel from the stage's cached bins (or recomputes a block from
//! the ring on a cache miss) and delivers blocks in order with gap markers.
//!
//! Scheduling is by priority, then round-robin one block per turn
//! (`sched.rs`); each subscription's lag behind the stage is in [`BusStats`].

mod sched;
mod sub;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use dsp::channelizer::{DesignError, design_channel};
use parking_lot::Mutex;
use sdr_core::Complex32;

use crate::budget::SourceId;
use crate::source::{ReadError, Source};
use crate::stage::{BlockSink, Stage, StageConfig};

use sched::{Latest, Shared};
use sub::Sub;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u64);

/// Lowest first (the spec's §4.3 list reversed): a worker always takes the
/// highest-priority subscription that has a block ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Recorder,
    PagerMonitor,
    Classify,
    EmitterDecoder,
    TrunkControl,
    Listen,
}

#[derive(Debug, Clone)]
pub struct ChannelSpec {
    pub source: SourceId,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    pub min_rate: f64,
    /// A second-stage channel cut from another channel: not supported yet.
    pub parent: Option<ChannelId>,
}

/// Where a subscription starts: the block after the stage's latest, or the
/// block holding input sample `s` (back-filled from the ring).
#[derive(Debug, Clone, Copy)]
pub enum Start {
    Live,
    At(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapReason {
    Unreadable(ReadError),
    /// The channel lies outside the usable span at the tuning of the time.
    OutOfSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    SourceDetached,
    /// A retune changed the input rate, and with it the channel's output rate.
    RateChanged,
    Panicked,
}

pub enum Delivery<'a> {
    /// Block `block`'s channel samples. `first_sample = block·L`, in the
    /// input (ring) domain. The samples carry the channel filter's group
    /// delay: their content lags `first_sample` by `N/4` input samples
    /// (`K/4` output samples); nothing corrects it.
    Samples {
        block: u64,
        first_sample: u64,
        samples: &'a [Complex32],
    },
    /// Input samples `from_sample .. to_sample` produced no output.
    /// Coalesced; always precedes the next `Samples`.
    Gap {
        from_sample: u64,
        to_sample: u64,
        reason: GapReason,
    },
    Shed,
    Ended {
        reason: EndReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Called on a bus worker, one delivery at a time, never concurrently.
pub trait Consumer: Send + 'static {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow;
}

impl<F: FnMut(Delivery<'_>) -> Flow + Send + 'static> Consumer for F {
    fn deliver(&mut self, d: Delivery<'_>) -> Flow {
        self(d)
    }
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub id: ChannelId,
    pub source: SourceId,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    /// Output samples per second.
    pub sample_rate: f64,
    /// Output samples per block (`K/2`).
    pub samples_per_block: usize,
    /// Input samples per block (`L`).
    pub hop: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerState {
    Running,
    OutOfSpan,
    Shed,
    Ended,
}

#[derive(Debug, Clone)]
pub struct ConsumerStats {
    pub channel: ChannelInfo,
    pub priority: Priority,
    /// The next block to deliver.
    pub next_block: u64,
    /// `latest_block + 1 − next_block`: 0 when caught up.
    pub lag_blocks: u64,
    /// `lag_blocks·L` seconds at the source's current rate.
    pub lag_secs: f64,
    /// `Samples` deliveries.
    pub blocks: u64,
    /// Blocks recomputed from the ring after a cache miss.
    pub recomputed: u64,
    /// `Gap` deliveries.
    pub gaps: u64,
    pub state: ConsumerState,
}

#[derive(Debug, Clone)]
pub struct BusStats {
    pub workers: usize,
    pub saturated: bool,
    pub shed_total: u64,
    pub panicked_total: u64,
    pub consumers: Vec<ConsumerStats>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BusError {
    NoSuchSource,
    NestedChannelsNotSupported,
    Design(DesignError),
    /// The start point has already left the ring.
    NotHeld {
        oldest: u64,
    },
}

#[derive(Debug, Clone)]
pub struct BusConfig {
    /// Worker threads; default `available_parallelism − 2`, at least 1.
    pub workers: usize,
    /// Lag beyond which the lowest-priority consumer is shed while the pool is saturated.
    pub max_lag_secs: f64,
    pub stage: StageConfig,
}

impl Default for BusConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        BusConfig { workers: cpus.saturating_sub(2).max(1), max_lag_secs: 2.0, stage: StageConfig::default() }
    }
}

pub struct ChannelBus {
    cfg: BusConfig,
    shared: Arc<Shared>,
    stages: Mutex<HashMap<SourceId, Arc<Stage>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    next_id: AtomicU64,
    stopped: AtomicBool,
}

/// The stages' sink: a `Weak`, so bus and stage do not keep each other alive.
struct BusSink(Weak<ChannelBus>);

impl BlockSink for BusSink {
    fn on_block(&self, source: SourceId, block: u64) {
        if let Some(bus) = self.0.upgrade() {
            bus.on_block(source, block);
        }
    }
}

impl ChannelBus {
    /// Starts threads "bus-worker-<i>".
    pub fn new(cfg: BusConfig) -> Arc<ChannelBus> {
        let shared = Arc::new(Shared::new(cfg.workers, cfg.max_lag_secs));
        let workers = sched::spawn_workers(&shared, cfg.workers);
        Arc::new(ChannelBus {
            cfg,
            shared,
            stages: Mutex::new(HashMap::new()),
            workers: Mutex::new(workers),
            next_id: AtomicU64::new(1),
            stopped: AtomicBool::new(false),
        })
    }

    /// Starts the source's stage with a `BusSink(Weak<ChannelBus>)` as its
    /// BlockSink (a Weak, so bus and stage do not keep each other alive).
    /// A source already attached keeps its stage. Attaching after
    /// [`ChannelBus::stop`] is not supported: the stage runs, but no worker
    /// is left to serve its subscriptions.
    pub fn attach(self: &Arc<Self>, source: Arc<Source>) -> Arc<Stage> {
        if self.stopped.load(Ordering::SeqCst) {
            log::warn!("channel bus: source {} attached after the bus stopped; nothing will serve it", source.id().0);
        }
        let mut stages = self.stages.lock();
        stages
            .entry(source.id())
            .or_insert_with(|| Stage::start(source, self.cfg.stage.clone(), Arc::new(BusSink(Arc::downgrade(self)))))
            .clone()
    }

    /// Ends every subscription of `source` with `Ended { SourceDetached }`
    /// (delivered by the pool, once each, in a last turn), removes the stage
    /// and stops it. The ended subscriptions stay in the stats as `Ended`
    /// until unsubscribed; a later `subscribe` for the source fails with
    /// `NoSuchSource`. False if the source was not attached.
    pub fn detach(&self, source: SourceId) -> bool {
        let stage = {
            // Under the stages lock, so a concurrent subscribe either sees the
            // stage and is marked here, or does not see it (see `subscribe`).
            let mut stages = self.stages.lock();
            let stage = stages.remove(&source);
            if stage.is_some() {
                self.shared.detach(source);
            }
            stage
        };
        let Some(stage) = stage else { return false };
        stage.stop(); // outside every lock: it joins the stage thread, which calls `on_block`
        true
    }

    pub fn stage(&self, source: SourceId) -> Option<Arc<Stage>> {
        self.stages.lock().get(&source).cloned()
    }

    pub fn subscribe(
        &self,
        spec: ChannelSpec,
        start: Start,
        priority: Priority,
        consumer: Box<dyn Consumer>,
    ) -> Result<ChannelInfo, BusError> {
        if spec.parent.is_some() {
            return Err(BusError::NestedChannelsNotSupported);
        }
        let stage = self.stage(spec.source).ok_or(BusError::NoSuchSource)?;
        let source = stage.source().clone();
        let (l, n) = (stage.hop(), stage.fft_size());
        let seg = source.segment_at(source.head());
        let usable = source.snapshot().caps.usable_bandwidth_fraction;
        let design = design_channel(n, seg.sample_rate, usable, spec.center_hz - seg.center_hz, spec.bandwidth_hz, spec.min_rate)
            .map_err(BusError::Design)?;
        let next_block = match start {
            Start::Live => stage.latest_block().map_or(1, |b| b + 1),
            Start::At(s) => {
                let b = (s / l).max(1);
                let oldest = source.oldest();
                if (b - 1) * l < oldest {
                    return Err(BusError::NotHeld { oldest });
                }
                b
            }
        };
        let info = ChannelInfo {
            id: ChannelId(self.next_id.fetch_add(1, Ordering::Relaxed)),
            source: spec.source,
            center_hz: spec.center_hz,
            bandwidth_hz: spec.bandwidth_hz,
            sample_rate: design.out_rate,
            samples_per_block: design.out_per_block,
            hop: l,
        };
        let sub = Sub::new(info.clone(), spec, usable, stage, seg.id, design, next_block, consumer);
        {
            // Added under the stages lock: a source detached meanwhile is not subscribed to.
            let stages = self.stages.lock();
            if !stages.contains_key(&info.source) {
                drop(stages);
                drop(sub); // the consumer drops outside the lock
                return Err(BusError::NoSuchSource);
            }
            self.shared.add(sub, priority);
        }
        Ok(info)
    }

    /// The consumer is dropped by the pool after any delivery in progress;
    /// no callback. Also removes the entry of a subscription that ended.
    pub fn unsubscribe(&self, id: ChannelId) -> bool {
        self.shared.remove(id)
    }

    pub fn stats(&self) -> BusStats {
        let snap = self.shared.snapshot();
        let rates: HashMap<SourceId, f64> =
            self.stages.lock().iter().map(|(&id, s)| (id, s.source().segment_at(s.source().head()).sample_rate)).collect();
        let consumers = snap
            .consumers
            .into_iter()
            .map(|(channel, priority, c)| {
                // Only a running consumer lags: one out of span, shed or ended waits for nothing.
                let lag_blocks = match (c.state, snap.latest.get(&channel.source)) {
                    (ConsumerState::Running, Some(l)) => (l.block + 1).saturating_sub(c.next_block),
                    _ => 0,
                };
                let lag_secs = rates.get(&channel.source).map_or(0.0, |r| (lag_blocks * channel.hop) as f64 / r);
                ConsumerStats {
                    channel,
                    priority,
                    next_block: c.next_block,
                    lag_blocks,
                    lag_secs,
                    blocks: c.blocks,
                    recomputed: c.recomputed,
                    gaps: c.gaps,
                    state: c.state,
                }
            })
            .collect();
        BusStats {
            workers: self.cfg.workers,
            saturated: snap.saturated,
            shed_total: snap.shed_total,
            panicked_total: snap.panicked_total,
            consumers,
        }
    }

    /// Detaches every source (their subscriptions end with
    /// `SourceDetached`), then stops the workers once those last turns are
    /// delivered, and joins them. Afterwards `subscribe` fails with
    /// `NoSuchSource`. Idempotent; Drop calls it.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let sources: Vec<SourceId> = self.stages.lock().keys().copied().collect();
        for source in sources {
            self.detach(source);
        }
        self.shared.stop();
        let handles: Vec<JoinHandle<()>> = self.workers.lock().drain(..).collect();
        for h in handles {
            if h.thread().id() == std::thread::current().id() {
                continue; // called from a consumer: that worker exits after its delivery
            }
            if h.join().is_err() {
                log::warn!("channel bus: a worker thread panicked");
            }
        }
    }

    fn on_block(&self, source: SourceId, block: u64) {
        // The block's segment, for subscriptions waiting out of span in another.
        let Some(stage) = self.stage(source) else { return }; // detached
        let segment = stage.cache().get(block).map(|(seg, _)| seg.id);
        // The rate stats() uses for lag: the source's current segment's.
        let rate = stage.source().segment_at(stage.source().head()).sample_rate;
        self.shared.on_block(source, Latest { block, segment, rate });
    }
}

impl Drop for ChannelBus {
    fn drop(&mut self) {
        self.stop();
    }
}
