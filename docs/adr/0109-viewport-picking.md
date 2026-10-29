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

## As built (E2-3a, 2026-10-21)

- **`engine_render::pick`:** `Picker::add_pass` draws the view's culled commands (both
  phases, the meshlet lists too) into a 32 × 32 square, the viewport shifted so the
  pixel lands in its middle. The new entry points write the instance index + 1 into
  R32_UINT with a D32 depth; masked materials are alpha-tested; blended ones are drawn
  with depth writes when the pick takes them (on in the editor). The square is copied
  into a ring of readback buffers (frames in flight + 1) and polled.
  - `PickResult::nearest(radius)` takes the pixel's instance, or the nearest within the
    radius (3 px in the editor).
  - `add_pass` says whether it declared the pick, found the view empty (nothing to
    pick), or every readback busy.
  - No other SPIR-V changed: the four entry points are new files, and the shaders' build
    rewrote nothing else, so no other pipeline's registers can move.
- **The player** gives an overlay `OverlayPass::view`: the view's frame (culled commands,
  render and output sizes, depth), its camera and rectangle, the geometry and material
  frames, the frame's number.
- **The editor:**
  - the pointer over the viewport asks for its pixel (at the render size) whenever the
    pointer, the camera or the history's head changed;
  - the overlay polls the picker and declares the frame's pick;
  - after each frame, `sync_assets` names the instance's entity through the extraction
    (the scene's entities and their slots), as the hovered one;
  - a press and release within 4 px, not on a gizmo handle, waits for a pick drawn
    from the next frame on, then selects (Shift or Ctrl: toggles; on nothing: clears,
    unless toggling), opening the tree to the row.
- **Measured** on Sponza, 1920 × 1080, `scene_bench --pick --scene-draws` (the pick and
  its copy, median): 19–20 µs on the RTX 4060 Ti, about 0.5 ms on the Radeon iGPU. The
  iGPU's cost is the visible instances' vertex work (its depth prepass is 0.29–0.94 ms).
  Drawing only candidate instances under the pointer (E2-3c's machinery) would cut it
  most: to measure there.
- **Tested:**
  - on both GPUs, with synchronization validation: an opaque quad picked over the one it
    hides; a masked quad whose alpha is zero picks the one behind it; a blended quad is
    picked only while transparent surfaces are; the background and the view's edge pick
    nothing;
  - the nearest id within the radius;
  - the editor's logic: the pointer's pixel at the view's size, hovered, not asked
    again while nothing changes; a click waits for a pick of its frame or later; Shift
    adds and takes out; a click on nothing clears or, with Shift, keeps; a press on a
    handle and a drag are no clicks;
  - on the player, both GPUs: the pointer over the red cube hovers it, a click selects
    it, a click on the sky clears the selection.
- **Not done:** the outline and the hover's (E2-3b); boxes, cycling and the list
  (E2-3c); the switch for transparent surfaces (on).

## As built (E2-3b, 2026-10-21)

- **`engine_render::outline`:** `Outliner::add_passes` draws the chosen instances (their
  slots) alone:
  - the commands are written on the CPU: each slot's mesh at its finest resident level,
    opaque ones, then masked ones;
  - they are drawn with the view's camera without jitter, both faces, into an R32_UINT
    mask with a D32 depth of its own; new entry points (`outlineVertex`,
    `outlineFragment` and their masked twins) write `(class + 1) << 24 | (item + 1)`;
  - **the edge pass** (`outline.slang`) blends a line around the mask over the target. Each
    class has its width and color, the strongest wins (active, selected, hovered), with a
    soft outer edge. Where the outlined surface lies farther than the scene's depth (1 %
    plus 1 cm, which the scene's lower resolution and jitter need), the line keeps
    `hidden` of its alpha.
- **Bounded on the screen:** each item's world box (its transform as last uploaded and
  its mesh's box) gives its pixels with the lines' margin. Overlapping rectangles merge
  until disjoint (more than 32 become their union). The mask spans their union, the view's
  pixel grid shifted onto it, and the edge pass draws each rectangle once. A box reaching
  behind the camera takes the whole view.
- **The editor:**
  - after each frame it lists what to outline: each selected entity with its children, the
    active one's as the active class, then the hovered entity when it is not among them;
  - the overlay draws the outlines under the panels, 2 logical pixels (1 for the hovered
    one) times the UI's scale;
  - the player's `OverlayView` gives the GPU scene's frame.
- **Measured** on Sponza, 1920 × 1080 (`scene_bench --outline small|large --scene-draws
  --rays`, the mask and the edge pass, medians), as the RTX 4060 Ti, then the Radeon iGPU:
  - the three smallest instances: 6.1 + 10.2 µs, then 63 + 108 µs;
  - the three largest: 57 + 27 µs, then 0.92 + 0.51 ms.

  Before the bounds, the edge pass over the whole view took 135 µs and 3.1 ms, and the mask
  of the smallest 9 µs and 0.22 ms.
- **Tested:**
  - on both GPUs, with synchronization validation: the active color just outside an outlined
    quad, nothing inside it, 35 % behind a nearer quad, the hovered one's thin light line,
    the background untouched;
  - what is outlined: the selection with its children, the active class, the hovered one
    unless selected;
  - on the player, both GPUs: the cube outlined in the active color after the click
    (`ENGINE_EDITOR_PICK_PNG` writes the frame).
- **Not done:**
  - a cheaper edge pass for large objects on the iGPU (tiles classified first, so that
    only tiles on an edge test their neighbours);
  - the outline under MSAA views (the mask is single-sampled, so it does not follow the
    view's own samples).

## As built (E2-3c-1, 2026-10-21)

- **Candidates for every pick of the GPU scene.** The scene keeps each slot's world box
  (over its whole move for an instance moving between ticks) as six arrays; a pick's
  pixels give six planes, and `SceneFrame::candidates` scans the boxes against them
  sixteen at a time, each sixteen left at the first plane none of them reaches. The
  candidates are drawn alone from commands written on the CPU (their meshes' finest
  resident level), grouped by the faces' and the alpha test's state, through the camera
  without jitter.
- **The nearest surfaces from the candidates** (`Picker::add_scene_pass`): the 32 × 32
  square of E2-3a with the same entry points, the region within the view. Measured
  cheaper than drawing the view's culled commands again, so the editor uses it;
  `add_pass` stays for views that draw a list.
- **What lies under a pixel** (`Picker::add_list_pass`, `poll_list`): the candidates
  within a radius (3 px in the editor), drawn without a depth test and without
  attachments, each fragment an `atomicMin` of its depth into its item's word (new entry
  points `pickListVertex`, `pickListFragment` and their masked twins); at most 1,024
  items, sorted front to back on the CPU.
- **The editor:**
  - a plain click within 4 px of the last plain click, while the entity it left active
    still is, asks for the list and selects the next entity behind the active one (the
    nearest when the active one is not there, round to it after the farthest);
  - Alt+click asks for the list and offers it in the palette, nearest first;
  - "Viewport: Pick Transparent Surfaces" switches whether picks take blended surfaces
    (on); the session keeps it.
- **Measured** on Sponza, 1920 × 1080 (`scene_bench --scene-draws --rays --pick
  view|scene|list`, the passes with their copies, medians), the RTX 4060 Ti, then the
  Radeon iGPU:
  - the view's commands drawn again: 18–20 µs, then 508–513 µs;
  - the candidates' square: 7.0 µs, then 134 µs (2.6–3.8 times less), and 1.4–2.5 µs
    on the CPU;
  - the list (4 instances under the view's middle): 10 µs, then 107 µs.

  The scan alone (release, ref-1's CPU): 0.9 µs for 1,000 boxes, 9.8 µs for 10,000,
  111 µs for 100,000; written per box it took 3.4, 35 and 346.
- **Tested:**
  - on both GPUs, with synchronization validation: the square picks a blended quad
    while transparent surfaces count and the opaque one behind it otherwise; the list
    holds the quads under the pixel front to back, hidden ones too, never a masked quad
    whose alpha is zero there or one beside it; a pixel outside the view is empty;
  - the scan and the planes on the CPU;
  - the editor's logic: cycling, round to the nearest, starting over elsewhere or after a
    Shift+click; Alt+click's list in the palette and a pick from it; the switch kept by
    the session;
  - on the player, both GPUs: a click again on the cube selects a cube hidden inside it,
    and another the outer one.
- **Not done:** boxes (E2-3c-2). The select tool's middle handle sits at an object's
  middle and takes a click there before cycling does.

## As built (E2-3c-2, 2026-10-21)

- **A list of any rectangle** (`ListRequest { rect, visible, transparent }`;
  `ListRequest::around` for a pixel and a radius):
  - without `visible`, every surface there, as E2-3c-1's list (a box with Alt);
  - with `visible`, what shows: the candidates' depth first (the existing `depthVertex`,
    and `depthVertexMasked` with `shadowFragmentMasked`, in pipelines of the picker's
    own), then the candidates again over it with the list's vertex shaders and new
    fragment entry points `pickVisibleFragment` and its masked twin, tested before
    shading (`[earlydepthstencil]`, less or equal). The positions are `precise`, so both
    passes rasterize the same depths, as the view's depth prepass and its forward pass do.
    Blended surfaces count with `transparent` and then hide what is behind them, as in
    the square.
- **One atomic per wave:** a box over the whole view put every fragment of an object on
  its item's word. The list's fragments now take the wave's minimum when the wave shades
  one item and write it from one lane, and skip the atomic when the word already holds a
  depth as near (texture feedback's way). A whole-view box on the RTX: 1,035 → 110 µs
  (visible), 3,427 → 95 µs (every surface); the 7 × 7 list unchanged. This changed the
  list fragments' SPIR-V of E2-3c-1.
- **Candidates of one mesh are one instanced draw:** they are sorted by state and mesh,
  and consecutive items share a command.
- **The editor's box:**
  - a press in the viewport not on a handle, dragged past 4 px, draws a box (a light
    frame, dashed with Alt); Escape takes it back;
  - while it is dragged, its box is asked for whenever it changed, one at a time (a slow
    GPU draws a box every few frames, not every frame; one not answered within 8 frames
    is asked again), and what it takes is outlined in the hovered one's line; with Ctrl
    the selected ones it would take out lose their own;
  - at the release its last box is asked again and, answered, replaces the selection,
    adds to it with Shift, or takes out of it with Ctrl; the nearest taken ends active.
- **Measured** on Sponza, 1920 × 1080 (`scene_bench --scene-draws --rays --pick
  scene|list|box|box-hidden`, the passes with their copies, medians), the RTX 4060 Ti,
  then the Radeon iGPU:
  - the square 7.0–7.8 µs, then 130 µs; the list 10 µs, then 112 µs;
  - a box over the whole view (66 instances show, 75 are there): 110 µs and 95 µs, then
    2.2 ms and 3.0 ms; the CPU's part 3–5 µs.
- **Tested:**
  - on both GPUs, with synchronization validation: over the whole view what shows is the
    blended quad, the large one behind and the one aside while transparent surfaces count
    (the blended quad covers the opaque one on the screen and hides it), the opaque one
    instead of the blended one otherwise; every surface there is all four drawn; a box
    around the one aside holds it alone;
  - the editor's logic: a box asked for one at a time, its catch outlined, replacing,
    adding with Shift, taking out with Ctrl, hidden objects with Alt, taken back by
    Escape, no click;
  - on the player, both GPUs: a box over the view takes the outer cube; with Alt the cube
    hidden inside too.
- **Not done:** a box's cost on the iGPU over a whole heavy view (2–3 ms while dragging);
  coarser detail levels or a lower resolution for the box would cut it.

## As built (E2-5, 2026-10-21): the surface under a pixel

- **`Picker::add_surface_pass`:** the candidates under a pixel but the slots it is told to
  leave out (what is being moved over the surface), drawn by the square's entry points
  into a 3 × 3 square whose ids and depths are read back (72 bytes). `poll_surface` gives
  the center's instance and point (its depth unprojected through the view's inverse) and
  the normal across and down from the neighbours on the same instance (one side when the
  other is not on it), facing the view; back along the view where they give none.
- **Below a point:** the same pass through an orthographic camera looking down from the
  point, a centimeter a pixel: the first surface under it.
- **Tested** on both GPUs, with synchronization validation: a crate's face with its normal;
  with the crate left out, the floor behind it where the pixel's ray meets it; the floor
  under a point from above; nothing in the sky (a huge floor's box still reaches the
  sky's pixel's planes one by one, so it is drawn and covers no pixel).
- **The editor** (ADR 0107 E2-5) places on surfaces with it: Ctrl+Shift drags and End.

## As built (E10-4e, 2026-10-29): instances picked as solid

- **`Picker::add_scene_pass(…, solid, …)`:** the slots `solid` holds are taken whatever
  their materials: blended ones even when the request leaves blended surfaces out, masked
  ones drawn by the unmasked entry points. Only the candidates' filter and their state key
  change, on the CPU; no shader, no other pass.
- **Why:** the editor shows a dragged material on the object under the pointer before the
  drop (ADR 0120's as-built E10-4e). A see-through material would make the object vanish
  from its own hover pick, and the material flicker between it and what is behind.
- **Tested** on both GPUs, with synchronization validation: told the blended quad is
  solid, the square picks it while blended surfaces do not count; beside the front quads,
  where only the invisible masked one and the large one behind it show, it picks the large
  one, and the masked one once told it is solid.

## Steps

- **E2-3a:** front-most picks (hover, click, Shift); the player's extension.
- **E2-3b:** the outline (selection, active, hover).
- **E2-3c-1:** cycling, Alt+click's list, "pick transparent"; picks from candidates.
- **E2-3c-2:** boxes (visible, with Alt hidden too); one atomic per wave in lists.
