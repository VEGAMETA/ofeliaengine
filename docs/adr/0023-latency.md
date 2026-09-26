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
