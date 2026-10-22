# The viewport's view modes

Checked: 2026-10-22. Products were studied as products (their documentation); Godot (MIT),
Bevy (MIT or Apache-2.0) and Blender (GPL, ideas only) were read for how they draw. No code
was carried over. Asked for by the owner (2026-10-22): wireframe and unlit, the colliders of
every body shown, and lines drawn on surfaces without z-fighting.

## How editors do it

| Editor | Modes | Colliders | Lines on surfaces |
|---|---|---|---|
| Godot 4 ([`Viewport.DebugDraw`](https://docs.godotengine.org/en/stable/classes/class_viewport.html)) | the viewport's Display menu: Normal, Wireframe ("objects are displayed as wireframe models"), Overdraw (additive, semi-transparent), Lighting ("without textures and only with lighting information"), Unshaded ("without light information"); a Display Advanced submenu of buffers (normals, GI, shadow atlases, SSAO, clusters, motion vectors…) | collision shapes are gizmos of their nodes, drawn in the editor; Debug > Visible Collision Shapes while the game runs | wireframe pipelines use the rasterizer's line mode |
| Unreal 5 ([view modes](https://dev.epicgames.com/documentation/en-us/unreal-engine/viewport-modes-in-unreal-engine), [show flags](https://dev.epicgames.com/documentation/en-us/unreal-engine/viewport-show-flags-in-unreal-engine)) | Lit (Alt+4), Unlit (Alt+3, "Base Color only"), Wireframe (Alt+2, every polygon edge), Detail Lighting (Alt+5), Lighting Only (Alt+6), Light and Shader Complexity (Alt+7, Alt+8), buffer visualization | the Collision show flag (Alt+C) over any mode; Player Collision and Visibility Collision view modes show only what blocks a pawn or visibility | — |
| Unity 6 ([draw modes](https://docs.unity3d.com/Manual/overlays-reference-draw-modes.html)) | Shaded, Wireframe, Shaded Wireframe ("textured and with wireframes overlaid"), Unlit, and Debug modes (overdraw, mipmaps, lighting buffers…) | colliders draw their wireframe gizmo when selected; the Physics Debugger shows them all | — |
| Blender | the viewport's shading: Wireframe, Solid, Material Preview, Rendered; the Wireframe overlay over the others | — | a depth offset toward the viewer ([2019](https://developer.blender.org/rB600da00a94e6b7ad65daf3a077353c8837b582bf)); then wire vertices nudged half a pixel along the normal and toward the camera; with smooth wires, the scene's depth copied and sampled instead ([commit](https://developer.blender.org/rBe000dcb8490880d6d49aa91588c457612685e9f1)). Up close the offset shows; without it the wire flickers |
| Bevy 0.17 (`bevy_pbr::wireframe`) | a `Wireframe` component per entity, or all of them (`WireframeConfig::global`), a color each, an X-ray mode | — | its own pass after the main one, on the view's color and depth; `PolygonMode::Line`, the depth bias's slope at 1 for triangles; X-ray: depth test off. Needs the device's line polygon mode |

## What the graphics APIs give

- **A triangle drawn in line mode** (Vulkan's `polygonMode = LINE`, the `fillModeNonSolid`
  feature: every desktop GPU) is still a polygon to the depth bias: a constant and a
  slope-scaled bias apply to its fragments. An API-level line (a line list) takes no depth
  bias (Direct3D 9's documentation says so; Mesa's Asahi driver
  [follows it](https://gitlab.freedesktop.org/asahi/mesa/-/merge_requests/265)).
  Lines in line mode are one pixel wide unless the device has `wideLines`, and aliased.
- **Barycentric coordinates in the fragment shader** (`VK_KHR_fragment_shader_barycentric`,
  Slang's `SV_Barycentrics`) draw a triangle's edges as a band of constant width in pixels,
  anti-aliased by `fwidth`. This is the single-pass wireframe of Bærentzen et al., "Single-pass
  Wireframe Rendering" (SIGGRAPH 2006). The band is on the triangle's own surface, so it
  never z-fights with it.
- **The scene's depth sampled in the fragment shader** decides visible against hidden with a
  tolerance, at any resolution (the frame may be rendered smaller and upscaled). This is how
  our selection outline (ADR 0109) tells its hidden part, and Blender's smooth wires.

## What we have

- `ViewFeatures` (ADR 0099) turns the renderer's features on per view; a view without one
  pays nothing: its passes are not declared, its variants not drawn with.
- The selection outline (ADR 0109) is the precedent for an editor pass over the frame: its
  own pipelines, the camera without jitter, at the output size, the scene's depth sampled
  for what is hidden.
- The editor's colliders (ADR 0110 E2b-4) and navigation overlays are lines the 2D canvas
  draws over the viewport, without depth: they show through walls.
- The device does not ask for `fillModeNonSolid`. The physics API (ADR 0066) has no
  geometry for drawing a body's shapes.

## Scenarios

| The developer wants to… | Ours |
|---|---|
| see a model's triangles (how dense, where its LOD changes) | Wireframe (Alt+2): every edge, lines only |
| check a model's colors without the lights | Unlit (Alt+3): base color, emission on top |
| see the triangles over the shaded scene | Lit + Wireframe (Alt+5): edges over the surfaces, hidden ones not drawn |
| find why the player walks through a wall | Colliders (Alt+C) over any mode: every body's shapes, colored by how it moves |
| get back | Lit (Alt+4) |

## What we take

1. **A view's mode** beside its features: Lit, Unlit, Wireframe, Lit + Wireframe. The
   editor's viewport menu shows the current one beside the camera's label, and Unreal's keys
   switch them (Alt+4, Alt+3, Alt+2; Alt+5 for the overlay, which Unreal lacks).
2. **The cost rule:** each mode's pipelines are its own, made the first time a view uses
   them. Lit views draw with nothing new, and their shaders are untouched.
3. **Wireframe:** the meshes' triangles in line mode, through their own vertex path; one color,
   lines only, no depth test, as Unreal's and Godot's show every edge. The device asks for
   `fillModeNonSolid` where it has it; a device without it offers no wireframe.
4. **Lit + Wireframe:** the edges drawn after the scene in line mode, with a depth bias toward
   the camera (constant and slope-scaled, Bevy's slope 1), so lines on a surface win over
   it. Hidden edges are not drawn.
5. **Unlit:** each material's base color (its texture, its tint) and emission, no light,
   no shadow passes. The view's lighting features are off with it.
6. **Colliders** are a second step: the physics API gives each shape's edges (made in Rust
   from its description), the editor draws them for every body, depth tested against the
   scene with a tolerance, hidden parts faint (Unity's and Unreal's colors: static, moving,
   kinematic, sensor). They replace the blockout's X-ray overlay.

## What we do not take

- Godot's and Unity's long lists of debug buffers in the first step: they come one at a time,
  each with its use.
- X-ray lines in Lit + Wireframe (Bevy's option): the colliders' faint hidden parts show
  what is behind.
- Barycentric wireframes in this step: they need an extension, and line mode with a depth
  bias is enough on both reference GPUs. They stay the way to thicker, smooth lines.
