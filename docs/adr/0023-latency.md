# 0023. Latency: Reflex, Anti-Lag 2 and a generic path behind one interface

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/latency.md](../research/latency.md)

## Context

CLAUDE.md §7:
- Reflex through `VK_NV_low_latency2`;
- Anti-Lag 2 through `VK_AMD_anti_lag`;
- our own measures on every GPU;
- shared markers.

It also asks to minimize and measure input-to-photon at every milestone. ref-1 has both
vendors' extensions, so both paths run in CI.

## Options

1. **Vendor SDKs** (Reflex SDK / Streamline, AMD Anti-Lag 2 SDK). Need DLLs or
   closed-source libraries. The engine does not ship them (§0.2).
2. **The driver extensions directly, plus our own path** (chosen). No DLLs.
   Streamline's Reflex joins later only where DLSS Frame Generation needs it.

## Decision

- **`engine_rhi`:**
  - optional device extensions `VK_KHR_present_id`, `VK_KHR_present_wait`,
    `VK_NV_low_latency2`, `VK_AMD_anti_lag`, enabled when supported and presenting;
    features are queried first (`Device::latency_extensions`);
  - `SwapchainDesc::low_latency` for Reflex;
  - `Swapchain::present_with_id`, `wait_for_present`, `raw`;
  - `Frames::submit_tagged`: present id on submissions.
  - The Anti-Lag structures are defined by hand, because ash 0.38 predates them.
- **`engine_latency::Latency`**, one per swapchain:
  - `begin_frame` (sleep before input sampling), `mark` (shared markers),
    `before_present`, `present_id`, `submit_present_id`, `history`, `nv_reports`;
  - backends: Reflex, Anti-Lag 2, generic (limiter on the precise waitable timer plus a
    queue bound: `present_wait` when the device has it, otherwise waiting for the GPU to
    finish the frame `max_queued_presents` behind on the frames' timeline semaphore);
  - present ids continue from the swapchain's last one (`Swapchain::last_present_id`), so
    several `Latency` objects in turn on one swapchain stay valid;
  - `LatencySettings`: mode (off, on, boost), frame-rate cap, queued presents.
- **The `latency` example** runs GPU-bound frames for every available configuration. It
  prints frame time, input to present call, input to displayed (`present_wait`) and
  Reflex's input to GPU end. It runs in the GPU workflow on NVIDIA and AMD: a debug run
  with validation, and a release run for the numbers.

## Measurements

**Local:** clippy and builds for Windows and Linux; `engine_rhi` tests on lavapipe with
validation (the new optional extensions are not present there and stay off).

**ref-1, release build.** The `latency` example (GPU workflow on PR #20, commit
`20c98c8`) runs 300 frames per configuration, presents with `Immediate` at 1280×720 and
loads the GPU with 32 MiB buffer fills. It reports medians in ms:

| GPU (load) | Configuration | Frame | Input → present call | Input → displayed | Input → GPU end (Reflex) |
|---|---|---:|---:|---:|---:|
| RTX 4060 Ti (48 fills) | off | 5.52 | 4.29 | 15.63 | — |
| | Reflex on | 5.61 | 0.41 | **11.79** | 13.74 |
| | Reflex boost | 5.61 | 0.42 | 11.80 | 13.61 |
| | generic (`present_wait`, 1 queued) | 5.51 | 0.91 | 12.26 | — |
| Radeon iGPU (12 fills) | off | 6.55 | 6.55 | — | — |
| | Anti-Lag 2 on | 6.55 | 6.55 | — | — |
| | generic (1 queued) | 6.55 | 6.55 | — | — |

Findings:
- **On NVIDIA,** Reflex cuts input-to-display by 3.8 ms (−25 %) at the same frame time.
  The generic `present_wait` path gets most of that (−3.4 ms) on any GPU that has the
  extension.
- **On the Radeon iGPU,** Anti-Lag 2 changes nothing measurable. The driver exposes
  `VK_AMD_anti_lag`, but its effect may be limited to discrete Radeons; this needs a
  discrete RDNA card to verify.
- **The GPU-timeline fallback works where `present_wait` is missing.** On the iGPU
  (commit `7759e54`, 12 fills) the generic path now waits on the frames' timeline
  semaphore: input → present call drops from 6.55 ms to **0.52 ms** at the same 6.55 ms
  frame time. Before, that GPU had no `present_wait` and the generic path bounded
  nothing.
- **No display time without `present_wait` on AMD.** "Input → displayed" needs
  `present_wait`, which the AMD driver does not expose; PresentMon covers that case.

## Consequences

- **Games call four functions per frame.** The frame loop helpers will do it for them
  once the runtime loop exists.
- **Follow-ups:**
  - the in-game latency overlay (§7) reading `history` and `nv_reports`;
  - PresentMon and hardware (LDAT/OSLTT) measurements at milestones;
  - Streamline `sl.reflex`/`sl.pcl` with DLSS Frame Generation;
  - XeLL through the XeSS DLL;
  - `VK_EXT_present_timing`, when drivers ship it, for exact display times instead of
    polling `present_wait`.

## Addendum (2026-10-16): Reflex's pacing measured (core audit 3, §3.4)

The third review found the RTX's median frame interval 1.92–1.95 ms with latency control
on, against 1.78 ms off, and a 45 % wider tail (99th percentile 3.27–3.36 against
2.27 ms), in a window. It asked for borderless and exclusive full screen, and the sleep
checked against `VK_NV_low_latency2`'s recommendations.

**Against the extension.** The frame loop sleeps before input (`vkLatencySleepNV`, then a
wait for its timeline semaphore), sends the seven markers with the frame's present id,
and presents with `VkPresentIdKHR`. The frame's own submission carried
`VkLatencySubmissionPresentIdNV`, but the graph's own did not: async compute's segments
and the empty batch that waits for them. Now they do (`FrameGraph::tag_submissions`).

**Measured** on ref-1 (RTX 4060 Ti, driver 616.92), release. The flight: Intel's Sponza,
2000 movers, 64 characters, a 1797 × 1011 window, immediate presentation, 3000 frames;
medians. Input to GPU done comes from `flight --input-latency`: a thread waits on the
frames' timeline, on the CPU's clock.

| GPU frame (setting) | Reflex on: interval / sleep | off: interval | input to GPU done, on / off |
|---|---|---:|---|
| 0.95 ms (`--quality quality`) | 1.98 / 1.50 ms | 1.30 ms | 1.50 / 2.59 ms |
| 1.13 ms (`balanced`) | 1.25 / 0.03 ms (99th 2.64, worst 13.3) | 1.22 ms | — |
| 1.44 ms (native) | 2.01 / 1.53 ms | 1.83 ms | 1.97 / 3.63 ms |
| 2.01 ms (native, `--gi`) | 2.05 / 1.56 ms | 2.08 ms | 3.10 / 4.15 ms |
| 2.52 ms (`--gi --ssr --probes`) | 2.56 / 1.86 ms | 2.58 ms | — |

- **The driver's sleep is never shorter than about 1.5 ms** when it sleeps, and comes in
  0.5 ms steps. At the flight's 1.44 ms GPU frame, 78 % of the intervals are 1.9–2.1 ms,
  11 % 2.4–2.6 ms and 1 % 3.0 ms: those are the wider tail. A frame there takes the CPU's
  0.47 ms plus the 1.5 ms sleep, and the GPU idles 28 %. With Reflex's correct reports
  (below), the sleep ends 82 µs after the previous frame's GPU work.
  - Where the GPU frame is 2 ms or more, Reflex runs at the GPU's rate.
  - Below about 1.2 ms the driver mostly stops sleeping. The frame slot then bounds the
    CPU, and one frame in eight sleeps 2 ms.
- **Not ours:** the same with Boost, without async compute, drawn apart, with 3 or 5
  swapchain images, with the process's timer at 0.5 or 1 ms, with untagged submissions,
  and with the overlays' layers off. Borderless full screen behaves the same (2.01 against
  1.93 ms off). A cap of 400 FPS gives 2.50 ms exactly.
