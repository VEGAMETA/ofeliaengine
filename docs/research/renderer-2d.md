# 2D renderer: batching, shapes, draw order

Checked on 2026-09-26. Everything below was read as code or documentation. No code was
copied: the implementations in `engine_render2d` are our own.

## Sources

| Source | Version | License | What was read |
|---|---|---|---|
| Godot 4, `servers/rendering/renderer_rd/renderer_canvas_render_rd.{h,cpp}`, `shaders/canvas.glsl` | master `46173009dc4a` (2026-09-25) | MIT | instance layout, batch breaking, canvas shader |
| Bevy, `bevy_sprite_render/src/render/{mod.rs,sprite.wesl}`, `bevy_ui_render/src/ui.wesl` | main `0f38358f573a` | MIT / Apache-2.0 | sprite instancing, batching, UI rounded-box SDF |
| Wicked Engine, `WickedEngine/wiImage.cpp`, `shaders/image{VS,PS}.hlsl` | master `2ff1d9e7b360` | MIT | immediate image drawing, corner rounding |
| I. Quilez, "2D distance functions" (iquilezles.org) | article | math (formulas) | rounded box with per-corner radii |
| B. Bilodeau, "Vertex Shader Tricks" (AMD, GDC 2014) | talk | — | generating quads from the vertex ID instead of instancing |
| P. Terdiman, "Radix Sort Revisited" (2000) | article | — | LSD radix sort, one histogram pass, skipping trivial passes |
| F. Giesen, "float->half variants" (2012) | gist, public domain | — | float → half rounding with float arithmetic |
| G. Taubin, "Distance Approximations for Rasterizing Implicit Curves" (1994) | paper | — | first-order distance to ellipses (Bevy's elliptical corners) |

## How the others do it

### Godot 4 (RenderingDevice canvas renderer)

- **Data:**
  - one 128-byte `InstanceData` per rect or primitive, written into a per-frame instance
    buffer;
  - it holds an affine 2×3 world transform, modulation color, nine-patch margins,
    destination and source rects, flags, a material-uniform offset, and four light indices
    (`static_assert(sizeof(InstanceData) == 128)`).
- **Batches** (`Batch`) break on:
  - clip owner, material, and texture (`TextureInfo`: diffuse, normal, specular; no
    bindless);
  - primitive type, lighting, MSDF/LCD text, and blend.
- **Order:** the canvas-item tree order, with `z_index` and optional y-sorting of children.
  There is no global sort by texture, so draw order is preserved and batching depends on
  content order.
- **Shader:** one uber-shader with specialization for nine-patch, MSDF, lights, and
  instancing (particles).

### Bevy (sprites and UI)

- **Sprites:**
  - an instance-rate vertex buffer: a 3×4 transposed affine, color, and UV offset/scale
    (80 bytes);
  - 4 vertices generated from `vertex_index`.
- **Batches** break on image handle change. Sprites are sorted per view by z, then by
  image asset id among equal z, which helps batching but changes the order among equal z.
- **UI:**
  - a separate vertex-based pipeline;
  - rounded boxes with per-corner elliptical radii, borders, and antialiasing, all as an SDF
    in the fragment shader (`sd_rounded_box`, `sd_inset_rounded_box`);
  - elliptical corners use Taubin's first-order distance approximation;
  - antialiasing is `saturate(0.5 - distance)` in pixels, since UI is always in pixel units.
    Their comment notes that `fwidth(distance)` caused artifacts at corners (the distance
    field's gradient is discontinuous there).

### Wicked Engine (`wiImage`)

- **Immediate mode:** one draw per image, parameters in a constant/push block.
- **Corner rounding** builds a triangle-fan geometry with configurable segments per corner
  on the CPU (no SDF).
- **Bindless:** textures are bindless indices in the push data. Batching is not the goal;
  Wicked uses it for UI and debug overlays.

## What we take, what we don't

1. **Bindless textures (ours, following Wicked's direction): texture changes never break
   batches.**
   - Every quad carries its sampled-image index and sampler index.
   - Godot and Bevy break on texture because they bind per batch; both need atlases to
     batch well. With bindless, a scene of sprites from any number of textures is one batch
     per blend pipeline and clip rectangle.
2. **Draw order: layer → key → submission order, never reordered for batching.**
   - Bevy's image-id tiebreak changes the result for overlapping sprites with equal z.
     Godot keeps tree order.
   - Since texture changes are free for us, nothing is gained by reordering. The order is
     fully deterministic (CLAUDE.md §6: "Sorting: layers + y-sort + user key;
     deterministic").
3. **One quad primitive for sprites and shapes (ours; the SDF from Bevy's UI and Quilez).**
   - A 64-byte instance: half-extent axes, center, `f16×4` color, UV rect, texture, flags,
     and `f16×4` corner radii.
   - An SDF flag turns a quad into an antialiased rounded rectangle; a thickness turns it
     into an outline. Circles and capsule lines are rounded rectangles.
   - So sprites and vector shapes share one pipeline and one batch. Godot needs separate
     primitives (polygons, lines, styleboxes as geometry); Wicked builds geometry.
4. **Antialiasing width from the derivatives of the local position, not of the distance.**
   - `sqrt((|ddx(p)|² + |ddy(p)|²) / 2)` is rotation invariant and constant over a quad,
     because `p` is affine. This avoids the corner artifacts Bevy saw with
     `fwidth(distance)`.
   - It works under any zoom, which Bevy's pixel-unit assumption does not.
   - SDF quads grow by one pixel so the outer half of the ramp is not clipped. Sprites do
     not grow, so they stay pixel exact.
5. **Premultiplied output with "additive = alpha 0".**
   - Alpha and additive quads share a pipeline (a classic premultiplied-alpha property), so
     mixing them does not break batches.
   - Multiply needs its own blend state: `dst × lerp(1, color, a)`.
6. **Vertex generation: the vertex index, no vertex buffers.**
   - Bilodeau recommends generating small quads from the vertex ID instead of instancing
     a 4-vertex mesh, because tiny instances pack poorly into vertex waves on AMD
     hardware.
   - We implemented three modes (indexed with a shared `0 1 2 2 1 3` index buffer, a
     6-vertex list, and instancing) and measured them on ref-1 (ADR 0015):
     - indexed is fastest on NVIDIA;
     - list and instancing are fastest on AMD, where indexed is up to 37% slower.

     The renderer picks the mode per vendor.
7. **Radix sort with trivial-pass skipping (Terdiman) over 11-bit digits, stable.**
   - Y-sorting one layer costs three scatter passes over 16-byte entries.
   - In-order input is detected with one comparison per key and not sorted.
   - Zero allocations per frame: the scratch buffer is reused, unlike `slice::sort`, which
     allocates.
8. **Sorting moves 16-byte entries, not 64-byte instances.**
   - Instances are copied into the upload ring in submission order.
   - When sorting moved anything, a 4-byte-per-quad draw-order array is written too, and
     the vertex shader reads `instances[order[i]]`.
   - This replaces a CPU gather with random 64-byte reads. See the ADR for the measurement.
9. **Not taken (yet):**
   - Godot's per-instance light indices and normal/specular maps (2D lighting is its own
     step);
   - nine-patch (a later `Quad` mode);
   - MSDF text (belongs to `engine_text`);
   - particles and tilemaps (chunked, GPU-culled: later steps of CLAUDE.md §6);
   - custom materials: a per-material pipeline slot in the batch state is the planned
     extension.

## Half-precision colors

- Colors and radii are packed as IEEE half floats, which keeps the instance at 64 bytes
  while allowing HDR tints (values above 1).
- `f16` is not stable in Rust:
  - `engine_core::half` converts with F16C when present (runtime dispatch; the baseline
    x86-64-v2 lacks it);
  - otherwise it uses a scalar round-to-nearest-even conversion based on Giesen's
    technique, checked exhaustively against all 65536 halves.
- In shaders, Slang's `f16tof32` declares the `Float16`/`Int16` capabilities, which need
  device features we do not require. `sprite.slang` decodes halves with one multiply that
  rebiases the exponent. Half subnormals may flush to zero, which does not matter for
  colors and radii.

## Pitfalls found

- **Slang maps `SV_VertexID` and `SV_InstanceID` to D3D semantics.**
  - It subtracts `BaseVertex` / `BaseInstance`, so the draw's vertex offset and first
    instance are invisible to the shader.
  - The indexed mode selects the first quad through the vertex offset, so every batch but
    the first drew the first batch's quads again.
  - Fixed with `SV_VulkanVertexID` / `SV_VulkanInstanceID`, found by the pixel test.
- **glam 0.33.10's `camera::rh::proj::vulkan::orthographic` has the wrong Y offset.**
  - It flips the Y scale but not the Y translation, so any projection with
    `bottom + top != 0` is wrong (pixel space: `bottom = height`, `top = 0`).
  - `engine_core::math::vulkan_orthographic` now composes the DirectX-convention
    projection with a Y flip, with a test for asymmetric bounds.
  - Shadow maps were unaffected: their bounds are symmetric.
