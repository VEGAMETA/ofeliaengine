# The global allocator

Research for the decision CLAUDE.md §5 names but nobody made (review 2 §3.9; ADR 0003
deferred memory profiling "until the runtime chooses its allocator"): which allocator the
engine's executables use. Measured on ref-1 (`docs/perf/reference-hw.md`: Ryzen 7 7700,
64 GB DDR5-6000, Windows 11 24H2 26100) on 2026-10-11, release builds for x86-64-v2, with
the harness `bench/alloc-compare` (a workspace of its own: nothing of it reaches the
engine). Crate versions checked on crates.io the same day.

## Candidates

| | Version (2026-10-11) | License | Build | How it works |
|---|---|---|---|---|
| **System** (Rust's default on Windows) | Windows 11 24H2 | OS | none | `HeapAlloc` on the process heap: the NT heap with the Low Fragmentation Heap in front (buckets of fixed sizes, a lock-free-ish front end per size, a locked back end) |
| **Segment heap** | Windows 10 2004+ | OS | an application manifest (`heapType` = `SegmentHeap`) | Windows' newer heap (default for UWP and system processes): low-fragmentation buckets, variable-size segments, large blocks from the OS; chosen per process by the manifest |
| **mimalloc** | crate `mimalloc` 0.1.52 (2026-05-22) with `libmimalloc-sys` 0.1.49: mimalloc **3.3.2** by default, **2.3.2** with the `v2` feature | MIT (Microsoft) | C, through `cc` (5 s) | free-list sharding (Leijen, Zorn, de Moura, "Mimalloc: Free List Sharding in Action", APLAS 2019): per-thread heaps of 64 KiB pages, a free list per page (local, plus a separate list for frees from other threads, moved over in batches), memory from large OS-reserved segments/arenas, freed pages purged after a delay |
| **snmalloc** | crate `snmalloc-rs` 0.7.6 (2026-10-02) | MIT (Microsoft Research) | C++ through CMake (29 s) | message passing (Liétar et al., "snmalloc: A Message Passing Allocator", ISMM 2019): a free from another thread is batched and sent to the owning allocator's queue; slabs per size class |
| rpmalloc | `rpmalloc` 0.2.2 (2021), `rpmalloc-sys` 0.2.3 (2022) | MIT / Apache-2.0 bindings (Embark), the library public domain / MIT | C | per-thread caches of spans; the Rust bindings are unmaintained since 2022: not measured |
| jemalloc | `tikv-jemallocator` | BSD-2 | C | no MSVC support: excluded |

No code of any of them was copied; the harness only links them.

## Workloads (`bench/alloc-compare`)

One workload per process, so that the peak memory is the workload's own:

- **churn:** one thread, 20 M allocations of 16 B–16 KiB (70 % 16–128 B, 25 % up to
  1 KiB, 5 % up to 16 KiB, the same sizes for every allocator), 4,096 alive, the oldest
  freed first;
- **parallel:** the same on all 16 hardware threads at once, 5 M each;
- **handoff:** allocated on four threads, freed on four others (job results, messages);
- **import:** Sponza's glTF imported (JSON, buffers, the scene's tables);
- **cook:** Sponza cooked from a warm derived data cache and written to a pack;
- **load:** that 72.7 MB pack loaded (entries read and decompressed on the job pool,
  textures kept): what a game does when it opens a level;
- **load-collect:** the load, the scene dropped, the allocator asked to give memory back,
  first on the main thread only, then on every worker of the pool too.

## Results

Median milliseconds of 5–15 repetitions (two rounds each; ranges are the two rounds);
peak working set / peak committed memory in MiB:

| workload | system | segment heap | mimalloc 2.3.2 | mimalloc 3.3.2 | snmalloc 0.7.6 |
|---|---:|---:|---:|---:|---:|
| churn | 716–735 | 629 | 196–197 | 186 | 3,463–3,552 |
| parallel | 435 | 304 | 93 | 89 | 1,480 |
| handoff | 122–124 | 80 | 37 | 46–48 | 323–329 |
| import | 11.1–11.2 | 10.9 | 7.5–7.6 | 7.4–7.6 | 9.4–9.6 |
| cook | 1,200–1,238 | 1,187 | 1,195–1,199 | 1,189–1,196 | 1,256–1,257 |
| load | 53 | 57–90 | 33–34 | 33 | 49–50 |
| memory, parallel | 94 / 96 | 569 / 570 | 85–97 / 148 | 159 / 195 | 83 / 102 |
| memory, cook | 470 / 488 | 489 / 502 | 546 / 653 | 740 / 1,157 | 528 / 568 |
| memory, load | 205 / 207 | 222 / 221 | 421 / 483–500 | 486–511 / 737–741 | 281 / 291 |
| private after load + collect on the main thread | 8 | — | 341 | 473 | — |
| private after load + collect on every worker | 7 | — | 6 | 12 | — |

