# NVIDIA DLSS Super Resolution on Vulkan

Research for ADR 0063: how the engine runs DLSS Super Resolution (and DLAA, the same
network at the native size) from DLLs the user supplies (CLAUDE.md §0, §6). All versions
and terms checked on 2026-10-01.

## Sources

| Source | Version | License | What it is |
|---|---|---|---|
| NVIDIA Streamline (github.com/NVIDIAGameWorks/Streamline) | v2.14.1, 2026-09-08 | SDK code and headers: MIT (`license.txt`); `sl_nvperf.h`/`sl.nvperf.dll`: Nsight Perf SDK license | a framework over NVIDIA's (and other vendors') libraries: DLSS SR, RR, frame generation, Reflex, NIS; C API `sl*` |
| Streamline SDK release zip (`streamline-sdk-v2.14.1.zip`, 276 MB) | same | MIT for the code; the signed DLLs in `bin/x64` are NVIDIA's (`nvngx_dlss.license.txt`: the NVIDIA RTX SDKs License) | prebuilt, NVIDIA-signed `sl.*.dll`, `nvngx_dlss.dll` 310.9.1 (59 MB), `NvLowLatencyVk.dll` |
| NVIDIA DLSS SDK (github.com/NVIDIA/DLSS) | v310.9.1, 2026-09-08 | NVIDIA RTX SDKs License (proprietary) | the NGX API (`nvsdk_ngx*.h`), static libraries `nvsdk_ngx_s/d.lib`, `nvngx_dlss.dll`, the DLSS Programming Guide (31 March 2026) |
| DLSS Programming Guide | 310.x, 31 March 2026 | part of the SDKs | inputs, jitter, mip bias, presets, VRAM |
| Bevy `dlss_wgpu` (github.com/bevyengine/dlss_wgpu) | 6.x | MIT OR Apache-2.0 | DLSS for wgpu on Vulkan through the NGX SDK directly |

No NVIDIA code was copied. The engine's ABI declarations (`crates/engine_dlss/src/sl.rs`)
are written from Streamline's MIT headers (layouts, GUIDs, enumeration values), and are
checked against the offsets Streamline asserts in its own source.

## Licensing: what may go where

- **Streamline's source and headers are MIT.** Declaring its ABI in our code is fine
  (attribution in `THIRD_PARTY_NOTICES.md`).
- **The DLLs are NVIDIA's** (the RTX SDKs License, `bin/x64/nvngx_dlss.license.txt`):
  - distribution only "as incorporated in object code format into a software application"
    with "material additional functionality";
  - "you may not use the SDK in any manner that would cause it to become subject to an
    open source software license" (§4e): we must not commit them to this MIT repository,
    and do not (CLAUDE.md §3; `.gitignore` has `/plugins/` and `*.dll`);
  - licensed "only for use in systems with NVIDIA GPUs" (supplement §1);
  - **notify NVIDIA before a commercial release** of an application using DLSS
    (supplement §4, developer.nvidia.com/sw-notification);
  - **attribution**: "attribute the use of the applicable SDK and include the NVIDIA Marks
    on splash screens, in the about box of the application (if present), and in credits
    for game applications" (exhibit §7.1(b)); NVIDIA's marks need NVIDIA's approval
    (§7.2(c)).
- **These obligations fall on whoever ships the DLLs** — the game developer who accepts
  NVIDIA's license, or the player who drops them in. The engine ships none and has no
  NVIDIA mark. The dev guide tells game developers what they take on.
- **The DLSS (NGX) SDK itself is proprietary**: its headers under the same license, and
  its loader is a closed static library (`nvsdk_ngx_s.lib`). CLAUDE.md §0 forbids closed
  static libraries in the build.

## Integration routes

### A. The NGX SDK directly (UE's DLSS plugin, Unity, Bevy)

`NVSDK_NGX_VULKAN_Init_with_ProjectID`, `NVSDK_NGX_VULKAN_CreateFeature`,
`NVSDK_NGX_VULKAN_EvaluateFeature`. The application links NVIDIA's static library, which
finds the driver's NGX core and loads `nvngx_dlss.dll`.

- Pro: one library fewer at run time; the API DLSS is designed around.
- Con: **a closed static library in the build** (CLAUDE.md §0), and proprietary headers
  (not vendorable here). Bevy's `dlss_wgpu` makes each user download the SDK and point
  `DLSS_SDK` at it at build time: the binary then contains NVIDIA's code.
- Con: DLSS only; frame generation and Reflex through Streamline would be a second
  integration.

### B. Streamline (chosen)

