//! The worker pool and its scheduler.
//!
//! One `Mutex<Sched>` + `Condvar`. `Sched` holds every subscription's entry,
//! one FIFO of ready subscriptions per priority and the latest block of each
//! source. A worker pops from the highest non-empty FIFO, marks the entry
//! running, releases the lock and runs one block of that subscription under
//! the subscription's own mutex; then it re-takes the scheduler lock and puts
//! the subscription at the back of its FIFO if it is still behind, else
//! marks it idle until the next `on_block`. One block per turn is the
//! round-robin. A subscription out of span stays idle until a block of
//! another segment is cached: it then skips the out-of-span stretch in one
//! turn instead of spending a turn on every block of it.
//!
//! Lock order: the scheduler lock and a subscription's lock are never held
//! together; the scheduler lock is never held while a block runs or a
//! consumer is called.

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex};

use super::sub::{Counters, Ran, Sub};
use super::{ChannelId, ChannelInfo, ConsumerState, Priority};
use crate::budget::SourceId;
use crate::segments::SegmentId;

const PRIORITIES: usize = Priority::Listen as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// Caught up (or ended): waits for the next block.
    Idle,
    /// In its priority's FIFO.
    Queued,
    /// A worker is running one of its blocks.
    Running,
}

struct Entry {
    sub: Arc<Mutex<Sub>>,
    info: ChannelInfo,
    priority: Priority,
    /// As of the end of its last turn.
    counters: Counters,
    turn: Turn,
}

/// A source's latest cached block and its segment (`None`: not known).
#[derive(Debug, Clone, Copy)]
pub(super) struct Latest {
    pub block: u64,
    pub segment: Option<SegmentId>,
}

impl Entry {
    /// Has a block to run once `latest` is cached. Out of span, only once
    /// the latest block lies in another segment.
    fn behind(&self, latest: Option<Latest>) -> bool {
        let Some(latest) = latest else { return false };
        let c = &self.counters;
        match c.state {
            ConsumerState::Running => c.next_block <= latest.block,
            ConsumerState::OutOfSpan => c.next_block <= latest.block && latest.segment != Some(c.segment),
            ConsumerState::Shed | ConsumerState::Ended => false,
        }
    }
}

#[derive(Default)]
struct Sched {
    subs: HashMap<ChannelId, Entry>,
    queues: [VecDeque<ChannelId>; PRIORITIES],
    latest: HashMap<SourceId, Latest>,
    stop: bool,
    panicked_total: u64,
    shed_total: u64,
}

impl Sched {
    fn enqueue(&mut self, id: ChannelId) {
        if let Some(e) = self.subs.get_mut(&id) {
            e.turn = Turn::Queued;
            self.queues[e.priority as usize].push_back(id);
        }
    }

    /// The highest-priority queued subscription, marked running. Ids of
    /// subscriptions removed while queued are skipped.
    fn take_next(&mut self) -> Option<(ChannelId, Arc<Mutex<Sub>>)> {
        let Sched { subs, queues, .. } = self;
        for q in queues.iter_mut().rev() {
            while let Some(id) = q.pop_front() {
                if let Some(e) = subs.get_mut(&id) {
                    e.turn = Turn::Running;
                    return Some((id, e.sub.clone()));
                }
            }
        }
        None
    }
}

/// What [`ChannelBus::stats`](super::ChannelBus::stats) reads under the lock.
pub(super) struct Snapshot {
    pub latest: HashMap<SourceId, Latest>,
    pub panicked_total: u64,
    pub shed_total: u64,
    pub consumers: Vec<(ChannelInfo, Priority, Counters)>,
}

pub(super) struct Shared {
    sched: Mutex<Sched>,
    ready: Condvar,
}

impl Shared {
    pub fn new() -> Shared {
        Shared { sched: Mutex::new(Sched::default()), ready: Condvar::new() }
    }

    pub fn add(&self, sub: Sub, priority: Priority) {
        let (info, counters) = (sub.info().clone(), sub.counters());
        let id = info.id;
        let mut s = self.sched.lock();
        let latest = s.latest.get(&info.source).copied();
        let entry = Entry { sub: Arc::new(Mutex::new(sub)), info, priority, counters, turn: Turn::Idle };
        let behind = entry.behind(latest);
        s.subs.insert(id, entry);
        if behind {
            s.enqueue(id);
            self.ready.notify_one();
        }
    }

    /// Removes the entry. A worker running it drops the subscription (and
    /// its consumer) after the block in progress; otherwise it is dropped
    /// here, outside the lock.
    pub fn remove(&self, id: ChannelId) -> bool {
        let entry = self.sched.lock().subs.remove(&id);
        entry.is_some()
    }

    /// Block `latest.block` of `source` is cached: queue every idle
    /// subscription of that source that it brings work for.
    pub fn on_block(&self, source: SourceId, latest: Latest) {
        let mut s = self.sched.lock();
        s.latest.insert(source, latest);
        let ready: Vec<ChannelId> = s
            .subs
            .iter()
            .filter(|(_, e)| e.info.source == source && e.turn == Turn::Idle && e.behind(Some(latest)))
            .map(|(&id, _)| id)
            .collect();
        for &id in &ready {
            s.enqueue(id);
        }
        match ready.len() {
            0 => {}
            1 => {
                self.ready.notify_one();
            }
            _ => {
                self.ready.notify_all();
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.sched.lock();
        let mut consumers: Vec<_> = s.subs.values().map(|e| (e.info.clone(), e.priority, e.counters)).collect();
        consumers.sort_by_key(|c| c.0.id.0);
        Snapshot { latest: s.latest.clone(), panicked_total: s.panicked_total, shed_total: s.shed_total, consumers }
    }

    /// Workers exit after the turn they are in.
    pub fn stop(&self) {
        self.sched.lock().stop = true;
        self.ready.notify_all();
    }

    fn work(&self) {
        loop {
            let (id, sub) = {
                let mut s = self.sched.lock();
                loop {
                    if s.stop {
                        return;
                    }
                    if let Some(next) = s.take_next() {
                        break next;
                    }
                    self.ready.wait(&mut s);
                }
            };
            let (counters, ran) = {
                let mut g = sub.lock();
                let ran = match catch_unwind(AssertUnwindSafe(|| g.run_block())) {
                    Ok(ran) => ran,
                    Err(_) => {
                        log::warn!("channel bus: channel {} panicked outside its consumer; ended", id.0);
                        g.abandon();
                        Ran { panicked: true, wait: false }
                    }
                };
                (g.counters(), ran)
            };
            {
                let mut guard = self.sched.lock();
                let s = &mut *guard;
                if ran.panicked {
                    s.panicked_total += 1;
                }
                if let Some(e) = s.subs.get_mut(&id) {
                    e.counters = counters;
                    if !ran.wait && e.behind(s.latest.get(&e.info.source).copied()) {
                        e.turn = Turn::Queued;
                        s.queues[e.priority as usize].push_back(id);
                    } else {
                        e.turn = Turn::Idle;
                    }
                }
            }
            drop(sub); // unsubscribed meanwhile: the last reference, so the consumer drops here
        }
    }
}

/// Starts threads "bus-worker-<i>".
pub(super) fn spawn_workers(shared: &Arc<Shared>, n: usize) -> Vec<JoinHandle<()>> {
    (0..n)
        .map(|i| {
            let shared = shared.clone();
            std::thread::Builder::new().name(format!("bus-worker-{i}")).spawn(move || shared.work()).expect("spawn a channel bus worker")
        })
        .collect()
}
