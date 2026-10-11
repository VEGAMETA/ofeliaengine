# Frame generation

What the engine must provide so frame generation (FG) can be plugged in on Vulkan, which
implementations exist, and how they present, pace and treat the UI. The decision is
[ADR 0091](../adr/0091-frame-generation.md).

The requirements (first part) were fixed on 2026-09-26. The implementations were studied
on 2026-10-11 (second part).

## Sources

| Source | Version | License | Finding |
|---|---|---|---|
| AMD FSR SDK ("Redstone") | v2.3.0, `60f4ea8` | MIT | Upscaler 3.1.5, FSR FG 3.1.6, FG swapchain 3.1.7 (described as a DXGI replacement), ML upscaler 4.1.1, ML FG 4.0.1. The readme says: *"Vulkan is currently not supported in SDK"* (checked again 2026-10-11). |
| AMD FidelityFX SDK | v1.1.4, `c6efa6b` | MIT | Frame interpolation 1.1.3, optical flow 1.1.2, and a Vulkan frame interpolation swapchain 1.1.3 (`sdk/src/backends/vk/FrameInterpolationSwapchain/`), besides the FSR 3.1.4 upscaler the engine builds already (ADR 0060). Read in full for this note. |
| NVIDIA Streamline | v2.14.1, `2122257` (2026-09-08) | MIT (SDK); DLSS binaries proprietary, user-supplied | DLSS Frame Generation on Vulkan (`docs/ProgrammingGuideDLSS_G.md`, `ProgrammingGuideManualHooking.md`, `ProgrammingGuideReflex.md`, `ProgrammingGuidePCL.md`). Inputs: depth, motion vectors, **hudless color**, **UI alpha or UI color + alpha (premultiplied)**, backbuffer subrect, optional distortion field. |
| AMD's FSR 3 plugin for Unreal Engine | 3.1.4 guide (gpuopen.com/learn/ue-fsr3, read 2026-10-11) | studied as a product, through its guide | Two UI modes: the UI redrawn on generated and real frames, or extracted by comparing the frame before and after the UI. |
| D. Andreev, "Real-time frame rate up-conversion for video games" | SIGGRAPH 2010 talk (LucasArts; slides at andcoder.com/rtfrucvg_html_slides) | — | 30 → 60 fps on Xbox 360 and PS3 with a flip thread between vblanks; the HUD left unsolved. |
| OpenXR composition layers (Khronos), Oculus ATW/ASW | OpenXR 1.0+ | open standard | World-locked UI quads composited per displayed frame with that frame's pose, apart from the reprojected scene. |
| Lossless Scaling FG / lsfg-vk | — | lsfg-vk: CC BY-NC-ND 4.0 since 2026-08-27 (not open source) | Not read. |
| Intel XeSS | not checked again | — | XeSS-FG was DX12-only when last known; it stays out (§6). |

No code was taken from a GPL or non-commercial source. The FidelityFX SDK and Streamline
are MIT.

## Part 1: requirements (2026-09-26, with the owner's of 2026-10-11)

### Paths for the engine

1. **FSR 3.1 FG from FidelityFX SDK 1.1.4, built from source (MIT).**
   - Works on every vendor; the default FG on Vulkan.
   - The 2.x SDK line dropped Vulkan, so we stay on 1.1.4 for Vulkan. We re-check every
     release, as for FSR 4 upscaling (§6).
2. **DLSS FG through Streamline.**
   - RTX GPUs, with user-supplied DLLs (`sl.dlss_g.dll` and the rest). The engine ships
     none of them.
   - With DLSS FG active, Reflex must go through Streamline (`sl.reflex` / `sl.pcl`), not
     `VK_NV_low_latency2` directly (ADR 0023, §7).
3. **Later, to research:** our own vendor-neutral extrapolation / frame warp in the spirit
   of VR asynchronous reprojection and Reflex 2 "Frame Warp". It reduces latency rather
   than adding frames.

### What the engine must provide

1. **Hudless color:** the scene after post-processing and before any UI, at backbuffer
   size, in the same color space as the final image.
2. **UI in its own target**, or as data the engine draws:
   - premultiplied RGBA or alpha only, at backbuffer size, composited as
     `final = ui.rgb + (1 − ui.a) · hudless.rgb`;
   - the UI never drawn straight into the scene color.