The application loads `sl.interposer.dll`, calls `slInit`, gives it the Vulkan device, and
per frame `slSetConstants`, `slDLSSSetOptions`, `slEvaluateFeature` (with the resources
tagged). The interposer loads the plugins (`sl.common.dll`, `sl.dlss.dll`), which load
`nvngx_dlss.dll`.

- Pro: nothing closed in the build; the headers are MIT; everything is runtime-loaded from
  a folder the user fills.
- Pro: the same integration later carries DLSS frame generation, Ray Reconstruction and
  Reflex-with-FG (CLAUDE.md §7 requires Reflex through Streamline when FG runs through it).
- Con: a process-wide library that must start before the Vulkan device exists (below).
- Con: more DLLs, and Streamline's own behavior (logging, garbage collection on present)
  to live with.

### Vulkan hooking: interposer or manual hooking

Streamline can **replace the Vulkan loader**: the application loads `sl.interposer.dll`
instead of `vulkan-1.dll` and every Vulkan call goes through Streamline's proxies. Or,
with `PreferenceFlags::eUseManualHooking` (`docs/ProgrammingGuideManualHooking.md`), the
application keeps its loader and:

1. calls `slInit` **before creating the device** (manual hooking relaxes this for D3D,
   not for Vulkan);
2. asks `slGetFeatureRequirements(kFeatureDLSS)` for the instance and device extensions,
   Vulkan 1.2/1.3 features and extra queues, and enables them itself;
3. hands over the device with `slSetVulkanInfo` (instance, physical device, device, queue
   families and where Streamline's own queues start);
4. calls Streamline's proxies only for the hooked functions (`sl_hooks.h`): present,
   swapchain creation/destruction/images/acquire, device wait idle, surface
   creation/destruction. For DLSS Super Resolution only **present** matters: it runs
   `presentCommon()`, which advances Streamline's frame counter and collects its garbage
   (resources it retired a few frames earlier).

We take manual hooking: the engine keeps its own loader and dispatch (no proxy overhead on
every call, no confusion for RenderDoc, Nsight or the validation layer), and hooks present
through `Swapchain::set_present_function` with Streamline's `vkQueuePresentKHR` (from the
interposer's `vkGetDeviceProcAddr`).

Consequences of manual hooking found in Streamline's source (v2.14.1):

- **No present, no garbage collection.** Without presents (headless tests and
  benchmarks), Streamline logs once after 60 evaluations "presentCommon() was not
  observed" (`commonInterface.cpp`). Its own deferred releases — notably of a DLSS feature
  it recreates when a viewport's size or mode changes (`dlssEntry.cpp`: released 3 frames
  later by the garbage collector) — never happen. The engine therefore never lets
  Streamline recreate a feature: a new size or mode gets a **new viewport id**, and the old
  viewport is freed with `slFreeResources` once its frames are done. Nothing then depends
  on presents.
- **Command-buffer state is not restored** (`eDisableCLStateTracking`, the default): the
  host rebinds what it needs after `slEvaluateFeature`. The render graph binds per pass, as
  with FSR.
- Warnings at attach: "Hook sl.common:Vulkan:CmdBindPipeline is NOT supported" (and two
  more): the state-tracking hooks manual hooking does not provide; harmless.

### What DLSS asks of the device (measured, RTX 4060 Ti, driver 616.92)

`slGetFeatureRequirements(kFeatureDLSS)`:

- instance extensions: `VK_KHR_external_semaphore_capabilities`,
  `VK_KHR_get_physical_device_properties2`, `VK_KHR_external_memory_capabilities` (all
  core since 1.1; enabled anyway when requested);
- device extensions: `VK_KHR_push_descriptor`, `VK_NVX_binary_import`,
  `VK_NVX_image_view_handle`, `VK_KHR_buffer_device_address`. Streamline's source lists
  `VK_EXT_buffer_device_address`; NGX answered with the KHR one here. The EXT one must not
  be enabled with the core `bufferDeviceAddress` feature
  (`VUID-VkDeviceCreateInfo-pNext-04748`), so the engine filters it out;
- Vulkan 1.2 features `timelineSemaphore`, `descriptorIndexing`, `bufferDeviceAddress`
  (`dlssEntry.cpp`): the engine enables them always;
- no extra queues (DLSS frame generation would ask for some).

Not in the list but needed: **`privateData`** (Vulkan 1.3). Streamline creates a private
data slot in `slSetVulkanInfo`; the validation layer reports
`VUID-vkCreatePrivateDataSlot-privateData-04564` without it. It is a required feature of
every Vulkan 1.3 device, so the engine now enables it always.

On the Radeon iGPU, `slIsFeatureSupported` with the adapter's LUID answers
`eErrorAdapterNotSupported`; the device is created without the extensions and DLSS is
reported unavailable. Note: `slIsFeatureSupported` ignores `vkPhysicalDevice` when no LUID
is given (it then only checks the OS and driver), so the engine passes the LUID from
`VkPhysicalDeviceIDProperties`.

