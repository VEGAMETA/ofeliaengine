//! Jobs that foreign code hands to a pool from any thread (ADR 0002 addendum): a C
//! library's task callbacks, Jolt Physics' job system first.
//!
//! [`Scope::spawn`](super::Scope::spawn) takes jobs from its owner thread only. A library
//! calls back on whichever thread it is running (a finished job queues the jobs that waited
//! for it), so these jobs go through the pool's injector, in records from a fixed set made
//! once: no allocation per job. A job submitted while every record is in use, or the
//! injector is full, is refused and runs at once on the submitting thread, as every overflow
//! of the pool degrades.

use super::job::{JobHeader, JobRef};
use super::pool::ThreadPool;
use crate::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::CachePadded;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr::NonNull;
use std::sync::Arc;

/// A foreign job's function: called once with its argument, on any thread of the pool.
pub type ForeignFn = unsafe extern "C" fn(*mut c_void);

/// A job record: the header the pool executes, then what to run.
#[repr(C)]
struct Record {
    header: JobHeader,
    run: UnsafeCell<Option<ForeignFn>>,
    arg: UnsafeCell<*mut c_void>,
    owner: *const Shared,
}

struct Shared {
    pool: Arc<ThreadPool>,
    records: Box<[Record]>,
    free: FreeList,
    /// Jobs submitted and not finished yet.
    outstanding: CachePadded<AtomicUsize>,
}

/// Jobs foreign code hands to a [`ThreadPool`] from any thread (see the module
/// documentation).
pub struct ForeignJobs {
    // Shared and never mutated after it is made: the records point back at it.
    shared: Arc<Shared>,
}

// SAFETY: the records are written by the thread that popped them from the free list and
// read by the one thread that executes them, ordered by the injector's release and acquire;
// everything else is atomics and the pool, which is Send and Sync.
unsafe impl Send for ForeignJobs {}
// SAFETY: as above.
unsafe impl Sync for ForeignJobs {}
// SAFETY: as above (the pool's workers reach `Shared` through the records).
unsafe impl Sync for Shared {}
// SAFETY: as above.
unsafe impl Send for Shared {}

impl core::fmt::Debug for ForeignJobs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ForeignJobs")
            .field("capacity", &self.shared.records.len())
            .field("outstanding", &self.outstanding())
            .finish()
    }
}

impl ForeignJobs {
    /// Records for `capacity` jobs in flight at once, run by `pool`.
    pub fn new(pool: Arc<ThreadPool>, capacity: usize) -> Self {
        let capacity = capacity.clamp(1, u32::MAX as usize - 1);
        let shared = Arc::new_cyclic(|this| {
            // Where the `Shared` being made will live.
            let owner = this.as_ptr();
            Shared {
                pool,
                records: (0..capacity)
                    .map(|_| Record {
                        header: JobHeader::new(execute_record),
                        run: UnsafeCell::new(None),
                        arg: UnsafeCell::new(core::ptr::null_mut()),
                        owner,
                    })
                    .collect(),
                free: FreeList::new(capacity),
                outstanding: CachePadded::new(AtomicUsize::new(0)),
            }
        });
        Self { shared }
    }

    /// The pool the jobs run on.
    pub fn pool(&self) -> &Arc<ThreadPool> {
        &self.shared.pool
    }

    /// Jobs submitted and not finished yet.
    pub fn outstanding(&self) -> usize {
        self.shared.outstanding.load(Ordering::Acquire)
    }

    /// Hands `run(arg)` to the pool. Returns `false`, having run nothing, when every record
    /// is in use or the pool's injector is full: the caller runs the job itself then.
    ///
    /// # Safety
    ///
    /// Calling `run(arg)` once, on any thread of the pool, at any time before
    /// [`Self::wait`] returns, must be sound.
    pub unsafe fn submit(&self, run: ForeignFn, arg: *mut c_void) -> bool {
        let shared = &*self.shared;
        let Some(index) = shared.free.pop() else {
            return false;
        };
        let record = &shared.records[index as usize];
        // SAFETY: the record was free: no other thread touches it until it is queued.
        unsafe {
            *record.run.get() = Some(run);
            *record.arg.get() = arg;
        }
        shared.outstanding.fetch_add(1, Ordering::Relaxed);
        // The header is the record's first field: a pointer to the record is one to it.
        let job: JobRef = NonNull::from(record).cast::<JobHeader>();
        let registry = shared.pool.registry();
        if registry.injector.push(job).is_err() {
            shared.outstanding.fetch_sub(1, Ordering::Relaxed);
            shared.free.push(index);
            return false;
        }
        registry.notify_injected();
        true
    }

    /// Waits until every submitted job has run. A thread of the pool runs jobs meanwhile;
    /// another thread hands the waiting to the pool and sleeps.
    pub fn wait(&self) {
        let shared = &*self.shared;
        let done = || shared.outstanding.load(Ordering::Acquire) == 0;
        if done() {
            return;
        }
        let registry = shared.pool.registry();
        match registry.current_index() {
            Some(index) => registry.wait_until(index, done),
            None => shared.pool.install(|| {
                if let Some(index) = registry.current_index() {
                    registry.wait_until(index, done);
                }
            }),
        }
    }
}

impl Drop for ForeignJobs {
    fn drop(&mut self) {
        // The records must outlive the jobs that use them.
        self.wait();
    }
}

