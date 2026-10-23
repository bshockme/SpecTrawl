//! The keeper thread: the single caller of [`IqRing::housekeep`](super::IqRing::housekeep)
//! for every source's ring.
//!
//! Every [`Keeper::TICK`] it asks the budget for every member's allowance
//! (once per tick) and hands each to the registered member with that id,
//! which applies it, housekeeps its ring, prunes its segment table and
//! timeline, and runs its device watchdog.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// Member ticks that panicked (the keeper logs them and carries on).
    tick_panics: AtomicU32,
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
            tick_panics: AtomicU32::new(0),
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

    /// How many member ticks have panicked since the keeper started.
    pub fn tick_panics(&self) -> u32 {
        self.tick_panics.load(Ordering::Relaxed)
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
            if h.join().is_err() {
                log::warn!("ring-keeper: the keeper thread panicked");
            }
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
            if let Some(&(_, a)) = allowances.iter().find(|(i, _)| *i == id)
                && let Err(payload) = catch_unwind(AssertUnwindSafe(|| m.tick(now, a)))
            {
                // parking_lot mutexes do not poison, so the next tick is safe.
                self.tick_panics.fetch_add(1, Ordering::Relaxed);
                log::error!("ring-keeper: tick for source {} panicked: {}", id.0, panic_message(payload.as_ref()));
            }
        }
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.stop();
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "(non-string panic payload)"
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::budget::{BudgetConfig, Mode};

    struct Counter {
        id: SourceId,
        ticks: AtomicU64,
        panic_once: AtomicBool,
    }

    impl Housekept for Counter {
        fn id(&self) -> SourceId {
            self.id
        }

        fn tick(&self, _now: Instant, _allowance: Allowance) {
            self.ticks.fetch_add(1, Ordering::SeqCst);
            if self.panic_once.swap(false, Ordering::SeqCst) {
                panic!("test double panics once");
            }
        }
    }

    fn wait_until(f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < deadline, "condition not met within 20 s");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_panicking_member_tick_is_counted_and_the_keeper_keeps_ticking() {
        let budget = RingBudget::new(BudgetConfig { guaranteed_secs: 0.1, ceiling_bytes: 64 << 20, sweep_secs: 0.1 });
        let keeper = Keeper::start(budget.clone());
        let bad = Arc::new(Counter { id: SourceId(9001), ticks: AtomicU64::new(0), panic_once: AtomicBool::new(true) });
        let good = Arc::new(Counter { id: SourceId(9002), ticks: AtomicU64::new(0), panic_once: AtomicBool::new(false) });
        for m in [&bad, &good] {
            budget.join(m.id, 1e6, Mode::Live, 1.0, 1 << 20);
            let m: Arc<dyn Housekept> = m.clone();
            keeper.register(Arc::downgrade(&m));
        }
        wait_until(|| keeper.tick_panics() == 1);
        let seen = good.ticks.load(Ordering::SeqCst);
        wait_until(|| good.ticks.load(Ordering::SeqCst) >= seen + 3 && bad.ticks.load(Ordering::SeqCst) >= 3);
        assert_eq!(keeper.tick_panics(), 1);
    }
}