### Loading and trust

- **Where**: CLAUDE.md §6: `plugins/nvidia/` next to the executable, or the executable's
  folder; plus `ENGINE_STREAMLINE_DIR` (development, tests) and an explicit folder.
- **Files** (Streamline's guide, §4 "Distributing SL"): `sl.interposer.dll`,
  `sl.common.dll`, `sl.dlss.dll`, `nvngx_dlss.dll`. `NvLowLatencyVk.dll` is optional:
  without it `sl.common` logs an error at startup ("Failed to load NvLowLatencyVk.dll") and
  DLSS works. Other `sl.*.dll` in the folder are ignored (with a warning each) and slow
  `slInit` (each is signature-checked).
- **Signatures** (Streamline's guide §2.1.1): the engine checks `sl.interposer.dll` with
  `WinVerifyTrust` (embedded Authenticode, as `sl::security::verifyEmbeddedSignature`)
  and that the leaf signer is "NVIDIA Corporation"; the NVIDIA-built interposer checks
  every plugin it loads the same way. The development (unsigned) builds need
  `verify_signature: false`.
- **No over-the-air updates**: `eAllowOTA` and `eLoadDownloadedPlugins` are on by default
  in `sl::Preferences`; the engine clears them, so the DLLs the user supplied are the ones
  that run and nothing is downloaded behind the player's back.
- **Identity**: NGX wants an NVIDIA application id, or an engine type, engine version and
  a project id (a GUID). The engine passes `eCustom`, "ofeliaengine 0.0" and a project id
  games should replace with their own.

## Inputs, and how the engine's map to them

| DLSS input | Engine | Notes |
|---|---|---|
| color (`kBufferTypeScalingInputColor`) | forward HDR color, jittered, render size | HDR flag on |
| output (`kBufferTypeScalingOutputColor`) | a graph texture RGBA16F at the output size | DLSS **clears it with `vkCmdClearColorImage`** on reset: needs `TRANSFER_DST` usage and transfer-stage synchronization |
| depth (`kBufferTypeDepth`) | D32, 0 near / 1 far | `depthInverted = false` |
| motion vectors (`kBufferTypeMotionVectors`) | `MotionOutput::velocity`: UV delta current − previous, render size, no jitter | DLSS wants the vector to the previous position; Streamline multiplies `mvecScale` by the render size: `mvecScale = (−1, −1)`; `cameraMotionIncluded = true`, not jittered, not dilated |
| jitter (`Constants::jitterOffset`) | `MotionOutput::jitter`, pixels, x right, y down | same sign as the engine's (checked: flipping it loses 8.5 dB) |
| exposure | none: auto exposure | the engine's exposure buffer as a 1×1 texture later, as for FSR |
| reset | `MotionOutput::reset`, `Dlss::reset`, and every new feature | |
| reactive / transparency masks | not passed | the DLSS Programming Guide §3.15: the current networks (presets J–M) take no bias/reactive mask |
| camera matrices | identity | DLSS SR reads them only when camera motion is not in the vectors |

Layouts: Streamline's own resource-state table (`sl.chi/vulkan.cpp`) does not know
`VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL`, in which the render graph leaves sampled images; it
maps unknown layouts to GENERAL and would issue wrong barriers. The pass moves the inputs
to `SHADER_READ_ONLY_OPTIMAL` around the evaluation (as `engine_fsr` does for the
FidelityFX SDK), states the output as GENERAL, and puts a full memory barrier before and
after DLSS's work (its transfer clear and compute passes use stages the graph's barriers do
not cover). Synchronization validation then reports nothing.

## Quality modes and presets

`slDLSSGetOptimalSettings` at 1920×1080 (NGX 310.9.1): DLAA 1920×1080, Quality 1280×720,
Balanced 1114×626 (1.72×), Performance 960×540, Ultra Performance 640×360; each mode
accepts render sizes down to its minimum (dynamic resolution; 960×540 for Quality,
Balanced and Performance at 1080p; Ultra Performance only its own size). The engine picks
the mode from the render and output sizes it is given: the mode with the smallest optimal
size not below the render size, DLAA at the output size or above Quality's size.

Presets (`sl::DLSSPreset`): A–D removed, E/F deprecated, G–I and N–O revert to the default;
J, K (transformer, NVIDIA's default for DLAA, Quality, Balanced), L (default for Ultra
Performance), M (default for Performance). Jitter phases: NVIDIA's table (§3.7.1.1) is
8·s², 8/18/24/32/72 — the engine's TAAU rule.

Texture LOD bias: NVIDIA recommends log2(render / output) − 1 (§3.5), one level more
than the standard bias; AMD recommends the same for FSR, which measured worse there
(ADR 0060). With DLSS it measured better (below).

## Measurements

ref-1, RTX 4060 Ti, driver 616.92, NGX 310.9.1, Streamline 2.14.1. `cargo run -p
engine_dlss --release --example upscale_bench` (the FSR benchmark's scene: 576 cubes,
camera circling, HDR); GPU µs from the graph's pass timestamps, medians of 200 frames.

| output ← render | forward | TAAU | FSR 3.1 | DLSS (default preset) |
|---|---:|---:|---:|---:|
| 1080p ← 1080p (DLAA) | 155 | 149 | 786 | 890 |
| 1080p ← 720p | 74 | 121 | 534 | 871 |
| 1080p ← 540p | 46 | 121 | 441 | 1,122 |
| 1440p ← 1440p (DLAA) | 264 | 253 | 1,407 | 1,529 |
| 1440p ← 960p | 124 | 216 | 958 | 1,484 |
| 1440p ← 720p | 75 | 213 | 848 | 2,001 |

Presets at 1080p (µs): DLAA J 897 / K 892 / L 3,936 / M 3,119; from 720p 870 / 873 /
2,100 / 1,659; from 540p 861 / 861 / 1,388 / 1,122 (the default there is M).

Image quality (PSNR of tonemapped colors against 2×2 supersampling, 1080p, 64 frames;
camera still / circling):

| render | TAAU | FSR 3.1 | DLSS |
|---|---|---|---|
| 1080p | 44.80 / 39.36 | 45.54 / 41.26 | 47.14 / 42.03 |
| 720p | 42.63 / 36.81 | 43.54 / 38.88 | 45.99 / 39.81 |
| 540p | 40.39 / 35.10 | 41.63 / 37.03 | 44.25 / 41.48 |

`tests/dlss.rs` (128² output, 4×4 supersampling, synchronization validation): DLAA 46.8,
TAAU 41.1, FSR 43.7 dB; from half the size 42.6 / 34.8 / 38.1; panning from half the size
42.3 / 33.7 / 37.6. Flipping the jitter's sign: 34.1 dB from half the size (−8.5);
flipping the motion vectors' sign: 38.0 panning (−4.3). LOD bias from half the size on a
fine texture: 0 → 30.35, standard −1 → 30.48, NVIDIA's −2 → 30.56 dB.

GPU memory (the process's device memory as the OS counts it, before and after DLSS's first
frames; includes the RGBA16F output image): 1080p DLAA or from 720p 85 MiB, from 540p
124 MiB (preset M), 1440p from 960p 149 MiB, 2160p from 1080p 470 MiB. NGX's own estimate
agrees within 5%, and NVIDIA's table (K: 86 / 144 / 307 MB at 1080p / 1440p / 2160p; M:
116 / 204 / 456 MB).

