//! The public thread pool.

use super::job::{JobHeader, JobRef, Panic, StackJob, execute};
use super::latch::{CountLatch, Latch, ThreadLatch};
use super::registry::{MAX_PARTICIPANTS, Registry, RegistryConfig};
use super::scope::Scope;
use crate::sync::CachePadded;
use core::fmt;
use core::ops::Range;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

type StartHook = Arc<dyn Fn(usize) + Send + Sync + 'static>;

/// Configures and builds a [`ThreadPool`].
#[must_use]
pub struct ThreadPoolBuilder {
    workers: Option<usize>,
    external: usize,
    deque_capacity: usize,
    injector_capacity: usize,
    arena_bytes: usize,
    spin_rounds: u32,
    stack_size: Option<usize>,
    name: &'static str,
    on_thread_start: Option<StartHook>,
}

impl Default for ThreadPoolBuilder {
    fn default() -> Self {
        Self {
            workers: None,
            external: 2,
            deque_capacity: 1024,
            injector_capacity: 1024,
            arena_bytes: 64 * 1024,
            spin_rounds: 64,
            stack_size: None,
            name: "engine-worker",
            on_thread_start: None,
        }
    }
}

impl ThreadPoolBuilder {
    /// Number of worker threads. Default: available parallelism minus one (the main
    /// thread is expected to register and participate), at least one.
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = Some(workers);
        self
    }

    /// Number of slots for non-worker threads (main thread, render thread, ...) that join
    /// the pool with [`ThreadPool::register_current_thread`]. Default: 2.
    pub fn external_participants(mut self, count: usize) -> Self {
        self.external = count;
        self
    }

    /// Capacity of each participant's deque. A push into a full deque runs the job
    /// inline. Default: 1024.
    pub fn deque_capacity(mut self, capacity: usize) -> Self {
        self.deque_capacity = capacity;
        self
    }

    /// Capacity of the queue for jobs from non-participant threads. Default: 1024.
    pub fn injector_capacity(mut self, capacity: usize) -> Self {
        self.injector_capacity = capacity;
        self
    }

    /// Bytes of per-participant arena for [`Scope::spawn`] job records. When it is full,
    /// spawned jobs run inline. Default: 64 KiB.
    pub fn spawn_arena_bytes(mut self, bytes: usize) -> Self {
        self.arena_bytes = bytes;
        self
    }

    /// Idle rounds (32 spin-loop hints plus a scan of all queues each) before an idle
    /// participant parks. Higher values trade CPU time for wake-up latency. Default: 64.
    pub fn spin_rounds(mut self, rounds: u32) -> Self {
        self.spin_rounds = rounds;
        self
    }

    /// Stack size of worker threads. Default: the platform default.
    pub fn stack_size(mut self, bytes: usize) -> Self {
        self.stack_size = Some(bytes);
        self
    }

    /// Worker thread name prefix. Threads are named `<prefix>-<index>`.
    pub fn thread_name(mut self, prefix: &'static str) -> Self {
        self.name = prefix;
        self
    }

    /// Called on each worker thread before it takes jobs, with the worker's index.
    /// Used to set CPU sets, priorities and profiler thread names.
    pub fn on_thread_start(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.on_thread_start = Some(Arc::new(hook));
        self
    }

    /// Spawns the workers and returns once every worker has started and run its
    /// [`on_thread_start`](Self::on_thread_start) hook.
    pub fn build(self) -> Result<ThreadPool, BuildError> {
        let workers = self
            .workers
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, |n| n.get() - 1))
            .max(1);
        let total = workers + self.external;
        if total > MAX_PARTICIPANTS {
            return Err(BuildError::TooManyParticipants {
                requested: total,
                max: MAX_PARTICIPANTS,
            });
        }
        let registry = Arc::new(Registry::new(&RegistryConfig {
            workers,
            external: self.external,
            deque_capacity: self.deque_capacity,
            injector_capacity: self.injector_capacity,
            arena_bytes: self.arena_bytes,
            spin_rounds: self.spin_rounds,
        }));
        let mut pool = ThreadPool {
            registry: registry.clone(),
            workers: Vec::with_capacity(workers),
        };
        // Workers report when they are bound and their start hook has run; `build`
        // returns only then, so thread placement is done before the first frame and
        // one-time thread start-up costs never land in a frame.
        let started = Arc::new((AtomicUsize::new(0), thread::current()));
        for index in 0..workers {
            let registry = registry.clone();
            let hook = self.on_thread_start.clone();
            let started = started.clone();
            let mut builder = thread::Builder::new().name(format!("{}-{index}", self.name));
            if let Some(size) = self.stack_size {
                builder = builder.stack_size(size);
            }
            let handle = builder
                .spawn(move || {
                    registry.bind_worker(index);
                    if let Some(hook) = hook {
                        hook(index);
                    }
                    if started.0.fetch_add(1, Ordering::AcqRel) + 1 == workers {
                        started.1.unpark();
                    }
                    drop(started);
                    registry.wait_until(index, || registry.terminate.load(Ordering::Acquire));
                })
                .map_err(BuildError::Spawn)?;
            // On error, dropping `pool` shuts down the workers spawned so far.
            pool.workers.push(handle);
        }
        while started.0.load(Ordering::Acquire) < workers {
            thread::park();
        }
        Ok(pool)
    }
}

