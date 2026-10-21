# Viewport picking and selection outlines

Checked: 2026-10-21 (every fact; sources at the end, cited as [G1] etc.). Input for
[ADR 0109](../adr/0109-viewport-picking.md); the UX is in `ux/viewport-tools.md` §1.

- **How the sources were studied:**
  - read as code (permissive): Godot (MIT), Bevy and bevy_mod_outline (MIT/Apache-2.0),
    Fyrox (MIT), Wicked Engine (MIT), Filament (Apache-2.0);
  - Blender (GPL-2.0-or-later): read for ideas only. **No code was carried over;**
  - Unreal Engine and Unity: studied as products (public docs, API reference pages, PR
    descriptions, issue trackers). No engine code was read.
- **The UX is decided** (`ux/viewport-tools.md` §1, team decisions 2026-10-21):
  - a click picks the nearest object; clicking again at the same spot cycles; Alt+click
    lists what is under the cursor;
  - Shift+click adds an object, or takes it out when it is already selected (the scene
    tree keeps Shift for a range and Ctrl for one row);
  - a drag on empty space draws a box: alone it replaces the selection, with Shift it
    adds, with Ctrl it takes out; it takes objects it touches with a visible pixel, and
    with Alt hidden ones too;
  - the object under the pointer is highlighted before the click;
  - transparent surfaces can be picked, with a switch to turn that off;
  - the selection's outline is 2 px wide, its hidden parts dimmed.
- Guesses are marked **(guess)**.

## 1. Picking

### Godot 4 (master `1b16fd14`, 2026-10-20; release 4.7.2): CPU rays against gizmo geometry

- **Click** (`_select_ray` [G1]):
  - a ray against a `DynamicBVH` of gizmo bounds that the editor keeps itself
    (`gizmo_bvh_ray_query` [G3]);
  - then each gizmo tests ([G2]): icons (lights, particles [G5]) by a rectangle around the
    billboard on screen, lines within 8 px of a projected segment, meshes by the ray
    against a `TriangleMesh` (its own BVH);
  - the nearest hit wins and resolves to the deepest editable node, so an instanced
    scene's root is picked; locked nodes are skipped.
- **History:** until 4.3 the first step was the renderer's BVH. PR #94553 ("Fix gizmos
  without visible geometry not being selectable", merged 2024-08-19, milestone 4.4)
  replaced it: "Node3DEditorViewport was using the render server's BVH to filter out
  nodes, which is not correct for gizmos that have no renderable components" [G6].
  `RenderingServer::instances_cull_ray` / `instances_cull_convex` remain, "primarily
  intended for editor usage" [G7].
- **Box** (`_select_region`): a frustum from the rectangle, queried against the gizmo BVH.
  A node is selected only if **wholly inside**: every triangle vertex
  (`TriangleMesh::inside_convex_shape` [G4]), every segment point, or an icon's origin.
  Occlusion is ignored.
- **List:** Alt+right click lists every node along the ray by depth (`_list_select`).
- **Pickable:** meshes by their resource's triangles, with no alpha and skinned meshes in
  their rest pose (`generate_triangle_mesh()` [G5]); particles by their icon
  (`is_selectable_when_hidden` [G5]).
- **Latency, cost:** none (synchronous on the CPU). A CPU copy of every mesh's triangles
  plus a BVH; the BVH build on gizmo redraws is slow enough that a subdivided `PlaneMesh`
  is replaced by two triangles (a code comment [G5]).
- **Pros:** exact and instant; picks objects without geometry. **Cons:** duplicated
  geometry, no alpha, no current skinned pose.

### Unreal Engine 5 (docs 5.8): hit proxies, a GPU id buffer drawn on demand

