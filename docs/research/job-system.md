# Job system: research

Date: 2026-09-25. Input for [ADR 0002](../adr/0002-job-system.md).

**No code was copied from any of the projects below.** The implementation in
`crates/engine_core/src/jobs` is written from the papers; the projects were read for
design ideas only. Ideas taken from permissively licensed projects are credited in the
code comments where they are used.

## What the engine needs

From CLAUDE.md §5 and the planned subsystems:

| Need | Why |
|---|---|
| Zero heap allocations per frame | §5 hard rule, enforced by a test |
| No locks on hot paths, no `Mutex` on render/audio threads | §5 |
| Fork-join (`join`, `scope`) over borrowed data | ECS systems, culling, animation, physics islands |
| Data-parallel loops with **deterministic** partitioning | physics determinism (§8), reproducible benchmarks |
| Main and render threads participate instead of blocking | the main thread has spare time between frames |
| Thread placement hook (P-cores, CPU sets, MMCSS) | §5 hybrid CPUs; implemented later in `engine_platform` |
| Low wake-up latency after idle | a frame starts with idle workers every 4–16 ms |
| Long background work must not delay frame work | asset import, shader compilation |

## Sources

| Project / paper | Version checked | License | What it is |
|---|---|---|---|
| [rayon](https://github.com/rayon-rs/rayon) (`rayon-core`) | rayon-core 1.13.0, commit `ee0a00b` (2026-08-28) | MIT OR Apache-2.0 | Rust work-stealing fork-join pool |
| [crossbeam-deque](https://github.com/crossbeam-rs/crossbeam) | 0.8.8, commit `38dacb4` (2026-09-08) | MIT OR Apache-2.0 | Chase–Lev deque used by rayon |
| [bevy_tasks](https://github.com/bevyengine/bevy/tree/main/crates/bevy_tasks) | 0.20.0-dev, commit `0f38358` (2026-09-25) | MIT OR Apache-2.0 | Bevy's task pools |
| [enkiTS](https://github.com/dougbinks/enkiTS) | commit `4cba61a` (2026-09-02) | zlib | C/C++ game task scheduler |
| [chili](https://github.com/dragostis/chili) | 0.2.1, commit `6c49338` (2025-05-01) | MIT OR Apache-2.0 | Rust port of Spice (heartbeat scheduling) |
| Gyrling, [Parallelizing the Naughty Dog Engine Using Fibers](https://media.gdcvault.com/gdc2015/presentations/Gyrling_Christian_Parallelizing_The_Naughty.pdf), GDC 2015 | talk | — | fiber job system of The Last of Us Remastered |
| Chase, Lev, *Dynamic Circular Work-Stealing Deque*, SPAA 2005, [doi:10.1145/1073970.1073974](https://doi.org/10.1145/1073970.1073974) | paper | — | the deque |
| Lê, Pop, Cohen, Zappa Nardelli, *Correct and Efficient Work-Stealing for Weak Memory Models*, PPoPP 2013, [doi:10.1145/2442516.2442524](https://doi.org/10.1145/2442516.2442524) | paper | — | C11 orderings for the deque, proven |
| Blumofe, Leiserson, *Scheduling Multithreaded Computations by Work Stealing*, JACM 1999, [doi:10.1145/324133.324234](https://doi.org/10.1145/324133.324234) | paper | — | work-stealing bounds |
| Frigo, Leiserson, Randall, *The Implementation of the Cilk-5 Multithreaded Language*, PLDI 1998, [doi:10.1145/277650.277725](https://doi.org/10.1145/277650.277725) | paper | — | work-first principle |
| Acar, Charguéraud, Guatto, Rainey, Sieczkowski, *Heartbeat Scheduling: Provable Efficiency for Nested Parallelism*, PLDI 2018, [doi:10.1145/3192366.3192391](https://doi.org/10.1145/3192366.3192391) | paper | — | amortized fork cost |
| Vyukov, *Scalable Go Scheduler Design Doc*, 2012 ([golang.org/s/go11sched](https://golang.org/s/go11sched)) | design doc | — | spinning-thread throttling |
| Vyukov, *Bounded MPMC queue* (1024cores.net) | article | — | injector queue |
| Lerche, *Making the Tokio scheduler 10x faster*, tokio.rs blog, 2019 | article | — | throttled stealing in practice |

## How each one works

### rayon

- Per-worker crossbeam deque (LIFO for the owner), a global `Injector` for jobs from
  outside the pool, random-victim stealing.
- `join(a, b)`: `b` is a `StackJob` on the caller's stack, pushed to the local deque; the
  caller runs `a`, then pops `b` back or, if it was stolen, waits on a latch while
  executing other jobs ("help while waiting"). **No allocation.**
- `scope.spawn` / `spawn`: every job is a `HeapJob` (`Box`) — **one allocation per
  spawn**. Measured: 256 allocations for a scope with 256 spawns (ADR 0002).
- crossbeam's deque buffer grows ×2 when full and **shrinks ÷2 when a quarter full**
  (`crossbeam-deque/src/deque.rs`, `MIN_CAP = 64`); old buffers are reclaimed with epoch
  GC. Bursty load can therefore allocate and free in steady state. The `Injector` allocates
  a block per 63 pushes.
- Sleep: per-worker `Mutex<bool>` + `Condvar`, and a packed atomic counter (sleeping,
  inactive threads, a "jobs event counter") to avoid lost wake-ups
  (`rayon-core/src/sleep/README.md`, rayon RFC #5). Pushes to the local deque skip the
  fence: a missed wake-up only loses parallelism because the owner will run the job
  itself (`new_internal_jobs`). Injected jobs pay a `SeqCst` fence.
- Thread placement: `ThreadPoolBuilder::start_handler` runs on each worker at start.
- Non-worker threads cannot join a pool; they inject and block (`install`).
- Partitioning of parallel iterators is adaptive: splits happen on steals, so chunk
  boundaries depend on timing and thread count.

### bevy_tasks

- Three `TaskPool`s (compute, async compute, IO) on top of `async-executor`; every task is
  a heap-allocated future (`async-task`), `scope` collects results into a `Vec`.
- `ThreadExecutor` lets a specific thread (the main thread) tick its own tasks.
- Built for Bevy's async-flavored ECS scheduling. Bevy itself plans to replace the
  executor (the "forte" scheduler was discussed for a `bevy_tasks` rewrite).
- Per-task allocation and an async runtime in the core both conflict with §5.

### enkiTS

- C/C++, zlib. Per-thread `LockLessMultiReadPipe` of **fixed size** (single writer,
  multiple readers). When a pipe is full, the task is **executed immediately** instead of
  queued — no growth, no allocation.
- Task sets are split into partitions (`m_MinRange`, `m_RangeToRun`) up front.
- 3 priority levels (configurable 1–5) as separate pipes; priorities are cooperative, so a
  long low-priority task still occupies a thread.
- Pinned tasks run on a chosen thread (main thread, IO thread); external threads can be
  registered as task threads.
- Dependencies between tasks (`SetDependency`) for task graphs.
- Waiting: spin with back-off, then OS semaphores (`CreateSemaphore` on Windows).
- Custom allocator hook for all internal allocations (done once at init).

### chili (Spice, heartbeat scheduling)

- `join` normally runs both closures sequentially on the same thread; only when a
  "heartbeat" (a periodic flag set by a timer thread every ~100 µs) fires does the next
  `join` publish its second half for stealing. The cost of a fork is amortized to almost
  nothing (published numbers: fine-grained binary tree sum 7.8× faster than baseline on
  8 cores where rayon gets 1.6×).
- Only `join`; no scopes, loops, external threads or placement control. A periodic timer
  thread costs wake-ups (power, jitter) even when idle is desired.

### Naughty Dog (fibers)

- 6 worker threads locked to cores, 160 fibers (128 × 64 KiB + 32 × 512 KiB stacks,
  ~24 MiB), 3 priority queues, dependencies as atomic counters. Waiting on a counter
  parks the **fiber** and the worker picks another job, so waiting never nests stacks
  and never runs unrelated work on top of the waiter.
- Needs care with TLS, locks held across switches, debugging and profiling (Tracy needs
  explicit fiber markers).

### Go scheduler / Tokio

- At most a few threads are "spinning" (searching for work). A pusher wakes a sleeper only
  when nobody is spinning; a spinner that finds work and was the last one wakes the next.
  Wake-ups cascade with the amount of work but do not stampede.

## What we take, what we don't, and why

| Idea | Source | Decision |
|---|---|---|
| Chase–Lev deque with C11 orderings | Chase–Lev 2005, Lê et al. 2013 | **Take**, with a **fixed-size** buffer (below) |
| Fixed-size queues, run inline on overflow | enkiTS | **Take**: removes all allocation and the deque's memory-reclamation problem |
| `AtomicPtr` slots | — (our choice) | **Take**: thief reads of reused slots are not data races |
| `join` protocol on the stack, help while waiting | rayon, Cilk | **Take** |
| No fence on local pushes, fence on injection | rayon | **Take**: in the first prototype a `SeqCst` fence per push made 256 tiny spawns take 161 µs vs 70 µs without it (single container run, indicative only) |
| Throttled wake-ups (one searcher at a time) | Go, Tokio | **Take** |
| Park via `WaitOnAddress` (`std::thread::park`) instead of mutex + condvar | — | **Take**: no locks anywhere |
| Deterministic chunking (fixed grain, atomic chunk counter) | enkiTS partitions (fixed up front) | **Take**, but chunk boundaries depend only on `len` and `grain`, never on the thread count: needed for physics determinism |
| External thread registration | enkiTS | **Take**: main/render threads participate |
| Per-thread spawn arena, rewound at scope end | — (our design) | **Take**: `Scope::spawn` without allocation |
| Thread start hook | rayon `start_handler` | **Take**: CPU sets / priorities from `engine_platform` |
| Heap-allocated spawns, async executor | rayon `spawn`, bevy_tasks | **Don't take**: violates §5 |
| Priority levels inside one pool | enkiTS, Naughty Dog | **Not now**: cooperative priorities don't stop a long job from blocking a worker. Background work gets its own pool with lower OS thread priority; the OS preempts it |
| Pinned tasks, task-graph dependencies | enkiTS, Naughty Dog | **Later**, when the render graph and ECS scheduler need them (ADR 0002) |
| Fibers | Naughty Dog, FiberTaskingLib | **Don't take now**: see ADR 0002 |
| Heartbeat scheduling | chili, Acar et al. | **Not now**: only `join`, needs a timer thread. Worth a prototype if fine-grained recursive `join` becomes a hotspot |
