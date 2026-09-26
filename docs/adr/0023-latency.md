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
  - backends: Reflex, Anti-Lag 2, generic (limiter on the precise waitable timer plus
    `present_wait` queue bound);
  - `LatencySettings`: mode (off, on, boost), frame-rate cap, queued presents.
- **The `latency` example** runs GPU-bound frames for every available configuration. It
  prints frame time, input to present call, input to displayed (`present_wait`) and
  Reflex's input to GPU end. It runs in the GPU workflow on NVIDIA and AMD: a debug run
  with validation, and a release run for the numbers.

## Measurements

- **Local:** clippy and builds for Windows and Linux; `engine_rhi` tests on lavapipe
  with validation (the new optional extensions are not present there and stay off).
- **ref-1 numbers** come from the GPU workflow on the PR that implements this ADR, and
  go into `docs/perf/history.md`.

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
