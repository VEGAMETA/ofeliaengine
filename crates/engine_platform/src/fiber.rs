//! Fibers (ADR 0119): a stack of its own, on the thread that runs it, for work that
//! stops halfway and goes on later. The script debugger's stage runs on one while a
//! session is on: a stop gives the thread back to the editor, whose frames go on, and
//! Continue goes back into the stage where it stopped.
//!
//! - One fiber is made once and runs one job after another (no stack made per job).
//! - The thread that runs it becomes a fiber itself the first time (`ConvertThreadToFiber`),
//!   and stays one.
//! - Thread locals are the thread's, shared by its fibers: a job that stops leaves them as
//!   they are for the code that runs meanwhile.
//! - A panic in a job is caught at the fiber's entry and handed back: it never crosses a
//!   switch.

use core::ffi::c_void;
use std::any::Any;
use std::cell::Cell;

use windows_sys::Win32::System::Threading::{
    ConvertThreadToFiber, CreateFiberEx, DeleteFiber, IsThreadAFiber, SwitchToFiber,
};

/// The running fiber, as Windows' `GetCurrentFiber` reads it: an inline function of
/// `winnt.h`, not an export of kernel32, so not in `windows-sys`. On x64 it is the TEB's
/// `NT_TIB.FiberData`, at `gs:[0x20]`.
fn current_fiber() -> *mut c_void {
    let fiber: *mut c_void;
    // SAFETY: reads one pointer of the running thread's TEB, which `gs` addresses on x64
    // Windows; nothing is written.
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[0x20]",
            out(reg) fiber,
            options(nostack, readonly, preserves_flags)
        );
    }
    fiber
}

/// What [`Fiber::run`] and [`Fiber::resume`] came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    /// The job returned (or panicked: [`Fiber::take_panic`]).
    Done,
    /// The job gave the thread back halfway ([`suspend`]): [`Fiber::resume`] goes on.
    Suspended,
}

/// A job: a function and the context it is given.
type Job = (unsafe fn(*mut c_void), *mut c_void);

struct Shared {
    /// The fiber that entered this one: where a suspend and a job's end go back to.
    back: Cell<*mut c_void>,
    /// The job and its context, until it starts.
    job: Cell<Option<Job>>,
    /// Whether the last job returned (true before the first).
    done: Cell<bool>,
    panic: Cell<Option<Box<dyn Any + Send>>>,
}

thread_local! {
    /// The fiber running now, while it runs a job: where [`suspend`] goes back from.
    static CURRENT: Cell<*const Shared> = const { Cell::new(core::ptr::null()) };
}

/// A fiber that runs jobs, made once.
pub struct Fiber {
    handle: *mut c_void,
    shared: Box<Shared>,
}

impl Fiber {
    /// A fiber with a stack of `reserve` bytes (a few pages committed), on the calling
    /// thread, which becomes a fiber if it is not one.
    ///
    /// # Errors
    ///
    /// Windows could not make it.
    pub fn new(reserve: usize) -> std::io::Result<Self> {
        let shared = Box::new(Shared {
            back: Cell::new(core::ptr::null_mut()),
            job: Cell::new(None),
            done: Cell::new(true),
            panic: Cell::new(None),
        });
        // SAFETY: plain calls; the parameter is the boxed state, which outlives the fiber.
        let handle = unsafe {
            if IsThreadAFiber() == 0 && ConvertThreadToFiber(core::ptr::null()).is_null() {
                return Err(std::io::Error::last_os_error());
            }
            CreateFiberEx(
                64 << 10,
                reserve,
                0,
                Some(entry),
                core::ptr::from_ref::<Shared>(&shared).cast(),
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle, shared })
    }

    /// Runs `job(context)` on the fiber until it returns or suspends.
    ///
    /// # Safety
    ///
    /// `context` stays valid, and what `job` reaches through it is not used meanwhile in a
    /// way that conflicts, until the job is done (this or a later [`Self::resume`] gives
    /// [`Ran::Done`]). No other job of this fiber is running.
    pub unsafe fn run(&self, job: unsafe fn(*mut c_void), context: *mut c_void) -> Ran {
        debug_assert!(self.shared.done.get() || self.shared.job.get().is_none());
        self.shared.job.set(Some((job, context)));
        self.shared.done.set(false);
        self.enter()
    }

    /// Goes back into the job where it suspended.
    pub fn resume(&self) -> Ran {
        if self.shared.done.get() {
            return Ran::Done;
        }
        self.enter()
    }

    /// Whether a job suspended and waits for [`Self::resume`].
    pub fn suspended(&self) -> bool {
        !self.shared.done.get() && self.shared.job.get().is_none()
    }

    /// The panic of the last job, if it panicked.
    pub fn take_panic(&self) -> Option<Box<dyn Any + Send>> {
        self.shared.panic.take()
    }