3. **Motion vectors and depth, shared with the upscalers:**
   - per-pixel motion including camera motion;
   - depth with its convention (reversed Z, near/far);
   - jitter, camera matrices (current and previous), exposure.
   - **Camera cut / reset flag** when the view jumps (teleport, cut scene), so no frame
     is interpolated across a cut.
4. **A pluggable present path.** The frame loop must not assume that one present is one
   displayed frame.
5. **FG-aware latency and pacing** (`engine_latency`):
   - the limiter caps *real* frames (half the target rate with 2× FG);
   - Anti-Lag 2 and Reflex markers are for real frames;
   - latency reports tell real frames from generated ones.
6. **Settings and fallbacks:**
   - FG off by default in menus and when the base frame rate is too low (FG adds latency
     below roughly 60 real fps). The owner, 2026-10-11: out of the box, and off by
     default;
   - the option is hidden when the DLLs or device features are missing, with no errors
     (§6).

### The owner's requirements (2026-10-11)

Queue item 3b in `docs/status.md`: right after GI, off by default.

- **Out of the box, both FSR FG and DLSS FG.** The game developer does nothing to offer
  it. FSR FG is built from source and runs on any GPU. DLSS FG needs the user's DLLs, as
  DLSS does.
- **The HUD is never interpolated, by default.** In many shipped FSR FG games the HUD
  smears, and players install "HUD fix" mods. Here the frame renderer always gives FG the
  hudless color and composites the UI on every presented frame.
- **Immersive UI with nothing to mark.** A developer may make the HUD diegetic, in the
  world and 3D, and still sets nothing. Since every piece of UI comes through the engine,
  the engine sorts it:
  - **2D canvases:** composited after FG.
  - **World-space and camera-attached widgets** (a helmet visor, a cockpit, a gun's ammo
    counter): drawn after FG on each presented frame, with that frame's camera
    (interpolated on generated frames) and a depth test against the nearest real frame's
    depth.
  - **UI baked into materials:** scene content, interpolated with its motion vectors.
- **Researched below:** calling the engine per presented frame (FSR FG's swapchain does;
  DLSS FG does not), what happens to world-space UI on DLSS FG's generated frames, and
  text in 3D.

## Part 2: implementations (studied 2026-10-11)

### AMD FidelityFX SDK 1.1.4: frame interpolation and optical flow

The two components only compute images; presentation is separate.

- **Optical flow** (`ffx_opticalflow.h`, 7 passes). From the scene color alone: a luma
  pyramid, a scene change detector (histograms of 9 screen sections against the previous
  frame), then 7 iterations of block matching (8×8 blocks, SAD over a 24×24 window, a 3×3
  median filter, 2× upscaling of the estimates). Outputs: one `R16G16_SINT` vector per 8×8
  block, and the scene change flag. Up to 512 pixels of motion. 26–28 MB at 4K. One
  permutation axis: HDR input.
- **Frame interpolation** (`ffx_frameinterpolation.h`, 11 passes):
  1. *prepare* (`ffxFrameInterpolationPrepare`, for engines without the FSR upscaler):
     dilates depth and motion vectors and reconstructs the previous frame's depth, into
     three images the application allocates;
  2. *setup*, then the interpolated frame's depth (depth pushed half a motion vector);
  3. the game's motion vectors scattered into the interpolated frame's grid, with a
     priority (primary/secondary, distance, color similarity) and atomics, then a pyramid
     to fill the holes;
  4. the optical flow vectors, the same way;
  5. two disocclusion masks (towards the previous and the current frame);
  6. the interpolation, blending game-vector and optical-flow results by how well each
     matches the colors; the first frame after a reset copies the current frame;
  7. an inpainting pyramid and the inpainting of holes.

  Inputs per dispatch: the current back buffer, or the hudless color when one is given
  (the previous interpolation source is kept inside the context); depth and motion at the
  render size; camera near, far, field of view and units; the frame time; `reset`; the
  transfer function (sRGB, PQ, scRGB) and luminance range for HDR. The output needs an
  alpha channel (intermediate data). `frameID` must grow by exactly one per frame, or the
  context resets. Permutation axes: low-resolution motion vectors, jittered motion
  vectors, inverted depth.
