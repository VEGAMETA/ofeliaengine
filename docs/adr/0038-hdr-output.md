# 0038. HDR output: scRGB by default, following Windows at run time

- Status: Accepted
- Date: 2026-09-27
- Research: [docs/research/hdr-output.md](../research/hdr-output.md)

## Context

CLAUDE.md §6 lists HDR output. The team asked for HDR in viewports that follows the
user's switch while running (turning HDR on or off, moving the window to another
monitor). Windows composes HDR in FP16 scRGB and shows ordinary (SDR) windows at a
user-chosen SDR white level. The engine draws its UI and its 3D views into one image, so
it has to match that level itself. Vulkan on Windows exposes HDR through
`VK_EXT_swapchain_colorspace`. Drivers differ: NVIDIA offers scRGB and HDR10 whether or
not the display is in HDR; the Radeon iGPU of ref-1 offers neither for a display on the
other GPU. Godot 4.7 ships HDR on Windows only through Direct3D 12 for that reason.

## Options

**Encoding of the swapchain:**

1. **scRGB: FP16 linear, BT.709, 1.0 = 80 nits (chosen as the default).**
   - Windows composes in it, blending stays linear, and it needs no encoding pass.
   - Costs 8 bytes per pixel instead of 4.
2. **HDR10: 10-bit PQ, BT.2020 (kept as fallback and option).**
   - 4 bytes per pixel.
   - Blending in PQ is wrong, so an image with UI must be composed in linear light first
     and encoded afterwards (an extra FP16 target and pass).
   - A windowed HDR10 swapchain is converted to scRGB by Windows anyway.

**When to output HDR:**

1. **When the surface offers HDR formats.** Wrong: NVIDIA offers them in SDR too, and
   Windows would clip the image to SDR white.
2. **When Windows runs the display in HDR (chosen).** Read through `DisplayConfig`: the
   24H2 `activeColorMode`, falling back to "advanced color on, not forced wide color".

**How to notice changes:**

1. **Poll DXGI every frame (Godot).** About 200 µs per frame on the render thread,
   480 µs at p99.
2. **The window thread asks on `WM_DISPLAYCHANGE`, on moves to another monitor and once a
   second (chosen).** 58 µs per query, off the render thread. It reports differences as
   `WindowEventKind::Display`.

**Brightness model:**

1. **Map scene 1.0 to the display's peak.** SDR content and the UI become blinding, and
   the look depends on the display.
2. **Reference white = Windows' SDR white level (chosen).**
   - The UI is scaled by `white / 80` like every other SDR window.
   - The tone mapper keeps SDR's look below reference white and rolls highlights off
     towards `peak / white`.
   - The peak is the display's maximum from DXGI, or 400 nits when it reports none.
   - Games can override both levels later (brightness settings).

## Measurements / arguments

ref-1, release build, editor at 1600×900 on the RTX 4060 Ti, Windows HDR toggled by the
engine (`DisplayQuery::set_hdr`).

- **GPU per frame at the editor's default v-sync (240 Hz), averaged over 4,000 frames:**
  SDR 515 µs, scRGB 585 µs (+14%), HDR10 670 µs (+30%).
  - HDR10 composes the UI into FP16 and then encodes it (139 µs).
  - scRGB writes the same FP16 image directly.
  - At this rate the GPU runs at low clocks in every mode; at full clocks the SDR frame
    is 68 µs.
- **Frame rate:** the editor stays at its cap (233 FPS) in every mode. Uncapped with
  immediate presentation, a windowed FP16 or HDR10 swapchain on NVIDIA goes through DWM
  composition: about 480 FPS instead of 5,800, with the GPU idling in P3.
- **Detection:**
  - 58 µs per query with a cached DXGI factory; creating the factory takes 9 ms, on the
    window thread during start-up.
  - DisplayConfig reports the new HDR state about 100–120 ms after the switch.
- **Correctness:**
  - Tone map pass vs. a CPU reference within 2e-4 relative, on both GPUs: SDR, scRGB with
    peak 5, HDR10 with and without tone mapping.
  - PQ code values match ST 2084 (100 nits = 520, 1000 nits = 769 in 10 bits).
  - A GDI capture of the scRGB editor is the SDR capture × 6.0 = 480 / 80 in linear
    light, and the HDR10 capture matches the scRGB one within 2 of 255.
  - Toggling HDR on and off under validation (with synchronization validation) gives 0
    errors. It first found that recreating a swapchain destroyed semaphores still used
    by its last present; recreation now waits for the device.

## Decision

