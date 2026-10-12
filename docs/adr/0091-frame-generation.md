# 0091. Frame generation: FSR 3.1 frame interpolation presented by the engine, DLSS FG through Streamline, the UI drawn on every presented frame

- Status: Accepted (2026-10-12) with the results below: FSR FG measured on both GPUs, DLSS
  FG on the RTX (after the owner turned on hardware-accelerated GPU scheduling).
- Date: 2026-10-11
- Builds on:
  - [ADR 0023](0023-latency.md): latency, the limiter, Reflex and Anti-Lag;
  - [ADR 0060](0060-fsr-3-1-upscaling.md): FSR 3.1 from source;
  - [ADR 0063](0063-dlss-through-streamline.md): Streamline with manual hooking;
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - [ADR 0090](0090-brixelizer-gi-reference.md): one FidelityFX SDK library.
- Research: [docs/research/frame-generation.md](../research/frame-generation.md)

## Context

Queue item 3b (`docs/status.md`), the owner's requirements of 2026-10-11:

- **Two paths, out of the box:**
  - FSR FG on any GPU with nothing to install;
  - DLSS FG on RTX GPUs with the user's DLLs (the engine ships none, §6).
- **Off by default**, and hidden where it cannot run.
- **The HUD never interpolated, by default:** FG gets the hudless color, and the UI is
  composited on every presented frame, real and generated.
- **Immersive UI with nothing to mark:**
  - 2D canvases are composited after FG;
  - world-space and camera-attached widgets are drawn after FG on each presented frame,
    with that frame's camera, and depth-tested against the nearest real frame's depth;
  - UI baked into materials is scene content.
- **A presenter interface:** native, FidelityFX FG, Streamline.
- **Latency and pacing:**
  - the limiter caps real frames;
  - latency markers belong to real frames;
  - reports tell real frames from generated ones;
  - no frame is interpolated across a camera cut.
- **Measured on both GPUs:** presented and real frame rates, latency, image quality
  against real frames (FLIP), and the HUD with 2D and 3D UI.