/// Runs a record's job, then frees the record.
///
/// # Safety
///
/// `job` points to a queued [`Record`] of a live [`ForeignJobs`].
unsafe fn execute_record(job: JobRef) {
    let record = job.cast::<Record>();
    // SAFETY: guaranteed by the caller; the record's fields were written before it was
    // queued (ordered by the injector).
    unsafe {
        let record = record.as_ref();
        let shared = &*record.owner;
        if let Some(run) = *record.run.get() {
            run(*record.arg.get());
        }
        let index = (record as *const Record).offset_from(shared.records.as_ptr()) as u32;
        shared.free.push(index);
        shared.outstanding.fetch_sub(1, Ordering::Release);
    }
}

/// A lock-free stack of the free records' indices (Treiber's, its head tagged against ABA).
pub(crate) struct FreeList {
    /// Low 32 bits: the top index ([`EMPTY`] when none); high 32 bits: a tag counting
    /// changes.
    head: CachePadded<AtomicU64>,
    next: Box<[AtomicU32]>,
}

const EMPTY: u32 = u32::MAX;

impl FreeList {
    /// Every index of `0..capacity` free.
    pub(crate) fn new(capacity: usize) -> Self {
        let next = (0..capacity)
            .map(|i| {
                AtomicU32::new(if i + 1 < capacity {
                    i as u32 + 1
                } else {
                    EMPTY
                })
            })
            .collect();
        Self {
            head: CachePadded::new(AtomicU64::new(if capacity > 0 {
                0
            } else {
                u64::from(EMPTY)
            })),
            next,
        }
    }

    /// Takes a free index.
    pub(crate) fn pop(&self) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let index = head as u32;
            if index == EMPTY {
                return None;
            }
            let next = self.next[index as usize].load(Ordering::Relaxed);
            let new = ((head >> 32).wrapping_add(1) << 32) | u64::from(next);
            match self
                .head
                .compare_exchange_weak(head, new, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => return Some(index),
                Err(current) => head = current,
            }
        }
    }

    /// Gives an index back.
    pub(crate) fn push(&self, index: u32) {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            self.next[index as usize].store(head as u32, Ordering::Relaxed);
            let new = ((head >> 32).wrapping_add(1) << 32) | u64::from(index);
            match self
                .head
                .compare_exchange_weak(head, new, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Counter;

    static RUNS: Counter = Counter::new(0);

    unsafe extern "C" fn add(arg: *mut c_void) {
        RUNS.fetch_add(arg as u64, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn jobs_from_many_threads_all_run_once() {
        let pool = Arc::new(ThreadPool::builder().workers(4).build().unwrap());
        let jobs = Arc::new(ForeignJobs::new(pool, 64));
        RUNS.store(0, std::sync::atomic::Ordering::Relaxed);
        // Four foreign threads submit 10,000 jobs each; refused ones run on the spot.
        let (mut queued, mut inline) = (0u64, 0u64);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let jobs = jobs.clone();
                std::thread::spawn(move || {
                    let (mut q, mut i) = (0u64, 0u64);
                    for _ in 0..10_000 {
                        // SAFETY: `add` is sound on any thread.
                        if unsafe { jobs.submit(add, core::ptr::without_provenance_mut(1)) } {
                            q += 1;
                        } else {
                            // SAFETY: as above.
                            unsafe { add(core::ptr::without_provenance_mut(1)) };
                            i += 1;
                        }
                    }
                    (q, i)
                })
            })
            .collect();
        for t in threads {
            let (q, i) = t.join().unwrap();
            queued += q;
            inline += i;
        }
        jobs.wait();
        assert_eq!(jobs.outstanding(), 0);
        assert_eq!(RUNS.load(std::sync::atomic::Ordering::Relaxed), 40_000);
        assert!(queued > 0, "nothing reached the pool ({inline} inline)");
        // Every record came back.
        let mut free = 0;
        while jobs.shared.free.pop().is_some() {
            free += 1;
        }
        assert_eq!(free, 64);
    }

    #[test]
    fn a_pool_thread_waits_by_working() {
        let pool = Arc::new(ThreadPool::builder().workers(2).build().unwrap());
        let jobs = ForeignJobs::new(pool.clone(), 8);
        pool.install(|| {
            for _ in 0..100 {
                // SAFETY: `add` is sound on any thread.
                if !unsafe { jobs.submit(add, core::ptr::null_mut()) } {
                    // SAFETY: as above.
                    unsafe { add(core::ptr::null_mut()) };
                }
            }
            jobs.wait();
        });
        assert_eq!(jobs.outstanding(), 0);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn loom_free_list_pops_and_pushes_from_two_threads() {
        loom::model(|| {
            let list = Arc::new(FreeList::new(2));
            let other = {
                let list = list.clone();
                thread::spawn(move || {
                    let got = list.pop();
                    if let Some(i) = got {
                        list.push(i);
                    }
                    got.is_some()
                })
            };
            if let Some(i) = list.pop() {
                list.push(i);
            }
            other.join().unwrap();
            let mut seen = [false; 2];
            while let Some(i) = list.pop() {
                assert!(!seen[i as usize], "index {i} twice");
                seen[i as usize] = true;
            }
            assert_eq!(seen, [true, true]);
        });
    }
}
