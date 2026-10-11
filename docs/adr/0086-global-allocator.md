# 0086. The global allocator: mimalloc 2, its memory given back by idle workers

- Status: Proposed (a question for the owner, 2026-10-11)
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

Pending.

## Consequences

- If accepted: a C dependency (mimalloc, MIT) in `engine_core` behind a default feature,
  THIRD_PARTY_NOTICES updated, `check.ps1 -Deny`; the job pool gets an idle hook; the
  flight reports heap memory beside the process's (review 2 §3.9's unexplained private
  bytes).
- The harness `bench/alloc-compare` stays for re-measuring with new versions (mimalloc
  3.5+, a fixed snmalloc build).
