//! The keeper thread: the single caller of [`IqRing::housekeep`](super::IqRing::housekeep)
//! for every source's ring.
//!
//! Every [`Keeper::TICK`] it asks the budget for every member's allowance
//! (once per tick) and hands each to the registered member with that id,
//! which applies it, housekeeps its ring, prunes its segment table and
//! timeline, and runs its device watchdog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::budget::{Allowance, RingBudget, SourceId};

/// Something the keeper looks after once per tick.
pub trait Housekept: Send + Sync {
    fn id(&self) -> SourceId;
    /// Apply `allowance`, housekeep the ring, prune, run the watchdog.
    fn tick(&self, now: Instant, allowance: Allowance);
}

pub struct Keeper {
    budget: Arc<RingBudget>,
    members: Mutex<Vec<Weak<dyn Housekept>>>,
    stop: AtomicBool,
    paused: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Keeper {
    pub const TICK: Duration = Duration::from_millis(20);

    /// Start the keeper thread (`"ring-keeper"`). The thread holds only a
    /// weak reference, so dropping the last `Arc<Keeper>` stops it.
    pub fn start(budget: Arc<RingBudget>) -> Arc<Keeper> {
        let keeper = Arc::new(Keeper {
            budget,
            members: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let weak = Arc::downgrade(&keeper);
        let handle = std::thread::Builder::new().name("ring-keeper".into()).spawn(move || run(weak)).expect("spawn the ring-keeper thread");
        *keeper.thread.lock() = Some(handle);
        keeper
    }

    pub fn budget(&self) -> &Arc<RingBudget> {
        &self.budget
    }

    pub fn register(&self, m: Weak<dyn Housekept>) {
        self.members.lock().push(m);
    }

    /// Tests only: a paused keeper skips ticks.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Stop the thread and join it (unless called on the keeper thread itself).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.thread.lock().take();
        if let Some(h) = handle {
            if h.thread().id() == std::thread::current().id() {
                // The keeper thread held the last strong reference: it exits
                // on its own once this tick returns.
                return;
            }
            h.thread().unpark();
            let _ = h.join();
        }
    }

    /// One pass over the members: live ones get their allowance, dead ones
    /// are dropped from the list.
    fn tick(&self, now: Instant) {
        let live: Vec<Arc<dyn Housekept>> = {
            let mut members = self.members.lock();
            let mut live = Vec::with_capacity(members.len());
            members.retain(|w| match w.upgrade() {
                Some(m) => {
                    live.push(m);
                    true
                }
                None => false,
            });
            live
        };
        if live.is_empty() {
            return;
        }
        let allowances = self.budget.allowances();
        for m in &live {
            let id = m.id();
            if let Some(&(_, a)) = allowances.iter().find(|(i, _)| *i == id) {
                m.tick(now, a);
            }
        }
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(weak: Weak<Keeper>) {
    let mut next = Instant::now();
    loop {
        {
            let Some(keeper) = weak.upgrade() else { return };
            if keeper.stop.load(Ordering::SeqCst) {
                return;
            }
            if !keeper.paused.load(Ordering::SeqCst) {
                keeper.tick(Instant::now());
            }
        }
        next += Keeper::TICK;
        let now = Instant::now();
        if next <= now {
            // Fell behind (a slow pass): restart the cadence rather than burst.
            next = now;
        } else {
            std::thread::park_timeout(next - now);
        }
    }
}
