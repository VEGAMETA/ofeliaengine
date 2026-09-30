# 0061. Failure visibility: a lock-free log behind the `log` facade, out-of-process crash reports, GPU breadcrumbs and `VK_EXT_device_fault`

- Status: Accepted
- Date: 2026-10-01
- Research: [docs/research/crash-and-gpu-fault-diagnostics.md](../research/crash-and-gpu-fault-diagnostics.md)
- Related: [ADR 0003](0003-profiling.md) (Tracy), [ADR 0006](0006-vulkan-baseline-and-device.md),
  [ADR 0011](0011-render-graph.md) (the render graph)

## Context

The engine had no logging (a few `eprintln!`), no crash handler (`engine_profiler`
switched Tracy's off, "the engine will install its own") and no path for
`VK_ERROR_DEVICE_LOST`: `VK_EXT_device_fault` was detected but never enabled, and nothing
recorded where the GPU was. CLAUDE.md §6 and §14 ask for all three: device-fault data and
markers for GPU crashes, minidumps with context (recent logs, GPU markers), structured
logging without allocation on hot paths.

## Measurements

ref-1 (Ryzen 7 7700; RTX 4060 Ti 616.92; Radeon iGPU 25.10.36), release builds.

**Logging hot path** (`cargo bench -p engine_log --bench log`, ns per call on the calling
thread; the writer thread writes a file meanwhile):

| call | ns | with Tracy compiled in |
|---|---:|---:|
| disabled level (`trace!` under `info`) | 0.2 | 0.2 |
| disabled target (`bench::off=off`) | 5.3 | 5.2 |
| literal message, 23 bytes | 40 | 56 |
| integer, string and float (~50 bytes) | 115 | 132 |
| 900-byte record (4 ring entries) | 277 | 465 |
| the ~50-byte message from 4 threads at once | 157–187 | 180 |

0 allocations per call (`tests/zero_alloc.rs`, 30,000 records).

**Breadcrumbs** (`cargo run -p engine_render --release --example breadcrumb_bench`: two
devices on one GPU, one with markers, frames alternating; passes clear 512×512 RGBA16F
targets; medians of 300 frames):

| GPU | frame | passes | CPU µs off / on | CPU ns per pass | GPU µs off / on | GPU ns per pass |
|---|---|---:|---:|---:|---:|---:|
| RTX 4060 Ti | chain | 16 | 15.6 / 17.4 | 112 | 17.7 / 21.2 | 216 |
| RTX 4060 Ti | chain | 64 | 56.9 / 63.3 | 100 | 66.1 / 78.6 | 195 |
| RTX 4060 Ti | chain | 256 | 224.1 / 249.1 | 98 | 237.9 / 280.5 | 166 |
| RTX 4060 Ti | independent | 64 | 52.5 / 58.6 | 95 | 60.2 / 71.4 | 174 |
| Radeon iGPU | chain | 16 | 53.6 / 53.2 | noise | 394.6 / 396.0 | 90 |
| Radeon iGPU | chain | 64 | 239.2 / 237.9 | noise | 1612.3 / 1615.4 | 49 |
| Radeon iGPU | chain | 256 | 985.7 / 987.9 | 9 | 6414.8 / 6450.7 | 140 |
| Radeon iGPU | independent | 64 | 271.8 / 271.7 | noise | 2093.5 / 2097.9 | 69 |

- On NVIDIA a top-of-pipe marker alone costs ~100 ns of GPU time (measured with end
  markers off); marker memory in VRAM instead of system memory changes nothing.
- The editor (`--frames 600`, 3 alternating runs each): RTX GPU 95 → 97 µs per frame,
  record 63–72 µs either way; Radeon GPU 2049–2062 µs either way.

**GPU fault**: a compute pass writing to an unmapped address on the RTX
(`ENGINE_TEST_GPU_FAULT=page`) gave `VK_ERROR_DEVICE_LOST` in `vkWaitSemaphores` and this
report (in the log and in `%LOCALAPPDATA%\ofeliaengine\reports`):

```text
GPU device lost (in vkWaitSemaphores)
device: NVIDIA GeForce RTX 4060 Ti (discrete, Vulkan 1.4.351, 15.7 GiB, NVIDIA 616.92)
  driver: (no description)
  instruction pointer of the faulting instruction: address 0x000000020000b970 (somewhere in 0x20000b970..0x20000b980)
  invalid write: address 0x00007f00df852000 (somewhere in 0x7f00df852000..0x7f00df853000)
  vendor crash dump: 1220084 bytes
  breadcrumbs:
    track 0 (render graph, frame slot 0): frame 1, 3 passes: the GPU last started pass 1 'fault' and last finished pass 0 'before the fault'
      running: 1 'fault'
```

The Radeon iGPU drops writes to unmapped addresses without losing the device, and did not
lose it on a 2^48-iteration loop either (it finished after 4.4 s); its report path is
covered by the normal-frame test only.

**Crash reports**: `install` 1.8–2.4 ms on the calling thread; editor cold start with it
413–477 ms (window + device 376–396 ms, as before). A test child that writes to address
0x10 leaves a 59 KB minidump and a report with the exception, the report sections and the
log tail; a panicking child leaves a 56 KB minidump and a report with the message and
backtrace. Clean compile of `engine_crash` and its dependencies: 16.9 s dev, 8.7 s release.

## Options

### Logging front end

1. **The `log` facade with our own backend** (chosen): dependencies that log (minidumper,
   gpu-allocator, …) land in the same place; `log!` macros check a global maximum level
   first (0.2 ns when disabled). The backend receives `fmt::Arguments`, so formatting
   happens on the caller.
2. **`tracing`**: spans and typed fields, heavier dispatch, and it overlaps Tracy.
3. **Macros of our own with deferred formatting** (NanoLog/defmt style): ~2–3× faster per
   call, but a second logging vocabulary and dependencies' logs lost unless bridged.

### Logging hot path

1. **Format on the caller into a stack buffer, copy into a lock-free ring of fixed
   entries; a writer thread feeds the sinks** (chosen). No locks, no allocation, bounded
   memory (1 MiB by default); the ring doubles as the crash report's log tail. Cost:
   records can be lost when a flood outruns the writer thread (counted and reported).
2. **Mutex-protected queue** (spdlog async): simpler, but a lock on the render thread
   (CLAUDE.md §5).
3. **Synchronous sinks**: stderr/file writes of several µs on the caller.

### Crash reports

1. **Embark `crash-handler` + `minidumper`** (chosen): out-of-process minidumps, all of
   Windows' crash paths hooked, MIT OR Apache-2.0, maintained.
2. **Crashpad**: C++ build of a large project for the same result.
3. **In-process `MiniDumpWriteDump`**: fails when the heap or stack is corrupt.
4. **Our own monitor**: the edge cases are the hard part and already solved.

### GPU progress

1. **`VK_AMD_buffer_marker`, top and bottom of pipe per render graph pass** (chosen):
   exposed by AMD and NVIDIA; one mechanism.
2. **`VK_NV_device_diagnostic_checkpoints` on NVIDIA**: vendor-specific, same
   information.
3. **Markers per draw/dispatch**: the LunarG Crash Diagnostic Layer does this on demand;
   too costly always-on (~100–200 ns each on NVIDIA).
4. **No markers**: `VK_EXT_device_fault` alone gives an address but not the pass.

## Decision

- **`engine_log`**: the backend of the `log` facade. Records passing the per-target filter
  (`ENGINE_LOG`, e.g. `info,engine_render=debug`; `engine_log::set_filter` at run time)
  are formatted into a 3,480-byte stack buffer and copied into a ring of 256-byte entries
  (seqlock entries of atomic words; a record takes one entry per 232 bytes, up to 15;
  model-checked with loom). A writer thread drains it every 50 ms, at once after a warning
  or an error, to stderr, the debugger's output window (`OutputDebugStringW` while one is
  attached) and a file. Tracy gets every record as a colored message on the calling
  thread. `engine_log::write_recent` reads the ring without locks or allocation for crash
  reports; `engine_log::report` holds sections that subsystems add to them.
