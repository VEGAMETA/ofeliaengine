# 0079. Pipelines made on first use and warmed up in the background; start-up phases overlapped

- Status: Accepted
- Date: 2026-10-10
- Research: [docs/research/pipeline-cache.md](../research/pipeline-cache.md) ("Creating
  pipelines on first use")
- Related: [ADR 0055](0055-pipeline-cache-and-warm-up.md) (pipeline cache; its "no
  pipeline is created while drawing" no longer holds for shading variants),
  [ADR 0058](0058-ambient-occlusion.md) (register rule),
  [ADR 0072](0072-frame-renderer.md), core review §3.8 (queue item 0g)

## Context

A game's first frame waited for everything, one phase after another, and for every
pipeline the renderers might ever draw with. `flight` (Sponza pack, the device class's
defaults, ref-1, release) on main:

| first frame since `main`, ms | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| our pipeline cache file | 324 | 432 |
| no file, the driver's own cache warm | 550 | 430 |
| drivers cold (`ENGINE_PIPELINE_SALT`) | 796 | 1,028 |

- Phases: window 14 ms; instance and device 154-189; surface and swapchain 11 on the RTX
  and 174 on the Radeon (it presents across adapters, ADR 0007's addendum); renderers'
  pipelines 13-30 with the file, 252 with only NVIDIA's cache, 496-575 cold; the pack 13;
  the scene and the cooked Fox 30.
- 85 pipelines were made; the first frame bound 28, and 600 frames 31. Of the forward
  renderer's 62 shading variants (4 lit variants × opaque, masked, after the prepass,
  with motion vectors, transparent, reactive; again with ambient occlusion, local
  shadows, both, contact shadows, subsurface, reflections) it drew with one.

## Options

### When a variant is made

1. **All at start-up** (main): no pipeline is ever made while drawing; the first frame
   pays for every variant, used or not, and so does memory.
2. **On first use only:** a frame that first needs a variant compiles it (with the
   drivers cold, 26-35 ms per shading pipeline here); a hitch whenever content first
   brings a variant (a transparent object, a light coming into reach).
3. **In the background, drawing something else meanwhile** (Godot 4.4's ubershaders,
   Unreal's PSO precaching that skips the draw or uses the default material, Bevy's
   asynchronous `PipelineCache`): no stall, but a wrong picture for a few frames, and an
   ubershader would cost every view registers (ADR 0058's rule).
4. **Described up front, made on first use or by a warm-up thread, whichever comes
   first** (chosen): the first frame waits for the variants it draws with; a thread below
   normal priority makes the others from the start, so a later frame finds them made;
   nothing draws wrong. A frame that needs one the thread has not reached makes it
   itself (option 2's cost, only in the first second or so of a cold start).

### Whose thread

- **The device's** (chosen): one thread per device, started with the first description;
  the device stops it before destroying the pipeline cache and itself, so it can never
  outlive what it creates with. Every renderer's variants share it.
- A renderer's own: a thread per renderer, and every set's constructor would have to be
  handed it.

A pipeline the thread makes after its renderer was dropped needs its layout:
`SharedPipelineLayout` keeps the bindless layout alive until the last description that
may still be made with it is gone, and the thread holds descriptions weakly, so one whose
renderer dropped them is skipped.

### Overlapping start-up

The instance and device (the drivers' own start-up) need nothing but the window's handle
later; the swapchain needs only the device and the surface, the renderers only its
format. So the device is created on a thread while the window, the job pool and the
CPU side of the scene are made, and the swapchain on a thread while the renderers, the
pack and the scene are (`Swapchain::format_for` gives the format first).

## Measurements

`flight`, first frame since `main`, median of three, each phase's own time in the notes
(ref-1, release; the file case after a 300-frame run that let the warm-up finish):

| ms | main | this |
|---|---:|---:|
| RTX: our cache file | 324 | 261 |
| RTX: no file, NVIDIA's cache warm | 550 | 289 |
| RTX: drivers cold | 796 | 504 |
| Radeon: our cache file | 433 | 367 |
| Radeon: no file | 430 | 364 |
| Radeon: drivers cold | 1,028 | 547 |

- The renderers' phase: RTX 30 → 4.5 ms with the file, 252 → 30 with NVIDIA's cache
  only, 496 → 173 cold; Radeon 13 → 12, 575 → 239 cold. Before the first frame 29
  pipelines are made eagerly and the shading variants it draws with (2-5) on first use.
- The Radeon's swapchain (175 ms) now runs beside the renderers and the scene (57 ms of
  work there), so the first frame still waits 140 ms for it.
- The scene's phase 30 → 9 ms: the Fox's import and cook moved beside the device.
- `pipeline_bench`: the forward renderer's own creation 49 ms (RTX) and 40 (Radeon)
  cold, 8.9 and 0.8 with the file. The warm-up thread then makes its 33 variants
  in 1,145 ms (RTX) and 867 (Radeon) cold, one after another, and in 37 and 2 ms from
  the file. On NVIDIA these lookups take about 1.1 ms each against 0.1 for small shaders.
- 600 frames of `flight` cold: no frame over twice the median on either GPU. The RTX
  made 21 pipelines during the timed frames, all on the warm-up thread. Without the
  warm-up (`--no-warm-up`) no frame made one: every variant this scene needs is drawn
  in its first frame.
- Golden images: byte-identical to main on both GPUs except
  `procedural_contact_shadows`, which varies from run to run on either build
  (`docs/status.md` §6).

## Decision

- **`engine_rhi::LazyPipeline`**:
  - `describe_many(device, layout, descs)` keeps an owned description (SPIR-V shared
    per module) and creates nothing;
  - `get_or_create()` makes the pipeline on the calling thread unless it exists (the
    pipeline cache is asked first) and waits if another thread is making it. A failure
    is logged once and kept;
  - `create_all(&[..])` makes several as `Pipeline::create_many` does.
- **The device's warm-up thread** (below normal priority) makes every described pipeline
  in the order described:
  - `Device::set_pipeline_warm_up(false)` turns it off for what is described afterwards;
  - `Device::wait_for_pipeline_warm_up()` waits for it, for tests that count allocations
    and benchmarks that time frames; the golden bed and `scene_bench` do;
  - `Device::pipelines_warming_up()` counts what is left (a progress bar).
- **`SharedPipelineLayout`**, `Bindless::shared_pipeline_layout()`: the layout lives until
  the last pipeline still to be made with it is gone.
- **`PipelineContext`** (`Device::pipeline_context`): what creating pipelines needs,
  sendable to another thread.
- **The forward renderer describes its shading variants** and makes them on first use,
  plain and multisampled: the base sets, ambient occlusion, local shadows, contact
  shadows, subsurface, reflections, the atmosphere's composite and transparent surfaces
  under it. The prepass, shadow, culling, sky and background pipelines are made at once:
  nearly every frame draws with them. A view whose variant cannot be made is not drawn.
- **`Swapchain::format_for`**: the format a swapchain will have, before it exists.
- **`flight`** overlaps its start-up as above; the player crate (queue item 6f) takes it
  over.

## Consequences

- The first frame of a game comes 60-65 ms sooner with a warm cache. With the drivers
  cold, after an install or a driver update, it comes 290 ms sooner on the RTX and
  480 ms on the Radeon. A renderer feature costs start-up nothing until a view draws
  with it.
- A frame can now make a pipeline: the first one that draws with a variant the warm-up
  has not reached, during the first second or so of a cold start (26-35 ms, once).
- The warm-up thread works in the background during the first seconds of a cold start
  (1.1-1.4 s on one thread here), and allocates while it does: allocation tests wait for
  it.
- The other renderers' pipelines (TAA and upscalers, post, AO's passes, local shadows'
  passes, skinning) are still made at start-up; most are drawn with every frame.
- Next steps:
  - acquire the swapchain image late, so the Radeon's first frame records beside its
    swapchain (140 ms of waiting);
  - `VK_EXT_shader_module_identifier` lookups (1.1 ms per large shader on NVIDIA);
  - a record of the variants a game draws with, so the warm-up makes those first (with
    the material graph, which needs it anyway);
  - more than one warm-up thread, if cold starts show variants needed before the thread
    gets to them.
