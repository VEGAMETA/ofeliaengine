# 0016. Viewports: persistent textures, topological order, graph scopes, composition

- Status: Accepted
- Date: 2026-09-26
- Builds on: [ADR 0011](0011-render-graph.md), [ADR 0013](0013-bindless-and-pipelines.md),
  [ADR 0014](0014-pbr-and-shadows.md), [ADR 0015](0015-renderer-2d.md)
- Research: [docs/research/viewports.md](../research/viewports.md)

## Context

CLAUDE.md §6 makes viewports first-class:
- own camera, resolution, post-processing, visibility layers and render tick;
- nesting (a viewport inside a viewport, a viewport as a texture in the 3D world and in UI);
- split screen, picture-in-picture, minimaps, portals;
- pixel-perfect mode with integer scaling;
- the editor uses the same system;
- each viewport's cost visible separately in the profiler.

§17 step 2 ends with them, after the 2D batcher.

## Options

### What a viewport is

1. **A camera with a target** (Bevy). Ordering by a global integer; render-to-texture and
   window output are different targets.
2. **A render target with settings and a parent** (Godot) (chosen, with changes):
   - a persistent texture;
   - an update mode;
   - an optional parent (the viewport that shows it);
   - an optional placement in the window;
   - a scaling mode.

   Cameras and scene content stay with the caller (a content callback per viewport),
   because the scene model (ECS, §17 step 4) does not exist yet.

### Rendering several viewports in one frame

1. **One render graph per viewport** (Bevy's sub-graphs, run in camera order). No
   cross-viewport aliasing or global barrier derivation.
2. **All viewports in the frame's one graph** (chosen):
   - children declared before parents;
   - each viewport's texture imported, written by its content, then "published" by a no-op
     pass that declares a sampled read in every always-available shader stage;
   - later passes that sample it through bindless (without declaring it) run after the
     transition.

### Showing a viewport

1. **Special nodes or targets per use** (Godot's `ViewportTexture`, Bevy's image targets).
2. **Its bindless index** (chosen). Any renderer that takes a texture index can show it:
   2D sprites, UI, and later 3D materials.

### Per-viewport profiling

1. **Timestamps around each viewport** (Godot's `measure_render_time`). Duplicates the
   graph's own timing.
2. **Graph scopes** (chosen):
   - `FrameGraph::begin_scope(id, label)` / `end_scope`;
   - passes carry the scope: a debug label around them in captures, and
     `PassTime::scope` in the GPU timings;
   - `Viewports::gpu_time_us` sums a viewport's passes.

## Decision

- **`engine_render::viewport::Viewports`:**
  - `create(ViewportDesc) -> ViewportId`; `destroy`; `desc_mut`; `request_update`;
  - `texture_index` (bindless); `extent`; `gpu_time_us`;
  - `render(fg, device, allocator, bindless, window, frame_number, content)`;
  - `composite(fg, tonemap, bindless, output, extent, frame_number, clear)`;
  - `destroy_all`.
- **`ViewportDesc`:**
  - `name`, `format` (HDR by default);
  - `size`: `Fixed { width, height }`, or `Placement { scale }` (the placement's pixels ×
    scale, the hook for upscalers);
  - `update`: `Always`, `Every { n, offset }`, `OnRequest`, `Disabled`;
  - `parent`, `placement` (a normalized rectangle plus an order);
  - `scaling`: `Stretch`, or `PixelPerfect` (largest integer scale, centered, nearest,
    letterboxed);
  - `exposure`, `tonemap`, `clear`.
- **Textures:**
  - created on first render; recreated on resize, with the old one freed after the frames
    in flight;
  - `COLOR_ATTACHMENT | SAMPLED | TRANSFER_SRC | TRANSFER_DST` usage;
  - resting in `READ_ONLY_OPTIMAL` between frames, so anything may sample them at any
    time.
- **A viewport renders at least once before it is shown**, whatever its mode except
  `Disabled`. `texture_index` is `None` until then.
- **Composition** uses `Tonemap::add_composite_pass`: a rectangle, a sampler, exposure,
  tone map or copy, and a load operation. The first placed viewport clears the output.
- **Graph additions:**
  - `FrameGraph::begin_scope` / `end_scope`;
  - `PassTime::scope`;
  - `ShaderStages::ALL` (vertex, fragment, compute: stages every device has);
  - `TextureState::undefined`.
- **`engine_render2d` API change:** `Canvas::prepare` is explicit and `Renderer2d::add_pass`
  takes `&'g Canvas` (it returns `Render2dError::NotPrepared` otherwise). Content callbacks
  over several viewports can then choose among canvases. Also new: `Quad::solid`, a
  hard-edged untextured rectangle for pixel art.

## Measurements

`crates/engine_render2d/tests/viewports.rs` on lavapipe, validation plus synchronization
validation, 0 errors. Four frames at 128 × 128:
- **Nesting and render tick:**
  - the world (left half, placement-sized 64 × 128) shows a 32 × 32 minimap (its child,
    updated on odd frames);
  - frame 2 shows the frame-1 minimap (kept between updates);
  - frame 3 shows the minimap re-rendered earlier in the same frame (children first);
  - frame 4 keeps it.
- **Pixel-perfect:** a 16 × 16 viewport in the right half (64 × 128) is scaled 4× to
  (64, 32)–(128, 96), with exact pixel blocks and black letterbox above and below.
- **Per-viewport GPU time:** the minimap reads 0 µs in the frame after one it skipped, and
  non-zero otherwise.

`tests/viewports_zero_alloc.rs`: 100 frames of three viewports (a ticked child, split
screen, pixel-perfect) through graph, 2D content and composite: 0 heap allocations.

The windowed `viewports` example also runs in the GPU workflow on NVIDIA and AMD with
validation:
- a 3D scene with a 2D minimap child;
- pixel art;
- a picture-in-picture 3D view updated every third frame;
- per-viewport GPU times in the title and output.

ref-1 (GPU workflow on the PR, dev build with validation), per-viewport GPU time in µs:

| viewport | NVIDIA RTX 4060 Ti | AMD Radeon iGPU |
|---|---:|---:|
| 3D scene | 50.2 | 822 |
| minimap (2D child) | 5.7 | 13.6 |
| pixel art (upscaled) | 2.0 | 5.9 |
| top view (every third frame) | 25.6 | 345.8 |

## Consequences

- **Anything that renders into a `Texture` can be a viewport's content:** the forward
  renderer and the 2D renderer today, and the editor's views later.
- **Visibility layers, per-viewport MSAA, AA and upscaling** are not in this step:
  - layers belong to the scene model (ECS, §17 step 4);
  - AA and upscaling belong to the upscaler work.

  `ViewportSize::Placement { scale }` already renders at a lower resolution and stretches.
- **Sampling a viewport inside a 3D material** waits for textured materials (glTF, §13).
- **A viewport whose content samples its own texture** (feedback) reads last frame's
  contents in the wrong layout: the texture is a color attachment during its own pass.
  Portals that recurse need a copy (ping-pong). Revisit with portals.