/// Error returned by [`ThreadPoolBuilder::build`].
#[derive(Debug)]
pub enum BuildError {
    /// Workers plus external participants exceed the supported maximum.
    TooManyParticipants {
        /// Requested participants.
        requested: usize,
        /// Supported maximum.
        max: usize,
    },
    /// The OS refused to create a worker thread.
    Spawn(std::io::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyParticipants { requested, max } => {
                write!(
                    f,
                    "{requested} participants requested, at most {max} supported"
                )
            }
            Self::Spawn(e) => write!(f, "failed to spawn a worker thread: {e}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) => Some(e),
            Self::TooManyParticipants { .. } => None,
        }
    }
}

/// Error returned by [`ThreadPool::register_current_thread`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterError;

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no free external participant slot in the thread pool")
    }
}

impl std::error::Error for RegisterError {}

/// Work-stealing thread pool for fork-join parallelism.
///
/// All operations are blocking and structured: they return only after every job they
/// started has finished, so jobs may borrow from the caller's stack. A thread that waits
/// always runs other jobs meanwhile. In steady state no operation allocates.
///
/// Threads that are neither workers nor registered participants may use the pool too;
/// their call is handed to a worker and they block until it finishes (see
/// [`install`](Self::install)). Register long-lived threads that submit work every frame
/// (main, render) with [`register_current_thread`](Self::register_current_thread) so they
/// participate instead of blocking.
///
/// See `docs/adr/0002-job-system.md` for the design.
pub struct ThreadPool {
    registry: Arc<Registry>,
    workers: Vec<JoinHandle<()>>,
}

impl ThreadPool {
    /// Returns a builder with default settings.
    pub fn builder() -> ThreadPoolBuilder {
        ThreadPoolBuilder::default()
    }

    /// Number of worker threads.
    pub fn num_workers(&self) -> usize {
        self.registry.num_workers
    }

    /// Makes the calling thread a participant of this pool for the pool's lifetime: its
    /// calls run jobs directly, and while it waits it executes other jobs. A thread can be
    /// a participant of one pool at a time; registering with another pool detaches it
    /// from the previous one.
    pub fn register_current_thread(&self) -> Result<(), RegisterError> {
        self.registry
            .bind_external()
            .map(|_| ())
            .ok_or(RegisterError)
    }

    /// Whether the calling thread is a worker or a registered participant of this pool.
    pub fn is_participant(&self) -> bool {
        self.registry.current_index().is_some()
    }

