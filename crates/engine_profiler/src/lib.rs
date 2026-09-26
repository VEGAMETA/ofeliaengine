//! Profiling instrumentation for every engine subsystem (CLAUDE.md §5, §14).
//!
//! Zones, frame marks, plots and messages go to [Tracy](https://github.com/wolfpld/tracy)
//! when the `tracy` feature is on (build with `cargo profiling`); otherwise every macro
//! and function here compiles to nothing. Instrument freely: a zone costs a few
//! nanoseconds with Tracy and zero without it (`benches/zones.rs`, ADR 0003).
//!
//! The Tracy client starts before `main` and records from process start, so start-up
//! (editor cold start, §5 budgets) is visible. Connect with the Tracy **0.14.1** profiler.
//!
//! ```
//! fn update_physics() {
//!     engine_profiler::zone!("physics::step");
//!     // ... work ...
//!     engine_profiler::plot!("physics::bodies", 1024.0);
//! }
//!
//! for _frame in 0..3 {
//!     update_physics();
//!     engine_profiler::frame_mark();
//! }
//! ```

/// Whether profiling support is compiled in.
pub const ENABLED: bool = cfg!(feature = "tracy-base");

#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "tracy-base")]
    pub use tracy_client;
}

/// Opens a profiling zone that lasts until the end of the enclosing block.
///
/// `zone!()` names the zone after the enclosing function; `zone!("name")` uses a string
/// literal. Zones nest.
///
/// ```
/// fn cull() {
///     engine_profiler::zone!();
///     for _ in 0..4 {
///         engine_profiler::zone!("cull::batch");
///     }
/// }
/// cull();
/// ```
#[cfg(feature = "tracy-base")]
#[macro_export]
macro_rules! zone {
    () => {
        let _engine_profiler_zone = $crate::__private::tracy_client::Client::running()
            .map(|c| c.span($crate::__private::tracy_client::span_location!(), 0));
    };
    ($name:literal) => {
        let _engine_profiler_zone = $crate::__private::tracy_client::Client::running()
            .map(|c| c.span($crate::__private::tracy_client::span_location!($name), 0));
    };
}

/// Opens a profiling zone that lasts until the end of the enclosing block.
///
/// Profiling is compiled out (`tracy` feature off): expands to nothing.
#[cfg(not(feature = "tracy-base"))]
#[macro_export]
macro_rules! zone {
    () => {};
    ($name:literal) => {};
}

/// Records a value on a named plot (a graph over time in the profiler).
///
/// ```
/// engine_profiler::plot!("streaming::pending_requests", 12.0);
/// ```
#[cfg(feature = "tracy-base")]
#[macro_export]
macro_rules! plot {
    ($name:literal, $value:expr) => {
        if let Some(client) = $crate::__private::tracy_client::Client::running() {
            client.plot(
                $crate::__private::tracy_client::plot_name!($name),
                ($value) as f64,
            );
        }
    };
}

/// Records a value on a named plot.
///
/// Profiling is compiled out: the value expression is still evaluated (so it cannot
/// cause unused-variable warnings), then discarded.
#[cfg(not(feature = "tracy-base"))]
#[macro_export]
macro_rules! plot {
    ($name:literal, $value:expr) => {
        let _ = $value;
    };
}

/// Marks a secondary frame set, for things that tick at their own rate (physics steps,
/// audio blocks). The main frame is marked with [`frame_mark`].
#[cfg(feature = "tracy-base")]
#[macro_export]
macro_rules! secondary_frame_mark {
    ($name:literal) => {
        if let Some(client) = $crate::__private::tracy_client::Client::running() {
            client.secondary_frame_mark($crate::__private::tracy_client::frame_name!($name));
        }
    };
}

/// Marks a secondary frame set. Profiling is compiled out: expands to nothing.
#[cfg(not(feature = "tracy-base"))]
#[macro_export]
macro_rules! secondary_frame_mark {
    ($name:literal) => {};
}

/// Marks the end of the main frame. Call once per frame, after present.
#[inline]
pub fn frame_mark() {
    #[cfg(feature = "tracy-base")]
    if let Some(client) = tracy_client::Client::running() {
        client.frame_mark();
    }
}

