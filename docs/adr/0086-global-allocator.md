# 0086. The global allocator: mimalloc 2, its memory given back by idle workers

- Status: Accepted (option 3, the owner, 2026-10-11)
- Date: 2026-10-11
- Research: [docs/research/global-allocator.md](../research/global-allocator.md)
- Related: CLAUDE.md §5 ("Global allocator: mimalloc; in dev builds, a wrapper that
  collects statistics"), [ADR 0002](0002-job-system.md) (the job pool),
  [ADR 0003](0003-profiling.md) (memory profiling deferred to this choice), review 2 §3.9
  (queue item 7a)

## Context

CLAUDE.md §5 names mimalloc, but no executable ever set a global allocator: everything
runs on Rust's default, the Windows process heap, and nothing counts the CPU memory a game
holds (0.9–1.1 GiB private in `flight`, unexplained, review 2). Tests set counting
allocators of their own (the zero-allocation tests); a library cannot set one, so the
choice belongs to the executables.

## Options

1. **The system heap** (as today): no dependency; 3.3–4.7× slower than mimalloc on
   allocation-heavy patterns, 1.5–1.6× on importing glTF and loading a pack; gives memory
   back at once.
2. **The segment heap** (a manifest per executable): 1.1–1.5× faster than the system heap
   on synthetic patterns, nothing on the engine's work, a 6× working set in the parallel
   pattern.
3. **mimalloc 2.3.2** (`mimalloc` =0.1.52 with `v2`, MIT, C built by `cc` in 5 s): the
   fastest; holds freed memory in the heap of the thread that allocated it until that
   thread runs mimalloc again (after a level load 341 MiB stayed committed until every
   worker collected; then 6 MiB).
4. **mimalloc 3.3.2** (the crate's default): no faster, commits 1.5–1.8× as much as 2.3.2,
   and is the version with the thread-identity bug the validation layer hit
   (`docs/research/vulkan-layer-thread-lifetime.md`).
5. **snmalloc 0.7.6**: slower than the system heap on Windows here, needs CMake and C++.

## Recommendation (to confirm)

Option 3, CLAUDE.md §5 as written, with what the measurements add:

- `engine_core::alloc::EngineAllocator`: mimalloc 2.3.2; in debug builds (and with
  Tracy, ADR 0003) a counting layer on top (bytes now and at peak, allocations, per
  thread, merged on request), so a game's CPU memory can be accounted for.
- Executables set it: `ofeliaengine` (the CLI), the player crate (queue item 8a), the
  examples that measure real work (`flight`, `game_loop`). Tests keep the system heap
  and their own counters.
- **Workers give memory back when they go idle:** a worker of `engine_core::jobs` about to
  park after running jobs calls `mi_collect(false)` (and `ThreadPool::collect_memory` asks
  every worker to after a level load), or loading leaves hundreds of MiB committed for as
  long as the pool sleeps.
- The frame's CPU time does not change (nothing allocates in the frame); loading, cooking,
  the editor and tools get faster.

## Questions for the owner

1. mimalloc trades memory for speed: `flight` holds 4–6 % more private memory (1,004–1,023
   against 873–970 MiB), and a level load peaks at about twice the system heap's working
   set while it runs (421 against 205 MiB for Sponza's 72.7 MB pack) before the workers
   give it back. Take mimalloc 2.3.2 with idle workers collecting (the recommendation), or
   keep the system heap?

## Decision

**Option 3, mimalloc 2.3.2** (the owner, 2026-10-11), with the job pool's idle workers
giving memory back and a counting layer in debug builds, as recommended above.

## Consequences

- If accepted: a C dependency (mimalloc, MIT) in `engine_core` behind a default feature,
  THIRD_PARTY_NOTICES updated, `check.ps1 -Deny`; the job pool gets an idle hook; the
  flight reports heap memory beside the process's (review 2 §3.9's unexplained private
  bytes).
- The harness `bench/alloc-compare` stays for re-measuring with new versions (mimalloc
  3.5+, a fixed snmalloc build).

## Implementation (2026-10-11)

- `engine_core::alloc::EngineAllocator`: mimalloc 2.3.2 (`mimalloc` =0.1.52 with `v2`,
  behind `engine_core`'s default feature `mimalloc`); in debug builds it counts the bytes
  and allocations Rust holds (`heap_stats`).
- `release_thread_memory`: two forced `mi_collect`s, the first taking in what other
  threads freed into the heap and freeing the pages that became empty, the second giving
  back the segments those left (one alone left 64–96 of 385 MiB behind). Under a
  microsecond with nothing to give, about 5 µs after a burst of jobs. (The addendum of
  2026-10-15 found the real reason and replaced the second collect.)
- The job pool's participants call it before they sleep when they ran jobs since their
  last sleep, and on `ThreadPool::release_memory` (every sleeper wakes, gives memory
  back and sleeps again): memory the workers allocated and another thread freed later
  (a loaded level dropped) otherwise stays committed while they sleep
  (`tests/alloc_release.rs`: 385 MiB kept, then back to the 35 MiB before the load).
- Set in `ofeliaengine` (the CLI), `flight` and `game_loop`; tests keep the system heap.

Measured with `flight` on the RTX (600 frames, the engine's pipeline cache warm): the
first frame 234.6–235.2 → 229.9 ms, the scene opened 9.9–12.7 → 9.1–9.7 ms, the CPU per
frame the same (animation on the job system 49.0–51.5 → 49.6–50.6 µs, inside the noise),
private memory 798–831 → 847–873 MiB (+5–6 %). The cook through the CLI is bound by
zstd (1.24–1.26 s either way).

Left: Tracy memory events (ADR 0003) through the same allocator.

## Addendum (2026-10-15): purges one at a time, and a missed release

`tests/alloc_release.rs` failed under load: in the gate's parallel tests, and in 11 of 72
runs with 24 copies running at once. 64–128 MiB stayed committed after `release_memory`,
always whole 32 MiB segments.

- **Cause.** A forced `mi_collect` frees the heap's empty segments into mimalloc's arenas,
  then purges (decommits) the arenas' free blocks. mimalloc 2.3.2 lets one thread purge at
  a time (`mi_arenas_try_purge`); a thread asking meanwhile goes on without purging.
  - Workers woken together by `release_memory` freed their segments while one of them was
    purging and had passed their blocks; their own purges were skipped.
  - Then every thread slept, and nothing purged later.
  - With `MIMALLOC_PURGE_DELAY=0` (a segment purged as it is freed, without the shared
    purge) none of 72 runs failed: the residue was freed segments waiting for a purge.
  - The second forced collect of the first implementation was a second chance at that
    purge, not a second step of the collect.
- **Options:**
  1. purging at once (`purge_delay` 0): every segment and page span freed is decommitted
     on the spot and committed again on its next use, which mimalloc's delay (10 ms, 100 ms
     for arenas) saves loading from;
  2. a lock around the releases: on the path every worker takes when it goes idle;
  3. mimalloc patched to wait for the purge when forced: a fork to keep;
  4. **purge turns of our own (taken).** `release_thread_memory` collects the thread's heap
     once and asks for a purge; it purges unless another caller is purging, and the one
     purging purges again while requests come in. So one purge starts after every
     caller's collect. Lock-free: two flags.
- **`mi_thread_init` first.** mimalloc's collect does nothing, purge included, on a thread
  that never allocated, and such a thread could be the one purging for the others.
- **A missed release, found on the way.**
  - A release asked for between a participant's read of the epoch and its sleep
    announcement passed it by: `wake_all` found it awake, and it slept without giving
    memory back.
  - The sleeper now reads the epoch again after announcing itself (Dekker-style, as for
    jobs).
  - `a_release_asked_for_on_the_way_to_sleep_is_not_missed` pauses a worker in that
    window. Without the check, the worker never gives memory back for the new epoch.
- **Measured (ref-1):**
  - 24 copies of `tests/alloc_release.rs` at once: 11 of 72 runs failed before; 17 of 72
    with only the epoch check; 0 of 120 with the purge turns, with or without it.
  - A 300 ms pause in the sleep window: the old protocol failed 3 runs of 3 (64–96 MiB
    left), the new one passed 3 of 3.
  - `release_thread_memory` alone (release build): 0.45 µs with nothing to give either
    way; 2.8 → 2.6 µs (median) after freeing 256 small blocks.
- **Left:** a thread outside `release_thread_memory` purging at that moment (one freeing a
  segment after the delay) can still make the last purge skip. That memory is then purged
  at the next allocation after the delay, as before.
