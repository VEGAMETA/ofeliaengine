# 0063. DLSS Super Resolution through Streamline with manual hooking, from user-supplied DLLs; one `Upscaler` interface

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0043](0043-motion-vectors-and-taau.md) (motion vectors, jitter, TAAU),
  [ADR 0060](0060-fsr-3-1-upscaling.md) (FSR 3.1, texture LOD bias),
  [ADR 0011](0011-render-graph.md) (the render graph)
- Research: [`docs/research/dlss.md`](../research/dlss.md)

## Context

CLAUDE.md §6 lists DLSS as the third upscaler behind the engine's inputs, "via NVIDIA
Streamline (open source)", with the DLLs supplied by the user: the engine never ships or
commits NVIDIA binaries (§0, §3), looks for them in `plugins/nvidia/` or the game folder,
checks their signature, and hides the option cleanly when they are missing. §17 puts DLSS
right after FSR 3.1. ADR 0043 planned an `Upscaler` interface for the second
implementation; ADR 0060 deferred it to DLSS, the third, because the three need different
things at `add_pass`.

## Options

**How DLSS is reached** (research note):

1. **The NGX SDK directly** (what UE's DLSS plugin, Unity and Bevy do): link NVIDIA's
   static loader library, include its headers. Both are under the proprietary NVIDIA RTX
   SDKs License, which forbids use that would subject them to an open-source license; a
   closed static library in the build is against CLAUDE.md §0. Rejected.
2. **Streamline (chosen).** Its code and headers are MIT; the engine declares the ABI it
   uses in Rust and loads the NVIDIA-signed DLLs at run time from the user's folder. It is
   also the road to DLSS frame generation and Ray Reconstruction later.

**How Streamline meets Vulkan:**

- a. **As the Vulkan loader** (`sl.interposer.dll` instead of `vulkan-1.dll`): every call
  through Streamline's proxies. More CPU per call, tools and layers see proxies, and the
  whole engine depends on the DLL being there. Rejected.
- b. **Manual hooking (chosen)**: the engine keeps its loader; Streamline is initialized
  before the device, gets it with `slSetVulkanInfo`, and only presents go through its
  `vkQueuePresentKHR` (`Swapchain::set_present_function`).

**Resizes and mode changes:**

- Streamline recreates a viewport's DLSS feature itself and releases the old one three
  frames later from its present hook: with manual hooking and no presents (headless) the
  old feature is never freed.
- **A new viewport per output size, mode and preset (chosen)**, the old one freed with
  `slFreeResources` once its frames in flight are done (as `engine_fsr` retires contexts).

**The upscaler interface:**

- an enum dispatching to the three concrete types: every caller must link every
  upscaler's crate, and DLSS would drag Streamline's loader into `engine_upscale`;
- **a trait in `engine_upscale` (chosen)**: `Upscaler` with one input type
  (`UpscaleInput`: color, depth, `MotionOutput`, camera planes and field of view, frame
  time — the superset FSR introduced) and one context (`UpscaleContext`: device,
  allocator, bindless set — what TAAU needs), implemented by `Taau`, `Fsr` and `Dlss` in
  their crates; `Quality` levels shared, each upscaler mapping them to its own render
  sizes; `UpscalerKind` for menus. The concrete APIs stay as they were.

## Measurements

ref-1, RTX 4060 Ti (driver 616.92), release; NGX 310.9.1, Streamline 2.14.1.
`cargo run -p engine_dlss --release --example upscale_bench`: FSR's benchmark scene (576
cubes, camera circling, HDR, shadow map 2048²); GPU µs from the graph's pass timestamps,
medians of 200 frames.

| output ← render | forward | TAAU | FSR 3.1 | DLSS |
|---|---:|---:|---:|---:|
| 1920×1080 ← 1920×1080 (DLAA) | 155 | 149 | 786 | 890 |
| 1920×1080 ← 1280×720 (Quality) | 74 | 121 | 534 | 871 |
| 1920×1080 ← 960×540 (Performance) | 46 | 121 | 441 | 1,122 |
| 2560×1440 ← 2560×1440 (DLAA) | 264 | 253 | 1,407 | 1,529 |
| 2560×1440 ← 1706×960 (Quality) | 124 | 216 | 958 | 1,484 |
| 2560×1440 ← 1280×720 (Performance) | 75 | 213 | 848 | 2,001 |

- DLSS costs 1.6× FSR and 7× TAAU at 1080p from 720p. The default network for
  Performance is preset M (1,122 µs); preset K runs it in 861 µs. L and M cost 2-4×
  K at DLAA and Quality.
- Image quality, PSNR of tonemapped colors against 2×2 supersampling at 1080p after 64
  frames, camera still / circling:

  | render | TAAU | FSR 3.1 | DLSS |
  |---|---|---|---|
  | 1920×1080 | 44.80 / 39.36 | 45.54 / 41.26 | 47.14 / 42.03 |
  | 1280×720 | 42.63 / 36.81 | 43.54 / 38.88 | 45.99 / 39.81 |
  | 960×540 | 40.39 / 35.10 | 41.63 / 37.03 | 44.25 / 41.48 |

  DLSS is 1.6-2.6 dB better than FSR with the camera still and 0.8-4.5 dB moving; from
  540p it beats FSR from 720p. In `tests/dlss.rs` (128², 4×4 supersampling,
  synchronization validation, 0 errors): DLAA 46.8 dB (TAAU 41.1, FSR 43.7), from half
  the size 42.6 (34.8, 38.1), panning 42.3 (33.7, 37.6).
