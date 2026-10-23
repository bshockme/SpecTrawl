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
//! Shedding (spec §4.3): every `on_block` counts whether it found every
//! worker running with a non-empty ready queue. Once a second the share of
//! such calls decides `saturated` (at least 90 %); if saturated, the
//! lowest-priority running subscription lagging more than `max_lag_secs`
//! (ties: the largest lag) is shed: marked `Shed` and queued at the front of
//! the highest priority for one last, cheap turn in which the worker
//! delivers `Delivery::Shed` and drops the consumer.
//!
//! Detaching a source uses the same last turn: every subscription of the
//! source still running (or out of span) is marked and queued at the front
//! of the highest priority; its last turn delivers `Ended { SourceDetached }`
//! and drops the consumer. Once a subscription has ended (or been shed) and
//! has no last turn pending, the scheduler drops it and keeps only its
//! counters for the stats, so a detached source's stage and ring go with the
//! last subscription's last turn. After `stop`, workers run the last turns
//! already queued, then exit.
//!
//! Lock order: the scheduler lock and a subscription's lock are never held
//! together; the scheduler lock is never held while a block runs or a
//! consumer is called.

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::sub::{Counters, Ran, Sub};
use super::{ChannelId, ChannelInfo, ConsumerState, Priority};
use crate::budget::SourceId;
use crate::segments::SegmentId;

const PRIORITIES: usize = Priority::Listen as usize + 1;
/// Where a shed subscription waits for its last turn: the front of the highest priority.
const TOP: usize = PRIORITIES - 1;
/// The saturation window.
const WINDOW: Duration = Duration::from_secs(1);

/// What a subscription's last turn delivers before it drops the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastTurn {
    Shed,
    Detached,
}

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
    /// `None` once the subscription has ended (or been shed) for good.
    sub: Option<Arc<Mutex<Sub>>>,
    info: ChannelInfo,
    priority: Priority,
    /// As of the end of its last turn.
    counters: Counters,
    turn: Turn,
    /// Shed or detached: its next turn delivers `Delivery::Shed` or
    /// `Ended { SourceDetached }` and drops the consumer.
    last_turn: Option<LastTurn>,
}

/// A source's latest cached block, its segment (`None`: not known) and the
/// source's current sample rate (for lag in seconds).
#[derive(Debug, Clone, Copy)]
pub(super) struct Latest {
    pub block: u64,
    pub segment: Option<SegmentId>,
    pub rate: f64,
}

impl Entry {
    /// Has a block to run once `latest` is cached. Out of span, only once
    /// the latest block lies in another segment.
    fn behind(&self, latest: Option<Latest>) -> bool {
        let Some(latest) = latest else { return false };
        let c = &self.counters;
        match c.state {
            ConsumerState::Running => c.next_block <= latest.block,
            // An unknown segment (cache miss) counts as the same one: waking on every block would spin.
            ConsumerState::OutOfSpan => c.next_block <= latest.block && latest.segment.is_some_and(|s| s != c.segment),
            ConsumerState::Shed | ConsumerState::Ended => false,
        }
    }

    /// Seconds behind `latest` at the source's rate, as the stats count it;
    /// only a running subscription lags.
    fn lag_secs(&self, latest: Option<Latest>) -> f64 {
        match (self.counters.state, latest) {
            (ConsumerState::Running, Some(l)) if l.rate > 0.0 => {
                ((l.block + 1).saturating_sub(self.counters.next_block) * self.info.hop) as f64 / l.rate
            }
            _ => 0.0,
        }
    }
}

/// The current saturation window's counts.
#[derive(Default)]
struct Window {
    /// `None` until the first `on_block`.
    start: Option<Instant>,
    calls: u64,
    saturated_calls: u64,
}

struct Sched {
    subs: HashMap<ChannelId, Entry>,
    queues: [VecDeque<ChannelId>; PRIORITIES],
    latest: HashMap<SourceId, Latest>,
    stop: bool,
    panicked_total: u64,
    shed_total: u64,
    workers: usize,
    max_lag_secs: f64,
    /// Workers in a turn.
    running: usize,
    window: Window,
    /// As of the last full window.
    saturated: bool,
}

impl Sched {
    fn enqueue(&mut self, id: ChannelId) {
        if let Some(e) = self.subs.get_mut(&id) {
            e.turn = Turn::Queued;
            self.queues[e.priority as usize].push_back(id);
        }
    }