Today every frame loop (`game_loop`, `flight`, the editor) acquires a swapchain image on
its thread, draws the frame into it (or apart and copied, ADR 0007's addendum), draws its
UI into the same image, and presents. One present is one displayed frame.

## Options

### A. How FSR FG's frames reach the screen

1. **AMD's Vulkan frame interpolation swapchain** (`FrameInterpolationSwapchainVK`, the
   SDK's way). It replaces five swapchain functions, and the game renders into its
   replacement images. Against it (research note):
   - It needs **four distinct queues** (game, async compute, present, image acquire).
   - It runs **two `THREAD_PRIORITY_HIGHEST` threads**, one of which busy-waits on
     `QueryPerformanceCounter` until each present's time.
   - It **asserts `VkPresentInfoKHR::pNext == nullptr`**. Without present ids there is no
     `present_wait`, and no Reflex present markers tied to the real frame.
   - It **copies every frame even with FG off**. Leaving it means recreating the swapchain.
   - Its pacing knows nothing of the engine's limiter or latency mode. Its UI callback runs
     on its thread, with a command buffer of its present queue. Its GPU work is invisible
     to the frame graph and the profiler.
2. **AMD's frame interpolation and optical flow components, presented by the engine
   (chosen).**
   - The two components are compute work that the frame graph records like the FSR
     upscaler (ADR 0060).
   - The engine presents with one thread of its own, which copies, times and presents.
   - With FG off nothing changes from today: the frame loop presents directly, with no
     copy and no thread.
   - More code of ours: the pacing must be measured to be even.

### B. How presents are timed (FSR FG)

1. **Busy-waiting** (AMD's). The most precise, but it spins a core during every wait.
2. **A thread sleeping on the high-resolution waitable timer and spinning the learned
   margin** (`engine_platform::time::PreciseSleeper`, already the limiter's; chosen).
   CLAUDE.md §5 asks for waitable timers.
3. **`VK_EXT_present_timing`:** the driver holds a present until a target time, with no
   thread waiting. NVIDIA 616.92 has it (absolute and relative times); AMD 26.8.1 has
   neither it nor `present_wait`. So it can only be an addition on NVIDIA, measured later
   against option 2.
4. **FIFO alone:** both presents queued at once, vsync spacing them. Correct only at
   exactly half the refresh rate, and wrong with VRR or tearing. Rejected.

### C. The UI on generated frames

1. **Hudless only.** FI detects UI pixels and avoids distorting them: AMD warns of
   artifacts on translucent UI. Rejected as the default; it is what the "HUD fix" mods fix.
2. **A UI texture blended onto both frames.** The UI is crisp, but world-space widgets on
   the generated frame sit at the real frame's camera, while the scene sits half a frame
   earlier: they slide against the world.
3. **The UI drawn on every presented frame from a UI layer the game hands to the frame
   renderer (chosen, FSR FG):**
   - screen canvases as they are;
   - world-space and camera-attached widgets with each presented frame's camera (the
     generated frame's interpolated);
   - like AMD's present callback and UE's Slate redraw, but the game draws and ticks
     nothing twice.
4. **DLSS FG** calls the application for no presented frame: it composites the UI buffer
   of the real frame itself. The UI layer is drawn into that buffer with the real frame's
   camera, world-space widgets included; "user interface recomposition" interpolates it
   apart from the scene.
   - The alternative is drawing world-space widgets into the scene with motion vectors.
     It is measured against the UI buffer with a 3D HUD shot (the owner's question about
     text in 3D).
   - The developer sets nothing either way.

### D. Where the code lives

- **One new crate for presentation**, or spread over the existing ones by what each
  already does. Spread (chosen); no new crate:
  - `engine_ffx` builds the two components;
  - `engine_fsr` wraps them as it wraps the upscaler;
  - `engine_latency` paces ("frame pacing, limiter", CLAUDE.md §2);
  - `engine_frame` composes the frame;
  - `engine_render2d` draws the UI;
  - `engine_dlss` holds Streamline.

## Decision

### 1. The presenter (`engine_latency::Presenter`)

It owns the swapchain and has two modes.

- **Direct**, today's path and the only one with FG off.
  - The frame loop acquires, the frame renderer draws into the swapchain image (or apart
    and copied across adapters), and the frame loop presents with the frame's present id.
  - No thread, no copy.
- **Paced**, with FSR FG on.
  - A present thread on a second queue of the graphics family (both GPUs have spare ones
    with present support).
  - MMCSS "Games", on a P-core (§5).
  - The frame graph draws each real frame's images into a ring of present images, two
    frames deep: the real frame R and, when one is generated, the generated frame G.
  - The thread gets them through `engine_core::spsc` (no lock). For each image it:
    1. acquires a swapchain image;
    2. submits a copy that waits for the frame's timeline value;
    3. sleeps until the image's time;
    4. presents.
  - The copy's completion releases the ring slot to the frame two frames later (a timeline
    semaphore of the present queue, waited for on the GPU).
  - The swapchain is touched only by that thread while paced. Recreating it (resize, mode
    change) stops the thread first.

### 2. Pacing (FSR FG)

- **The interval:** the thread waits on the CPU for each real frame's GPU work to finish
  (the frame's timeline value), as AMD's does. From the intervals between finishes it
  keeps a moving average of 10 and their standard deviation σ.
- **The spacing:** `Δ = max(0, avg / 2 − 0.1 σ − 0.1 ms)`.
  - G is presented at `max(finish, last present + Δ)`, R at `G + Δ`.
  - The average starts over after any interval over 100 ms, and at a reset.
- This is AMD's rule (research note). It is kept until measurements on both GPUs argue
  for another.
- **The real frame is shown Δ after the generated one.** That half real frame, plus the
  interpolation's GPU time, is the latency frame generation adds; it is measured (§6).

### 3. The frame with FSR FG

In the frame graph of real frame N, after the scene and the post chain:

1. **The hudless image:** the tone mapper's output (and the filters after it, ADR 0075),
   at the output size, before any UI. With FG on, it is an image of its own, not a
   present image.
2. **Frame interpolation:**
   - prepare: depth and motion vectors at the render size, jitter, motion vector scale,
     camera planes and field of view;
   - optical flow on the hudless image;
   - frame interpolation, from the hudless images N−1 (kept inside the context) and N to G.
3. **The two compositions:**
   - G = the interpolated image + the UI layer, with the camera interpolated half-way
     between N−1 and N;
   - R = hudless N + the UI layer, with camera N.
   - World-space and camera-attached widgets are depth-tested against frame N's depth at
     the render size: the nearest real frame, the owner's rule. The interpolated frame's
     own depth estimate inside FI is aliased with FI's other temporaries, so it cannot be
     read afterwards.

Then:

- **FG forces motion vectors on** (the depth prepass writes them, ADR 0043's addendum).
- **Each pass is a graph pass** with its own timestamp, on the graphics queue. Async
  compute only if it measures faster (ADR 0065).
- **Permutations:** low-resolution motion vectors when upscaling; depth not inverted (the
  engine's depth runs from 0 near to 1 far). The optical flow's HDR input only with HDR
  output; the transfer function follows `OutputColor`.

### 4. The UI layer (`engine_frame`, drawn with `engine_render2d`)

- **The game hands the frame renderer a list of prepared canvases,** each with a placement:
  - **screen:** pixels;
  - **world:** `world_from_canvas`;
  - **camera:** `view_from_canvas`.
- **They are drawn in order on every presented frame, after FG:**
  - screen canvases with the output's orthographic view;
  - world and camera ones with the presented frame's camera, through a depth-compared
    variant of the 2D pipeline. It has its own entry points, so screen canvases pay
    nothing (the register rule).
- **With FG off, the layer is drawn once** onto the target: the same picture as a game
  drawing its UI itself.
- **What a game draws into the scene's targets itself is scene content and is
  interpolated.** UI shown on in-game screens through materials is scene content.
- **The editor keeps drawing its UI directly** and never turns FG on.

### 5. Cuts, resets and the first frames

- **What resets the interpolation:**
  - `ViewCamera::cut`;
  - a change of output size, upscaler or antialiasing (the view's `Resets`);
  - the first frame;
  - FG being turned on.
- **On a reset no frame is generated:** R is presented as soon as it is ready, and the
  pacing average starts over.
- **Cuts the game does not mark** are caught by the optical flow's scene change detection
  inside FI.

### 6. Latency (`engine_latency`)

- **The limiter caps real frames.**
  - With 2× FG and VRR, the cap is (the refresh rate − the VRR margin) / 2.
  - With FIFO, the real rate is at most half the refresh rate.
- **The markers are for real frames:**
  - Reflex (`VK_NV_low_latency2`): the sleep and the simulation and render markers on the
    frame loop's thread. The present markers on the present thread around R's present,
    with the frame's present id (the swapchain may be used from both threads for these
    calls, by the spec). G is presented without an id.
  - Anti-Lag 2: the input stage at the frame's start, and the present stage for R only.
- **Frame history:** the presented frames' records (kind, present time, display time
  where `present_wait` or `VK_EXT_present_timing` gives it) go into the latency history
  next to the real frames'.

### 7. DLSS FG (`engine_dlss`)

- **Streamline at start-up.** When `sl.dlss_g.dll` and its dependencies are present and
  signed, Streamline is initialized with DLSS-G, Reflex and PCL before the device. Their
  extra queues, extensions and features (`slGetFeatureRequirements`) are created with the
  device, including the optical flow queue.
- **The swapchain through Streamline.** With DLSS FG chosen, the swapchain is created
  through Streamline's hooks: the eight functions of manual hooking, extending ADR 0063's
  present-only hook. Streamline paces; the presenter stays in Direct mode with
  Streamline's functions.
  - As built: `engine_rhi::VulkanHooks` (the interposer's `vkGetInstanceProcAddr`) gives
    `Surface::from_win32_hooked`, `Swapchain::new_hooked` and `device_wait_idle`; the
    queues Streamline asks for come after the engine's (`Device::with_extras`,
    `extra_queue_start`).
  - **Optical flow in Streamline's interop mode,** not on a native queue: the native mode
    needs a queue of the optical flow family (`VK_NV_optical_flow` and its feature) and is
    left for when DLSS FG runs on ref-1, measured against interop.
- **Toggling.** Turning DLSS FG on or off recreates the swapchain (Streamline copies
  while loaded), with `eRetainResourcesWhenOff` for short pauses.
- **Each frame:**
  - tags for depth, motion vectors, hudless and the UI layer drawn into a premultiplied
    UI color-and-alpha image (8 bits of alpha at least, also with HDR);
  - the backbuffer = hudless + UI;
  - common constants once per frame with the frame id;
  - "user interface recomposition" on.
  - As built (`engine_dlss::DlssFrameGeneration`, `ViewFrame::dlss_generation_input`):
    the frame is drawn apart (the hudless image), copied into the swapchain image, and the
    UI drawn over the copy and into its own cleared layer; the four images are tagged
    "valid only now" from a pass after the UI's (Streamline copies them: the graph reuses
    transient memory before the present).
  - The constants in Streamline's conventions, those of its Direct3D origin: row vectors,
    view space looking down +Z, clip space with y up (`sl_matrix_helpers.h`). The engine's
    matrices are converted (a unit test checks the reprojection). That Streamline wants
    them so on Vulkan too is to confirm on the first run.
- **Latency:** `engine_latency` takes a Streamline backend (`slReflexSleep`,
  `slPCLSetMarker`) while DLSS-G is loaded, as the guide requires.
  - As built: `engine_latency::ExternalLatency` and `Latency::with_external`, implemented
    by `engine_dlss::StreamlineLatency`. Reflex stays at least "on" while DLSS-G is loaded:
    it reports `eFailReflexNotDetectedAtRuntime` otherwise.
- **Requirements Streamline checks at start-up:** Windows' hardware-accelerated GPU
  scheduling (`slIsFeatureSupported` returns `eErrorOSDisabledHWS` without it),
  `Streamline::frame_generation_error` says which.
- **Generated frames:** counted from `slDLSSGGetState`.

### 8. Settings and availability

- **The setting:** the frame renderer offers `FrameGeneration::{Off, Fsr, Dlss}`.
  - **Off by default.**
  - `check()` says why a mode cannot run, as for antialiasing (ADR 0075). Reasons:
    - missing device features;
    - DLSS FG's DLLs or OS requirements missing;
    - more than one 3D view in the frame (split screen: FG stays off in this version).
- **Where the option appears:** in every game, with nothing for its developer to do;
  hidden where `check()` fails.

## Measurement protocol

On ref-1, both GPUs, release builds, the Sponza flight at 1080p (and 1440p on the RTX),
TAA and FSR Quality:

1. **GPU time per pass:** prepare, optical flow, interpolation, both compositions, and the
   present thread's copies.
2. **Rates and pacing:**
   - presented and real frame rates;
   - the spacing of presents (present thread timestamps);
   - display times through `present_wait` / `VK_EXT_present_timing` on NVIDIA, and
     PresentMon's `MsBetweenDisplayChange` on both GPUs if it runs on ref-1;
   - at a capped and an uncapped real rate, FIFO and immediate.
3. **Latency:** input sample → R's present, and → displayed where measurable. FG off
   against on, at the same real rate.
4. **Image quality:**
   - the flight rendered deterministically at twice the rate. Even frames are given as
     real frames, odd frames are the truth for the generated ones;
   - FLIP of generated against truth, over the image and in the HUD's regions: a 2D HUD,
     a world-space panel with text, a camera-attached panel;
   - FSR FG, and DLSS FG with world-space widgets in the UI buffer against in the scene.
5. **Memory:** the components' contexts and the present images.
6. **A golden shot:** a generated frame with 2D and 3D UI, per vendor.

## Results (2026-10-12)

Ref-1, release builds, `flight` through Sponza at 1080p with the device class's defaults
(the RTX: TAAU at native size; the Radeon: TAAU Quality, presenting across adapters), 2000
movers, 64 characters, 7 local lights, the UI layer on.

**FSR FG, rates and pacing:**

| GPU, present mode, cap | FG off: real/s | FG on: real/s, presented/s | interpolation, ms | latency added, ms (median, p99) |
|---|---:|---:|---:|---:|
| RTX 4060 Ti, immediate, none | 526 | 211.5 / 423.0 | 1.95 | 2.33 / 4.79 |
| RTX 4060 Ti, immediate, 60 real | 60 | 60.0 / 120.0 | 2.62 | 8.29 / 9.19 |
| RTX 4060 Ti, immediate, 120 real | 120 | 120.1 / 240.1 | 1.84 | 4.12 / 4.54 |
| RTX 4060 Ti, FIFO at 240 Hz | 240 | 120.0 / 240.0 | 1.75 | 4.12 / 4.62 |
| Radeon iGPU (2 CU), immediate, none | 36.3 | 12.7 / 25.4 | 49.4 | 43.2 / 82.1 |

- **Pacing is exact:** the generated frame's present to the real one's equals the spacing
  to 0.000 ms at the median and the 99th percentile in every run (0.012 ms at most).
- **Latency added** is the real frame's wait from its GPU work done to its present: half
  a real interval, as designed (§2). Input to photon was not measured (PresentMon is not
  installed on ref-1, no LDAT).
- **When FG pays:** only when a real frame costs well more than the interpolation. In
  `flight` uncapped the RTX's frame costs 1.6 ms of GPU against 1.95 ms of interpolation,
  and FG shows fewer frames than it draws without it. Capped at 60 real it doubles them.
  The Radeon iGPU interpolates 1080p in 49 ms and should never offer it.
- **The UI layer** costs 2–3 µs per canvas on the RTX ("2d", "2d in scene").
- **Memory:** the RTX's device-local usage +163 MiB (612 → 775: the contexts and the two
  pairs of present images), the C runtime heap +33 MiB. On the Radeon the GPU heap grows
  +50 MiB, but the process commits +850 MiB of driver-mapped memory (two new 375 MiB
  allocations): unexplained, to investigate.

**FSR FG, image quality** (`tests/golden`, `framegen::generated_frames_are_nearer_the_truth_than_real_ones`):
Sponza at 640 × 360 without antialiasing, the camera moving 5.7 cm a frame, the UI layer
on. The frame generated between real frames 7 and 8 against the frame rendered at that
moment, and against real frame 8 (what a display shows there without FG), mean FLIP:

| where | RTX: generated | RTX: real held | Radeon: generated | Radeon: real held |
|---|---:|---:|---:|---:|
| the image | 0.0253 | 0.0904 | 0.0258 | 0.0910 |
| HUD bar | 0.0115 | 0.0615 | 0.0129 | 0.0641 |
| HUD map frame | 0.0089 | 0.0192 | 0.0091 | 0.0195 |
| panel in the world | 0.0448 | 0.0922 | 0.0437 | 0.0907 |
| panel on the camera | 0.0108 | 0.0474 | 0.0111 | 0.0475 |

The world panel's error is at its edge where a column crosses it: it is depth-tested
against the nearest real frame's depth (§3), which puts the column's edge half a frame
off. The golden shot `sponza_fg` (TAAU at 67 %, the camera moving) renders a generated
frame with the three kinds of UI, the same bytes on every run on both GPUs.

**DLSS FG** (RTX 4060 Ti, `flight --fg dlss`, the same flight; 2026-10-12, after the owner
turned on hardware-accelerated GPU scheduling, `HwSchMode = 2`):

| cap | real/s | shown/s | shown per real frame | status |
|---|---:|---:|---:|---|
| none | 213.3 | 426.6 | 2.00 | ok |
| 60 real | 60.1 | 120.1 | 2.00 | ok |
| 120 real | 120.1 | 240.2 | 2.00 | ok |

- The inputs' pass (Streamline's copies of the four images) costs 0.115 ms on the frame's
  queue; the interpolation runs on Streamline's queues, outside the engine's timings.
- **Two bugs found by the first run:** the frame graph culled the tagging pass (it only
  reads; it now has side effects) and with it the UI layer's passes, so Streamline got no
  inputs and showed one frame per real one with status "ok"; and `flight` dropped its
  `Arc<Device>` before Streamline, whose shutdown then crashed on the destroyed device.
- **Generated frames alone** (`eShowOnlyInterpolatedFrame`, `FLIGHT_DLSS_DEBUG=generated`,
  the camera moving four times as far a frame) show clean edges on the scene and the
  movers. A capture with the constants in Vulkan's clip space did not show a difference
  either: DLSS-G leans on the motion vectors and its optical flow. The Direct3D
  convention stays, as Streamline's headers define it.
- **Without GPU scheduling** the option cannot run; `Streamline::frame_generation_hint`
  gives the menu a sentence for the player (turn it on and restart; or update the driver
  or Windows), and FSR FG stays offered. The owner saw some applications flicker or show
  stale frames with the scheduling on (ref-1 has two GPUs), so the engine never asks for
  it beyond that hint.

**The paced presenter's deadlock** (found in `flight`, fixed before these runs): a frame's
submission waited on the GPU for a copy count whose signal the present thread had not
submitted yet, and NVIDIA's driver held the present thread's `vkQueuePresentKHR` behind it
for good. The thread now submits both copies before presenting, and `next_images` returns
once the copies the frame waits for are submitted.

## Consequences

- **What a game does differently:** it hands the frame renderer its UI as a layer instead
  of drawing it into the target. The examples move to it; the editor does not.
- **With FG off nothing changes:** no thread, no copy, no extra image. The second
  graphics queue is created but idle.
- **With FSR FG on, the costs:**
  - one more thread;
  - two present images per frame in flight;
  - a copy per presented frame (19.5 µs on the RTX in `flight`'s window, ADR 0007's
    addendum; measured again at 1080p);
  - the components' memory (AMD: 57–129 MB at 1080p);
  - motion vectors (+0.5 ms on the iGPU);
  - the UI drawn twice per real frame.
- **Split screen** has no FG in this version.
- **Later:**
  - `VK_EXT_present_timing` pacing on NVIDIA;
  - async compute for the interpolation;
  - multi-frame generation (FSR 3.1 has one frame; DLSS MFG needs RTX 50);
  - our own extrapolation (research note, path 3).
- **To revisit:**
  - the pacing rule, if the measurements show uneven spacing;
  - world-space widgets under DLSS FG, after the 3D HUD measurement.
