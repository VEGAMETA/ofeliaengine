//! Shared state of a pool: participants, their deques, the injector, and the sleep/wake
//! protocol.
//!
//! # Sleeping without locks
//!
//! Idle participants block in `std::thread::park`, which on Windows 10+ is `WaitOnAddress`
//! and on Linux a futex; there is no mutex or condition variable anywhere. Each
//! participant has a `state` (`AWAKE`, `SLEEPING`, `WOKEN`, `WOKEN_SEARCH`) and a bit in
//! the `sleeping` mask. A waker claims a sleeper with a CAS on its state, then unparks it;
//! only the sleeper itself clears its mask bit, so a stale bit never hides a sleeper.
//!
//! Lost wake-ups are ruled out Dekker-style where they would matter: a thread that
//! injects a job and blocks issues a `SeqCst` fence after the push and then reads the
//! mask; the sleeper sets its mask bit with a `SeqCst` RMW and then re-checks every queue
//! before parking, so one of the two always sees the other. Latch completions use the
//! same pattern on the per-participant `state`, and so do requests to give memory back
//! (`request_release`: the epoch's `SeqCst` RMW before claiming sleepers, re-read by a
//! sleeper after it announced itself). Pushes to a participant's own deque skip the fence
//! (see `notify_work`).
//!
//! # Throttled wake-ups
//!
//! Like the Go scheduler (Vyukov, "Scalable Go Scheduler Design Doc", 2012) and Tokio,
//! a single push wakes a sleeper only when nobody is already searching for work. A woken
//! searcher that finds work and was the last searcher wakes the next one, so wake-ups
//! cascade as long as work keeps appearing but do not stampede when one thread produces
//! and consumes its own jobs (deep `join` recursion). Bulk submissions
//! (`parallel_for`) wake as many sleepers as they published jobs.

use super::arena::SpawnArena;
use super::deque::{Deque, Steal};
use super::injector::Injector;
use super::job::{JobHeader, JobRef, execute};
use crate::sync::CachePadded;
use core::cell::Cell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::OnceLock;
use std::thread::{self, Thread};

const AWAKE: u8 = 0;
const SLEEPING: u8 = 1;
const WOKEN: u8 = 2;
const WOKEN_SEARCH: u8 = 3;

/// Maximum number of participants (workers + registered external threads) per pool.
pub(crate) const MAX_PARTICIPANTS: usize = 64;

/// Spin-loop hints per idle round before a participant considers sleeping.
const SPINS_PER_ROUND: u32 = 32;

thread_local! {
    /// The pool this thread participates in and its slot index. The address of this
    /// thread-local also serves as the thread's identity for `Slot::bound`.
    static CURRENT: Cell<(*const Registry, usize)> = const { Cell::new((ptr::null(), 0)) };
}

/// State only the participant bound to a slot may touch.
pub(crate) struct OwnerState {
    pub(crate) arena: SpawnArena,
    rng: Cell<u64>,
    searching: Cell<bool>,
    /// Ran a job since it last went to sleep: its allocator heap may hold memory to give
    /// back (ADR 0086).
    ran_jobs: Cell<bool>,
    /// The last [`Registry::release_epoch`] it gave memory back for.
    released: Cell<u64>,
}

pub(crate) struct Slot {
    pub(crate) deque: Deque<JobHeader>,
    state: CachePadded<AtomicU8>,
    thread: OnceLock<Thread>,
    /// Identity (thread-local address) of the thread bound to this slot; 0 if free.
    /// A slot is bound once and never released for the lifetime of the pool.
    bound: AtomicUsize,
    pub(crate) owner: OwnerState,
}

// SAFETY: `owner` (the only non-`Sync` part) is accessed exclusively by the thread bound
// to the slot, which `Registry::current_index` verifies before handing out the index.
unsafe impl Sync for Slot {}
// SAFETY: as above; all other fields are `Send`.
unsafe impl Send for Slot {}