    /// Runs `f` on a participant of the pool and returns its result. On a participant
    /// thread it simply calls `f`. From any other thread, `f` is queued for a worker and
    /// the caller blocks (without spinning) until it completes. Panics in `f` are
    /// propagated to the caller.
    pub fn install<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        if self.registry.current_index().is_some() {
            return f();
        }
        let job = StackJob::new(ThreadLatch::for_current_thread(), f);
        let mut job_ref = job.as_job_ref();
        while let Err(back) = self.registry.injector.push(job_ref) {
            // The injector only fills up under pathological external load.
            job_ref = back;
            thread::yield_now();
        }
        self.registry.notify_injected();
        job.latch.wait();
        job.into_result().into_value()
    }

    /// Runs `a` and `b`, potentially in parallel, and returns both results.
    ///
    /// `b` is offered to other threads while the caller runs `a`; if nobody took it, the
    /// caller runs it too. If either closure panics, the panic is propagated after both
    /// have finished (or `b` was discarded unstarted).
    ///
    /// # Examples
    ///
    /// ```
    /// use engine_core::jobs::ThreadPool;
    ///
    /// fn fib(pool: &ThreadPool, n: u64) -> u64 {
    ///     if n < 20 {
    ///         return (1..=n).fold((0, 1), |(a, b), _| (b, a + b)).0;
    ///     }
    ///     let (a, b) = pool.join(|| fib(pool, n - 1), || fib(pool, n - 2));
    ///     a + b
    /// }
    ///
    /// let pool = ThreadPool::builder().workers(2).build().unwrap();
    /// assert_eq!(fib(&pool, 25), 75025);
    /// ```
    pub fn join<A, B, RA, RB>(&self, a: A, b: B) -> (RA, RB)
    where
        A: FnOnce() -> RA + Send,
        B: FnOnce() -> RB + Send,
        RA: Send,
        RB: Send,
    {
        self.with_participant(|registry, index| join_in(registry, index, a, b))
    }

    /// Creates a [`Scope`] for spawning jobs that borrow from the caller, runs `f`, and
    /// waits for all spawned jobs. Panics from `f` or from any job are propagated after
    /// all jobs have finished.
    pub fn scope<'env, F, R>(&self, f: F) -> R
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> R + Send,
        R: Send,
    {
        self.with_participant(|registry, index| scope_in(registry, index, f))
    }

    /// Calls `f(chunk_index, range)` for every chunk of `0..len`, in parallel.
    ///
    /// Chunk `i` always covers `i * grain .. min((i + 1) * grain, len)`, independent of
    /// the number of threads and of scheduling, so per-chunk results combined in chunk
    /// order are deterministic (for example floating-point reductions in physics).
    /// Threads claim chunks from a shared counter, which balances uneven chunks
    /// dynamically. Pick `grain` so that one chunk takes at least a few microseconds.
    ///
    /// # Examples
    ///
    /// ```
    /// use engine_core::jobs::ThreadPool;
    /// use std::sync::atomic::{AtomicU64, Ordering};
    ///
    /// let pool = ThreadPool::builder().workers(3).build().unwrap();
    /// let values: Vec<u64> = (0..10_000).collect();
    /// // One slot per chunk: combining them in order is deterministic.
    /// let partial: Vec<AtomicU64> = (0..values.len().div_ceil(1000)).map(|_| AtomicU64::new(0)).collect();
    /// pool.parallel_for(values.len(), 1000, |chunk, range| {
    ///     partial[chunk].store(values[range].iter().sum(), Ordering::Relaxed);
    /// });
    /// let total: u64 = partial.iter().map(|p| p.load(Ordering::Relaxed)).sum();
    /// assert_eq!(total, 49_995_000);
    /// ```
    pub fn parallel_for<F>(&self, len: usize, grain: usize, f: F)
    where
        F: Fn(usize, Range<usize>) + Sync,
    {
        self.with_participant(|registry, index| parallel_for_in(registry, index, len, grain, &f));
    }

    /// Runs `op` with the calling thread's participant index, moving it to a worker first
    /// if the calling thread does not participate.
    #[inline]
    fn with_participant<R, Op>(&self, op: Op) -> R
    where
        Op: FnOnce(&Registry, usize) -> R + Send,
        R: Send,
    {
        match self.registry.current_index() {
            Some(index) => op(&self.registry, index),
            None => self.install(|| match self.registry.current_index() {
                Some(index) => op(&self.registry, index),
                None => unreachable!("install runs on a participant"),
            }),
        }
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.registry.terminate.store(true, Ordering::SeqCst);
        self.registry.wake_all();
        for handle in self.workers.drain(..) {
            // Job panics are caught inside jobs; a worker only fails on an internal bug,
            // and there is nothing more to do about it during shutdown.
            let _ = handle.join();
        }
    }
}

impl fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadPool")
            .field("workers", &self.registry.num_workers)
            .field("participants", &self.registry.slots.len())
            .finish()
    }
}

fn join_in<A, B, RA, RB>(registry: &Registry, index: usize, a: A, b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    let deque = &registry.slots[index].deque;
    let job_b = StackJob::new(CountLatch::new(1, registry, index), b);
    let job_b_ref = job_b.as_job_ref();
    // SAFETY: `index` is the calling thread's slot.
    if unsafe { deque.push(job_b_ref) }.is_err() {
        // Deque full: no parallelism at this level.
        let ra = a();
        return (ra, job_b.run_inline());
    }
    registry.notify_work();

    let result_a = panic::catch_unwind(AssertUnwindSafe(a));

    while !job_b.latch.probe() {
        // SAFETY: own slot.
        match unsafe { deque.pop() } {
            Some(job) if job == job_b_ref => {
                // Nobody stole `b`: run it here (or drop it if `a` panicked).
                return match result_a {
                    Ok(ra) => (ra, job_b.run_inline()),
                    Err(payload) => {
                        drop(job_b);
                        panic::resume_unwind(payload)
                    }
                };
            }
            // Jobs above `b` belong to `a`, which has finished, so this cannot normally
            // happen; running the job is still correct.
            // SAFETY: popped from our own deque, so this thread owns the reference.
            Some(job) => unsafe { execute(job) },
            None => {
                registry.wait_until(index, || job_b.latch.probe());
                break;
            }
        }
    }
    let result_b = job_b.into_result();
    match result_a {
        Ok(ra) => (ra, result_b.into_value()),
        Err(payload) => panic::resume_unwind(payload),
    }
}

