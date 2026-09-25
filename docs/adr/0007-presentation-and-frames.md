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