/// Sends a text message to the profiler timeline. Cold path: meant for events such as
/// "level loaded", not for per-frame logging.
#[inline]
pub fn message(text: &str) {
    #[cfg(feature = "tracy-base")]
    if let Some(client) = tracy_client::Client::running() {
        client.message(text, 0);
    }
    #[cfg(not(feature = "tracy-base"))]
    let _ = text;
}

/// Names the calling thread in the profiler. Call once when a thread starts. Interior NUL
/// characters are removed.
pub fn set_thread_name(name: &str) {
    #[cfg(feature = "tracy-base")]
    if let Some(client) = tracy_client::Client::running() {
        if name.contains('\0') {
            client.set_thread_name(&name.replace('\0', ""));
        } else {
            client.set_thread_name(name);
        }
    }
    #[cfg(not(feature = "tracy-base"))]
    let _ = name;
}

/// Whether a profiler is connected right now. Always `false` without the `tracy`
/// feature.
pub fn is_connected() -> bool {
    #[cfg(feature = "tracy-base")]
    return tracy_client::Client::is_connected();
    #[cfg(not(feature = "tracy-base"))]
    return false;
}

/// A GPU timeline in the profiler (one per queue). GPU zones are declared while
/// recording (with the timestamp query ids that bracket them) and get their timestamps
/// later, when the GPU has finished. Does nothing without the `tracy` feature.
pub struct GpuContext {
    #[cfg(feature = "tracy-base")]
    inner: tracy_client::GpuContext,
}

impl core::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuContext").finish_non_exhaustive()
    }
}

impl GpuContext {
    /// A Vulkan GPU context. `gpu_timestamp` is a timestamp taken at (about) the current
    /// CPU time and `period_ns` the timestamp period. `None` when profiling is compiled
    /// out or the client is not running.
    pub fn new_vulkan(name: &str, gpu_timestamp: i64, period_ns: f32) -> Option<Self> {
        #[cfg(feature = "tracy-base")]
        {
            let client = tracy_client::Client::running()?;
            let inner = client
                .new_gpu_context(
                    Some(name),
                    tracy_client::GpuContextType::Vulkan,
                    gpu_timestamp,
                    period_ns,
                )
                .ok()?;
            Some(Self { inner })
        }
        #[cfg(not(feature = "tracy-base"))]
        {
            let _ = (name, gpu_timestamp, period_ns);
            None
        }
    }

    /// Opens a GPU zone at record time; `query` identifies its start timestamp.
    pub fn begin_zone(&self, name: &str, query: u16) {
        #[cfg(feature = "tracy-base")]
        self.inner.begin_span_alloc(name, "gpu", "", 0, query);
        #[cfg(not(feature = "tracy-base"))]
        let _ = (name, query);
    }

    /// Closes the innermost GPU zone at record time; `query` identifies its end timestamp.
    pub fn end_zone(&self, query: u16) {
        #[cfg(feature = "tracy-base")]
        self.inner.end_span(query);
        #[cfg(not(feature = "tracy-base"))]
        let _ = query;
    }

    /// Supplies the GPU timestamp of `query` once it is available.
    pub fn upload(&self, query: u16, gpu_timestamp: i64) {
        #[cfg(feature = "tracy-base")]
        self.inner.upload_gpu_timestamp(query, gpu_timestamp);
        #[cfg(not(feature = "tracy-base"))]
        let _ = (query, gpu_timestamp);
    }

    /// Re-synchronizes the GPU and CPU clocks (some GPUs reset their timer in low-power
    /// states). `gpu_timestamp` must be taken right before the call.
    pub fn sync(&self, gpu_timestamp: i64) {
        #[cfg(feature = "tracy-base")]
        self.inner.sync_gpu_time(gpu_timestamp);
        #[cfg(not(feature = "tracy-base"))]
        let _ = gpu_timestamp;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn instrumentation_compiles_and_runs() {
        fn work(n: u64) -> u64 {
            crate::zone!();
            let mut acc = 0;
            for i in 0..n {
                crate::zone!("tests::inner");
                acc += i;
            }
            crate::plot!("tests::acc", acc);
            acc
        }
        crate::set_thread_name("profiler-test");
        crate::message("test start");
        for _ in 0..3 {
            assert_eq!(work(10), 45);
            crate::secondary_frame_mark!("tests::tick");
            crate::frame_mark();
        }
        assert!(!crate::is_connected() || crate::ENABLED);
    }
}
