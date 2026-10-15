# 0099. The player crate (`engine_app`) and its window modes

- Status: Accepted (step 1 done)
- Date: 2026-10-15
- Related:
  - [ADR 0023](0023-latency.md): the latency providers;
  - [ADR 0055](0055-pipeline-cache-and-warm-up.md): the pipeline cache;
  - [ADR 0064](0064-gpu-scene-extraction-runtime.md): the runtime's frame;
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - research note `docs/research/player.md`;
  - the second review (`docs/research/core-audit-2.md` §3.7);
  - queue item 8a.

## Context

- **Nothing owns "a game".** Each program wires the device, the swapchain, latency, the
  frame loop, the process set-up and the pipeline cache itself (review §3.7):
  - the golden test bed, `game_loop`, `flight`, `scene_bench`, `stream_bench`,
    `tools/stress`, the frame renderer's tests and the editor.
- **What that left out:**
  - `RenderDefaults::ambient_occlusion` is applied by nothing;
  - the latency providers and the CLAUDE.md §5 process set-up run in the editor only;
  - lights, skinned meshes, probes and the environment are passed by hand;
  - several features are renderer-wide switches, so two views cannot differ.
- **The window.** The owner asked (2026-10-10) for three modes:
  - a window not larger than its monitor's work area;
  - borderless full screen;
  - exclusive full screen (`VK_EXT_full_screen_exclusive`), measured with PresentMon.
  `flight`'s 1920 × 1080 window was larger than the 1080p screen. On NVIDIA it was
  copied through GDI and showed 24 of its 603 frames a second (the research note).

## Options

**The crate.**

1. **Keep wiring each program.** Rejected: the review's drift, features applied by nothing.
2. **A player crate that owns the wiring, with hooks for the game** (chosen).
   - It owns the device, the window and its swapchain, latency, the process, the
     pipeline cache, the scene's streaming, the frame renderer and the runtime.
   - The game supplies start, tick, present and UI, as Wicked Engine's render paths and
     Unreal's game tick do.
3. **Bevy's `App` with plugins and schedules.**
   - The runtime (ADR 0064) already orders a frame for latency, with the job system inside
     it.
   - Plugins would add a layer the engine does not need yet.
   - Bevy's pipelined rendering adds about a frame of latency.

**The full-screen modes** (measured on ref-1, the research note):

1. **Borderless, the driver's default.** NVIDIA copies it through GDI without vsync:
   9.32 (10.47) ms from the frame's start to the screen.
2. **Borderless, stating "disallowed"** (chosen as the default). NVIDIA flips it
   independently through DXGI: 7.90 (7.97) ms. Equal with vsync, and on the Radeon.
3. **Exclusive, application controlled** (offered). NVIDIA grants it with vsync only,
   and holding it changed nothing measured; the Radeon refuses it across adapters.
4. **Godot's 2-pixel oversize.** It keeps drivers from treating the window as full
   screen; stating "disallowed" does that directly.