pub(crate) struct Registry {
    pub(crate) slots: Box<[Slot]>,
    pub(crate) injector: Injector<JobHeader>,
    sleeping: CachePadded<AtomicU64>,
    searching: CachePadded<AtomicUsize>,
    pub(crate) terminate: AtomicBool,
    /// Counts [`Registry::request_release`]s: a participant that has not given memory
    /// back since the last one does so before it sleeps.
    release_epoch: AtomicU64,
    pub(crate) num_workers: usize,
    spin_rounds: u32,
    #[cfg(test)]
    pub(crate) test_hook: TestHook,
}

/// Test instrumentation of giving memory back: the epoch a participant last gave it back
/// for, and a pause after doing so, before it announces its sleep (the window a release
/// asked for meanwhile falls into).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHook {
    pub(crate) pause_after_release_ms: AtomicU64,
    pub(crate) released_epoch: AtomicU64,
}

pub(crate) struct RegistryConfig {
    pub(crate) workers: usize,
    pub(crate) external: usize,
    pub(crate) deque_capacity: usize,
    pub(crate) injector_capacity: usize,
    pub(crate) arena_bytes: usize,
    pub(crate) spin_rounds: u32,
}

impl Registry {
    pub(crate) fn new(config: &RegistryConfig) -> Self {
        let total = config.workers + config.external;
        debug_assert!(total <= MAX_PARTICIPANTS);
        let slots = (0..total)
            .map(|i| Slot {
                deque: Deque::with_capacity(config.deque_capacity),
                state: CachePadded::new(AtomicU8::new(AWAKE)),
                thread: OnceLock::new(),
                bound: AtomicUsize::new(0),
                owner: OwnerState {
                    arena: SpawnArena::new(config.arena_bytes),
                    // Any odd non-zero seed works for xorshift; spread them per slot.
                    rng: Cell::new(0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(i as u64 + 1) | 1),
                    searching: Cell::new(false),
                    ran_jobs: Cell::new(false),
                    released: Cell::new(0),
                },
            })
            .collect();
        Self {
            slots,
            injector: Injector::with_capacity(config.injector_capacity),
            sleeping: CachePadded::new(AtomicU64::new(0)),
            searching: CachePadded::new(AtomicUsize::new(0)),
            terminate: AtomicBool::new(false),
            release_epoch: AtomicU64::new(0),
            num_workers: config.workers,
            spin_rounds: config.spin_rounds,
            #[cfg(test)]
            test_hook: TestHook::default(),
        }
    }

    /// Binds the calling thread to slot `index` (worker startup).
    pub(crate) fn bind_worker(&self, index: usize) {
        let slot = &self.slots[index];
        let _ = slot.thread.set(thread::current());
        CURRENT.with(|c| {
            slot.bound
                .store(ptr::from_ref(c) as usize, Ordering::Release);
            c.set((ptr::from_ref(self), index));
        });
    }

