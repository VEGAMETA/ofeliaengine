# Crash reports, GPU fault diagnostics and logging

Research for ADR 0061 (CLAUDE.md §6 "`VK_EXT_device_fault` and markers for diagnosing GPU
crashes", §14 "Crashes", "Logging"). Sources checked 2026-09-30 / 2026-10-01; driver and
extension facts were read on ref-1 with `vulkaninfo` (Vulkan loader 1.4.350) on those
dates.

## The problem

Three kinds of failure leave today's engine silent or nearly so:

1. **GPU faults.** A bad address, a hang or a driver bug ends in `VK_ERROR_DEVICE_LOST`
   from a submit, a wait or a present, which the engine only turned into an error string.
   What the GPU was doing is gone by the time anyone looks.
2. **CPU crashes.** An access violation, a stack overflow or an `abort` in a C/C++
   dependency (Jolt, Box2D, Luau, the drivers) kills the process without a trace; a Rust
   panic prints to a console that the editor may not have.
3. **Logging.** There was none: `eprintln!` in a few places, validation messages printed
   and counted.

## GPU faults

### What the APIs offer

- **`VK_EXT_device_fault`** (Khronos, [spec][fault]): after a device loss,
  `vkGetDeviceFaultInfoEXT` returns a description, **address records** (kind: invalid
  read / write / execute, instruction pointer of the faulting or an unrelated
  instruction; the address and a power-of-two precision), **vendor records** (code,
  data, text) and optionally a **vendor binary dump** that starts with
  `VkDeviceFaultVendorBinaryHeaderVersionOneEXT` and is meant for the vendor's tools.
  Two-call idiom (counts, then data). ref-1: RTX 4060 Ti (616.92) has `deviceFault` and
  `deviceFaultVendorBinary`; the Radeon iGPU (25.10.36) has `deviceFault` only. The NVIDIA
  driver also lists the newer **`VK_KHR_device_fault`** (revision 1, with
  `deviceFaultReportMasked`), which ash 0.38 does not know yet; the EXT version covers
  both GPUs.
- **`VK_AMD_buffer_marker`** ([spec][marker]): `vkCmdWriteBufferMarkerAMD(cmd, stage,
  buffer, offset, u32)` writes a value when the given pipeline stage is reached (top of
  pipe: when the command processor gets there; bottom of pipe: when all earlier work has
  finished). **Both vendors expose it** on ref-1 (NVIDIA too). With
  **`VK_AMD_device_coherent_memory`** the buffer can live in device-coherent, uncached
  memory, so the writes reach memory before a reset rather than staying in GPU caches.
  Finding on ref-1: the synchronization2 variant `vkCmdWriteBufferMarker2AMD` **writes
  nothing** on the AMD 25.10.36 driver (validation silent), the original entry point
  works on both GPUs.
- **`VK_NV_device_diagnostic_checkpoints`** ([spec][checkpoints]): `vkCmdSetCheckpointNV`
  with an opaque pointer, read back per stage with `vkGetQueueCheckpointData2NV` after a
  loss. NVIDIA only; since NVIDIA also has buffer markers, one mechanism serves both
  vendors and checkpoints are not needed.
- **D3D12 DRED** (Microsoft): the same two ideas on D3D12 — automatic breadcrumbs per
  command list and page-fault data. It confirms the shape: markers + fault address.

### Tools and engines

- **NVIDIA Nsight Aftermath** ([site][aftermath]): GPU crash dumps with the faulting
  shader line, plus user markers. Proprietary SDK: ideas only (CLAUDE.md §14 allows it only
  as a user-supplied DLL). `VK_EXT_device_fault`'s vendor binary on NVIDIA is the driver's
  own dump, obtained without the SDK.
- **AMD Radeon GPU Detective** ([repo][rgd], MIT): offline analysis of AMD crash dumps
  captured with the Radeon Developer Panel; reports an execution marker tree per command
  buffer in flight, the in-progress markers and page-fault details (address, resource
  timeline). Takes the markers from the driver's own instrumentation, enabled in the
  panel — not something an engine can do in the field.
- **LunarG Crash Diagnostic Layer** ([repo][cdl], Apache-2.0; installed on ref-1 as
  `VK_LAYER_LUNARG_crash_diagnostic`): a layer that uses exactly these extensions —
  `VK_AMD_buffer_marker` (+ `VK_AMD_device_coherent_memory`) for progress inside command
  buffers, timeline semaphores for submissions, `VK_EXT_device_fault` on loss. It can
  instrument every command (off by default for overhead). Good for deep dives; our own
  per-pass markers are the always-on subset.
- **Unreal Engine** ([docs][ue-gpu]): "RHI breadcrumbs" write the active pass stack with
  `WriteBufferImmediate` (D3D12's buffer-marker equivalent) and print a GPU "callstack" in
  the log on a crash; `r.GPUCrashDebugging=1` / `-gpucrashdebugging` enables Aftermath or
  DRED for more. Unreal Fest 2023 talk "GPU Crash Debugging in Unreal Engine: Tools,
  Techniques, and Best Practices".
- **Unity** writes `crash.dmp`, `error.log` and the player log per crash into a
  `Crashes` folder under the temporary directory and uploads through Cloud Diagnostics;
  GPU-specific data comes from the platform (DRED on D3D12).

### What we take

- Markers **per render graph pass**, top of pipe before the pass and bottom of pipe
  after it, in one small host-visible buffer (device-coherent uncached on AMD, system
  memory otherwise). The value packs a 16-bit frame tag and the pass index; the pass
  names stay on the CPU (a lock-free per-slot table), so the GPU writes 4 bytes.
- `VK_EXT_device_fault` enabled wherever present, queried once per loss, vendor dump
  saved next to the report.
- A loss is noticed where it surfaces: every Vulkan error goes through
  `RhiError::vk`, which sees `VK_ERROR_DEVICE_LOST` from submit, wait, present or acquire.
  A 5 s frame wait that times out reports a hang with the breadcrumbs.

Not taken: per-draw markers (the Crash Diagnostic Layer's job when needed), checkpoints,
Aftermath, `VK_EXT_device_address_binding_report` (mapping fault addresses to named
resources — a follow-up).

### Measurements on ref-1 (ADR 0061 has the tables)

- Markers cost about 100 ns of CPU per pass on NVIDIA (≈ 50 ns per marker call, the
  driver's) and nothing measurable on AMD; on the GPU about 170–215 ns per pass on NVIDIA
  (a marker alone ≈ 100 ns; system memory or VRAM makes no difference) and 50–140 ns on
  AMD (noise level). The editor frame: 95 → 97 µs GPU on the RTX, unchanged on the Radeon.
- A deliberate page fault on the RTX (compute writes to an unmapped address) produced
  `VK_ERROR_DEVICE_LOST` in `vkWaitSemaphores`, the faulting address (invalid write, 4 KiB
  precision), the faulting instruction pointer, a 1.2 MB vendor dump and breadcrumbs
  naming the faulting pass as the one running. The Radeon iGPU drops writes to unmapped
  addresses without losing the device, and a 2^48-iteration loop ended after ~4.4 s
  without a device loss either; its path is tested with the normal-frame test only.

## CPU crashes

### Options

- **Embark `crash-handler` + `minidumper`** ([repo][embark], MIT OR Apache-2.0;
  crash-handler 0.8.1, minidumper 0.11.0, minidump-writer 0.13.0 on crates.io): the
  handler hooks `SetUnhandledExceptionFilter`, a vectored handler for heap corruption,
  the CRT's invalid-parameter and pure-call handlers and `SIGABRT`; the callback gets a
  `CrashContext` (exception pointers, thread). `minidumper` is an IPC client/server over a
  local socket: the monitor process writes the dump with `MiniDumpWriteDump` from outside
  and accepts user messages. Used by Embark's games, Sentry's Rust SDK, and others.
- **Crashpad** (Google, Apache-2.0): the reference out-of-process handler in C++; a large
  build and a C++ dependency for what the Embark crates already provide.
- **In-process `MiniDumpWriteDump`**: simplest, but writing a dump from the crashing
  process fails exactly when the heap or the stack is broken; Microsoft recommends another
  process.
- **Our own Win32 monitor** (event + shared memory + `MiniDumpWriteDump` in a child):
  ~300 lines, but the edge cases (stack overflow guard, CRT handlers, abort) are what the
  Embark crates already handle.

Taken: the Embark crates. The monitor is the application's own executable started with an
environment variable, so nothing extra ships. The context (log tail, report sections, the
panic message) is formatted on the crashing thread into a buffer allocated at install
time and sent before the dump request.

Cost: `install` takes 1.8–2.4 ms on the calling thread (process spawn); the connection
(10–20 ms, the monitor starting up) is made on a thread, and a crash before it finishes
waits for it in the handler. Clean compile of the crates: 16.9 s dev, 8.7 s release on
ref-1 (16 threads).

## Logging

### Options

- **`log` facade** (MIT OR Apache-2.0, 0.4.x, already in the tree through other crates):
  the interface most Rust crates log through. A backend implements one trait; records
  that fail the global max-level check cost one comparison.
- **`tracing`** (MIT): spans and structured fields; heavier per event (subscriber
  dispatch, field visitors) and overlaps Tracy, which the engine uses for timing.
- **Deferred formatting** (NanoLog, Stanford, BSD-2; Quill, MIT, C++; `defmt` in
  embedded Rust): store arguments, format on another thread. Fastest per call (tens of
  ns) but needs compile-time log-site registration or macros of our own — incompatible
  with the `log` facade, which hands a `fmt::Arguments`.
- **spdlog** (MIT, C++): async mode formats on the caller and pushes into a ring with a
  mutex; the model most engine loggers follow.

Taken: `log` as the front end (dependencies' messages arrive for free, no new macro
vocabulary), our own backend formatting on the caller into a stack buffer and copying into
a lock-free ring (seqlock entries, atomic words), a writer thread for stderr / debugger /
file, Tracy messages on the caller. The ring doubles as the crash report's log tail.
Measured: 0.2 ns for a disabled level, 5 ns for a disabled target, 40 ns for a literal
message, 115 ns for a message with an integer, a string and a float, 0 allocations; loom
found (and the ring now avoids) an ordering bug where two writers lapping each other could
interleave an entry's words.

No code was copied from any of the projects above.

[fault]: https://registry.khronos.org/vulkan/specs/latest/man/html/VK_EXT_device_fault.html
[marker]: https://registry.khronos.org/vulkan/specs/latest/man/html/VK_AMD_buffer_marker.html
[checkpoints]: https://registry.khronos.org/vulkan/specs/latest/man/html/VK_NV_device_diagnostic_checkpoints.html
[aftermath]: https://developer.nvidia.com/nsight-aftermath
[rgd]: https://github.com/GPUOpen-Tools/radeon_gpu_detective
[cdl]: https://github.com/LunarG/CrashDiagnosticLayer
[ue-gpu]: https://dev.epicgames.com/documentation/unreal-engine/dealing-with-a-gpu-crash-when-using-unreal-engine
[embark]: https://github.com/EmbarkStudios/crash-handling