    /// The highest-priority queued subscription, marked running, and its
    /// last turn if it was shed or detached. With `last_only` (the bus is
    /// stopping), only a subscription due a last turn is taken; the rest
    /// stay queued. Ids of subscriptions removed while queued are skipped.
    fn take_next(&mut self, last_only: bool) -> Option<(ChannelId, Arc<Mutex<Sub>>, Option<LastTurn>)> {
        let Sched { subs, queues, running, .. } = self;
        for q in queues.iter_mut().rev() {
            let mut i = 0;
            while i < q.len() {
                let id = q[i];
                let Some(e) = subs.get_mut(&id) else {
                    q.remove(i);
                    continue;
                };
                if last_only && e.last_turn.is_none() {
                    i += 1;
                    continue;
                }
                q.remove(i);
                let Some(sub) = e.sub.clone() else { continue };
                e.turn = Turn::Running;
                *running += 1;
                return Some((id, sub, e.last_turn));
            }
        }
        None
    }

    /// Queue `id` at the front of the highest priority for its last turn.
    fn queue_last_turn(&mut self, id: ChannelId) {
        let Some(e) = self.subs.get_mut(&id) else { return };
        match e.turn {
            Turn::Idle => {
                e.turn = Turn::Queued;
                self.queues[TOP].push_front(id);
            }
            Turn::Queued => {
                let priority = e.priority as usize;
                self.queues[priority].retain(|&q| q != id);
                self.queues[TOP].push_front(id);
            }
            Turn::Running => {} // the worker queues it after the block in progress
        }
    }

    /// Counts this `on_block` call; at the end of each window decides
    /// `saturated` and, if so, sheds at most one laggard.
    fn check_saturation(&mut self) {
        let now = Instant::now();
        let busy = self.running >= self.workers && self.queues.iter().any(|q| !q.is_empty());
        let w = &mut self.window;
        w.calls += 1;
        if busy {
            w.saturated_calls += 1;
        }
        let start = *w.start.get_or_insert(now);
        if now.duration_since(start) < WINDOW {
            return;
        }
        // `calls` counts this call, so it is never 0 here.
        self.saturated = w.saturated_calls as f64 >= 0.9 * w.calls as f64;
        *w = Window { start: Some(now), calls: 0, saturated_calls: 0 };
        if self.saturated {
            self.shed_one();
        }
    }

    /// Sheds the lowest-priority running subscription lagging more than
    /// `max_lag_secs` (ties: the largest lag), if there is one.
    fn shed_one(&mut self) {
        let candidates = self
            .subs
            .iter()
            .filter(|(_, e)| e.last_turn.is_none())
            .map(|(&id, e)| (id, e.priority, e.lag_secs(self.latest.get(&e.info.source).copied())));
        let Some((id, priority, lag)) = choose_victim(candidates, self.max_lag_secs) else { return };
        let e = self.subs.get_mut(&id).expect("just found");
        e.counters.state = ConsumerState::Shed;
        e.last_turn = Some(LastTurn::Shed);
        self.queue_last_turn(id);
        self.shed_total += 1;
        let id = id.0;
        log::warn!("channel bus: shed channel {id} ({priority:?}) lagging {lag:.1} s under a saturated pool");
    }
}

/// Of `(id, priority, lag_secs)` candidates (lag 0 unless running), the one
/// to shed: the lowest priority lagging more than `max_lag_secs`; ties go to
/// the largest lag.
fn choose_victim(candidates: impl Iterator<Item = (ChannelId, Priority, f64)>, max_lag_secs: f64) -> Option<(ChannelId, Priority, f64)> {
    candidates.filter(|&(_, _, lag)| lag > max_lag_secs).min_by(|a, b| a.1.cmp(&b.1).then(b.2.total_cmp(&a.2)))
}

/// What [`ChannelBus::stats`](super::ChannelBus::stats) reads under the lock.
pub(super) struct Snapshot {
    pub latest: HashMap<SourceId, Latest>,
    pub panicked_total: u64,
    pub shed_total: u64,
    pub saturated: bool,
    pub consumers: Vec<(ChannelInfo, Priority, Counters)>,
}

pub(super) struct Shared {
    sched: Mutex<Sched>,
    ready: Condvar,
}

impl Shared {
    pub fn new(workers: usize, max_lag_secs: f64) -> Shared {
        let sched = Sched {
            subs: HashMap::new(),
            queues: Default::default(),
            latest: HashMap::new(),
            stop: false,
            panicked_total: 0,
            shed_total: 0,
            workers,
            max_lag_secs,
            running: 0,
            window: Window::default(),
            saturated: false,
        };
        Shared { sched: Mutex::new(sched), ready: Condvar::new() }
    }

