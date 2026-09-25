# Timers, thread placement and power settings on Windows 11: research

Date: 2026-09-25. Input for [ADR 0004](../adr/0004-platform-bindings-and-timing.md). No code
was copied from the projects below.

## Waiting precisely

| Source | Version checked | License | Approach |
|---|---|---|---|
| Rust std `thread::sleep` ([PR #116461](https://github.com/rust-lang/rust/pull/116461)) | toolchain 1.94.1 | MIT/Apache-2.0 | Since Rust 1.75, creates a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer and waits on it, falling back to `Sleep` before Windows 10 1803. A new timer per call. |
| [SDL3](https://github.com/libsdl-org/SDL) `SDL_DelayPrecise` | commit `fef0d24` (2026-09-24) | Zlib | One high-resolution waitable timer per thread, stored in TLS. `DelayPrecise` sleeps in 1 ms slices while tracking the largest overshoot seen **during this call**, then does one shorter sleep that subtracts that overshoot, then spins with `pause`. The overshoot estimate starts from scratch on every call. |
| [Godot](https://github.com/godotengine/godot) `OS_Windows::delay_usec` | commit `4617300` (2026-09-25) | MIT | `Sleep(ms)`, with at least 1 ms. Millisecond granularity, no spin. |
| Microsoft docs: `CreateWaitableTimerExW` | checked 2026-09-25 | — | The `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` flag (Windows 10 1803+) gives a timer that does not depend on the global timer resolution (`timeBeginPeriod`). |

What we take:
- **One high-resolution waitable timer per waiting thread** (as SDL3 does). It avoids std's
  create/close per call and needs no global `timeBeginPeriod`.
- **Sleep, then spin** for deadlines that must be hit precisely (frame limiter, §7).

What we do differently:
- The overshoot estimate **persists across calls** in `PreciseSleeper`: the worst of the last
  64 timer overshoots plus 20 µs. A frame limiter calls it every frame, so after the first few
  frames the margin reflects this machine's real timer behavior. There is no 1 ms slicing and
  no re-learning on every call. The first waits use a conservative 1 ms margin.

## Power throttling and timer resolution

- `SetProcessInformation(ProcessPowerThrottling)`:
  - `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` in the control mask with a zero state mask
    opts out of EcoQoS. Without it, Windows may run a background or occluded process at
    efficiency QoS (E-cores and low clocks on hybrid CPUs);
  - `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION` (Windows 11) stops Windows from
    ignoring the process's timer resolution requests while its window is minimized or
    occluded.
- Both matter for an editor that keeps running while it is behind other windows, and for
  asset cooking in the background.

## Thread placement on hybrid CPUs

- `GetSystemCpuSetInformation` reports, for each logical processor, the core index, the
  last-level cache index, the NUMA node and the **EfficiencyClass**. Higher classes are faster
  cores; on homogeneous CPUs all sets are class 0.
- `SetThreadSelectedCpuSets` is a *soft* restriction that the scheduler honors, and it composes
  with the scheduler's own decisions (Intel Thread Director hints). Hard affinity masks
  (`SetThreadAffinityMask`) are rigid and are best avoided.
- MMCSS (`AvSetMmThreadCharacteristicsW`): "Pro Audio" for the audio thread and "Games" for
  the render thread (CLAUDE.md §5). The registration is reverted on the same thread.
- Policy (applied later by `engine_runtime`, through the job pool's `on_thread_start` hook):
  - frame workers, main, render and audio run on performance sets;
  - the background pool runs on efficiency sets when the CPU is hybrid, and anywhere
    otherwise, at below-normal priority.

## Open questions, to measure on hardware

- The margin statistic: worst of 64 samples wastes more spin after a single outlier, and a
  high percentile would spin less. `benches/timers.rs` reports error and margin on ref-1.
- ref-1 (Ryzen 7 7700) is homogeneous. Placement on hybrid CPUs needs an Intel 12th-gen or
  newer machine (`docs/perf/reference-hw.md` lists this gap).
