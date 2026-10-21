# 0109. Viewport picking and the selection's outline: the view's commands drawn again into small regions, a selected-only mask

- Status: Accepted (team decisions, 2026-10-21)
- Date: 2026-10-21
- Related:
  - [ADR 0107](0107-editor.md) §7, phase E2 (the viewport tools);
  - [ADR 0076](0076-gpu-memory-budget-and-streaming.md) texture feedback: the template for drawing a
    view's culled commands again, and its readback ring;
  - [ADR 0064](0064-gpu-scene-extraction-runtime.md) the GPU scene's slots, [ADR 0068](0068-skeletal-animation-and-skinning.md)
    skinning before the depth passes, [ADR 0072](0072-frame-renderer.md) render and output
    sizes, [ADR 0085](0085-visibility-buffer-and-meshlets.md) meshlets, [ADR 0087](0087-acceleration-structures.md)
    ray queries, [ADR 0092](0092-custom-materials.md) material types;
  - research: `docs/research/viewport-picking.md`; UX: `docs/research/ux/viewport-tools.md`.

## Context

- **What exists:** the gizmo and keyboard transforms (E2-1, E2-2, E2-2b). The selection is
  picked in the scene tree only.
- **The UX** (`ux/viewport-tools.md`; team decisions 2026-10-21):
  - a click picks the nearest object under the pointer; clicking again at the same spot
    cycles through what is under it; Alt+click lists it;
  - Shift+click adds an object, or takes it out when it is already selected (the tree keeps
    Shift for a range and Ctrl for one row);
  - a drag on empty space draws a box: alone it replaces the selection, with Shift it adds,
    with Ctrl it takes out. It takes objects it touches with a visible pixel; with Alt,
    hidden ones too;
  - the object under the pointer is highlighted before a click;
  - transparent surfaces can be picked, behind a switch that is on by default;
  - the selection's outline is 2 px wide, its hidden parts dimmed.
- **What the renderer offers:**
  - **texture feedback** draws the view's culled commands again (both occlusion phases,
    with their detail levels) into a small target of its own, with `depthVertexMasked` and
    a fragment entry point of its own, and reads its atomics back through a ring. Its cost
    is the vertex work: 8–15 µs on the RTX and 49–204 µs on the iGPU on Sponza (at an
    eighth of the size);
  - **the draw area** keeps the viewport and the scissor apart, and the viewport's offset
    is signed: the view's pixel grid can shift onto a small target, so only a square of it
    is rasterized;
  - **the GPU scene** keeps each slot's mesh, material, pipeline state and batch on the CPU;
    the extraction maps an entity to its slot (`RenderExtract::instance`);
  - **skinned meshes** are skinned into the geometry pool before the depth passes, so every
    pass draws the current pose; masked materials test alpha in the depth variant.
- **Constraints:**
  - a feature costs nothing where it is not used, not even shader registers (CLAUDE.md
    §5): game views must not change;
  - TAA and upscalers render at the render size with a jittered projection;
  - depth targets have no stencil (ADR 0059);
  - the editor's frame takes 80–104 µs of CPU; input is answered within the frame.

## Options

**Front-most picks** (hover, click, a box by visible pixels):

1. **An id target in the view's own prepass** every frame (Bevy's PR, Unity's picking pass
   in every shader): a click reads the frame on screen at once. But the prepass must then
   run in editor views (it is optional, off by default), the view's passes change, and
   4 bytes a pixel are written each frame.
2. **The view's culled commands drawn again into a small region on request** (Blender's
   region, texture feedback's machinery): a 32 × 32 square under the pointer, or the box.
   Nothing changes in the view's passes; the cost is the visible instances' vertex work
   while the editor asks; results come back a few frames later.
3. **CPU rays against copies of the meshes** (Godot, Bevy, Fyrox, Wicked): instant, but
   duplicated geometry, no alpha, no current skinned pose.
4. **Ray queries** (ADR 0087): one tiny dispatch, but optional hardware, and alpha-tested
   surfaces and skinned meshes are wrong for rays.

**Hidden objects and lists** (Alt+box, cycling, Alt+click): the view's lists lack what
occlusion culling dropped. So:

