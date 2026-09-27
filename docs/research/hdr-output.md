# HDR output on Windows with Vulkan

Checked 2026-09-27. Sources at the end. No code was copied from any of them; Godot and
the Microsoft sample are MIT, and only their ideas and measurements are used here.

## How Windows does HDR

- **Composition.** While a display runs in HDR, the desktop window manager (DWM)
  composes in FP16 scRGB: BT.709 primaries, linear, 1.0 = 80 nits (Microsoft calls it the
  canonical composition color space). The display kernel converts the result to HDR10
  (BT.2100 PQ) for the cable. A windowed HDR10 swapchain is converted to scRGB by DWM
  anyway.
- **SDR white.** Windows shows sRGB (1.0, 1.0, 1.0) of ordinary applications at the
  user's *SDR white level*: the "SDR content brightness" slider, typically about 200 nits
  on desktop monitors, 480 nits at the slider's end on ref-1. An application that
  composes SDR (its UI) into an HDR image itself must scale it by `white_nits / 80`, or
  its UI looks dim next to other windows. Win32 reads the level through
  `DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL)`, in
  thousandths of 80 nits.
- **Two swapchain options** (Microsoft's guidance):
  1. FP16 + scRGB, recommended for general applications. It is the only option that
     blends correctly with transparency and works for every kind of display; it costs
     8 bytes per pixel.
  2. 10-bit + HDR10 (PQ, BT.2020): "a performance optimization" for full-screen content
     without alpha blending (video, games). 4 bytes per pixel.
- **An HDR swapchain on an SDR display** is clipped to [0, 1] by DWM, so HDR output is
  only chosen while the display runs in HDR.
- **Which HDR state.** Since Windows 11 22H2, "advanced color" also covers SDR displays
  with automatic color management (wide color, WCG), where scRGB 1.0 means the display's
  own white, not 80 nits. `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` (type 9) cannot tell
  HDR apart from forced wide color; Windows 11 24H2 added
  `DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO_2` (15) with `activeColorMode`
  (SDR, WCG, HDR) and `highDynamicRangeSupported`, and `SET_HDR_STATE` (16) next to the
  older `SET_ADVANCED_COLOR_STATE` (10). DXGI (`IDXGIOutput6::GetDesc1`) reports
  `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` for HDR, but cannot distinguish WCG from
  SDR.
- **Luminance.** `DXGI_OUTPUT_DESC1` gives min, max and max-full-frame luminance: the
  EDID's values, or what the Windows HDR Calibration app measured. Microsoft asks HDR
  applications to tone map to them.
- **Changes.** Win32 has no event for them. Microsoft suggests asking
  `IDXGIFactory1::IsCurrent` every frame and handling window moves. Turning HDR on or off
  sends `WM_DISPLAYCHANGE`; moving the SDR slider or recalibrating sends nothing.
- **Capture.** GDI (`PrintWindow`, `BitBlt`) gets an 8-bit image clipped at scRGB 1.0;
  `Windows.Graphics.Capture` and `DuplicateOutput1` can capture FP16.

## Vulkan on Windows

`VK_EXT_swapchain_colorspace` (instance) makes surfaces list HDR color spaces;
`VK_EXT_hdr_metadata` (device) sets ST 2086 / CTA-861.3 metadata. What ref-1 lists
(`engine_rhi` example `triangle`, 2026-09-27):

| | Windows HDR off | Windows HDR on |
|---|---|---|
| RTX 4060 Ti, driver 616.92 (the display is on this GPU) | SDR 8-bit, `R16G16B16A16_SFLOAT` + `EXTENDED_SRGB_LINEAR`, `A2B10G10R10` + `HDR10_ST2084` | the same |
| Radeon iGPU, driver 25.10.36 (presenting across adapters) | SDR only | **SDR only** |

- NVIDIA offers HDR color spaces whether or not Windows runs the display in HDR, so the
  surface's list cannot be the signal: the display's state decides.
- The Radeon offers none for a display attached to the other GPU, even in HDR. Godot's
  HDR work saw the same on Intel UHD 620 ("no HDR compatible format was found").

## Open-source implementations

### Godot 4.7 (MIT; PR #94496 and follow-ups, released 2026)

- On Windows, HDR output ships for **Direct3D 12 only**. The Vulkan path exists for
  other platforms but is "not supported on Windows", because drivers do not reliably
  offer HDR formats.
- "Extended dynamic range": 0–1 stays SDR and values above it are brighter, up to the
  display's peak. The reference luminance defaults to the OS SDR white level; the maximum
  luminance defaults to the display's. Tone mappers (AgX reworked in 4.6) take the peak
  as their white.
- Detection: `WM_DISPLAYCHANGE` for HDR on or off and moves between monitors. The SDR
  white level and luminance are polled **every frame**, about 200 µs on average and
  480 µs at p99. DXGI's COM objects could not move to another thread.
- Gotcha: right after `WM_DISPLAYCHANGE`, `DXGI_OUTPUT_DESC1` can still report the SDR
  color space, which is another reason they poll.
- The UI initially followed the maximum luminance; a separate paper white for UI was
  left for later.

### Microsoft D3D12HDR sample (MIT, DirectX-Graphics-Samples)

- Swapchain in FP16 scRGB or 10-bit HDR10, switchable at run time.
- Finds the output whose desktop rectangle overlaps the window most, and reads
  `GetDesc1`. Recreates the DXGI factory when `IsCurrent()` is false, on window moves
  and on display changes.
- UI in the HDR10 path is composed before encoding.

### Adam Sawicki, "Programming HDR monitor support in Direct3D" (2019, blog)

- The same DXGI path, plus vendor extensions (NVAPI, AGS) from before DXGI had
  `SetHDRMetaData` everywhere.
- Useful for the metadata semantics: a game that tone maps to the display states the
  display's own range, so the display does not tone map a second time.

## What we take, what we do not

- **scRGB by default, HDR10 as fallback and option (`--hdr hdr10`).** Measured on ref-1,
  HDR10 costs more in a windowed editor: an FP16 image is composed anyway (the UI
  blends), then encoded. See the numbers below.
- **HDR follows Windows.** Output is HDR only while `activeColorMode` says HDR (type 15,
  falling back to type 9 before 24H2), never merely because the surface offers it.
- **Reference white = SDR white level; peak = the display's maximum** (400 nits when it
  reports none). The UI is scaled by `white / 80` like every other SDR window. The 3D
  view uses PBR Neutral with its roll-off scaled to `peak / white` (unchanged below
  0.76 · peak): identical to the SDR curve when the peak is at or below white.