- **`engine_platform::display`:**
  - `DisplayQuery` reports per monitor: HDR supported and on, SDR white level, min, max
    and full-frame luminance, refresh rate.
  - `set_hdr` switches HDR as Win+Alt+B does, for tests and settings menus.
  - The window thread sends `WindowEventKind::Display` after creating a window and on
    every change.
- **`engine_rhi`:**
  - The instance enables `VK_EXT_swapchain_colorspace` and the device
    `VK_EXT_hdr_metadata` when present.
  - `SwapchainDesc::color` asks for `OutputColor::{Sdr, ScRgb, Hdr10}` and falls back to
    the other HDR encoding, then to SDR.
  - `Swapchain::recreate_with_color` switches at run time.
  - `Swapchain::set_hdr_metadata` states the display's range and is re-applied after
    recreation.
- **`engine_render`:**
  - `OutputLevels` (output, reference white, peak) and `OutputEncoding` (linear with a
    white scale, or PQ).
  - The tone map pass rolls off towards the peak and can encode PQ.
  - `Viewports` composites in the output's encoding and recreates a viewport's texture
    when its format changes.
- **`engine_render2d`:** `Renderer2d::set_white` scales light, including the clear color,
  but not multiplied quads.
- **The editor:**
  - Presents in scRGB while Windows runs the display in HDR (`--hdr hdr10` / `off`).
  - The 3D view's texture becomes FP16, the UI is drawn at the SDR white level, and the
    pipelines are rebuilt only when the swapchain format changes.

## Consequences

- HDR works on the RTX with Vulkan, which Godot does not offer on Windows. It degrades
  to correct SDR (Windows maps sRGB to the SDR white level) where the driver offers no
  HDR format, such as ref-1's Radeon presenting to the other GPU's display.
- UI and SDR content look the same as in other windows whatever the slider says; only
  highlights of 3D views gain range.
- A white level above the display's peak (480 vs. 344 nits on ref-1) leaves no headroom:
  the view is tone mapped as in SDR.
- PBR Neutral generalized to a peak is a stopgap. An HDR-aware tone mapper (AgX or
  ACES 2.0 with a display peak) belongs with the post-processing stack. Wide-color (WCG)
  output for automatically color-managed SDR displays is not done; they get sRGB.
- **To measure:** the presentation mode (composed or independent flip) of FP16 and HDR10
  swapchains in borderless full screen, with PresentMon. Latency (CLAUDE.md §7) depends
  on it; HDR10 may win there.
- `VK_EXT_swapchain_maintenance1` present fences would let recreation wait for exactly
  the old swapchain's presents instead of the whole device; recreation is rare enough
  for now.

## In the player (2026-10-17)

The player (ADR 0099) took over presenting from the editor's own set-up in ADR 0107's
E1-3, without HDR. It now does what the editor did, for games and the editor alike:

- **`PlayerSettings::hdr`** (scRGB by default, HDR10, or SDR) is the output while the
  window's display runs in HDR; `Player::set_hdr` changes it at run time.
- **The display at start** (`DisplayQuery`) picks the swapchain's color, so an HDR display
  gets HDR from the first frame; afterwards each `WindowEventKind::Display` makes the
  output follow it: the swapchain made again when HDR turns on or off, the levels applied
  either way, the metadata stated.
- **What follows the output:**
  - `FrameRenderer::set_output_format` remakes the tone mapper and the filters after it;
  - the views' encoding and peak;
  - the UI's white;
  - `Overlay::output_changed`: the editor's panels.
- **HDR10 is drawn linear** in an `HDR_FORMAT` intermediate and encoded into the swapchain
  at the end ("encode hdr10"): the UI and overlays blend in linear light, as in the
  editor before.
- **With frame generation:** FSR's interpolates scRGB (or SDR); DLSS's stays SDR until it
  is verified in HDR. A surface without scRGB falls back to SDR, not HDR10, when frames
  are generated.
- **Offscreen players** take a display with `Player::assume_display`, and capture in the
  output's format (half floats for scRGB, `A2B10G10R10` for HDR10): the test of the whole
  path (`engine_app` `hdr_output`) runs without switching Windows' HDR.
- **Measured** at 1920 × 1080 offscreen (`hdr_output_cost`): scRGB adds 11 µs on the RTX
  4060 Ti and 37 µs on the Radeon iGPU (the tone mapper writing half floats); HDR10 adds
  39 µs and 590 µs (its encode pass). scRGB stays the default.
- **Not verified on a display yet:** ref-1's monitor runs in SDR, and Windows' HDR
  setting is the owner's to switch.

