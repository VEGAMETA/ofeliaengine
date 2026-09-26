//! Type-erased job records.
//!
//! A job is any `#[repr(C)]` struct whose first field is a [`JobHeader`]. Queues store
//! pointers to the header; executing a job calls the function stored in it with that
//! pointer, and the function casts it back to the concrete type. Job records live on the
//! stack of the waiting thread or in its spawn arena, never on the heap.

use super::latch::Latch;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

/// Panic payload captured from a job, re-thrown on the thread that waits for it.
pub(crate) type Panic = Box<dyn Any + Send + 'static>;

#[repr(C)]
pub(crate) struct JobHeader {
    execute: unsafe fn(JobRef),
}

/// Pointer to a job's header, as stored in the queues.
pub(crate) type JobRef = NonNull<JobHeader>;

impl JobHeader {
    pub(crate) const fn new(execute: unsafe fn(JobRef)) -> Self {
        Self { execute }
    }
}

/// Runs a job.
///
/// # Safety
///
/// `job` must point to a live job record whose header was created for its concrete type,
/// and the job must not have been executed yet (or be of a kind that tolerates repeated
/// execution, like the parallel-for helper).
#[inline]
pub(crate) unsafe fn execute(job: JobRef) {
    // SAFETY: guaranteed by the caller.
    unsafe { ((*job.as_ptr()).execute)(job) }
}

pub(crate) enum JobResult<R> {
    Pending,
    Ok(R),
    Panicked(Panic),
}

impl<R> JobResult<R> {
    fn call(f: impl FnOnce() -> R) -> Self {
        match panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(r) => JobResult::Ok(r),
            Err(p) => JobResult::Panicked(p),
        }
    }

    /// Returns the value or re-throws the job's panic.
    pub(crate) fn into_value(self) -> R {
        match self {
            JobResult::Ok(r) => r,
            JobResult::Panicked(p) => panic::resume_unwind(p),
            // Callers only read the result after the job's latch was set.
            JobResult::Pending => unreachable!("job result read before completion"),
        }
    }
}

/// A job that lives on the stack of the thread that waits for it (`join`, `install`).
#[repr(C)]
pub(crate) struct StackJob<L, F, R> {
    header: JobHeader,
    pub(crate) latch: L,
    func: UnsafeCell<Option<F>>,
    result: UnsafeCell<JobResult<R>>,
}

impl<L: Latch, F: FnOnce() -> R, R> StackJob<L, F, R> {
    pub(crate) fn new(latch: L, func: F) -> Self {
        Self {
            header: JobHeader::new(Self::execute_raw),
            latch,
            func: UnsafeCell::new(Some(func)),
            result: UnsafeCell::new(JobResult::Pending),
        }
    }

    pub(crate) fn as_job_ref(&self) -> JobRef {
        // `header` is the first field of a `repr(C)` struct. The pointer is derived from
        // the whole job, not from `&self.header`: `execute_raw` reaches the other fields
        // through it, which a pointer to the header alone may not (Stacked Borrows).
        NonNull::from(self).cast()
    }

    unsafe fn execute_raw(job: JobRef) {
        let this = job.cast::<Self>().as_ptr();
        // SAFETY: `job` came from `as_job_ref` on a live `StackJob` that the owner keeps
        // alive until the latch is set; each stack job is executed at most once, so this
        // thread has exclusive access to `func` and `result`.
        unsafe {
            let func = (*(*this).func.get()).take();
            if let Some(func) = func {
                *(*this).result.get() = JobResult::call(func);
            }
            // After this call the owner may free the job; nothing touches it afterwards.
            L::set(&raw const (*this).latch);
        }
    }

    /// Runs the job on the owner thread after popping it back from the local deque.
    pub(crate) fn run_inline(self) -> R {
        let func = self.func.into_inner();
        match func {
            Some(func) => func(),
            // The job was popped back, so nobody else executed it.
            None => unreachable!("stack job executed twice"),
        }
    }

    /// Takes the result after the latch was observed set.
    pub(crate) fn into_result(self) -> JobResult<R> {
        self.result.into_inner()
    }
}

// SAFETY: the job moves `F` to the executing thread and `R` back; both are `Send` at the
// public API boundary. The latch types are `Sync`.
unsafe impl<L: Sync, F: Send, R: Send> Sync for StackJob<L, F, R> {}