- **Memory** (AMD's table, DX12, RX 7900 XTX, frame generation only): 1080p 57 MB
  (performance) to 129 MB (native), 1440p 86–222 MB, 4K 190–473 MB.
- **License and patents:** MIT, the same SDK as FSR 3.1 upscaling (ADR 0060) and
  Brixelizer (ADR 0090). The optical flow is "based on AMD Fluid Motion Frames technology".

### AMD FidelityFX SDK 1.1.4: the Vulkan frame interpolation swapchain

`FrameInterpolationSwapchainVK.cpp` (2,968 lines, C++ with Win32 threads and critical
sections) replaces `vkCreateSwapchainKHR`, `vkGetSwapchainImagesKHR`,
`vkAcquireNextImageKHR`, `vkQueuePresentKHR` and `vkSetHdrMetadataEXT`.

- **The game renders into replacement images**, never the swapchain's. Present hands the
  frame over:
  - with FG on, the game queue signals a timeline semaphore;
  - the interpolation runs on the game queue, or on an async compute queue;
  - a thread composes and presents.
  - With FG off, the frame is still copied: "present is slightly more expensive (one
    extra surface copy)".
- **Four distinct `VkQueue`s are required:** game, async compute, present, image acquire.
  Creation fails if any two are the same queue.
  - When the present queue's family has graphics and compute, the UI is composed on the
    present queue right before present.
  - Otherwise it is composed on the game queue, and the present thread only copies.
- **Two threads, both `THREAD_PRIORITY_HIGHEST`:**
  - *Interpolation thread.* Waits on the CPU for the interpolation to finish on the GPU
    and measures the interval between finishes. It keeps a moving average of 10 intervals
    and their standard deviation σ, reset after any interval over 100 ms. It sets the
    present spacing to `Δ = avg/2 − 0.1·σ − 0.1 ms`.
  - *Present thread.* For the generated frame, then the real frame, it acquires a
    swapchain image, composes, **busy-waits** (`QueryPerformanceCounter` in a loop) until
    the previous present + Δ, and presents.
  - The real frame is shown Δ after the generated one: the frame generation's latency is
    that half frame, plus the interpolation's GPU time.
- **UI, three ways.** In order of AMD's preference:
  1. A **present callback**, called for every presented frame (real and generated) on
     the present thread with a command buffer of the present queue. The application
     draws its UI into the swapchain image there. AMD: *"the UI can get rendered at
     presentation rate to achieve smooth UI animations"*, at the cost of drawing it twice.
  2. A **UI texture** blended onto both frames (optionally premultiplied, optionally
     double-buffered inside): the UI updates at the real rate.
  3. The **hudless color**, from which the interpolation detects UI pixels and avoids
     distorting them: "minor artifacts in semi-transparent parts of the UI".
- **`VkPresentInfoKHR::pNext` is not supported** (asserted): no present ids through it.
  So no `VK_KHR_present_wait`, and no present-id-tied latency markers (Reflex on Vulkan),
  on the frames it presents.
- **Fixed sizes:** at most 6 buffers. Its own pacing knows nothing of the application's
  limiter or latency mode.

### NVIDIA Streamline 2.14.1: DLSS Frame Generation (DLSS-G)