- **Vulkan validation messages** go to the log under target `vulkan` (to stderr when no
  logger is installed, as before).
- **`engine_crash`**: `install` starts the monitor (the same executable, told by
  `ENGINE_CRASH_MONITOR`), attaches `crash-handler`, and sets a panic hook that logs the
  panic and produces the same report through a simulated exception. The monitor writes
  `<app>-<time>-<pid>.dmp` and `.txt` into `%LOCALAPPDATA%\ofeliaengine\reports`. Opt-in per
  process: the CLI and the editor install it, tests do not unless they ask.
- **`engine_rhi`**: `VK_EXT_device_fault` (and its vendor binary feature) is enabled when
  present. Every error passing through `RhiError::vk` that is `VK_ERROR_DEVICE_LOST`
  produces, once per device, a `GpuFaultReport` (driver description, addresses, vendor
  records, vendor dump saved as `.bin`, breadcrumbs) — logged as errors, written with the
  log tail to `gpu-fault-<time>-<pid>.txt`, and kept for `engine_rhi::last_gpu_fault`.
  A frame wait that times out (5 s) reports a hang the same way.
- **Breadcrumbs, on by default** where `VK_AMD_buffer_marker` exists
  (`ENGINE_GPU_BREADCRUMBS=0` turns them off): the render graph writes a marker before
  and after every recorded pass with `vkCmdWriteBufferMarkerAMD` (the synchronization2
  variant is a no-op on AMD 25.10), into device-coherent uncached memory on AMD and
  system memory otherwise. Each graph frame slot takes a track; the pass names stay on
  the CPU. The breadcrumbs are also a crash-report section.
  Rationale for the default: ~0.1 µs CPU and ≤ 0.2 µs GPU per pass on NVIDIA, nothing
  measurable on AMD — 2 µs of the editor's 95 µs GPU frame, 6 µs for a 30-pass frame
  (0.04 % of 16.7 ms) — for knowing which pass crashed on a player's machine.

## Consequences

- A GPU crash now says which pass and, on NVIDIA, which address and instruction; the
  vendor dump can go to the vendor. A CPU crash or panic leaves a minidump plus the last
  300 log lines and the GPU markers.
- Code logs with `log::info!` and friends; `eprintln!` in engine crates should move to it
  over time (not done here beyond the Vulkan messages).
- Two processes per engine run (the monitor, idle, ~2 MB working set in Task Manager).
- The breadcrumb cost is paid by every frame by default. If a measured scene ever shows it
  above noise, `ENGINE_GPU_BREADCRUMBS=0` exists and the default can flip for shipping
  builds (ask the team: see the pull request).
- Follow-ups: map fault addresses to named resources
  (`VK_EXT_device_address_binding_report` or the allocator's own records); switch to
  `VK_KHR_device_fault` once ash has it; the editor console reading the ring with
  per-subsystem filters; symbol server upload for minidumps (CLAUDE.md §14); the
  deliberate-fault test has no way yet to lose the device on the Radeon iGPU.
