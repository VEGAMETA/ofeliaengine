# 0002. Job system

- Status: Proposed. Becomes Accepted when the ref-1 benchmark run confirms the container
  numbers below (Bench workflow on the PR that introduces it).
- Date: 2026-09-25
- Research: [docs/research/job-system.md](../research/job-system.md)

## Context

CLAUDE.md §5 requires our own job system, compared against rayon, bevy_tasks, enkiTS and
Naughty Dog's fibers, with zero heap allocations per frame, no locks on hot paths and
P/E-core aware placement. The engine needs fork-join over borrowed data (`join`, scoped
spawns), deterministic data-parallel loops (physics, §8), participation of the main and
render threads, and low wake-up latency at frame start.

## Options

1. **rayon as is.** Mature, fast `join`. But every `scope.spawn` allocates (measured
   below), crossbeam's deque grows and shrinks its buffer under bursty load, idle workers
   sleep on a `Mutex` + `Condvar`, parallel-iterator splitting depends on timing, and
   non-worker threads cannot participate (they block in `install`).
2. **bevy_tasks.** Built on `async-executor`: one heap allocation per task, results
   collected into a `Vec`, an async runtime in the core (§5 forbids it).
3. **enkiTS through FFI.** Good game-oriented design (fixed pipes, pinned tasks,
   priorities, external threads). But it is C++: a C callback per task, no borrow checking
   across the boundary, a C++ build in the core crate.
4. **Fibers (Naughty Dog style)** on the Windows fiber API. Waiting would never nest stacks
   or run unrelated work on top of a waiter. But in Rust a fiber that resumes on another
   thread invalidates thread-local addresses the compiler may have cached, `MutexGuard`s
   and `RefCell` borrows can be held across a switch, unwinding across fiber boundaries
   is unsupported, and debuggers and Tracy need extra fiber support. The stacks cost
   ~24 MiB for Naughty Dog's configuration.
5. **Heartbeat scheduling (chili).** Excellent for fine-grained recursive `join`, but it
   offers nothing else and needs a timer thread that wakes periodically.
6. **Our own work-stealing pool** (chosen), combining the best-fitting ideas from the
   research.

## Design of option 6

- **Deques:** fixed-capacity Chase–Lev (C11 orderings from Lê et al. 2013), `AtomicPtr`
  slots, default 1024 per participant. A push into a full deque runs the job inline.
  Verified with loom model checks (`RUSTFLAGS="--cfg loom"`); a mutation test (removing
  the `SeqCst` fence in `pop`) is caught by the models.
- **Injector:** Vyukov bounded MPMC queue for non-participant threads.
- **Jobs without allocation:** `join` and `parallel_for` keep their records on the
  caller's stack; `Scope::spawn` bump-allocates records in a per-participant arena (64 KiB
  default) that is rewound when the scope ends — scopes nest strictly per thread, so the
  arena behaves like a stack. Arena full → the job runs inline.
- **`parallel_for(len, grain, f)`:** chunk `i` is always `i*grain .. min((i+1)*grain, len)`.
  One record on the caller's stack; threads claim chunks from an atomic counter; up to
  (participants − 1) references to the same record are published for thieves. Per-chunk
  results combined in chunk order are deterministic regardless of thread count.
- **Sleeping:** `std::thread::park` (`WaitOnAddress` on Windows, futex on Linux), a
  per-participant state (`AWAKE/SLEEPING/WOKEN/WOKEN_SEARCH`) claimed by CAS, and a
  sleeping bitmask as a hint. No mutex or condition variable anywhere. Wake-ups are
  throttled Go-style: a single push wakes one sleeper only if nobody is searching; a
  searcher that finds work hands the search over; `parallel_for` wakes as many sleepers
  as it published references. Local pushes skip the `SeqCst` fence (a missed wake-up only
  costs parallelism, since the owner runs its own jobs while waiting); injected jobs and
  latch completions keep it (a missed wake-up there would hang).
- **Participants:** N workers plus E external slots (default 2). The main and render
  threads call `register_current_thread` and then run jobs while they wait.
- **Panics:** caught per job and re-thrown on the waiting thread after all jobs of the
  operation have finished; the pool stays usable.
- **Placement:** `on_thread_start(index)` hook; `engine_platform` will use it for CPU
  sets, P-cores and thread priority.
- **Background work:** a second `ThreadPool` with low OS priority rather than priority
  levels inside one pool, so the OS preempts long jobs when frame workers need a core.

## Measurements

`cargo bench -p engine_core --bench jobs` (harness in `crates/engine_core/benches/jobs.rs`):
engine and rayon measured in alternating blocks, rayon operations run inside
`rayon.install` (on a rayon worker) as ours run on a registered participant. Heap
allocations counted by a global counting allocator.

### Container (not reference hardware)

4 vCPU cloud container, Linux, `bench` profile, commit of this ADR; three runs.
Ratio = rayon median / engine median (> 1: engine faster).

| Case | Engine median (run 1) | Ratio, 3 runs | Allocations per op, engine / rayon |
|---|---|---|---|
| `join`: fib(30), sequential below 12 | 795 µs | 1.04 / 1.01 / 1.04 | 0 / 0 |
| `parallel_for`: 1M items, 256 even chunks | 664 µs | 1.06 / 1.04 / 1.04 | 0 / 0 |
| `parallel_for`: 256 chunks, triangular cost | 520 µs | 1.09 / 1.07 / 1.13 | 0 / 0 |
| `scope`: 256 spawns of ~50 ns | 64 µs | 1.16 / 1.02 / 1.01 | **0 / 256** |
| Cold burst after 3 ms idle | 48 µs | 1.31 / 1.03 / 1.01 | 0 / 0 |

Absolute times in the container vary by up to 2× between runs; the interleaved ratios
are stable to a few percent.

### ref-1

Filled in from the Bench workflow comment on the PR (see `docs/perf/history.md`).

### Zero allocations

`crates/engine_core/tests/jobs_zero_alloc.rs`: after warm-up, 500 "frames" of
`join` (fib 18), a scope with 256 spawns and a 64-chunk `parallel_for` perform **0**
heap allocations process-wide.

## Decision

Option 6: our own pool in `engine_core::jobs`, as designed above. rayon stays a
dev-dependency as the benchmark baseline only.

## Consequences

- Every engine subsystem uses `engine_core::jobs::ThreadPool`; no other thread pools in
  the runtime.
- Limits: at most 64 participants per pool; `Scope::spawn` only from the thread that owns
  the scope (nested parallelism opens a nested scope or uses `join`); job records larger
  than the arena run inline.
- Help-while-waiting can run an unrelated long job on top of a waiter and delay it. Frame
  work must therefore stay fine-grained, and long work goes to the background pool.
- Not implemented yet, to add when a subsystem needs them: task-graph dependencies (the
  render graph and ECS scheduler will drive this), pinned tasks, Tracy zones per job
  (with the Tracy integration), CPU-set placement (with `engine_platform`).
- To revisit with numbers: spin duration before parking (`spin_rounds`, default 64),
  deque capacity, heartbeat-style lazy splitting for fine-grained `join`, fibers if
  waiting latency becomes a measured problem.