- **Hit proxies** [U1]:
  - pickable primitives and editor widgets draw with an `HHitProxy`, whose `FHitProxyId`
    "maps the ID to a color"; `InvisibleHitProxyId` keeps a primitive out of the buffer;
  - `FViewport::GetHitProxy(X, Y)` "returns the dominant hit proxy at a given point";
  - a cached `FHitProxyMap` ("a map from 2D coordinates to cached hit proxies") is dropped
    by `Invalidate` / `InvalidateHitProxy` / `DeferInvalidateHitProxy`;
  - `GetHitProxyMap` copies an area; `EnumerateHitProxiesInRect` lists a rectangle.
- **Priorities** choose among proxies near the click (`EHitProxyPriority`): `HPP_World`
  (default), `HPP_Wireframe` (volumes), `HPP_Foreground`, `HPP_UI` (UI components such as
  the translation widget). So the gizmo is picked through the same buffer **(guess from
  the docs)**.
- **Box** [U2][U3]: by default "only objects with visible pixels are selected";
  `bTransparentBoxSelection` ("Box Select Occluded Objects") adds occluded ones;
  `bStrictBoxSelection` needs an actor "completely within" the box.
- **Also:** "Allow Translucent Selection" [U3]; `bEnableViewportHoverFeedback`, real-time
  hover [U2].
- **Latency:** `GetHitProxy` returns at once, so a stale map is drawn and read
  synchronously **(guess from the API; not documented)**.
- **Pros:** one mechanism for meshes, widgets and sub-elements; boxes by visible pixels;
  hover. **Cons:** a separate draw, and a stall on a stale map **(guess)**.

### Unity (docs 6.5–6.6): a picking pass in every shader

- **The pass** [Y1]:
  - "Each rendering must write out a Vector4-typed selectionId to the picking render
    texture"; "Most Unity shaders have a "ScenePickingPass" pass that can be used for the
    rendering" (`HandleUtility.RegisterRenderPickingCallback`, through which custom
    geometry joins);
  - HDRP added the pass in 2020, so picking follows each material's culling, vertex
    displacement and tessellation; decal projectors were left unpickable (PR #2234 [Y6]).
- **API** [Y1]: `HandleUtility.PickGameObject(position, …)` picks the "game object closest
  to specified position" and "must not be called during a repaint" (mouse events are
  advised); it returns at once: synchronous **(guess)**. `PickRectObjects(rect)` picks
  "GameObjects that lie within" a rectangle; occlusion is not documented.
- **UX** [Y2]: repeated clicks on a spot cycle; Ctrl+right click opens the "selection
  piercing menu" of everything at that point; "Scene picking controls" make objects
  unpickable.
- **Pros:** the material's own vertex and alpha code decides. **Cons:** an extra pass in
  every shader.

### Blender (main `20a8fb43`, 2026-10-20): GPU select of a small region, per-object results

Ideas only (GPL); no code was carried over.

- **The region** (`view3d_gpu_select_ex` [B3]):
  - the scene is drawn again into 12 px around the cursor (a click) or the rectangle (a
    box);
  - the view is set up for that region, so culling drops everything outside;
  - it is depth-tested unless X-ray is on: "allowing you to see and select items that would
    otherwise be occluded" [B5].
- **"Select-Next"** [B1][B2]: selection variants of the shaders write into a storage buffer
  indexed by a contiguous per-object id. No id texture is read back:
  - box (`SELECT_ALL`): `atomicOr`, one bit per object (optionally within a circle);
  - all under the cursor (`PICK_ALL`): `atomicMin` of each object's nearest depth;
  - nearest (`PICK_NEAREST`): `atomicMin` of 24-bit depth << 8 | ring distance to the
    cursor;
  - "in front" objects have their depth divided by 100 so they win;
  - the CPU reads the buffer at once after a barrier: synchronous.
- **Cycling** [B3]: a click without pointer motion since the last switches to `PICK_ALL`;
  the hits sorted by depth give the one after the active object. Alt+click lists up to 22.
- **Pros:** exact for every drawable (curves, point clouds, bones as sub-ids); cheap, as
  only the region is drawn; results of a few KB. **Cons:** a redraw per pick, read
  synchronously.

