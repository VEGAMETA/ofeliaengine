# 0003. Profiling instrumentation (Tracy)

- Status: Accepted
- Date: 2026-09-25
- Research: [docs/research/profiling.md](../research/profiling.md)

## Context

CLAUDE.md requires Tracy everywhere (§5, §14) and a `profiling` build profile of release,
plus debug info, plus Tracy. Instrumentation must be free when compiled out, cheap when
compiled in, and must never panic. The built-in editor profiler comes later and should reuse
the same instrumentation points.

## Options

1. **tracy-client at every call site.** It gives the complete API, but every crate needs
   feature plumbing, and `span!` panics when no client is running.
2. **`profiling` facade.** It exposes only a common subset (no plots, no GPU contexts, no
   Tracy options) and adds one more layer.
3. **`tracing` + `tracing-tracy`.** Subscriber dispatch is too expensive for dense hot-path
   zones.
4. **Our own thin crate `engine_profiler` over tracy-client** (chosen).

## Decision

- **Crate `engine_profiler`** sits at the bottom of the dependency graph (`engine_core`
  depends on it). It provides:
  - `zone!()` / `zone!("name")`;
  - `plot!("name", value)`;
  - `secondary_frame_mark!("name")`;
  - `frame_mark()`, `message()`, `set_thread_name()`, `is_connected()`, `ENABLED`.
- **Macros never panic.** They check `Client::running()`. Without a Tracy feature, every macro
  and function compiles to nothing. The macros are defined under `cfg` inside
  `engine_profiler`, so callers need no feature flags of their own.
- **Tracy version.** tracy-client 0.19 with tracy-client-sys 0.30 bundles the Tracy
  **0.14.1** client. The profiler GUI must also be 0.14.1.
- **Features:**
  - `tracy`: capture from process start, accept connections from localhost only;
  - `tracy-remote`: the same without the localhost restriction (do not combine with `tracy`,
    because Cargo features are additive and `only-localhost` would win);
  - `tracy-ondemand`: collect only while a profiler is connected, for long editor sessions.
- **Tracy options.** The crash handler is disabled, because the engine will install its own.
  System tracing, context switches, sampling, code transfer, broadcast, inline call stacks
  and wait stacks are enabled. The OS features need an elevated process on Windows and are
  skipped silently otherwise.
- **One button.** `cargo profiling` (an alias in `.cargo/config.toml`) builds the workspace
  with `--profile profiling --features engine_profiler/tracy`. Cargo profiles cannot enable
  features, hence the alias.
- **First instrumentation:**
  - job system zones `jobs::scope`, `jobs::parallel_for` and `jobs::park` (idle time);
  - worker threads are named in Tracy.
- **CI** runs the whole test suite a second time with Tracy enabled, including the
  zero-allocation test, and builds the `profiling` profile with Tracy.

## Measurements

Cost per call, Tracy compiled in, no profiler connected, from
`cargo bench -p engine_profiler --bench zones`:

| Mode | `zone!` | two nested | `plot!` |
|---|---|---|---|
| Container, on-demand | 4.5 ns | 12.0 ns | 2.7 ns |
| Container, capture from start | 60 ns | 851 ns | 205 ns |
| ref-1, on-demand | 6.0 ns | 8.3 ns | 1.5 ns |
| ref-1, capture from start | 18.5 ns | 37.7 ns | 9.0 ns |

ref-1 numbers are from Bench run on commit `009d234`.

Capture from start buffers every event in memory until a profiler connects. On ref-1 this
costs about 3× more per zone than on-demand mode. In the memory-constrained container it
cost much more, and the cost grew as the buffer filled. Connect the profiler early, or use `tracy-ondemand`, when a Tracy
build runs for a long time without Tracy attached. Benchmarks that are not about profiling run
without Tracy.

## Consequences

- Every subsystem instruments itself with `engine_profiler` macros from its first commit
  (CLAUDE.md §5).
- Per-frame zone creation does not allocate after the first execution of each call site.
  The zero-allocation test passes with Tracy enabled.
- Later work:
  - GPU zones through tracy-client's GPU context API, together with `engine_rhi`;
  - memory profiling through a profiled global allocator, when the runtime chooses its
    allocator (mimalloc, §5);
  - Luau zones (`engine_script`);
  - frame images;
  - the in-process editor profiler, which will hook into the same macros.
- A Tracy upgrade means bumping tracy-client-sys and telling developers which profiler
  version to download (`docs/dev-guide/profiling.md`).
