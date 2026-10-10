# 0007. Presentation and frames in flight

- Status: Accepted
- Date: 2026-09-25

## Context

CLAUDE.md §6–§7 ask for a user-selectable present mode, 1–3 frames in flight, `present_wait`
for queue-depth control later, and no hitches. Swapchain handling has well-known hazards:
reusing the "render finished" semaphore before presentation has consumed it, recreating
the swapchain while images are in use, and suboptimal or out-of-date results.

## Options

- **Frame completion tracking:**
  1. a fence per frame;
  2. one timeline semaphore (chosen).

  A timeline gives one monotonic counter the CPU can wait on for any past frame, and it fits
  async compute and transfer queues later.
- **"Render finished" semaphores:**
  1. one per frame in flight;
  2. one per swapchain image (chosen).

  With one per frame, a semaphore can be re-signaled while an earlier present still waits on
  it; validation reports this on some drivers. Per image is safe, because an image is not
  acquired again until its previous present has finished with it.
- **Resize handling:**
  1. recreate on every `Resized` event, immediately;
  2. mark as dirty and recreate at the start of the next frame, after waiting for the frames
     in flight (chosen). Out-of-date and suboptimal also mark it dirty.

## Decision

- `Frames`: 1–3 slots, each with a transient command pool, a command buffer and an "image
  available" semaphore; one timeline semaphore. `begin` waits for the slot's previous frame,
  and `submit` uses `vkQueueSubmit2`: it waits on image-available at color-attachment output
  and signals the timeline plus the swapchain's semaphore. No per-frame allocation.
- `Swapchain`:
  - one "render finished" semaphore per image;
  - the preferred format is B8G8R8A8/R8G8B8A8 sRGB with the sRGB-nonlinear color space;
  - `min_image_count + 1` images;
  - the requested present mode (FIFO, FIFO relaxed, mailbox, immediate) falls back to FIFO;
  - `recreate` passes the old swapchain as `oldSwapchain`.
- `Surface`: Win32 for windows; headless (`VK_EXT_headless_surface`) for testing the full
  acquire → render → present loop on lavapipe.

## Measurements

- `tests/swapchain.rs`: 60 presented frames with a resize from 128×96 to 64×64, mailbox mode, 2
  frames in flight. It passes on lavapipe with 0 validation errors. GPU drivers usually lack
  headless surfaces, so on ref-1 this test prints a skip note and the windowed example covers
  it.
- `examples/triangle.rs` (Windows) renders to a real window, recreates on resize, and shows FPS
  and the present mode in the title. It is run by hand on ref-1.

## Consequences

- The latency work (§7) builds on this: `present_wait` / `present_id`, the
  `VK_NV_low_latency2` and `VK_AMD_anti_lag` markers, and the frame limiter (`PreciseSleeper`,
  ADR 0004).
- HDR output (scRGB/HDR10 formats and color spaces) is a later extension of the format choice.
- The render graph will own image layout transitions; the example records them by hand.

## Addendum (2026-10-10): presenting across adapters

**Context.** The second core review (`docs/research/core-audit-2.md` §3.4) measured the
Radeon iGPU's tone mapper at 2.62 ms into the swapchain image against 0.86 ms into an
image of its own plus 0.71 ms to copy it, while the RTX showed no difference. On ref-1 the
monitor is connected to the RTX (2026-10-10: the Radeon drives no display), so the
Radeon presents across adapters: its swapchain images are shared with the RTX for scan-out
(D3D12 requires row-major layout for cross-adapter textures; the AMD driver's are slow to
shade into), and a transfer writes them far faster than fragment shading. The same holds
for the common hybrid laptop the other way round: a discrete GPU rendering for a display
on the integrated one.

**Options.**
1. *Always draw apart and copy:* the copy is pure cost where the monitor is the GPU's own
   (the RTX: tone mapper 27.6 µs either way, the copy +19.5 µs).
2. *Detect presenting across adapters, draw apart only then (chosen).* The window's
   monitor (`MonitorFromWindow`, `GetMonitorInfoW`) names its display adapter
   (`D3DKMTOpenAdapterFromGdiDisplayName` gives its LUID), compared with Vulkan's
   `deviceLUID`.
3. *Measure both paths on the first frames and keep the faster:* needs no Windows call,
   but costs frames of measuring on every start and every move of the window.

**Measurements** (ref-1, `flight` with the Sponza pack, 900 frames, GPU µs, median):

| | tone mapper | copy | GPU frame |
|---|---:|---:|---:|
| Radeon, into the swapchain image (across adapters) | 2,557 | – | 27,119 |
| Radeon, apart and copied (now chosen automatically) | 805-819 | 622 | 26,269-26,309 |
| RTX, into the swapchain image (now chosen automatically) | 27.6 | – | 1,936-2,051 |
| RTX, apart and copied | 27.6 | 19.5 | 2,076 |

**Decision.**
- `Adapter::luid` (Vulkan's `deviceLUID`), `Surface::display_adapter_luid` and
  `Surface::presents_across_adapters(adapter)`; `Swapchain::presents_across_adapters()`
  as of its last (re)creation.
- `engine_render::present::PresentTarget`: imports the acquired swapchain image and gives
  the texture a frame draws into: the swapchain image, or across adapters an image of the
  swapchain's format and size, copied into it by `finish` (`import_with` lets a
  measurement decide). The editor, `game_loop`, `flight` (`--present apart|direct`) and
  the windowed examples present through it.
- Test: `the_monitor_belongs_to_one_adapter_and_the_others_present_across` (a hidden
  window; on ref-1 the RTX presents directly, the Radeon across).

**Consequences.**
- A window moved to a monitor of another adapter keeps the old choice until the swapchain
  is recreated (a resize or an out-of-date present recreates it).
- The perf gate still renders offscreen: a windowed present scene is left for the player
  crate (queue item 6f), whose frame loop the gate can then run.