### Bevy 0.20 (2026-10-08): CPU mesh ray casts; GPU picking not merged

- **`MeshRayCast`** [V1]: a parallel ray–box test over all mesh entities, sorted; then
  Möller–Trumbore over **every triangle** of the asset's positions, skipping meshes whose
  box starts past the nearest hit. No per-mesh BVH, skinning, morphs or alpha; the CPU copy
  must be kept (`RenderAssetUsages::MAIN_WORLD`).
- **PR #8784** ("GPU Picking v2", closed unmerged 2024-08-08 [V3]): a mesh id attachment
  in the main pass, `Rg16Uint` (at most 65,536 visible entities); frame N renders, N+1
  maps ("could block"), N+2 answers: "the exact entity under the mouse might not be the
  same as the one visible".
- **Readback** [V2]: pooled staging buffers, `map_async`, a channel polled each frame.

### Fyrox (master `61befd64`, 2026-10-20; v1.0.0): CPU rays over the renderer's draw data

- **Click** (`CameraController::pick` [F1]): a recursion over the whole graph with a box
  test per node; `precise_ray_test` gathers the node's render data (the bundles the
  renderer draws) and tests every triangle. Nodes without a hull (lights, cameras, sounds)
  are hit through a 0.33 m box; prefab roots are preferred to their children.
- **Box** (`select_mode.rs`): world boxes projected to the screen; any overlap selects, so
  occluded objects are selected too.

### Wicked Engine (master `4f93f3fd`, 2026-10-19): CPU rays with CPU skinning