    /// Binds the calling thread to a free external slot. Returns the slot index.
    pub(crate) fn bind_external(&self) -> Option<usize> {
        if let Some(index) = self.current_index() {
            return Some(index);
        }
        CURRENT.with(|c| {
            let me = ptr::from_ref(c) as usize;
            for index in self.num_workers..self.slots.len() {
                let slot = &self.slots[index];
                if slot
                    .bound
                    .compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    let _ = slot.thread.set(thread::current());
                    c.set((ptr::from_ref(self), index));
                    return Some(index);
                }
            }
            None
        })
    }

    /// The calling thread's slot index, if it participates in this pool.
    #[inline]
    pub(crate) fn current_index(&self) -> Option<usize> {
        CURRENT.with(|c| {
            let (registry, index) = c.get();
            // Compare addresses first: `registry` may dangle if the thread was bound to a
            // pool that has been dropped since, so it is only dereferenced via `self`.
            if ptr::eq(registry, self)
                && self.slots[index].bound.load(Ordering::Relaxed) == ptr::from_ref(c) as usize
            {
                Some(index)
            } else {
                None
            }
        })
    }

    pub(crate) fn num_participants_hint(&self) -> usize {
        self.num_workers + 1
    }

    // ---------------------------------------------------------------------------------
    // Waking
    // ---------------------------------------------------------------------------------

    /// Wakes participant `index` if it sleeps (a latch it waits for was set).
    #[inline]
    pub(crate) fn wake_participant(&self, index: usize) {
        let slot = &self.slots[index];
        if slot
            .state
            .compare_exchange(SLEEPING, WOKEN, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            && let Some(thread) = slot.thread.get()
        {
            thread.unpark();
        }
    }

    /// Called after pushing one job to the caller's own deque.
    ///
    /// No fence: the pusher is a participant that waits for its jobs and pops its own
    /// deque while waiting, so a wake-up missed in a race costs parallelism, never
    /// progress (the same reasoning as rayon's `new_internal_jobs`). This keeps a push at
    /// two plain loads when nobody sleeps.
    #[inline]
    pub(crate) fn notify_work(&self) {
        if self.searching.load(Ordering::Relaxed) != 0 {
            return;
        }
        if self.sleeping.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.wake_sleepers(1);
    }

    /// Called after pushing `count` jobs at once to the caller's own deque.
    #[inline]
    pub(crate) fn notify_work_many(&self, count: usize) {
        if self.sleeping.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.wake_sleepers(count);
    }

    /// Called after pushing to the injector. The submitter blocks and never runs the job
    /// itself, so here a missed wake-up would be a deadlock: the `SeqCst` fence pairs
    /// with the sleeper's `SeqCst` mask RMW followed by its queue re-check.
    pub(crate) fn notify_injected(&self) {
        fence(Ordering::SeqCst);
        if self.sleeping.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.wake_sleepers(1);
    }

    /// Wakes up to `count` sleepers as searchers. Prefers low slot indices.
    fn wake_sleepers(&self, mut count: usize) {
        let mut mask = self.sleeping.load(Ordering::SeqCst);
        while mask != 0 && count > 0 {
            let index = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let slot = &self.slots[index];
            // Count the searcher before it can run, so its decrement never underflows.
            self.searching.fetch_add(1, Ordering::SeqCst);
            if slot
                .state
                .compare_exchange(SLEEPING, WOKEN_SEARCH, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                if let Some(thread) = slot.thread.get() {
                    thread.unpark();
                }
                count -= 1;
            } else {
                self.searching.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Asks every participant to give its allocator heap's memory back: the sleeping ones
    /// wake, do so on their way back to sleep, and sleep again (ADR 0086).
    pub(crate) fn request_release(&self) {
        // SeqCst: pairs with the epoch check a participant makes after announcing its sleep
        // (`sleep`): either that check sees the new epoch, or `wake_all` sees it asleep.
        self.release_epoch.fetch_add(1, Ordering::SeqCst);
        self.wake_all();
    }

    /// Wakes every sleeper (shutdown, [`Self::request_release`]).
    pub(crate) fn wake_all(&self) {
        for slot in self.slots.iter() {
            if slot
                .state
                .compare_exchange(SLEEPING, WOKEN, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
                && let Some(thread) = slot.thread.get()
            {
                thread.unpark();
            }
        }
    }

    // ---------------------------------------------------------------------------------
    // Finding work
    // ---------------------------------------------------------------------------------

    fn next_random(&self, index: usize) -> u64 {
        let rng = &self.slots[index].owner.rng;
        let mut x = rng.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        rng.set(x);
        x
    }

    /// Pops local work, then steals from other participants, then from the injector.
    #[inline]
    fn find_work(&self, index: usize) -> Option<JobRef> {
        // SAFETY: `index` is the calling thread's own slot.
        if let Some(job) = unsafe { self.slots[index].deque.pop() } {
            return Some(job);
        }
        self.steal_work(index)
    }

    fn steal_work(&self, index: usize) -> Option<JobRef> {
        let n = self.slots.len();
        for _ in 0..4 {
            let mut retry = false;
            let start = (self.next_random(index) % n as u64) as usize;
            for k in 0..n {
                let victim = (start + k) % n;
                if victim == index {
                    continue;
                }
                match self.slots[victim].deque.steal() {
                    Steal::Success(job) => return Some(job),
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
            }
            if let Some(job) = self.injector.pop() {
                return Some(job);
            }
            if !retry {
                break;
            }
        }
        None
    }

    fn has_work(&self) -> bool {
        !self.injector.is_empty() || self.slots.iter().any(|s| !s.deque.is_empty())
    }

    fn found_work(&self, index: usize) {
        let searching = &self.slots[index].owner.searching;
        if searching.get() {
            searching.set(false);
            // The last searcher to find work hands the search over to a sleeper, so
            // wake-ups keep up with a growing amount of work.
            if self.searching.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.notify_work();
            }
        }
    }

    fn stop_searching(&self, index: usize) {
        let searching = &self.slots[index].owner.searching;
        if searching.get() {
            searching.set(false);
            self.searching.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Runs jobs until `done()` returns true. Used by workers (with `done` = shutdown)
    /// and by every participant that waits for its own jobs: waiting always helps.
    pub(crate) fn wait_until(&self, index: usize, done: impl Fn() -> bool) {
        let mut idle_rounds = 0u32;
        loop {
            if done() {
                break;
            }
            if let Some(job) = self.find_work(index) {
                self.found_work(index);
                // SAFETY: jobs in the queues are live until executed, and each queued
                // reference is taken by exactly one thread.
                unsafe { execute(job) };
                self.slots[index].owner.ran_jobs.set(true);
                idle_rounds = 0;
                continue;
            }
            if idle_rounds < self.spin_rounds {
                idle_rounds += 1;
                for _ in 0..SPINS_PER_ROUND {
                    core::hint::spin_loop();
                }
                continue;
            }
            self.sleep(index, &done);
            idle_rounds = 0;
        }
        self.stop_searching(index);
    }

    /// Runs jobs until `done()` returns true without ever sleeping: for a wait whose end
    /// nobody signals to this participant (a second waiter of `ForeignJobs`).
    pub(crate) fn work_until(&self, index: usize, done: impl Fn() -> bool) {
        loop {
            if done() {
                break;
            }
            if let Some(job) = self.find_work(index) {
                self.found_work(index);
                // SAFETY: as in `wait_until`.
                unsafe { execute(job) };
                continue;
            }
            for _ in 0..SPINS_PER_ROUND {
                core::hint::spin_loop();
            }
            thread::yield_now();
        }
        self.stop_searching(index);
    }

    fn sleep(&self, index: usize, done: &impl Fn() -> bool) {
        let slot = &self.slots[index];
        let bit = 1u64 << index;
        self.stop_searching(index);
        // Jobs leave memory in this thread's allocator heap (their temporaries, what other
        // threads freed of theirs): give it back before sleeping, or it stays committed
        // for as long as the thread sleeps (ADR 0086). Cheap, and before announcing the
        // sleep, so no wake-up waits for it.
        let epoch = self.release_epoch.load(Ordering::Acquire);
        if slot.owner.ran_jobs.replace(false) | (slot.owner.released.replace(epoch) != epoch) {
            crate::alloc::release_thread_memory();
            #[cfg(test)]
            {
                let hook = &self.test_hook;
                hook.released_epoch.store(epoch, Ordering::SeqCst);
                let pause = hook.pause_after_release_ms.load(Ordering::SeqCst);
                if pause > 0 {
                    #[allow(clippy::disallowed_methods)] // a test's pause: no precision needed
                    thread::sleep(std::time::Duration::from_millis(pause));
                }
            }
        }

        slot.state.store(SLEEPING, Ordering::SeqCst);
        self.sleeping.fetch_or(bit, Ordering::SeqCst);

        // Re-check after announcing: a job pushed, a latch set or a release asked for before
        // our announcement is visible here; one after it finds us asleep. Without the epoch
        // check, a release asked for after the read above would pass this participant by
        // (`wake_all` found it awake), and it would sleep without giving that memory back.
        let cancel = done()
            || self.has_work()
            || self.terminate.load(Ordering::SeqCst)
            || self.release_epoch.load(Ordering::SeqCst) != epoch;
        if !cancel {
            engine_profiler::zone!("jobs::park");
            while slot.state.load(Ordering::SeqCst) == SLEEPING {
                thread::park();
            }
        }
        let previous = slot.state.swap(AWAKE, Ordering::SeqCst);
        self.sleeping.fetch_and(!bit, Ordering::SeqCst);
        if previous == WOKEN_SEARCH {
            // The waker already counted us in `searching`.
            slot.owner.searching.set(true);
        }
    }
}