- **Latency:** Reflex cuts input to GPU done by 1.65 ms (46 %) at the flight's rate, for
  9 % of its frame rate, and by 1.05 ms (25 %) at 2 ms GPU frames, for nothing. At
  0.95 ms GPU frames it cuts 1.09 ms (42 %) for a third of the frame rate. **Reflex stays
  on by default:** games run far below 500 FPS, where it costs nothing.
- **Reflex's own reports are a frame late with tagged submissions.** With the present id
  on the frame's submissions, the driver reports a frame's OS-queue and GPU times at the
  next frame's submission: its OS queue starts 50 µs before the next frame's
  `RENDERSUBMIT_END` (spread 19 µs). Untagged, the times follow the frame's own
  submission (+51 µs), and its input to GPU end is 1.94 ms, as the timeline shows.
  - The GPU does not wait. Waiting for the frame's timeline right after its present takes
    1.5 ms, its GPU time, tagged or not.
  - So `NvFrameReport::input_to_gpu_end_us` reads about one interval high when tagged:
    3.97 ms here. That explains the measurement above, where Reflex's input to GPU end
    (13.74 ms) exceeded input to displayed (11.79 ms).
  - The tags stay: the extension asks for them, and the pacing is the same either way.
- **Late wakes:** about one frame in 10,000–20,000, the driver signals 15–43 ms late
  while the GPU is idle. In two runs, the previous frame's GPU work was done 5.8 ms in,
  and the next frame woke at 35.2 and 47.9 ms.
  - The wait for the driver's signal is now cut at three frame intervals and a
    millisecond, within 4–200 ms (`Latency::sleeps_cut` counts the cut waits). Here that
    is 7 ms instead of up to 43; the frame slot still bounds the CPU.
- **Exclusive full screen** was not measured. From this session the flight's window
  cannot come to the front, and its runs hung without drawing. Reproducing that takes
  the monitor, which is the owner's call.

**Tools:**
- the flight's CPU table splits latency control's sleep from the wait for the frame
  slot;
- `--input-latency` times input to GPU done under any backend;
- `--nv-csv` writes every frame's stages with Reflex's report.

**Not done:**
- **A pacer of the engine's own** for frames shorter than the driver's floor. It would
  sleep on the precise timer until the GPU is predicted free, from the engine's own GPU
  times. The floor costs only above about 500 FPS, so this waits for a game that needs it.
- **The exclusive full-screen hang** in a session that cannot take the foreground.