- **Meshes** (`Scene::Intersects(Ray)` [W1]): a loop over object boxes, then the triangles
  (through the mesh's BVH if built). **Skinned and soft-body vertices are skinned on the
  CPU**, so the current pose is picked. No alpha.
- **Icons:** lights, decals and probes by the ray's distance to their position, under 5 %
  of the camera distance. Picking runs every frame, for hover.

### Summary

| | Approach | Alpha-tested | Skinned pose | Icons, particles | Box | Latency |
|---|---|---|---|---|---|---|
| Godot | CPU: gizmo BVH + triangles | no | rest | icon rectangles | frustum, wholly inside, through | 0 |
| Unreal | GPU hit proxies, cached | **(guess: yes)** | **(guess: yes)** | widgets are proxies | visible pixels; through and strict options | sync **(guess)** |
| Unity | GPU `ScenePickingPass` | yes | yes | callbacks | not documented | sync **(guess)** |
| Blender | GPU region, per-object atomics | **(guess: yes)** | yes | all draw ids | bitmap; depth or X-ray | sync |
| Bevy | CPU boxes + all triangles | no | bind | sprite and UI backends | — | 0 |
| Fyrox | CPU draw data + triangles | no | **(guess: bind)** | 0.33 m box | projected box overlap | 0 |
| Wicked | CPU boxes + BVH, CPU skinning | no | current | ray distance | — | 0 |

## 2. Selection outlines

- **Unity** [Y2]–[Y5][Y7]:
  - since 5.5: orange for the selection, blue for children (Preferences), plus an optional
    "Selection Wire";
  - drawn by an internal "SceneView Selected" shader of seven passes, undocumented (a mask
    plus dilation or edges **(guess)**); `Handles.DrawOutline(objects, parentColor,
    childColor, fillOpacity)` draws one for tools;
  - drawn over occluders, the occluded part in another colour since 2021.2 (issues 1119607,
    1325017): both pages were unreachable on 2026-10-21, known from search snippets only.
- **Unreal:** `bUseSelectionOutline` and `SelectionHighlightIntensity`, an overlay tint
  [U2]. The implementation is not in public docs. In games the same look is custom
  depth/stencil plus a post-process material (a community technique).
- **Blender** (the overlay engine's `Outline` [B4]):
  1. a prepass draws only the selected objects into a 16-bit uint id texture with its own
     32-bit depth; the top 2 bits are the colour class (selected, active, transformed);
  2. a resolve pass marks edges where neighbouring ids differ (`textureGather` of four
     neighbours); "thick" mode looks 2 px out; lines are smoothed along the edge;
  3. where the outline's depth is behind the scene's, its alpha drops to **0.35** (1.0 in
     X-ray).
- **Godot:** master has no mesh outline, only selection boxes plus an X-ray copy without
  the depth test at 15 % alpha [G3]. PR #109595 (open since 2025-08-13; proposal #2795
  since 2021) adds one [G8]: a SubViewport with only the selected geometry (unshaded, 2×
  MSAA), then an 8-neighbour edge shader over the view, so the outline shows over
  occluders.
- **bevy_mod_outline 0.13** [V4]:
  - *vertex extrusion* is "generally more performant, but is sensitive to the fidelity of
    meshes and requires smooth outline normals";
  - *jump flood* is "more robust especially with thicker outlines" but "requires `log2(n)`
    shader passes" over the outline's bounding box, n the width in pixels;
  - drawn after the main 3D pass "using a separate depth buffer", so other geometry does
    not clip it.
- **Fyrox** [F1]: the selected nodes drawn again into RGBA8 with a fresh depth, then a
  full-screen 3×3 Sobel on alpha. **Wicked** [W1]: a stencil reference on selected
  objects, a stencil-equal fill into a mask, then a 1 px Sobel (only the visible part is
  outlined **(guess)**).
- **Jump flood against a neighbourhood:**
  - JFA (Rong & Tan, I3D 2006 [J1]): the nearest seed in `log2(n)` passes of 9 reads.
  - Ben Golus, 2020 [J2] (RTX 2080 Super, 1920×1080): naive brute force reads (2r+1)²
    texels per pixel, "at 20 pixels it's nearly 10 ms"; at 1 px "the un-optimized brute
    force is faster than all of the options", beyond that JFA wins, full-screen outlines
    "for less than 1 ms".
  - **For the 1–3 px outlines an editor draws,** one 3×3 or 5×5 pass is enough; JFA pays
    only for wide outlines.

## 3. GPU readback of one pixel or a rectangle (Vulkan)

- **Steps** [K1][K2]: copy (`vkCmdCopyImageToBuffer`, or a shader writing to a buffer); a
  barrier to `HOST`/`HOST_READ`; wait on the frame's fence or timeline value; invalidate
  unless the memory is host-coherent; read.
- **Latency elsewhere:** Godot's `texture_get_data_async` answers after `frame_queue_size`
  frames, 2 by default [G9]; Bevy's PR 2 frames [V3]; Filament's `View::pick(x, y,
  callback)`: "Typically, it takes a couple frames to receive the result", with the
  entity, its depth and the fragment's coordinates; transparent picking is off by default
  because it "will create an extra pass" [M1]. Blender reads synchronously.
- **Avoiding stalls:** a ring of readback slots, one per frame in flight; polling, not
  waiting; copying only the cursor's neighbourhood, or reducing on the GPU first
  (Blender's bitmap and `atomicMin`); results accepted as 1–2 frames old.
- **Ours already:** the allocator's `Readback` location (ADR 0010); texture feedback's
  readback ring, "results a frame-in-flight later" (ADR 0076); timeline semaphores, so
  results are polled without blocking.

## 4. What fits us

- **Texture feedback (ADR 0076) is the template.** It draws the view's culled commands
  again (`ForwardPasses`, both occlusion phases, with their detail levels) into its own
  small target with `depthVertexMasked` and its own fragment entry point, keeps results
  with atomics, and reads them back through a ring. On Sponza it costs 8–15 µs on the RTX
  and 49–204 µs on the iGPU at an eighth of the size: the cost is the vertex work.
- **A small region at full resolution** is a viewport offset: our draw area keeps the
  viewport and the scissor apart, and the viewport's offset is signed, so the view's pixel
  grid shifts onto a 32 × 32 target and only that square is rasterized.
- **The GPU scene keeps, per slot on the CPU,** its mesh, material, pipeline state and
  batch (ADR 0064), and the extraction maps an entity to its slot
  (`RenderExtract::instance`): a few chosen slots (the selection) can be drawn alone from
  commands written on the CPU.
- **Skinned meshes** are skinned into the geometry pool before the depth passes (ADR 0068),
  so every pass draws the current pose; **masked materials** have their alpha test in the
  depth variant.
- **Ray queries** (ADR 0087) are optional hardware, and alpha-tested surfaces are solid to
  them; **CPU rays** would need CPU copies of every mesh. Neither covers alpha or the
  current skinned pose.
- **Views** under TAA and upscalers render at the render size with a jittered projection
  (ADR 0072): a pick reads the render size (a jitter under a pixel is harmless for a
  click); an outline drawn from the jittered ids would shimmer **(guess)**, so it is drawn
  at the output size without jitter. Depth targets have no stencil (ADR 0059).

## What we take

See ADR 0109:

- **picking by redrawing the view's culled commands** into a small region (a square under
  the pointer, the box), with entry points of its own writing slot ids, read back through
  a ring (Blender's region, texture feedback's machinery);
- **the hidden and the listed** (Alt+box, cycling, Alt+click) by drawing candidate slots
  without the depth test, an `atomicMin` of depth per slot (Blender's `PICK_ALL`);
- **the outline from a selected-only mask** with its own depth at the output size, then a
  2 px edge pass, hidden parts at 35 % (Blender, Fyrox, Godot's PR, bevy_mod_outline).

## What we do not take

- **CPU rays against copies of the meshes** (Godot, Bevy, Fyrox, Wicked): duplicated
  geometry, and neither alpha nor the current skinned pose.
- **An id attachment in the view's own passes** (Bevy's PR, Unity's picking pass in every
  shader): the view's prepass would have to run in the editor, the view's shading would
  change, and the outline from it would shimmer under TAA.
- **Jump flood** for now: the 2 px outline is one pass.
- **Synchronous readback** (Blender): a click waits a frame or two instead of stalling the
  GPU.

## Sources (all checked 2026-10-21)

Godot paths are at https://github.com/godotengine/godot/blob/1b16fd14e4ce73de1113ea04479fd09b227e9cd7/
(MIT; master 2026-10-20; release 4.7.2-stable, 2026-08-18). Blender's at
https://github.com/blender/blender/blob/20a8fb43b03b8e01e322d2c49f4545b6c63f1083/ (GPL, ideas only).

- [G1] `editor/scene/3d/node_3d_editor_viewport.cpp`. [G2] `…/node_3d_editor_gizmos.cpp`.
  [G3] `…/node_3d_editor_plugin.cpp`. [G4] `core/math/triangle_mesh.cpp`.
  [G5] `editor/scene/3d/gizmos/{mesh_instance_3d,light_3d,gpu_particles_3d}_gizmo_plugin.cpp`.
- [G6] https://github.com/godotengine/godot/pull/94553 (merged 2024-08-19, milestone 4.4).
  [G7] `doc/classes/RenderingServer.xml`.
- [G8] https://github.com/godotengine/godot/pull/109595 (open; head `0bc87d0f`, 2026-06-27);
  https://github.com/godotengine/godot-proposals/issues/2795 . [G9] `doc/classes/RenderingDevice.xml`,
  `doc/classes/ProjectSettings.xml`.
- Unreal and Unity: proprietary documentation, read as product docs. Unity's Graphics PR and
  issue pages: descriptions only.
- [U1] UE 5.8 API: https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Engine/HHitProxy ,
  `…/FHitProxyId`, `…/EHitProxyPriority`, `…/FViewport`, `…/FViewport/GetHitProxy`.
  [U2] `…/API/Editor/UnrealEd/ULevelEditorViewportSettings`.
  [U3] https://dev.epicgames.com/documentation/unreal-engine/quick-settings-in-the-unreal-engine-level-toolbar
- [Y1] Unity 6.6 API: https://docs.unity3d.com/ScriptReference/HandleUtility.PickGameObject.html ,
  https://docs.unity3d.com/ScriptReference/HandleUtility.RegisterRenderPickingCallback.html ,
  `…PickRectObjects`. [Y3] `…/Handles.DrawOutline.html`.
- [Y2] Unity 6000.5 manual: https://docs.unity.com/en-us/engine/6000.5/manual/working-with-scenes/scenes-manage-gameobjects/scene-picking/select-game-objects.md ,
  `…/selection-piercing-menu.md`. [Y4] https://docs.unity3d.com/2023.2/Documentation/Manual/GizmosMenu.html
- [Y5] Unity issues 1119607, 1325017 (https://issuetracker.unity3d.com/issues/game-objects-selection-outline-doesnt-clip-behind-other-objects ;
  unreachable, snippets only). [Y6] https://github.com/Unity-Technologies/Graphics/pull/2234
  (merged 2020-10-27; description only). [Y7] https://discussions.unity.com/t/sceneview-selected-shader-is-expected-to-have-7-passes-unityeditor-dockarea-ongui/648542
- [B1] `source/blender/gpu/intern/gpu_select.cc`, `gpu_select_next.cc`.
  [B2] `source/blender/draw/engines/select/select_instance.hh`, `shaders/select_lib.glsl`.
  [B3] `source/blender/editors/space_view3d/view3d_view.cc`, `view3d_select.cc`.
  [B4] `source/blender/draw/engines/overlay/overlay_outline.hh`, `shaders/overlay_outline_detect_frag.glsl`.
  [B5] Manual 5.2 LTS: https://docs.blender.org/manual/en/latest/editors/3dview/display/shading.html
- [V1] Bevy v0.20.0 (MIT/Apache-2.0): `crates/bevy_picking/src/mesh_picking/`. [V2]
  `crates/bevy_render/src/gpu_readback.rs`. [V3] https://github.com/bevyengine/bevy/pull/8784 .
  [V4] https://github.com/komadori/bevy_mod_outline 0.13.0 (`61151fbd`, 2026-07-09; MIT OR
  Apache-2.0): README, `src/lib.rs`, `src/flood/*.wgsl`.
- [F1] https://github.com/FyroxEngine/Fyrox (MIT; `61befd64`, 2026-10-20): `editor/src/camera/mod.rs`,
  `editor/src/interaction/select_mode.rs`, `editor/src/highlight.rs`.
- [W1] https://github.com/turanszkij/WickedEngine (MIT; `4f93f3fd`, 2026-10-19): `Editor/Editor.cpp`,
  `WickedEngine/{wiScene.cpp,wiRenderer.cpp,shaders/outlinePS.hlsl}`.
- [M1] https://github.com/google/filament (Apache-2.0; `d4cd5953`, 2026-10-20): `filament/include/filament/View.h`.
- [J1] G. Rong, T.-S. Tan, "Jump flooding in GPU with applications to Voronoi diagram and
  distance transform", I3D 2006, pp. 109–116, doi:10.1145/1111411.1111431.
- [J2] B. Golus, "The Quest for Very Wide Outlines", 2020-07-18,
  https://bgolus.medium.com/the-quest-for-very-wide-outlines-ba82ed442cd9 (read through the
  author's RSS feed; Medium returned 403).
- [K1] https://github.com/KhronosGroup/Vulkan-Docs/wiki/Synchronization-Examples (host
  readback). [K2] https://gpuopen-librariesandsdks.github.io/VulkanMemoryAllocator/html/usage_patterns.html
- Ours: ADRs 0010, 0059, 0064, 0068, 0072, 0076, 0087, 0107; `ux/viewport-tools.md`.