Time: `Streamline::load` (signature check, `slInit`) 0.70–0.74 s warm (1.2 s cold, and
1.2 s with the whole SDK's `bin/x64` in the folder); `attach` (NGX initialization) 0.39 s;
a new DLSS feature (first frame at a size) 26 ms; CPU recording of the frame 78–99 µs
(FSR 83–112 µs, TAAU 15–22 µs). Rust heap allocations per frame: 0.

## What UE, Unity, Godot and Bevy do (checked 2026-10-01)

- **Unreal Engine 5**: NVIDIA ships the plugins (DLSS 4 plugins for UE, NVIDIA license):
  DLSS Super Resolution and Ray Reconstruction on NGX; frame generation, Reflex and
  DLSS-G through Streamline. The engine itself contains no DLSS.
- **Unity**: HDRP/URP call DLSS through Unity's NVIDIA module (NGX), with
  `nvngx_dlss.dll` shipped in the package.
- **Godot**: no DLSS (the license cannot live in its MIT source tree); third-party
  extensions only.
- **Bevy 0.17+**: `dlss_wgpu` over the NGX SDK, which each user downloads (`DLSS_SDK` at
  build time); NVIDIA's static library ends up in the game binary.

## What we take, and what not

- Streamline, runtime-loaded, manual hooking, DLSS Super Resolution and DLAA; the engine's
  own ABI declarations; `WinVerifyTrust`; no OTA.
- New viewport per size or mode, freed explicitly: independent of presents.
- Present hooked through the swapchain when a game presents.
- Not now: DLSS Ray Reconstruction (needs the engine's ray-traced inputs), DLSS frame
  generation (the present path ADR 0060 lists as missing), Reflex through Streamline (the
  engine uses `VK_NV_low_latency2` directly while there is no FG), the exposure input,
  `slAllocateResources` ahead of the first frame (creation is 26 ms at the first frame).
