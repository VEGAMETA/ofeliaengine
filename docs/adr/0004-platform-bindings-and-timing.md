# 0004. Platform bindings, timers and thread placement

- Status: Accepted (timer accuracy numbers from ref-1 below).
- Date: 2026-09-25
- Research: [docs/research/timers-and-threads.md](../research/timers-and-threads.md)

## Context

`engine_platform` is the thin OS layer (CLAUDE.md §2, §5): waitable timers instead of
`Sleep`, CPU sets and EfficiencyClass for P/E cores, MMCSS, and power throttling disabled.
It needs Win32 bindings, and the choice affects every OS-facing crate.

## Options for bindings

1. **`windows-sys`** (Microsoft, MIT OR Apache-2.0). Raw `extern` declarations and types,
   gated by feature per API area. It compiles fast. Since 0.60 it links through
   `windows-link` (raw-dylib), so no import libraries are needed. There are no COM wrappers.
2. **`windows`** (Microsoft, same license). Safe-ish wrappers, COM interfaces and `Result`
   conversions. It is heavier to compile.
3. **`winapi`**. Unmaintained since 2021.
4. **Hand-written `extern "system"` blocks**. No dependency, but every struct layout has to
   be retyped and kept correct by hand.

## Decision

- **`windows-sys` 0.61** for `engine_platform` and other flat Win32 APIs. Every call is
  wrapped in a small safe function in our crate, with a `// SAFETY:` comment.
- The **`windows`** crate is allowed later, where COM is unavoidable (WASAPI and MMDevice in
  `engine_audio`), in a separate ADR if it brings in new crates.
- **Crate layout:** `engine_platform` is `cfg(windows)`. On other hosts it compiles to
  nothing, so the workspace still builds and tests in Linux containers. Windows tests run
  in CI.
- **First APIs:**
  - `time::HighResTimer`: a high-resolution waitable timer, one per thread, reused;
  - `time::PreciseSleeper`: sleep on the timer until `deadline − margin`, then spin. The
    margin is the worst of the last 64 overshoots plus 20 µs, kept across calls; the
    initial margin is 1 ms;
  - `cpu::CpuTopology`: CPU sets with efficiency class, core, LLC and NUMA; P/E split;
    summary;
  - `thread`: `set_current_thread_cpu_sets` (soft placement), priorities,
    `MmcssRegistration` (`Games`, `Pro Audio`; reverted on drop, `!Send`),
    `set_current_thread_description`;
  - `process::disable_power_throttling` (execution speed and timer resolution).
- `std::thread::sleep` stays disallowed by clippy (ADR 0001) in favor of these.

## Measurements

`cargo bench -p engine_platform --bench timers` on ref-1 (Bench workflow, commit `0f9a973`).
The table shows error (actual wait minus requested wait), median / p99 in µs, over 200
samples per cell:

| Requested | `std::thread::sleep` | `HighResTimer` | `PreciseSleeper` | `PreciseSleeper` margin |
|---|---|---|---|---|
| 0.5 ms | 513 / 531 | 514 / 537 | 0 / 3.0 | 1000 µs (initial) |
| 1 ms | 518 / 575 | 518 / 732 | 0 / 1.2 | 1000 µs (initial) |
| 2 ms | 500 / 893 | 499 / 1203 | 0 / 14.5 | 535 µs |
| 4.167 ms (240 Hz) | 331 / 507 | 335 / 1069 | 0 / 3.2 | 624 µs |
| 16.667 ms (60 Hz) | 330 / 514 | 349 / 525 | 0 / 3.0 | 531 µs |

Findings:
- On ref-1 the high-resolution timer fires on the first 0.5 ms boundary strictly after the
  due time, so it overshoots by 0–500 µs.
- `HighResTimer` is **no more accurate** than `std::thread::sleep` (both use the same kind of
  timer since Rust 1.75). Its only advantage is that it does not create and close a timer on
  every call. The differences in p99 are within noise.
- `PreciseSleeper` hits deadlines within microseconds (p99 1–15 µs; the worst sample was
  570 µs, when a preemption landed during the spin). It spins for about 0.5 ms per wait,
  roughly 12 % of one core at 240 Hz. That is acceptable for a frame limiter, and the ADR
  for §7 frame pacing may prefer `present_wait` / Reflex where the hardware offers them.

## Consequences

- Frame limiter (§7) and pacing code use `PreciseSleeper`; ordinary waits use
  `HighResTimer`.
- `engine_runtime` will apply the thread placement policy through the job pool's
  `on_thread_start` hook and on the main, render and audio threads.
- Placement on hybrid CPUs cannot be verified on ref-1 (homogeneous Zen 4). It needs a
  hybrid test machine.