- **Not every frame.** The window thread asks on `WM_DISPLAYCHANGE`, when a window moves
  to another monitor, and once a second: 58 µs per query with a cached DXGI factory,
  off the render thread. DisplayConfig's HDR state updates within about 100 ms of the
  switch, so the DXGI lag seen by Godot does not delay us. A missed luminance change is
  picked up within a second.
- **The DXGI factory is created once** (9 ms on ref-1), off the main thread, while the
  device is created, and recreated only when `IsCurrent` fails.
- **Metadata:** the display's range (max = peak, MaxCLL = peak, MaxFALL = max full frame).
- **Toggling HDR from the engine** (`DisplayQuery::set_hdr`, as Win+Alt+B does) is kept
  for tests and settings menus. It changes a user setting for every application.
- **Not taken:** WinRT `AdvancedColorInfo` (UWP only for desktop windows); per-frame
  polling; histogram-based MaxCLL; wide color (WCG) output for automatic color management
  displays (sRGB output stays correct there, since Windows color-manages it); a separate
  UI brightness setting.

## ref-1 findings (2026-09-27)

- The display (PG27F15RS1A, 1920×1080 at 240 Hz, attached to the RTX) reports
  0.0646–343.7 nits through DXGI in both modes. The SDR white level reads 80 nits with
  HDR off and 480 nits with HDR on (the user's slider). A white level above the peak
  leaves the tone mapper no headroom (peak / white < 1 is clamped to 1).
- Turning HDR on or off through `SET_HDR_STATE` takes about 100–120 ms until
  DisplayConfig reports it.
- **A windowed FP16 swapchain on NVIDIA goes through DWM composition.** With
  `--present immediate` and no frame cap, the editor runs at 5,800 FPS in SDR but at
  about 480 FPS (2× the refresh rate) in scRGB and HDR10. The GPU then idles in P3
  (930 MHz instead of 2,835). At the editor's default cap (97% of the refresh rate) both
  modes run at the cap. Borderless full-screen games may get independent flip; that
  remains to be measured with PresentMon.
- A GDI capture of the scRGB editor is the SDR capture times 6.0 in linear light (480 / 80
  nits) in every UI pixel, and the HDR10 capture matches the scRGB one within 2 of 255
  everywhere: both outputs put UI white at Windows' SDR white level.

## Sources

- [Microsoft: Use DirectX with Advanced Color on high/standard dynamic range displays](https://learn.microsoft.com/en-us/windows/win32/direct3darticles/high-dynamic-range) (updated 2025-09-19)
- [Microsoft: DISPLAYCONFIG_DEVICE_INFO_TYPE](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ne-wingdi-displayconfig_device_info_type) (types 15–17)
- [Godot PR #94496: [Windows] Support output to HDR monitors](https://github.com/godotengine/godot/pull/94496)
- [Godot: HDR output arrives in Godot 4.7](https://godotengine.org/article/hdr-output-arrives-in-godot-4-7/)
- [Microsoft DirectX-Graphics-Samples, D3D12HDR](https://github.com/Microsoft/DirectX-Graphics-Samples/tree/master/Samples/Desktop/D3D12HDR)
- [Adam Sawicki: Programming HDR monitor support in Direct3D](https://www.asawicki.info/news_1703_programming_hdr_monitor_support_in_direct3d)
- [Vulkan: VkColorSpaceKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkColorSpaceKHR.html)
- SMPTE ST 2084 (PQ), ITU-R BT.2087 (BT.709 → BT.2020), CTA-861.3 (MaxCLL, MaxFALL).
- ref-1: `engine_platform` example `hdr`, `engine_rhi` example `triangle`, the editor's
  `--frames` runs (`docs/perf/history.md`).