mimalloc's purge delay at 0 (`MIMALLOC_PURGE_DELAY=0`) lowered its load peak by 15–20 %
and doubled its load time (34 → 67 ms) and churn (+40 %); `MIMALLOC_ARENA_EAGER_COMMIT=0`
changed nothing measurable.

**The game's frame:** `flight` (`engine_runtime`, Sponza's pack streamed, 64 skinned
characters, 2,000 movers, 600 frames on the RTX) with mimalloc 2.3.2 against the system
heap: the CPU per frame the same (the frame allocates nothing, CLAUDE.md §5), the start-up
phases the same but the Fox cooked from the cache (25.9 → 20.5 ms), private bytes 873–970
→ 1,004–1,023 MiB (+4–6 %). Start-up's pipeline phase could not be compared: the NVIDIA
driver's Vulkan shader cache on ref-1 is full (`%LOCALAPPDATA%\NVIDIA\GLCache`, 1.1 GB), so
a newly built executable stays cold run after run whatever its allocator (a copy of the
system build was as slow as the mimalloc build: 174–182 ms against 29 ms for the binary
the cache already knew); `docs/perf/reference-hw.md`.

## Conclusions

- **mimalloc 2.3.2** is the fastest where allocation matters: 3.7× the system heap on
  churn, 4.7× in parallel, 3.3× on cross-thread frees, 1.5× on the glTF import, 1.6× on a
  pack load; the cook is compute-bound (the same). It holds more memory while it runs:
  freed memory stays in the heap of the thread that allocated it until that thread runs
  mimalloc again. The job pool's workers allocate while loading and then sleep, so after a
  level load 341 MiB stayed committed until every worker collected (then 6 MiB). The
  engine's workers must give memory back when they go idle (ADR 0086).
- **mimalloc 3.3.2** (the crate's default) is no faster than 2.3.2 except on one-thread
  churn (−5 %), slower on cross-thread frees (+27 %), and commits 1.5–1.8× as much (cook
  1,157 against 653 MiB). It is also the version whose main-thread bookkeeping broke when a
  thread's TEB address was reused (the crash of the validation layer that links it,
  `docs/research/vulkan-layer-thread-lifetime.md`, fixed upstream in 3.5): not now.
- **snmalloc 0.7.6** was slower than the system heap here (4.8× on churn, 2.6× on
  cross-thread frees) and needs CMake and C++. Its numbers on other platforms are good;
  something in its Windows build or our size mix does not suit it. Not investigated
  further.
- **The segment heap** is a free switch (a manifest) with modest gains (1.1–1.5× on the
  synthetic patterns, none on the engine's work) and a far larger working set in the
  parallel pattern (569 against 94 MiB).
- Nothing allocates in the frame, so the allocator moves loading, cooking, the editor and
  tools, not the frame's CPU time.

## Sources

- mimalloc: https://github.com/microsoft/mimalloc (v2.3.2 and v3.3.2 as bundled by
  `libmimalloc-sys` 0.1.49); D. Leijen, B. Zorn, L. de Moura, "Mimalloc: Free List
  Sharding in Action", APLAS 2019 / MSR-TR-2019-18.
- Rust bindings: https://github.com/purpleprotocol/mimalloc_rust (`mimalloc` 0.1.52, MIT).
- snmalloc: https://github.com/microsoft/snmalloc (`snmalloc-rs` 0.7.6, MIT); P. Liétar et
  al., "snmalloc: A Message Passing Allocator", ISMM 2019.
- rpmalloc: https://github.com/mjansson/rpmalloc; bindings
  https://github.com/EmbarkStudios/rpmalloc-rs.
- The segment heap: Microsoft Learn, "Application manifests" (`heapType`); M. V. Yason,
  "Windows 10 Segment Heap Internals", Black Hat USA 2016.
