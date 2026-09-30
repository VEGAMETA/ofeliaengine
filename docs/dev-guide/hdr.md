# HDR output

On an HDR display, highlights can be brighter than white: a sunlit sky, a lamp, sparks.
The engine outputs HDR while Windows runs the display in HDR ("Use HDR" in Settings, or
Win+Alt+B), and follows it while running: turning HDR on or off, moving the window to
another monitor, moving the "SDR content brightness" slider (ADR 0038).

## What the player sees

- **UI and SDR content** look exactly as bright as in every other window: white is drawn
  at Windows' SDR white level (the slider).
- **3D views** look the same up to white. Brighter parts, which SDR squeezes towards
  white, keep their range up to the display's peak luminance.
- On a display without HDR, or with HDR off, nothing changes.

## In your game

The editor does all of this; a game's loop does the same:

1. Listen for `WindowEventKind::Display(info)` (`engine_platform::display::DisplayInfo`).
   It arrives once after the window is created and again on every change.
2. While `info.hdr_enabled`, create or recreate the swapchain with
   `SwapchainDesc::color` / `Swapchain::recreate_with_color`: `OutputColor::ScRgb`, or
   `Hdr10`. It falls back to the other HDR encoding, then to SDR, if the driver offers
   no HDR format. `Swapchain::color()` tells what you got. Otherwise use
   `OutputColor::Sdr`.
3. Describe the output with `engine_render::post::OutputLevels`:
   - `color`: `swapchain.color()`;
   - `white_nits`: `info.sdr_white_nits` (or the player's brightness setting);
   - `peak_nits`: `info.max_nits`.
4. When the swapchain format changes, rebuild the pipelines that write it (`Tonemap`,
   `Renderer2d`). When only the levels change, just pass the new values:
   - `Composite::peak = levels.peak()` for the tone map;
   - `Composite::encoding = levels.swapchain_encoding()` for passes that write the
     swapchain, or `OutputEncoding::LINEAR` for intermediate textures;
   - `Renderer2d::set_white(white)` for UI drawn into an scRGB swapchain (the `white` of
     `OutputEncoding::Linear`);
   - `Viewports::set_output_levels(levels)` if you composite viewports with
     `Viewports::composite`.
5. With HDR10, draw the UI into an `HDR_FORMAT` texture first, then encode it into the
   swapchain with a `Tonemap` pass for the swapchain's format (`tonemap: false`,
   `encoding: levels.swapchain_encoding()`). Blending PQ values directly would be wrong.
6. `Swapchain::set_hdr_metadata` with the display's range (the editor uses `max_nits`
   for the peak and MaxCLL, `max_full_frame_nits` for MaxFALL).

Keep 3D views that feed an HDR output in `HDR_FORMAT` after tone mapping: an sRGB
texture cannot hold values above white.

## scRGB or HDR10?

scRGB (the default) is what Windows composes in: blending stays linear and there is no
extra pass. HDR10 has half the bytes per pixel, but anything with transparency must be
composed in linear light first and then encoded. In the windowed editor that made HDR10
the more expensive option (GPU per frame on an RTX 4060 Ti at 240 Hz: SDR 515 µs, scRGB
585 µs, HDR10 670 µs). Whether full-screen games present HDR10 or scRGB more directly
(lower latency) has not been measured yet.

## Testing

- `cargo run -p engine_platform --example hdr --release` prints what the display
  reports; `-- on`, `-- off` or `-- toggle` switches HDR as Win+Alt+B does. That is a
  setting for every application, and the screen goes dark for a moment while it
  switches.
- The editor prints `output: ...` when it switches, and shows the output in its status
  bar.
- Screenshots through GDI (`PrintWindow`, most capture tools in SDR mode) clip HDR at
  80 nits and look washed out. That is the capture, not the output. `Windows.Graphics.Capture`
  can capture FP16.

## Known limits

- Some drivers offer no HDR format:
  - a GPU presenting to a display attached to another GPU (the Radeon iGPU on ref-1);
  - older Intel drivers.
  
  Output stays SDR, correctly scaled by Windows.
- If the SDR white level is above the display's peak, HDR adds no headroom: the view is
  tone mapped as in SDR.
- The tone curve (ADR 0048) keeps mid-tones the same in SDR and HDR and rolls off towards
  the display's peak: Uchimura's curve by default, PBR Neutral with
  `Composite::curve = ToneCurve::PbrNeutral` (for material and product views). AgX and
  ACES (`ToneCurve::Agx`, `ToneCurve::Aces`) are SDR curves: on an HDR display they stop
  at reference white.
