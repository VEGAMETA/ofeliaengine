# Profiling instrumentation: research

Date: 2026-09-25. Input for [ADR 0003](../adr/0003-profiling.md). No code was copied from
the projects below.

## Requirements (CLAUDE.md §5, §14)

- Tracy is mandatory everywhere, with zones in every subsystem from day one.
- The `profiling` build profile is release, plus debug info, plus Tracy.
- Instrumentation must cost nothing when it is compiled out and very little when it is
  compiled in, because hot paths will be dense with zones.
- The built-in editor profiler (frame timeline, per-viewport GPU passes, physics, audio
  DSP, Luau) comes later and should reuse the same instrumentation points.
- There must be no panics on hot paths, including when the profiler is not running.

## Options

| Option | Version checked | License | Notes |
|---|---|---|---|
| [tracy-client](https://github.com/nagisa/rust_tracy_client) directly | 0.19.0 (tracy-client-sys 0.30.0 = Tracy **0.14.1**), 2026-09-25 | MIT OR Apache-2.0; Tracy: BSD-3-Clause | Thin, complete Tracy API: zones, frames, plots, messages, GPU contexts, allocation tracking. The `span!` macro **panics** if the client is not running. Every call site needs its own feature plumbing. |
| [`profiling`](https://github.com/aclysma/profiling) crate | — | MIT OR Apache-2.0 | Facade over puffin, optick, superluminal, tracy and tracing. Least-common-denominator API (scopes and frames), so it lacks plots, GPU zones and Tracy-specific options, and adds one more layer to change. |
| `tracing` + `tracing-tracy` | — | MIT | Structured spans. The subscriber dispatch per span costs far more than a Tracy zone. Fine for logs, too heavy for dense hot-path zones. |
| [puffin](https://github.com/EmbarkStudios/puffin) | — | MIT OR Apache-2.0 | In-process Rust profiler with an egui viewer. No sampling, no context switches and no GPU zones. A useful reference for the built-in editor profiler (in-process capture into a ring buffer). |
| Superluminal | — | proprietary | Excluded by the open-source rule (CLAUDE.md §0.2). |

The Tracy version was checked against the release list (Tracy 0.14.0 and 0.14.1, 2026) and
against `TracyVersion.hpp` inside tracy-client-sys 0.30.0. The client and the profiler GUI
must use the same protocol version.

## Findings from measurements (container, not reference)

`crates/engine_profiler/benches/zones.rs`, Tracy compiled in, no profiler connected:

| Mode | `zone!` | two nested `zone!` | `plot!` |
|---|---|---|---|
| on-demand (events dropped until a profiler connects) | 4.5 ns | 12.0 ns | 2.7 ns |
| capture from start (events buffered in memory until a profiler connects) | 60 ns | 851 ns | 205 ns |

With capture from start, cost grows as the in-memory buffer fills: the nested case ran after
several million buffered events. This is expected Tracy behavior and not a problem during a
real profiling session, where the profiler drains the buffer. It does mean that a Tracy build
that nobody connects to is not representative for benchmarks. ref-1 numbers come from the
Bench workflow.

## Decision input

- Wrap tracy-client in our own `engine_profiler` crate. The macros check `Client::running()`
  (never panic) and compile to nothing without the feature, which makes them the single hook
  point for the future in-process editor profiler.
- Capture from start by default, so that start-up (cold-start budget) is visible. Offer on-demand
  mode for long editor sessions.
- Accept profiler connections from localhost only by default. The Tracy client otherwise listens
  on all interfaces and exposes profiling data (names, messages, call stacks) to the LAN.
- Disable Tracy's crash handler. The engine will install its own minidump handler (§14), and two
  handlers would fight.
