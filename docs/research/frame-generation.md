# Frame generation

What the engine must provide so frame generation (FG) can be plugged in on Vulkan, and
which implementations exist. The implementation comes with the upscalers (CLAUDE.md §6,
§17 step 8). This note fixes the requirements early, because they shape the render graph,
the UI and the present path being built now.

Checked on 2026-09-26.

## Sources

| Source | Version | License | Finding |
|---|---|---|---|
| AMD FSR SDK ("Redstone") | v2.3.0, `60f4ea8` | MIT | Upscaler 3.1.5, FSR FG 3.1.6, FG swapchain 3.1.7 (described as a DXGI replacement), ML upscaler 4.1.1, ML FG 4.0.1. The readme says: *"Vulkan is currently not supported in SDK"*. |
| AMD FidelityFX SDK | v1.1.4, `c6efa6b` | MIT | FSR 3.1.4 upscaling + frame interpolation 1.1.3 on DX12 **and Vulkan**, including a Vulkan FG swapchain provider (`ffx-api/src/vk/ffx_provider_framegenerationswapchain_vk.cpp`). |
| NVIDIA Streamline | v2.9.0, `b998246` | MIT (SDK); DLSS binaries proprietary, user-supplied | DLSS Frame Generation (including multi-frame and dynamic modes) supports Vulkan (`slSetVulkanInfo`, `eVulkanSupported`). Inputs: depth, motion vectors, **hudless color**, **UI alpha or UI color + alpha (premultiplied)**, backbuffer subrect, optional distortion field. |
| Intel XeSS | not checked yet | — | XeSS-FG was DX12-only when last known; verify before relying on it. |

## Paths for the engine

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

## What the engine must provide (requirements for current work)

1. **Hudless color:** the scene after post-processing and before any UI, at backbuffer
   size, in the same color space as the final image.
2. **UI in its own target.**
   - Premultiplied RGBA or alpha only, at backbuffer size, composited as
     `final = ui.rgb + (1 − ui.a) · hudless.rgb`.
   - The UI system (`engine_ui`, the editor, text rendering) must render into a separate
     target from day one, never straight into the scene color. FG then interpolates the
     scene and composites a sharp UI on generated frames.
   - The viewport system (ADR 0016) already composites viewports; the HUD becomes the
     top layer with its own target.
3. **Motion vectors and depth, shared with the upscalers:**
   - per-pixel motion including camera motion;
   - depth with its convention (reversed Z, near/far);
   - jitter, camera matrices (current and previous), exposure.
   - **Camera cut / reset flag** when the view jumps (teleport, cut scene), so no frame
     is interpolated across a cut.
4. **A pluggable present path.**
   - FSR 3.1 FG on Vulkan presents through its own swapchain provider, which needs a
     queue for its presentation work and paces real and generated frames itself.
   - Streamline hooks presentation through its interposer.
   - So `engine_rhi::Swapchain` must become one of several presenters (native, FFX FG,
     Streamline) behind one interface. The frame loop must not assume that one present
     is one displayed frame.
5. **FG-aware latency and pacing** (`engine_latency`):
   - the limiter caps *real* frames (half the target rate with 2× FG);
   - Anti-Lag 2 and Reflex markers are for real frames;
   - latency reports tell real frames from generated ones.
6. **Settings and fallbacks:**
   - FG off by default in menus and when the base frame rate is too low (FG adds latency
     below roughly 60 real fps);
   - the option is hidden when the DLLs or device features are missing, with no errors
     (§6).

## Consequences for work in progress

- **Text and UI rendering** (next): render into their own target, with premultiplied alpha.
- **Upscaler interface** (§6): its inputs (color, depth, motion vectors, exposure, jitter,
  masks) are a superset of FG's. FG adds hudless, UI and the reset flag, so it is
  specified as one interface.
- **Swapchain abstraction:** before either FG path is integrated.