    fn enter(&self) -> Ran {
        let outer = CURRENT.replace(core::ptr::from_ref(&*self.shared));
        // SAFETY: the calling thread is a fiber (`new` made it one); this fiber belongs to
        // it and is not running.
        unsafe {
            self.shared.back.set(current_fiber());
            SwitchToFiber(self.handle);
        }
        CURRENT.set(outer);
        if self.shared.done.get() {
            Ran::Done
        } else {
            Ran::Suspended
        }
    }
}

impl Drop for Fiber {
    fn drop(&mut self) {
        // A suspended job's frames are dropped with its stack, their destructors not run.
        // SAFETY: the fiber is not running (only its own code runs on it).
        unsafe { DeleteFiber(self.handle) };
    }
}

/// From inside a fiber's job: gives the thread back to the code that ran or resumed it;
/// returns when [`Fiber::resume`] goes back into it. False (nothing done) outside a job.
pub fn suspend() -> bool {
    let shared = CURRENT.get();
    if shared.is_null() {
        return false;
    }
    // SAFETY: `CURRENT` is the running fiber's state while its job runs.
    unsafe { SwitchToFiber((*shared).back.get()) };
    true
}

/// Whether code runs in a fiber's job now.
pub fn in_job() -> bool {
    !CURRENT.get().is_null()
}

/// The fiber's code: each job in turn, a panic caught, then back to who entered it.
unsafe extern "system" fn entry(parameter: *mut c_void) {
    // SAFETY: the parameter is the fiber's boxed state, alive as long as the fiber.
    let shared = unsafe { &*parameter.cast::<Shared>() };
    loop {
        if let Some((job, context)) = shared.job.take() {
            // SAFETY: as `Fiber::run`'s caller said.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                job(context);
            }));
            if let Err(payload) = ran {
                shared.panic.set(Some(payload));
            }
        }
        shared.done.set(true);
        // SAFETY: back to the fiber that entered this one.
        unsafe { SwitchToFiber(shared.back.get()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Count {
        steps: u32,
        stop_at: u32,
    }

    /// Counts to 4, giving the thread back at `stop_at`.
    unsafe fn count(context: *mut c_void) {
        // SAFETY: the test's `Count`, alive until the job is done.
        let count = unsafe { &mut *context.cast::<Count>() };
        while count.steps < 4 {
            count.steps += 1;
            if count.steps == count.stop_at {
                assert!(in_job());
                assert!(suspend());
            }
        }
    }

    unsafe fn panics(_: *mut c_void) {
        panic!("a job's panic");
    }

    #[test]
    fn a_job_runs_suspends_goes_on_and_a_panic_comes_back() {
        assert!(!suspend(), "outside a job: nothing");
        let fiber = Fiber::new(1 << 20).unwrap();
        assert!(!fiber.suspended());
        assert_eq!(fiber.resume(), Ran::Done, "no job yet");
        let mut c = Count {
            steps: 0,
            stop_at: 0,
        };
        // SAFETY: `c` outlives the job, untouched while it runs.
        let ran = unsafe { fiber.run(count, core::ptr::from_mut(&mut c).cast()) };
        assert_eq!((ran, c.steps), (Ran::Done, 4), "to its end");
        // Stopped halfway: the counter waits at 2, then goes on.
        let mut c = Count {
            steps: 0,
            stop_at: 2,
        };
        // SAFETY: as above; read only while the job is suspended.
        let ran = unsafe { fiber.run(count, core::ptr::from_mut(&mut c).cast()) };
        assert_eq!(ran, Ran::Suspended);
        assert!(fiber.suspended() && !in_job());
        assert_eq!(c.steps, 2);
        assert_eq!(fiber.resume(), Ran::Done);
        assert_eq!(c.steps, 4);
        assert!(!fiber.suspended());
        assert_eq!(fiber.resume(), Ran::Done, "nothing left");
        // A panic is caught at the fiber's entry; the fiber runs the next job.
        // SAFETY: no context.
        let ran = unsafe { fiber.run(panics, core::ptr::null_mut()) };
        assert_eq!(ran, Ran::Done);
        let payload = fiber.take_panic().unwrap();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"a job's panic"));
        let mut c = Count {
            steps: 1,
            stop_at: 0,
        };
        // SAFETY: as above.
        let ran = unsafe { fiber.run(count, core::ptr::from_mut(&mut c).cast()) };
        assert_eq!((ran, c.steps), (Ran::Done, 4));
        // A second fiber on the same thread.
        let other = Fiber::new(256 << 10).unwrap();
        let mut c = Count {
            steps: 0,
            stop_at: 3,
        };
        // SAFETY: as above.
        let ran = unsafe { other.run(count, core::ptr::from_mut(&mut c).cast()) };
        assert_eq!(ran, Ran::Suspended);
        assert_eq!(other.resume(), Ran::Done);
    }
}