5. **Display mode changes** (winit's exclusive). Not taken: the frame has a render size
   of its own (ADR 0072), and a mode change is a disruptive transition.

## Decision

A crate `engine_app`, built in four steps, each its own pull request.

### Step 1 (done 2026-10-15): window modes and exclusive full screen

- **`engine_platform::window::WindowMode`:**
  - `Windowed`: a framed window on its monitor (`WindowDesc::monitor`, the primary one by
    default), the whole window within the monitor's work area. A client area too large is
    scaled down with its aspect ratio kept, and the window is centered.
  - `FullScreen`: a popup window covering its monitor.
  - `WindowSystem::set_mode` switches at run time, as Raymond Chen describes: the frame's
    style off and the monitor covered, then the frame on and the placement restored.
  - A full-screen window keeps covering its monitor across DPI and resolution changes.
- **`engine_platform::display`:**
  - `monitors()`, `primary_monitor()`, `window_monitor(hwnd)`, `monitor_info(handle)`;
  - each `Monitor` with its rectangle, work area and DPI.
- **`engine_rhi::FullScreenExclusive`** in `SwapchainDesc`:
  - `Default` (nothing stated), `Allowed`, `Disallowed`, `Exclusive { monitor }`;
  - with the extension: `VK_EXT_full_screen_exclusive` and
    `VK_KHR_get_surface_capabilities2`, enabled where both exist
    (`Device::full_screen_exclusive_enabled`). Without it, every choice is the default.
- **An exclusive swapchain acquires the monitor at every (re)creation.**
  - Refused, it is destroyed and made again from nothing as `Disallowed`
    (`Swapchain::exclusive`, `Swapchain::exclusive_refused`).
  - Made as the refused one's successor instead, NVIDIA kept the GDI copy: 9.35 against
    7.90 ms.
  - Lost later: `VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT`, which
    `RhiError::is_out_of_date` counts with `VK_ERROR_OUT_OF_DATE_KHR`. The frame loop
    recreates the swapchain and tries again.
- **`SwapchainDesc` has a `Default`:** the surface's size, FIFO, sRGB, SDR.
- **`flight`** runs borderless and "disallowed" by default, at the monitor's size
  (1920 × 1080 on ref-1, as before).
  - `--window exclusive|windowed` and `--fse default|allowed|disallowed` select the
    other modes.
  - Its report says which mode it got, and why the monitor was refused.

### Step 2: the crate owns the player's wiring

- **What it owns**, in one `Player`:
  1. **The process** (CLAUDE.md §5): power throttling off, the frame thread on P-cores
     with MMCSS "Games", the crash handler, logging.
  2. **The window and its swapchain:**
     - the mode and monitor from the settings, borderless and "disallowed" by default;
     - resizes, out-of-date and lost swapchains, HDR from the display (ADR 0038);
     - the frame generation presenters (ADR 0091).
  3. **The device and the latency provider:**
     - the adapter by preference (`ENGINE_ADAPTER`);
     - Reflex, Anti-Lag 2 or the limiter (ADR 0023), on by default, with the frame cap
       and the frames in flight from the settings.
  4. **The pipeline cache** (ADR 0055) in `%LOCALAPPDATA%\<game>\`, saved after start-up
     and at exit; the warm-up thread (ADR 0079).
  5. **The scene and the renderer:**
     - the pack's streaming (ADR 0076);
     - the GPU scene, the pools and the material table;
     - the frame renderer with `RenderDefaults::for_device` applied whole;
     - skinning, the environment, the UI layer.
  6. **The runtime** (ADR 0064): the world, the fixed step and extraction.
- **The game's hooks:** a trait, called by the frame loop:
  - `start` once the device, window and scene are up;
  - `tick` at the fixed rate;
  - `present` every rendered frame, after the ticks (the camera from the latest input);
  - `ui` for the frame's 2D and 3D canvases.
- **The frame loop**, in `flight`'s order:
  1. window events;
  2. the latency provider's wait;
  3. the frame slot and the swapchain image;
  4. input sampled;
  5. the runtime's ticks, presentation and interpolation;
  6. animation, streaming, the frame renderer, the UI;
  7. submit and present, with the latency markers.
  - `Player::frame` runs one frame and returns its report, for the benches and tests;
    `Player::run` loops until the window closes.
- **`flight` moves onto it first,** with its numbers compared before and after.

### Step 3: ECS components for lights, skinned meshes and probes

- **Components**, extracted by the runtime with the same change detection as meshes:
  - point and spot lights (with their shadow settings);
  - the sun;
  - skinned meshes, each pose set with its scene instance's interpolated transform, bit
    for bit (`Skinning::set_pose`);
  - reflection probes and the GI volume.
- The game no longer passes any of them by hand.

### Step 4: features per view; the benches and the test bed on the crate

- **Per view:** ambient occlusion, local and contact shadows, reflections, subsurface
  scattering and the prepass become view settings, so split screen and editor viewports
  can differ.
  - Each is made on first use, at no cost where unused, not even in registers (§5).
- **On the crate:** `scene_bench`, `stream_bench`, `tools/stress` and the golden test bed
  move onto `engine_app`. Their scenes and numbers stay comparable: each move is checked
  against the previous binary.

## Consequences

- **A game is a few hooks.** The latency provider, the process set-up and the device
  class's defaults run in every game, not just the editor.
- **`flight` has the presentation games get.** Its frames now flip independently on
  NVIDIA instead of being copied through GDI. Its history before 2026-10-15 is a window
  larger than the screen.
- **Exclusive full screen is offered but never the default.** It is revisited when a
  driver or display shows a measured gain: Granite reports HDR on AMD's Windows driver.
- **Steps 2 to 4** each change many programs. Each lands alone, with before and after
  numbers.
- **Not done in step 1:**
  - the taskbar's full-screen marking (`ITaskbarList2::MarkFullscreenWindow`);
  - variable refresh;
  - a monitor driven by the iGPU: ref-1's second display was inactive;
  - input-to-photon with real input.