    pub fn add(&self, sub: Sub, priority: Priority) {
        let (info, counters) = (sub.info().clone(), sub.counters());
        let id = info.id;
        let mut s = self.sched.lock();
        let latest = s.latest.get(&info.source).copied();
        let entry = Entry { sub: Some(Arc::new(Mutex::new(sub))), info, priority, counters, turn: Turn::Idle, last_turn: None };
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

    /// `source` is detached: every subscription of it still running (or out
    /// of span) is queued for a last turn that delivers
    /// `Ended { SourceDetached }`; one already shed, ended or due a last turn
    /// gets nothing more. The source's latest block is forgotten.
    pub fn detach(&self, source: SourceId) {
        let mut s = self.sched.lock();
        s.latest.remove(&source);
        let ids: Vec<ChannelId> = s
            .subs
            .iter()
            .filter(|(_, e)| {
                e.info.source == source
                    && e.sub.is_some()
                    && e.last_turn.is_none()
                    && matches!(e.counters.state, ConsumerState::Running | ConsumerState::OutOfSpan)
            })
            .map(|(&id, _)| id)
            .collect();
        for &id in &ids {
            s.subs.get_mut(&id).expect("just found").last_turn = Some(LastTurn::Detached);
            s.queue_last_turn(id);
        }
        if !ids.is_empty() {
            self.ready.notify_all();
        }
    }

    /// Block `latest.block` of `source` is cached: queue every idle
    /// subscription of that source that it brings work for; then count the
    /// call for the saturation window (and shed, once a second, if saturated).
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
        let shed_before = s.shed_total;
        s.check_saturation();
        if s.shed_total != shed_before {
            self.ready.notify_one(); // the shed subscription's last turn
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
        Snapshot { latest: s.latest.clone(), panicked_total: s.panicked_total, shed_total: s.shed_total, saturated: s.saturated, consumers }
    }

    /// Workers exit after the turn they are in.
    pub fn stop(&self) {
        self.sched.lock().stop = true;
        self.ready.notify_all();
    }

    fn work(&self) {
        loop {
            let (id, sub, last) = {
                let mut s = self.sched.lock();
                loop {
                    let stop = s.stop;
                    if let Some(next) = s.take_next(stop) {
                        break next;
                    }
                    if stop {
                        return;
                    }
                    self.ready.wait(&mut s);
                }
            };
            let (counters, ran) = {
                let mut g = sub.lock();
                let turn = || match last {
                    None => g.run_block(),
                    Some(LastTurn::Shed) => g.shed(),
                    Some(LastTurn::Detached) => g.detached(),
                };
                let ran = match catch_unwind(AssertUnwindSafe(turn)) {
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
                s.running -= 1;
                if ran.panicked {
                    s.panicked_total += 1;
                }
                if let Some(e) = s.subs.get_mut(&id) {
                    e.counters = counters;
                    // Shed or detached while this ordinary turn ran: its last turn is next, unless it ended meanwhile.
                    let pending = e.last_turn.filter(|_| last.is_none() && counters.state != ConsumerState::Ended);
                    if e.last_turn == Some(LastTurn::Shed) && last.is_none() && counters.state == ConsumerState::Ended {
                        // The consumer ended (Stop or panic) before `Shed` could be delivered:
                        // the shed is cancelled, so it no longer counts in `shed_total`.
                        s.shed_total = s.shed_total.saturating_sub(1);
                    }
                    if last.is_some() || counters.state == ConsumerState::Ended {
                        e.last_turn = None;
                    }
                    if let Some(kind) = pending {
                        if kind == LastTurn::Shed {
                            e.counters.state = ConsumerState::Shed;
                        }
                        e.turn = Turn::Queued;
                        s.queues[TOP].push_front(id);
                    } else if matches!(e.counters.state, ConsumerState::Shed | ConsumerState::Ended) {
                        // Done for good: only the counters stay, for the stats. The
                        // subscription (and its stage and source) drops below, outside the lock.
                        e.sub = None;
                        e.turn = Turn::Idle;
                    } else if !ran.wait && e.behind(s.latest.get(&e.info.source).copied()) {
                        e.turn = Turn::Queued;
                        s.queues[e.priority as usize].push_back(id);
                    } else {
                        e.turn = Turn::Idle;
                    }
                }
            }
            drop(sub); // unsubscribed or done meanwhile: the last reference, so the subscription drops here
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_victim_is_the_lowest_priority_laggard_then_the_largest_lag() {
        let mut c = vec![
            (ChannelId(1), Priority::Listen, 3.0),
            (ChannelId(2), Priority::Classify, 1.0),
            (ChannelId(3), Priority::Recorder, 0.6),
            (ChannelId(4), Priority::Recorder, 0.9),
            (ChannelId(5), Priority::Recorder, 0.4), // lowest priority but within max_lag_secs
            (ChannelId(6), Priority::PagerMonitor, 0.0),
        ];
        let mut order = Vec::new();
        while let Some((id, _, _)) = choose_victim(c.iter().copied(), 0.5) {
            order.push(id.0);
            c.retain(|x| x.0 != id);
        }
        assert_eq!(order, vec![4, 3, 2, 1]);
    }

    #[test]
    fn nobody_is_shed_when_no_one_lags_past_the_limit() {
        let c = [(ChannelId(1), Priority::Recorder, 0.5), (ChannelId(2), Priority::Listen, 0.1)];
        assert_eq!(choose_victim(c.iter().copied(), 0.5), None);
    }
}