1. **Candidate slots from the CPU:** their world bounds against the region's frustum, drawn
   alone without the depth test, an `atomicMin` of depth per slot (Blender's `PICK_ALL`).
2. **A culling pass of the picker's own** against the region's frustum: no CPU work, but a
   new compute shader and its indirect commands.

**The outline:**

1. **From the view's ids:** the cheapest; shimmers under TAA, hidden parts invisible.
2. **A selected-only mask** with its own depth at the output size, then an edge pass
   (Blender, Fyrox, Godot's PR, bevy_mod_outline): steady, hidden parts can be dimmed.
3. **Stencil** (Wicked): our depth has none.
4. **Vertex extrusion:** needs smooth normals and ignores alpha holes.

## Decision

1. **Picking:** option 2, then 1 for what is hidden, in a `Picker` per editor view
   (`engine_render::pick`).
   - **Front-most:**
     - after the view's forward passes, the view's culled commands are drawn again into a
       region at the render size: a 32 × 32 square centred on the pointer, or the box;
     - opaque and masked surfaces; blended ones too while "pick transparent" is on, with
       depth writes;
     - new entry points (`pickVertex`, `pickVertexMasked`, `pickFragment`,
       `pickFragmentMasked`) write the slot + 1 into R32_UINT, with a D32 depth of their
       own;
     - a square is read back whole (4 KB). A box is reduced on the GPU into a bitset of
       slots, read back as slots / 8 bytes.
   - **Hidden objects and lists:** the editor gives candidate slots (their world bounds
     against the region's frustum). They are drawn alone, from commands written on the
     CPU at their finest level, without the depth test: each fragment an `atomicMin` of
     its depth into its slot's word, or for a box a bit. Sorted by depth, they give the
     list and the next one after the active entity. A culling pass of its own comes if
     the candidates cost too much.
   - **Results** reach the CPU through a ring of readback buffers (frames in flight + 1),
     polled each frame, as texture feedback's.
2. **The editor's use:**
   - while the pointer is over the viewport, it asks for a square whenever the pointer
     moved or the scene changed. The object at its centre, or the nearest within 3 px, is
     the hovered one;
   - a click takes the hovered result when it is for the clicked pixel (the frame on
     screen); otherwise it asks for one and selects when that arrives;
   - Shift, Ctrl and Alt as decided above; the gizmo's handles come first (tested on the
     CPU, as now);
   - slots map back to entities through the extraction.
3. **The outline:** option 2.
   - The selected instances (the active one a class of its own) and the hovered one are
     drawn alone (commands written on the CPU, the finest level, no jitter) at the output
     size, into an R8_UINT class mask with a depth of its own.
   - An edge pass blends the outlines over the frame, below the editor's panels: 2 logical
     pixels; the selection in orange, the active entity brighter, the hovered one in a
     thin light line.
   - Where the mask's depth lies behind the scene's (its resolved depth, at the render size,
     with a small tolerance), the outline is at 35 %.
   - The pass is scissored to the outlined objects' bounds on the screen.
4. **What pays:** only editor views. The pipelines are made the first time the editor asks;
   no existing entry point or pipeline changes (checked with `--pipeline-stats` on both
   GPUs).
5. **The player** gives an overlay its view's culled commands, depth, camera and sizes, the
   geometry, material and GPU scene frames (an extension of `OverlayPass`), and an entity's
   slot through its extraction.

## Consequences

- **Latency:** a hover result is 2–3 frames old; a click on a pointer that just moved waits
  that long (under 50 ms at 60 frames a second).
- **Lights, cameras and empties** have no geometry: they are picked in the tree until their
  icons come (the overlays of E2).
- **Custom material types' own vertex code** (ADR 0092) is not run by the pick passes, as in
  texture feedback: picks follow the built-in positions.
- **Meshlet-drawn instances** (ADR 0085): the region redraw draws the view's meshlet lists
  too, as texture feedback does.
- **An editor that skips frames while idle** must keep drawing while a pick is pending.
- **Costs to measure,** each step in its as-built section:
  - the square's redraw: the visible instances' vertex work, about texture feedback's;
  - the mask: the outlined instances; the edge pass: a scissored full-resolution pass.

## Steps

- **E2-3a:** front-most picks (hover, click, Shift); the player's extension.
- **E2-3b:** the outline (selection, active, hover).
- **E2-3c:** boxes (visible, with Alt hidden too), cycling, Alt+click's list; "pick
  transparent".