fn scope_in<'env, F, R>(registry: &Registry, index: usize, f: F) -> R
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> R,
{
    let arena = &registry.slots[index].owner.arena;
    let mark = arena.mark();
    let scope = Scope::new(registry, index);
    let result = panic::catch_unwind(AssertUnwindSafe(|| f(&scope)));
    // Always wait, even if `f` panicked: spawned jobs borrow from the caller.
    registry.wait_until(index, || scope.core.latch.probe());
    arena.reset(mark);
    let job_panic = scope.core.take_panic();
    match result {
        Err(payload) => panic::resume_unwind(payload),
        Ok(r) => match job_panic {
            Some(payload) => panic::resume_unwind(payload),
            None => r,
        },
    }
}

/// Shared record of one `parallel_for` call. Lives on the caller's stack; `refs` counts
/// the helper references published to the deques.
#[repr(C)]
struct ForJob<'f, F> {
    header: JobHeader,
    next_chunk: CachePadded<AtomicUsize>,
    chunks: usize,
    len: usize,
    grain: usize,
    f: &'f F,
    refs: CountLatch,
    panic: AtomicPtr<Panic>,
}

impl<F: Fn(usize, Range<usize>) + Sync> ForJob<'_, F> {
    fn run_chunks(&self) {
        loop {
            let chunk = self.next_chunk.fetch_add(1, Ordering::Relaxed);
            if chunk >= self.chunks {
                return;
            }
            let start = chunk * self.grain;
            let end = (start + self.grain).min(self.len);
            if let Err(payload) =
                panic::catch_unwind(AssertUnwindSafe(|| (self.f)(chunk, start..end)))
            {
                self.record_panic(payload);
                // Stop handing out chunks.
                self.next_chunk.store(self.chunks, Ordering::Relaxed);
                return;
            }
        }
    }

    fn record_panic(&self, payload: Panic) {
        let boxed = Box::into_raw(Box::new(payload));
        if self
            .panic
            .compare_exchange(ptr::null_mut(), boxed, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: not published; still owned here.
            drop(unsafe { Box::from_raw(boxed) });
        }
    }

    fn take_panic(&self) -> Option<Panic> {
        let p = self.panic.swap(ptr::null_mut(), Ordering::Acquire);
        // SAFETY: non-null values come from `Box::into_raw` in `record_panic`.
        (!p.is_null()).then(|| *unsafe { Box::from_raw(p) })
    }

    unsafe fn execute_raw(job: JobRef) {
        let this = job.cast::<Self>().as_ptr();
        // SAFETY: the caller of `parallel_for` keeps the record alive until every helper
        // reference has been executed and has set `refs`.
        unsafe {
            (*this).run_chunks();
            CountLatch::set(&raw const (*this).refs);
        }
    }
}

fn parallel_for_in<F>(registry: &Registry, index: usize, len: usize, grain: usize, f: &F)
where
    F: Fn(usize, Range<usize>) + Sync,
{
    let grain = grain.max(1);
    let chunks = len.div_ceil(grain);
    if chunks <= 1 {
        if len > 0 {
            f(0, 0..len);
        }
        return;
    }
    let job = ForJob {
        header: JobHeader::new(ForJob::<F>::execute_raw),
        next_chunk: CachePadded::new(AtomicUsize::new(0)),
        chunks,
        len,
        grain,
        f,
        refs: CountLatch::new(0, registry, index),
        panic: AtomicPtr::new(ptr::null_mut()),
    };
    let job_ref = NonNull::from(&job.header);
    let deque = &registry.slots[index].deque;

    // One helper reference per other thread that could usefully join in.
    let helpers = (chunks - 1).min(registry.num_participants_hint() - 1);
    let mut published = 0;
    for _ in 0..helpers {
        job.refs.increment();
        // SAFETY: own slot.
        if unsafe { deque.push(job_ref) }.is_err() {
            job.refs.decrement_unpublished();
            break;
        }
        published += 1;
    }
    if published > 0 {
        registry.notify_work_many(published);
    }

    job.run_chunks();

    // Retire helper references nobody took (they are on top of our deque) and wait for
    // the ones that were taken.
    while !job.refs.probe() {
        // SAFETY: own slot.
        match unsafe { deque.pop() } {
            // SAFETY: popped references are owned by this thread. Executing our own
            // helper reference finds no chunks left and just sets `refs`.
            Some(popped) => unsafe { execute(popped) },
            None => {
                registry.wait_until(index, || job.refs.probe());
                break;
            }
        }
    }
    if let Some(payload) = job.take_panic() {
        panic::resume_unwind(payload);
    }
}
