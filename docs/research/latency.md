# Input latency and frame pacing on Vulkan

Checked on 2026-09-26.

CLAUDE.md §7 makes input latency a first-class subsystem:
- NVIDIA Reflex through `VK_NV_low_latency2` (no DLL);
- AMD Anti-Lag 2 through `VK_AMD_anti_lag` (no DLL);
- our own measures on every GPU;
- shared markers for all providers, Tracy and an overlay.

No code was copied. dxvk-nvapi is MIT and was read for the call pattern; the Vulkan
headers are Apache-2.0 / MIT.

## Sources

| Source | Version / date | License | How it was studied |
|---|---|---|---|
| Vulkan-Headers `vulkan_core.h` | main, 2026-09-26 | Apache-2.0 / MIT | read `VK_NV_low_latency2` (sleep mode, sleep, markers, timings, swapchain and submit structures) and `VK_AMD_anti_lag` (features, update, stages, modes; enum values used for our own definitions, since ash 0.38 predates it) |
| dxvk-nvapi | `34cd397` (2026-09-19) | MIT | read `nvapi_vulkan_low_latency_device.cpp` and `layer/vulkan_reflex_layer.cpp`: timeline semaphore for `vkLatencySleepNV`, present ids on submits and presents, 64 frame reports |
| ash 0.38 | 0.38.0+1.3.281 | MIT / Apache-2.0 | wrappers for `low_latency2` and `present_wait` exist; `VK_AMD_anti_lag` does not |
| ref-1 adapter capabilities | this repository's GPU workflow | ours | RTX 4060 Ti: `nv_low_latency2`, `present_wait`; Radeon iGPU: `amd_anti_lag` (no `present_wait`) |

## How the three paths work

### NVIDIA Reflex (`VK_NV_low_latency2`)

- **Setup:**
  - the swapchain is created with `VkSwapchainLatencyCreateInfoNV` (latency mode
    enabled);
  - `vkSetLatencySleepModeNV` sets low-latency mode, boost, and a minimum frame interval
    (Reflex's own frame-rate limiter). The mode belongs to the swapchain: set it again
    after recreating it.
- **Every frame:**
  - `vkLatencySleepNV` asks the driver when to start the frame; the driver signals a
    timeline semaphore at that moment. The application waits on the semaphore, then
    samples input.
  - Markers (`vkSetLatencyMarkerNV`): simulation start/end, render submit start/end,
    present start/end, input sample. Each is tagged with the frame's present id.
  - Queue submissions carry `VkLatencySubmissionPresentIdNV`, and presents carry
    `VkPresentIdKHR`, so the driver can match GPU work to frames.
- **Reporting:** `vkGetLatencyTimingsNV` returns up to 64 frame reports with timestamps
  for every stage, including the GPU's start and end of rendering.

### AMD Anti-Lag 2 (`VK_AMD_anti_lag`)

- **One function,** `vkAntiLagUpdateAMD(mode, maxFPS, presentation info)`:
  - the input stage is called right before input sampling (the driver may sleep there);
  - the present stage is called right before presenting;
  - both carry the same frame index;
  - `maxFPS` is the driver's limiter.
- **No report API.** Measurement is ours.
- **Off must be set explicitly:** the driver keeps its last mode.

### Generic (any GPU)

- **Frame-rate limiter** on the high-resolution waitable timer
  (`engine_platform::time::PreciseSleeper`: timer plus a short spin, microsecond error).
  After a stall it catches up by at most one frame, instead of bursting.
- **Queue bound with `VK_KHR_present_wait`:** before starting frame N, wait until
  present N − 1 − k is on screen. With k = 1, at most one finished frame waits for the
  display, while the CPU never runs further ahead.
- **Our own measures from CLAUDE.md §7:**
  - late input sampling (the frame starts with the wait, then input is read);
  - configurable frames in flight (`engine_rhi::Frames`, 1–3);
  - raw input on its own thread with timestamps (`engine_platform::raw_input`).

## What we take

- **One `Latency` object per swapchain,** with the same calls for every backend:
  `begin_frame`, `mark`, `before_present`, `present_id`, `submit_present_id`.
- **Backend choice:**
  - Reflex when the swapchain was created with `low_latency`;
  - otherwise Anti-Lag 2 when the driver has it;
  - otherwise the generic path.

  `with_backend` forces a choice (comparisons, user settings).
- **Markers are recorded on the CPU for every backend** (a 128-frame history) and fed
  to Tracy (`latency::input_to_present_ms`). Reflex receives them as well.
- **Measurement:** input-to-displayed via `present_wait` where available; on NVIDIA also
  Reflex's input-to-GPU-end.
- **The DLL-based paths come later.** Streamline's `sl.reflex`/`sl.pcl`, needed only
  with DLSS Frame Generation, and XeSS's XeLL will sit behind the same calls with the
  user-supplied DLLs (§6, §7).