- **Requirements:**
  - Windows 10 2004 or later;
  - hardware-accelerated GPU scheduling on;
  - an RTX 40 or newer GPU for frame generation;
  - Vulkan through manual hooking (ADR 0063's mode), with the eight hooked functions
    taken from `sl.interposer.dll`: present, create/destroy swapchain, get swapchain
    images, acquire, `vkDeviceWaitIdle`, create/destroy Win32 surface;
  - the queues `slGetFeatureRequirements` asks for, created with the device: extra
    graphics and compute queues, and the optical flow queue (`VK_NV_optical_flow`, a
    family of its own, index 0; without it DLSS-G runs optical flow in an "interop mode").
- **Inputs, tagged per frame:**
  - depth;
  - motion vectors;
  - hudless;
  - UI alpha (preferred) or UI color and alpha (premultiplied), all at the backbuffer's
    size;
  - optionally the backbuffer subrect and a bidirectional distortion field;
  - `slSetConstants` once per frame (matrices without jitter), with the frame id that the
    Reflex markers carry.
- **Reflex through `sl.reflex` and `sl.pcl` is required:**
  - "any existing Reflex SDK integration that does not use Streamline cannot be used with
    DLSS-G";
  - `slReflexSleep` and the PCL markers every frame, even with Reflex off.
- **Presentation is Streamline's:**
  - while `sl.dlss_g` is loaded the application renders off-screen, and Streamline
    copies, also with FG off ("always recreate the swap chain when DLSS-G is turned off");
  - its pacer presents the real frame always and the generated one when in time;
  - hardware flip metering on RTX 50 (`VK_NV_present_metering`);
  - `eRetainResourcesWhenOff` avoids a hitch when FG is toggled;
  - dynamic multi-frame generation is D3D12-only.
- **UI:**
  - DLSS-G composites the UI buffer onto its generated frames itself;
  - with hudless and UI alpha tagged, "user interface recomposition" interpolates the UI
    and the scene separately and composes them afterwards;
  - there is no call back into the application per presented frame;
  - fullscreen menus can turn FG off automatically (with the UI buffer tagged).
- **Cost** (NVIDIA's table, 2×): 1080p 1.07–1.45 ms on RTX 50 and 4090; 272 MB at 1080p,
  725 MB at 4K.
- **Measuring:** `MsBetweenPresents` is wrong for DLSS-G (the hardware holds images after
  present); `MsBetweenDisplayChange` (FrameView, PresentMon) is right.
- **Learned while building it (2026-10-12):**
  - Hardware-accelerated GPU scheduling is checked at `slInit`: without it `sl.dlss_g`
    loads, but `slIsFeatureSupported(kFeatureDLSS_G)` returns `eErrorOSDisabledHWS` and the
    log says "Feature 'kFeatureDLSS_G' requires GPU hardware scheduling to be enabled in
    the OS". Ref-1 has it off (`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers`,
    `HwSchMode = 1`), so DLSS FG has not run there yet.
  - The matrices follow Direct3D's conventions: `sl_matrix_helpers.h` builds
    `cameraViewToWorld` with the position in the fourth row and composes
    `clipToPrevClip = clipToCameraView · cameraViewToPrevCameraView · cameraViewToClipPrev`
    for row vectors, view space looking down +Z. Nothing in the documentation changes this
    for Vulkan; the engine converts its matrices (y down in clip space, −Z forward) and
    confirms it on the first run.
  - `sl.reflex` (its source ships with the SDK) uses the frame token's index as the present
    id of `vkSetLatencyMarkerNV`, injects `VkSwapchainLatencyCreateInfoNV` into the
    swapchain it creates when the device has `VK_NV_low_latency2`, and latches the
    present-start marker's frame for DLSS-G's present thread: the constants, the tags and
    the markers must carry the same frame index.
  - Tags take an array of `sl::ResourceTag` (not a chain); a volatile tag
    (`eOnlyValidNow`) is copied on the command buffer given, from the layout stated.
  - `NvLowLatencyVk.dll` ships beside the plugins: `sl.common` loads it for Reflex on
    Vulkan and logs an error without it (`docs/research/dlss.md`). DLSS alone works without
    it; DLSS FG needs Reflex, so the engine counts it among frame generation's files.

### AMD's FSR 3 plugin for Unreal Engine (the guide)

- `r.FidelityFX.FI.UIMode`:
  - **Slate redraw** (0): *"UI is rendered via Slate on to both the generated and normal
    frame"*. Widgets tick twice, the second time with a delta of 0.
  - **UI extraction** (1): compares the frame buffer before and after the UI. Worse with
    translucent UI. Forced when the interpolation runs asynchronously.
- With its own DXGI swapchain off, an "RHI pacing mode" turns vsync on for the second
  present.
- So the engine-side redraw of the UI per presented frame is what AMD ships by default in
  the largest engine, and its cost is the UI drawn twice.

### Andreev 2010 (SIGGRAPH talk, LucasArts)

- 30 → 60 fps in *Star Wars: The Force Unleashed II*. The in-between frame is the previous
  frame warped half-way along its velocity buffer, shown before the next real frame. So
  no latency is added: it is an extrapolation, with its artifacts. Characters are masked
  and filled.
- **Pacing:** a separate thread flips the in-between frame between vblanks, on PS3
  through a callback and on Xbox 360 through a GPU register.
- **Cost:** 1.5 ms of GPU time on Xbox 360.
- **The HUD was left open:** "remove the HUD completely or try using the overlays or
  restore it partially".

### VR compositors: OpenXR composition layers, Oculus ATW and ASW

- The runtime reprojects the eye buffers to each displayed frame's pose, and ASW
  synthesizes frames at half the render rate.
- Quad and cylinder layers (UI panels) are composited by the runtime per displayed frame,
  with that frame's pose and their own world or head-locked placement, never warped with
  the eye buffer.
- This is the model for the owner's world-space and camera-attached widgets: drawn per
  presented frame with that frame's camera, never interpolated.

### Presentation timing on ref-1's drivers (checked 2026-10-11, `vulkaninfo`)

| | NVIDIA 616.92 (RTX 4060 Ti) | AMD 26.8.1, Vulkan driver 2.0.353 (Radeon iGPU) |
|---|---|---|
| `VK_KHR_present_id` / `present_wait` (and `2`) | yes | **no** |
| `VK_EXT_present_timing` (rev 3) | yes: present at absolute and relative times | no |
| `VK_NV_present_metering`, `VK_NV_optical_flow` | yes | — |
| `VK_NV_low_latency2` / `VK_AMD_anti_lag` | yes / — | — / yes |
| Queue families (count, present support) | graphics 16 (present), compute 8 (present), transfer 2, optical flow 1 | graphics 8 (present), compute 4 (present), transfer 1 |

- On the Radeon only the CPU can time a present: there is no present timing and no
  present wait.
- `VK_EXT_present_timing` would let NVIDIA's driver hold a present until a target time,
  with no thread waiting.
- Both GPUs have spare graphics queues with present support: a second graphics-family
  queue for a present thread exists on both.

### Patents

- The components are AMD's MIT code, as for FSR 3.1 upscaling (ADR 0060).
- What the engine writes itself has prior art:
  - a thread that times presents (Andreev 2010);
  - the UI drawn on every presented frame (AMD's present callback and UE plugin, 2023);
  - world-space UI drawn per displayed frame with that frame's pose (OpenXR and Oculus
    compositor layers).
- A search for patents on world-space UI drawn on interpolated frames with an
  interpolated camera (2026-10-11, Google Patents) found nothing specific.

## What we take

- **FSR FG:**
  - We take AMD's two compute components (frame interpolation and optical flow), built
    from source into `engine_ffx` like the upscaler.
  - We do **not** take its Vulkan swapchain:
    - it needs four distinct queues and two busy-waiting threads;
    - it drops present ids (no `present_wait`, no Reflex present markers);
    - it copies every frame even with FG off;
    - its pacing knows nothing of our limiter and latency modes;
    - its UI callback runs on its own thread with a command buffer of its queue.
  - The engine presents itself instead (ADR 0091):
    - the interpolation and both frames' composition are recorded in the frame graph;
    - one present thread copies, times and presents, sleeping on the high-resolution
      timer (`PreciseSleeper`, ADR 0023) rather than spinning.
- **The UI on every presented frame, as AMD's present callback and UE's Slate redraw do.**
  But the engine draws it from the UI layer the game hands over, so the game draws
  nothing twice and ticks nothing twice. World-space and camera-attached widgets are
  drawn with each presented frame's camera, as VR compositors draw layers.
- **DLSS FG as Streamline defines it:**
  - hudless and UI buffers tagged, Reflex through `sl.reflex`/`sl.pcl`;
  - the UI buffer drawn with the real frame's camera, since DLSS-G calls nothing per
    presented frame.
  - World-space widgets go into that UI buffer, which "user interface recomposition"
    interpolates apart from the scene.
  - The other choice is drawing them into the scene, with motion vectors. It is measured
    against the first with a 3D HUD shot (the owner's question about text in 3D).
- **The pacing rule:** AMD's (the real interval's moving average, half of it, less a
  margin and a fraction of σ). It is the starting point, measured on both GPUs.
