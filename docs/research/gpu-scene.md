# Persistent GPU scene, extraction from the ECS, and the game frame loop

Date: 2026-10-01. Input for [ADR 0064](../adr/0064-gpu-scene-extraction-runtime.md). We
read documentation, talks and permissively licensed code for ideas only. No code was
copied.

## Sources

| Source | Version / date checked | License | What we looked at |
|---|---|---|---|
| Unreal Engine 5, "[Mesh Drawing Pipeline](https://dev.epicgames.com/documentation/en-us/unreal-engine/mesh-drawing-pipeline-in-unreal-engine)", API docs of `FGPUSceneWriteDelegateParams`, `FPrimitiveSceneProxy`; T. Looman, "[Unreal Engine 5.8 Performance Highlights](https://tomlooman.com/unreal-engine-5-8-performance-highlights/)" | docs read 2026-10-01 | proprietary (source under the Epic EULA: **not read**) | GPUScene: a scene-wide primitive buffer indexed by `PrimitiveID`; primitives marked dirty during the frame are gathered and uploaded through scatter-upload buffers (`FScatterUploadBuffer`); `r.GPUScene.UploadEveryFrame` as a debugging fallback |
| Unity, [BatchRendererGroup API](https://docs.unity3d.com/ScriptReference/Rendering.BatchRendererGroup.html), [BRG sample blog post](https://unity.com/blog/engine-platform/batchrenderergroup-sample-high-frame-rate-on-budget-devices), Entities Graphics manual | docs read 2026-10-01 | proprietary / Unity Companion License (**ideas only**) | instance data persistent in a `GraphicsBuffer`, rewritten only where it changed (`SetData` with offsets); Entities Graphics uploads by chunk change versions |
| [Bevy](https://github.com/bevyengine/bevy): retained render world ([0.15 release notes](https://bevy.org/news/bevy-0-15/), [PR #14449](https://github.com/bevyengine/bevy/pull/14449)); GPU preprocessing (`MeshInputUniform`, `previous_input_index`, [PR #17688](https://github.com/bevyengine/bevy/pull/17688), [PR #17717](https://github.com/bevyengine/bevy/pull/17717)); static subtree marking in transform propagation ([PR #18589](https://github.com/bevyengine/bevy/pull/18589), [0.16 notes](https://bevy.org/news/bevy-0-16/)) | 0.15–0.19, read 2026-10-01 | MIT / Apache-2.0 | extraction into a retained render world, previous-frame transforms for motion vectors, the "previous index points two frames back" pitfall, dirty bits up the hierarchy |
| N. Tatarchuk, "Destiny's Multithreaded Rendering Architecture", GDC 2015 | talk | — | extract → prepare → submit; game state snapshotted into render data by jobs; simulation of frame N+1 overlapped with rendering of frame N |
| C. Gyrling, "Parallelizing the Naughty Dog Engine Using Fibers", GDC 2015 | talk | — | frame-centric pipelining (game N, render N−1, GPU N−2), memory tagged by frame |
| U. Haar, S. Aaltonen, "GPU-Driven Rendering Pipelines", SIGGRAPH 2015 | talk | — | instance data resident on the GPU, culling and draw generation on the GPU |
| G. Fiedler, "[Fix Your Timestep!](https://gafferongames.com/post/fix_your_timestep/)", 2004 | article | — | accumulator, fixed steps, interpolation by the remainder |
| ADR 0019 addendum (team, 2026-10-01) | this repository | — | one world and one simulation in singleplayer; the camera updated every rendered frame |

## How they work

### Unreal Engine 5: GPUScene

- Every primitive (and every instance of an instanced primitive) has a slot in scene-wide
  GPU buffers, addressed by `PrimitiveID` / instance id. The slots persist across frames.
- Changes mark primitives dirty during the game frame. Before rendering, the dirty set is
  collected and uploaded with scatter uploads: a compact list of (index, data) pairs plus a
  compute pass that writes them into place. Nothing else is uploaded.
- Removals free slots; the buffers grow by reallocation and copy.
- Shaders, culling and Nanite read primitive data by id, so per-view state (HZB, previous
  frame data for velocity) keys off ids, never off a draw order.

### Unity: BatchRendererGroup and Entities Graphics

- The application owns a persistent `GraphicsBuffer` of instance properties and updates
  ranges of it (`SetData(src, srcStart, dstStart, count)`). The docs stress that data that
  did not change is not written again.
- Entities Graphics drives this from the ECS with chunk change versions: a chunk whose
  transforms did not change is not uploaded. Granularity is the chunk, as in our ECS.

### Bevy

- **Retained render world** (0.15): main-world entities marked `SyncToRenderWorld` get a
  `RenderEntity` counterpart; render data persists, extraction updates it. Before 0.15 the
  render world was rebuilt every frame.
- **GPU preprocessing**: `MeshInputUniform` per mesh instance in a buffer that persists
  across frames, with `previous_input_index` pointing at last frame's entry for motion
  vectors. PR #17688 stopped marking unchanged transforms as changed; PR #17717 then fixed
  motion vectors, because a retained entry's previous index referred to frame N−2. Our
  "settle" step (a slot that stops moving gets its previous transform caught up exactly
  once) avoids that class of bug by construction: last frame's buffer always holds last
  frame's values.
- **Transform propagation** (0.16): a `TransformTreeChanged` marker propagates a dirty bit
  up to the roots, so propagation skips subtrees whose markers did not change: 1.1 → 0.1 ms
  on a 127k-object scene. Writes happen only where something changed.

### Destiny and Naughty Dog: extraction and pipelining

- Destiny extracts the game state the renderer needs into render-owned data with jobs,
  then the game moves on to the next frame while the renderer prepares and submits the
  previous one. Extraction cost follows what is rendered, and it is the synchronization
  point between the two.
- Naughty Dog pipelines whole frames (game, render, GPU in flight at once) with per-frame
  memory, which removes locks but adds a frame of latency unless input is sampled late.

### Fixed timestep

- Fiedler's accumulator: real time accumulates, the simulation consumes fixed steps,
  rendering interpolates between the last two states by the remainder (`alpha`). A cap on
  steps per frame prevents the "spiral of death". Interpolation shows the simulation up
  to one step late, which is why the player's camera must not go through it (ADR 0019
  addendum).

## What we take

- **Stable slots with generations, dirty list, scatter upload** (UE5 GPUScene, Unity BRG):
  instance data in a persistent device buffer, a 16-byte update and 64 bytes of data per
  changed slot, applied by one compute dispatch. A frame without changes uploads nothing.
- **Previous transforms in the scene, kept exact**: the scatter copies the old value into
  last frame's buffer when a slot changes; a slot that stops moving is settled once. This
  replaces the per-view `Temporal` copy of every instance (ADR 0043's "remember
  instances", 5–25 µs on the RTX, 340–500 µs on the Radeon iGPU) and removes Bevy's
  N−2 pitfall.
- **Full copy with swapped roles when most slots changed**: our own addition. With
  everything moving, the scatter moved 1.4× the memory of a draw list (27 MB at 100k
  instances: 145 µs on the RTX); copying the whole array into the buffer that held the
  frame before last and swapping roles moves the same as a draw list (23 µs).
- **Holes parked in a sink batch** instead of a hole check in the culling shader: the
  forward shaders stay unchanged (register rule). Holes are short-lived because a freed
  slot is reused immediately (lowest first).
- **Extraction by change detection and removal logs** (Unity Entities Graphics, Bevy):
  chunk versions find additions and edits; an opt-in removal log per component finds
  despawns and removals without scanning. Cost follows changes plus chunk granularity.
- **Incremental transform propagation that writes only changed values** (Bevy 0.16's
  goal): our chunk change versions give the candidates; recomputation runs top-down by
  depth, walks below a value change only, and never marks a chunk whose values stayed.
- **Fixed step with interpolation at extraction**, the camera outside the tick (ADR 0019
  addendum), input sampled after the frame's wait (CLAUDE.md §7 late input sampling).

## What we do not take (yet)

- **Pipelining simulation and rendering** (Destiny, Naughty Dog). It needs the extraction
  to produce render data the simulation no longer touches (it does: the scene) and a
  render thread; the measured CPU frame of the example is 0.12 ms on the RTX, so there is
  nothing to hide yet, and pipelining adds a frame of queue latency unless input is
  sampled on the render side. Revisit with a heavy simulation.
- **Per-batch slot pages** (UE5 keeps an instance range per primitive): slots are assigned
  lowest first, so instances of different meshes interleave and the culling pass's
  one-atomic-per-wave path sees mixed waves. One batch per mesh × state keeps most waves
  uniform in our scenes; pages per batch are the next step if mixed scenes show it.
- **Parallel extraction and upload** (Destiny's jobs). Single-threaded now; the fully
  dynamic case (everything moves every frame) is where it would pay (ADR 0064 numbers).
- **A retained render world of entities** (Bevy): our renderer has no entity concept; the
  scene's slots are the render-side state.

## Patents

A quick search (Google Patents, 2026-10-01) for persistent instance buffers, scatter
uploads and change-driven uploads found no claims on these techniques, which are
documented since at least 2015 (Haar and Aaltonen) and shipped in open source (Bevy, MIT /
Apache-2.0). The search surfaced **US10672182B2** (Microsoft, "Compact visibility state
for GPUs compatible with hardware instancing", priority 2018-04-19, active to 2038): a
buffer of per-primitive visibility bits across instances, used to render only visible
primitives. This change adds no visibility state; ADR 0035's per-instance occlusion flags
(now keyed by slot) predate it in spirit (two-phase occlusion, 2015) but were not checked
against its claims. Listed as a question for the team.