- **Conventions checked, not assumed**: flipping the jitter's sign loses 8.5 dB from half
  the size, flipping the motion vectors' sign 4.3 dB panning.
- **Texture LOD bias**, from half the size on a fine texture: 0 → 30.35 dB, the standard
  log2(render / output) → 30.48, NVIDIA's one level more → 30.56. Unlike FSR (ADR 0060),
  NVIDIA's recommendation wins with DLSS: `Dlss`'s `Upscaler::texture_lod_bias` returns it.
- GPU memory (process device memory before/after, with the output image): 85 MiB at 1080p
  (DLAA or from 720p), 124 MiB from 540p (preset M), 149 MiB at 1440p from 960p, 470 MiB
  at 2160p from 1080p; FSR 134 MiB at 1440p.
- Startup: `Streamline::load` 0.70-0.74 s (1.2 s cold or with the whole SDK folder),
  `attach` (NGX) 0.39 s; a new DLSS feature 26 ms at its first frame (FSR's context
  11-14 ms). CPU recording per frame 78-99 µs (FSR 83-112, TAAU 15-22). Rust heap
  allocations per frame: 0.
- Radeon iGPU: DLSS unavailable (`slIsFeatureSupported`: `eErrorAdapterNotSupported`);
  the device is created without Streamline's extensions and TAAU and FSR pass their
  tests with synchronization validation.

## Decision

- **Crate `engine_dlss`**, on Streamline 2.14.1's ABI declared from its MIT headers
  (`src/sl.rs`, layouts tested against the offsets Streamline asserts):
  - `Streamline::load(&StreamlineDesc)`: finds the folder (explicit, `ENGINE_STREAMLINE_DIR`,
    `plugins/nvidia/` next to the executable, the executable's folder) with
    `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll`, `nvngx_dlss.dll`; checks
    `sl.interposer.dll`'s Authenticode signature and NVIDIA as the signer; loads it
    (kept loaded for the process); `slInit` with manual hooking, DLSS only, no
    over-the-air updates; reads DLSS's Vulkan requirements. Every failure is an error
    value, never a crash; one `Streamline` per process.
  - `instance_extensions()`, `check_adapter(&instance, &adapter)` (LUID →
    `slIsFeatureSupported`, device extensions present), `device_extensions()`,
    `attach(&device)` (`slSetVulkanInfo`), `present_function()`.
  - `Dlss::new(&streamline, frames_in_flight)`; `add_pass(fg, UpscaleInput, output_size,
    frame_number) -> Texture` (RGBA16F), the shape of `Fsr::add_pass`; `render_size` and
    `render_sizes` from `slDLSSGetOptimalSettings`; the mode follows from the render and
    output sizes; `set_preset`; `reset`; `gpu_memory`.
  - One graph pass `dlss` (plus an empty `dlss output usage` pass declaring the transfer
    write DLSS's reset clear needs): inputs moved to `SHADER_READ_ONLY_OPTIMAL` (the
    graph's `READ_ONLY_OPTIMAL` is unknown to Streamline), full memory barriers around
    DLSS's work, local resource tags valid until the evaluation (no copies).
- **`engine_rhi`** (small, additive): `Instance::with_extensions`,
  `Device::with_extensions` (extra extensions, enabled when listed),
  `extra_extension_enabled`, `Adapter::physical`, `Swapchain::set_present_function`;
  the device enables `privateData`, a required Vulkan 1.3 feature Streamline uses without
  listing it.
- **`engine_upscale::Upscaler`** and its types, implemented by `Taau`, `Fsr`, `Dlss`.
- **Default upscaler policy stays as ADR 0060 left it** (TAAU): DLSS is an option per view
  where the game offers it.

## Consequences

- Games get DLSS Super Resolution and DLAA on NVIDIA RTX GPUs when the player or the game
  supplies NVIDIA's DLLs; nothing NVIDIA-owned is in the repository or the build.
- Whoever ships the DLLs takes on NVIDIA's terms: notification before release, NVIDIA's
  marks on the splash screen, about box and credits, NVIDIA GPUs only
  (`docs/dev-guide/rendering.md`). The engine itself carries no NVIDIA mark.
- Streamline must start before the instance and device, and costs about 1.1 s at startup
  (load + NGX initialization): start it early (it needs no window), or only when the
  player enabled DLSS.
- Without presents (headless) Streamline logs once that it saw none; nothing depends on
  it, since the engine frees replaced features itself. Games should hook presents
  (`set_present_function`), which frame generation will require anyway.
- Streamline's and NGX's per-frame work and allocations are in their C++, outside the
  engine's allocation test.
- Revisit:
  - frame generation and Reflex through Streamline, with the present-path prerequisites
    of ADR 0060;
  - the exposure input (the engine's exposure as a 1×1 texture, also for FSR);
  - creating features ahead of time (`slAllocateResources`) if the 26 ms first frame
    matters when resizing in play;
  - the Streamline ABI when updating past 2.14.1 (the layout tests and `tests/dlss.rs`
    catch changes);
  - Ray Reconstruction once the engine has ray-traced inputs.
