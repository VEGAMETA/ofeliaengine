//! Waiting with sub-millisecond precision (CLAUDE.md §5: waits use high-resolution
//! waitable timers, not `Sleep`).
//!
//! - [`HighResTimer`] wraps a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer (Windows 10
//!   1803+). Create it once per thread and reuse it; `std::thread::sleep` creates and
//!   closes a timer on every call.
//! - [`PreciseSleeper`] sleeps on the timer until shortly before the deadline and spins
//!   for the rest. The margin is learned from the timer's recent overshoots on this
//!   machine and kept across calls, so a frame limiter spins only as long as the OS
//!   actually needs (see `benches/timers.rs` and ADR 0004).

use core::time::Duration;
use std::io;
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimerEx,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};

/// A high-resolution waitable timer owned by one thread at a time.
#[derive(Debug)]
pub struct HighResTimer {
    handle: HANDLE,
}

// SAFETY: a waitable timer handle may be used from any thread; `HighResTimer` is not
// `Sync`, so it is never armed from two threads at once.
unsafe impl Send for HighResTimer {}

impl HighResTimer {
    /// Creates the timer.
    pub fn new() -> io::Result<Self> {
        // SAFETY: null attributes and name are allowed; the flags and access mask are
        // valid constants.
        let handle = unsafe {
            CreateWaitableTimerExW(
                core::ptr::null(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Blocks the calling thread for at least `duration`.
    pub fn sleep(&self, duration: Duration) -> io::Result<()> {
        // Relative due times are negative, in 100 ns units.
        let ticks = i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX);
        let due = -ticks.max(1);
        // SAFETY: `self.handle` is a live timer handle; `due` outlives the call; no
        // completion routine, no wake context.
        let armed = unsafe {
            SetWaitableTimerEx(
                self.handle,
                &due,
                0,
                None,
                core::ptr::null(),
                core::ptr::null(),
                0,
            )
        };
        if armed == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `self.handle` is a live, armed timer handle.
        let waited = unsafe { WaitForSingleObject(self.handle, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blocks until `deadline` (at least). Returns immediately if it has passed.
    pub fn sleep_until(&self, deadline: Instant) -> io::Result<()> {
        let now = Instant::now();
        if deadline > now {
            self.sleep(deadline - now)?;
        }
        Ok(())
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Number of recent overshoots the margin is computed from.
const WINDOW: usize = 64;
/// Margin before any overshoot has been observed: conservative, spins up to 1 ms.
const INITIAL_MARGIN: Duration = Duration::from_millis(1);
/// Added on top of the worst recent overshoot.
const SAFETY: Duration = Duration::from_micros(20);

/// Sleeps until a deadline with microsecond accuracy at a small CPU cost.
///
/// # Examples
///
/// ```
/// use engine_platform::time::PreciseSleeper;
/// use std::time::{Duration, Instant};
///
/// let mut sleeper = PreciseSleeper::new().unwrap();
/// let frame = Duration::from_micros(4_167); // 240 Hz
/// let mut deadline = Instant::now();
/// for _ in 0..3 {
///     deadline += frame;
///     sleeper.sleep_until(deadline).unwrap();
///     assert!(Instant::now() >= deadline);
/// }
/// ```
#[derive(Debug)]
pub struct PreciseSleeper {
    timer: HighResTimer,
    /// Overshoots of the timer wake-up past its target, in nanoseconds (ring buffer).
    overshoots: [u32; WINDOW],
    next: usize,
    filled: usize,
    margin: Duration,
}

impl PreciseSleeper {
    /// Creates the sleeper and its timer.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            timer: HighResTimer::new()?,
            overshoots: [0; WINDOW],
            next: 0,
            filled: 0,
            margin: INITIAL_MARGIN,
        })
    }

    /// Current margin: how long before the deadline the timer is set to fire. The rest is
    /// spun.
    pub fn margin(&self) -> Duration {
        self.margin
    }

    /// Returns at `deadline` or very shortly after it.
    pub fn sleep_until(&mut self, deadline: Instant) -> io::Result<()> {
        let now = Instant::now();
        if deadline > now + self.margin {
            let target = deadline - self.margin;
            self.timer.sleep_until(target)?;
            self.record(Instant::now().saturating_duration_since(target));
        }
        while Instant::now() < deadline {
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Same as [`sleep_until`](Self::sleep_until) with a relative duration.
    pub fn sleep(&mut self, duration: Duration) -> io::Result<()> {
        self.sleep_until(Instant::now() + duration)
    }

    fn record(&mut self, overshoot: Duration) {
        let nanos = u32::try_from(overshoot.as_nanos()).unwrap_or(u32::MAX);
        self.overshoots[self.next] = nanos;
        self.next = (self.next + 1) % WINDOW;
        self.filled = (self.filled + 1).min(WINDOW);
        let worst = self.overshoots[..self.filled]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        self.margin = Duration::from_nanos(u64::from(worst)) + SAFETY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_sleeps_at_least_the_requested_time() {
        let timer = HighResTimer::new().unwrap();
        for micros in [0u64, 50, 500, 2_000] {
            let d = Duration::from_micros(micros);
            let start = Instant::now();
            timer.sleep(d).unwrap();
            let elapsed = start.elapsed();
            assert!(elapsed >= d, "requested {d:?}, slept {elapsed:?}");
            // Sanity bound only (unit mix-ups); accuracy is a benchmark, not a unit test.
            assert!(
                elapsed < d + Duration::from_millis(500),
                "requested {d:?}, slept {elapsed:?}"
            );
        }
    }

    #[test]
    fn sleep_until_past_deadline_returns_immediately() {
        let timer = HighResTimer::new().unwrap();
        let start = Instant::now();
        timer.sleep_until(start).unwrap();
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn precise_sleeper_hits_deadlines_and_learns_margin() {
        let mut sleeper = PreciseSleeper::new().unwrap();
        assert_eq!(sleeper.margin(), INITIAL_MARGIN);
        let mut deadline = Instant::now();
        for _ in 0..20 {
            deadline += Duration::from_millis(3);
            sleeper.sleep_until(deadline).unwrap();
            let late = Instant::now().saturating_duration_since(deadline);
            assert!(Instant::now() >= deadline);
            // Only a sanity bound: shared CI machines preempt threads for tens of ms.
            // Accuracy is measured by `benches/timers.rs` on the reference machine.
            assert!(late < Duration::from_millis(500), "late by {late:?}");
        }
        // The margin now reflects measured overshoots rather than the initial guess.
        assert_ne!(sleeper.margin(), INITIAL_MARGIN);
    }

    #[test]
    fn timer_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<HighResTimer>();
        assert_send::<PreciseSleeper>();
    }
}
