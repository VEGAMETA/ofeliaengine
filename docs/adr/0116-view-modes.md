# 0116. The viewport's view modes: wireframe, unlit, colliders

- Status: Accepted (2026-10-22). The owner asked for the modes; their design is decided
  here.
- Date: 2026-10-22
- Builds on: [ADR 0099](0099-player-crate.md) (`ViewFeatures`: a feature off costs
  nothing), [ADR 0109](0109-viewport-picking.md) (the selection outline, an editor pass over
  the frame), [ADR 0092](0092-custom-materials.md) (material types and their roles),
  [ADR 0066](0066-physics-queries-characters-rollback.md) and
  [ADR 0101](0101-mesh-colliders.md) (bodies and colliders), [ADR 0110](0110-blockout-brushes.md)
  (the blockout's colliders overlay)
- Research: [`docs/research/ux/view-modes.md`](../research/ux/view-modes.md)

## Context

- **The owner's request (2026-10-22):** view modes such as wireframe and unlit; the
  colliders of every body shown; lines on surfaces drawn without z-fighting.
- **Every editor has them:** Unreal (Lit, Unlit, Wireframe on Alt+4, 3, 2; Collision on
  Alt+C), Godot (Normal, Wireframe, Unshaded, Lighting, Overdraw), Unity (Shaded, Wireframe,
  Shaded Wireframe, Unlit), Blender (Wireframe, Solid; the wireframe overlay).
- **Constraints:**
  - The renderer's rule: a feature costs nothing where it is not used, not even shader
    registers (CLAUDE.md §5). A lit view must draw exactly as now.
  - The editor's view may render smaller than it shows (TAA and upscalers), with a jittered
    camera.
- **What exists:**
  - per-view features (ADR 0099);
  - the selection outline (ADR 0109), drawn at the output size with the camera without
    jitter, telling hidden parts by sampling the scene's depth;
  - material roles (ADR 0092): the engine's pipelines a material type derives its own from,
    compiled the first time a frame needs them;
  - the blockout's colliders, drawn by the 2D canvas without depth, so they show through
    walls.

## Options

1. **The modes inside the forward pass** (a specialization constant, or variants of its
   shaders): wireframe and unlit as branches of the lit shading.
   - Simple to switch.
   - **It breaks the cost rule:** dead code under a constant raised forward shading's
     registers on AMD (9–13 % slower, ADR 0058).
2. **Their own pipelines, made the first time a view uses them** (chosen):
   - The wireframes are a pass of their own after the frame, as the outline is, and as
     Bevy's wireframe pass is.
   - Unlit is a pass of its own too (decided at V1b): the instances drawn again over the
     frame in their base color and emission. A material role was the first plan: every
     material type would derive one more pipeline, and the forward pass's batching would
     pick it per view, for a debug view.
   - A lit view declares nothing new and draws with untouched shaders.
3. **Debug buffers written by the lit pass** (a G-buffer of base color for unlit, Godot's
   advanced modes). The forward renderer has no G-buffer, so the lit pass would write more
   in every view.

For lines on surfaces:

1. **A depth bias** on triangles drawn in line mode. A constant and a slope-scaled bias apply
   to polygons in line mode, not to API-level lines.
2. **The scene's depth sampled** in the fragment shader against a tolerance, as the
   outline's hidden part and Blender's smooth wires do.
   - Works at any render size, with a jittered or upscaled frame.
   - Gives hidden parts their own look (faint) instead of nothing.
3. **Barycentric coordinates** (`VK_KHR_fragment_shader_barycentric`): the edges as a band
   on the triangle's own surface, which cannot z-fight, anti-aliased, of any width. Needs an
   extension and draws every pixel of every triangle.

## Decision

- **A view has a mode:**
  - `Lit` (as now);
  - `Unlit`: base color and emission, no light, no shadow passes;
  - `Wireframe`: every triangle's edges, lines only;
  - `LitWireframe`: the lit frame with its visible edges.

  The colliders are an overlay over any mode, not a mode.
- **Wireframes** (`engine_render::wire`):
  - The meshes' triangles in line mode, each instance's finest resident level, drawn after
    the frame at the output size with the camera without jitter (the outline's way), through
    the outline's vertex entry and a fragment entry of its own.
  - The scene's depth is sampled against a tolerance that grows with distance: an edge is
    drawn where its surface is the one in front, so it never fights the surface it lies on.
  - In `Wireframe` the view is filled with the background first and every edge is drawn,
    hidden or not, as Unreal's and Godot's are.
  - The device asks for `fillModeNonSolid` where it has it (every desktop GPU); without it
    the editor offers no wireframe.
- **Unlit** (`engine_render::unlit`, decided at V1b): the instances the view sees drawn
  again over the frame, as the wireframes are, with a depth of their own: each material's
  base color plus its emission (clamped to the display's white), over a dark background.
  Materials are read as the engine's own pipelines read them, so a material type shows its
  glTF fallback; the forward pass is untouched.
- **Colliders** (the second step):
  - The physics API gives a shape's edges in its own space, made in Rust from the shape's
    description: a box's twelve edges, a sphere's and a capsule's rings, a hull's and a
    mesh's edges, children placed, a scaled shape scaled.
  - The editor draws them for every body it knows: the blockout's groups, the models'
    colliders (ADR 0101's rules), and in Play the scene's bodies. They are depth tested as
    the wireframes are, their hidden parts faint, colored by how the body moves (static,
    dynamic, kinematic, sensor).
  - They replace the blockout's overlay that shows through walls.
- **The editor:** the viewport's corner shows the mode beside the camera's label, as a menu
  (Godot's Display menu, Unreal's view mode list). Unreal's keys switch it: Alt+4 Lit, Alt+3
  Unlit, Alt+2 Wireframe, Alt+5 Lit + Wireframe; Alt+C turns the colliders on and off. The
  session keeps the choice.

## Steps

1. **V1a:** `engine_render::wire`, the device's line mode, the editor's menu and keys:
   Wireframe and Lit + Wireframe. Golden images on both GPUs; the cost measured in a heavy
   scene on both.
2. **V1b:** `engine_render::unlit` and the editor's Unlit (Alt+3); a render test of its
   colors on both GPUs.
3. **V2a:** line lists in `engine_render::wire`; the blockout's colliders and the
   navigation mesh drawn with them, depth tested; the blockout's X-ray overlay gone; Alt+C.
4. **V2b:** shapes' edges in `engine_physics`, the models' colliders and Play's bodies,
   colored by how they move.

## Consequences

- **Easier:**
  - checking a model's triangles, LOD and colors, and a level's colliders, without leaving
    the editor;
  - the debug buffers Godot and Unreal show (normals, lighting only, overdraw) can come one
    at a time as further modes, each with its own pipelines.
- **Harder:**
  - a material type's own base color code does not run in the unlit view: it shows its
    glTF fallback;
  - the editor writes a draw per visible instance for wireframes each frame on the CPU,
    from the slots' boxes. The GPU's culled draws are the faster path when scenes grow:
    revisit with the measurement of V1a.
- **To revisit:**
  - Thicker or smoother lines (barycentric wireframes) if one-pixel lines read badly on
    high-DPI screens.
  - `Wireframe` still renders the lit scene under its background: a view whose mode needs
    no shading skipping the forward pass is a later saving, measured first.

## As built (V1a, 2026-10-22): Wireframe and Lit + Wireframe

- **The device** asks for `fillModeNonSolid` where it has it
  (`OptionalCaps::fill_mode_non_solid`); a pipeline's `polygon_mode` takes `LINE`, and its
  statistics' attachments read `lines`.
- **`engine_render::wire::Wireframer`:**
  - its pipelines are `outlineVertex` with `wireFragment`, in line mode, and a background
    fill (`wire.slang`). No existing SPIR-V changed;
  - `add_passes` writes a draw per instance whose box the view sees (each one's finest
    resident level) and declares one pass, `wireframe`, blending into the frame;
  - `WireStyle::LINES` (light edges on a dark background, every one drawn) and
    `WireStyle::OVER` (dark edges, those behind left out);
  - with the scene's depth, an edge farther than the scene at its pixel by 1 % and 1 cm
    keeps `hidden` of its alpha: the outline's tolerance.
- **The editor:**
  - a chip beside "Perspective" with the modes' menu;
  - `view.lit`, `view.wireframe` and `view.lit_wireframe` on Alt+4, Alt+2 and Alt+5;
  - a click on the viewport's chips picks nothing;
  - the session keeps the mode.
- **Measured** (ref-1, release, `view_modes` `measure_wireframes_over_ten_thousand_cubes
  --ignored`, 10,000 cubes framed at 1920 × 1080, medians of 100 frames):

  | | RTX 4060 Ti | Radeon iGPU |
  |---|---|---|
  | the `wireframe` pass, Wireframe | 0.41 ms | 0.93 ms |
  | the `wireframe` pass, Lit + Wireframe | 0.79 ms | 0.99 ms |
  | composing the frame (CPU), Lit | 97 µs | 102 µs |
  | composing the frame (CPU), with wireframes | 456–498 µs | 399–400 µs |

  A lit view declares nothing new. The CPU's share, about 35 ns an instance (the slots'
  boxes and draws), is the one the GPU's culled draws would take away for larger scenes.
- **Tests:** a render test of the pass on both GPUs under synchronization validation (edges
  in front drawn, one behind a nearer quad not, every one over the background), and the
  editor's frame with the real player in each mode.

## As built (V1b, 2026-10-22): Unlit

- **`engine_render::unlit::Unlit`:**
  - `unlitVertex` and `unlitFragment`, `unlitFragmentMasked` and `unlitFragmentBlended`
    (`forward.slang`), whose material is read through `coverage_context`; the background is
    `wire.slang`'s fill. The existing SPIR-V stayed byte for byte the same;
  - one pass, `unlit`: the background, then the instances the view sees sorted by state
    (blended ones last), each run with its culling and winding, over a depth as large as
    the target (a pass's attachments share its area);
  - the output's white goes in `phase`, as its bits.
- **The editor:** Unlit in the chip's menu and on Alt+3.
- **Measured** (the same 10,000 cubes, 1920 × 1080, medians of 100 frames):

  | the pass | RTX 4060 Ti | Radeon iGPU |
  |---|---|---|
  | `unlit` | 0.28 ms | 1.15 ms |
  | `wireframe`, Wireframe (a second run) | 0.73 ms | 0.94 ms |
  | `wireframe`, Lit + Wireframe | 0.82 ms | 1.00 ms |

  Composing the frame took 123 µs lit and 460–500 µs in the other modes on the RTX
  (128 µs and 395–430 µs on the Radeon). The RTX's two runs of Wireframe measured 0.41 and
  0.73 ms.
- **Tests:** the pass's colors on both GPUs under synchronization validation (base colors,
  one in front of another, a blended one over the background, emission added), and the
  editor's frame in Unlit: one flat color for the cube's every face.

## As built (V2a, 2026-10-22): lines in the world

- **`engine_render::wire::LineLists`:**
  - segments given in the world, in batches (`LineBatch`: the segments, a color, the alpha
    kept behind a surface), at most `MAX_SEGMENTS` (200,000) a frame;
  - `lineVertex` (`wire.slang`) reads each batch's ends where they were pushed into the
    frame's ring (`[Vec3; 2]` as they are: no copy between), through the camera without
    jitter; `wireFragment` tests them against the scene's depth with the wireframes'
    tolerance. One pass, `lines`, a draw per batch, on any device (a line list needs no
    line mode);
  - a type apart from the `Wireframer`: a frame's graph holds each one for the frame;
  - `wire.slang`'s push constants now name the camera (`frame`) and the ends
    (`positions`), so its two entries' SPIR-V changed; no perf scene builds them, and the
    forward pipelines' SPIR-V stayed the same.
- **The editor:**
  - the blockout's colliders, cyan, and the navigation mesh, its boundaries green and its
    other edges faint 2 cm above the surface, are lines in the world over any mode: bright
    in front, faint behind a surface (0.2 and 0.25 of their alpha), steady on the surfaces
    they lie on. The 2D overlay that showed them through walls is gone;
  - each collider edge once (`Blockout::collider_edges`: a face's neighbour listed it
    again);
  - the overlay draws the editor's own lists, made when the brushes or a bake change:
    nothing is copied each frame but into the ring;
  - Alt+C turns the colliders on and off (Unreal's Collision show flag); in a code tab's
    find bar it stays the match-case toggle.
- **Measured** (ref-1, release, `view_modes` `measure_the_collider_lines --ignored`, 1,600
  crates in 16 groups, 19,200 edges, framed at 1920 × 1080, medians of 100 frames):

  | | RTX 4060 Ti | Radeon iGPU |
  |---|---|---|
  | the `lines` pass | 10 µs | 82 µs |
  | composing the frame (CPU), colliders hidden | 121 µs | 136 µs |
  | composing the frame (CPU), colliders shown | 162 µs | 177 µs |

  A first version copied the segments twice each frame and composed in 254 and 269 µs.
- **Tests:** a line list in the render test on both GPUs (whole on the surface it lies on,
  a quarter of its alpha behind a nearer quad); the editor's frame with a crate on a floor
  brush: the colliders' lines drawn, and at most one pixel of 1,500–1,700 changed in the
  frames after; each edge once; Alt+C.
