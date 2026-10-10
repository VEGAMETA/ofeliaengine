# 0064. Persistent GPU scene, extraction from the ECS, incremental transforms, the frame loop

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (its next step 1: "a
  persistent GPU scene: stable instance slots, only changed instances uploaded"),
  [ADR 0035](0035-occlusion-culling.md), [ADR 0043](0043-motion-vectors-and-taau.md),
  [ADR 0017](0017-ecs.md), [ADR 0027](0027-transforms.md),
  [ADR 0019](0019-networking-model.md) and its addendum, [ADR 0066](0066-physics-queries-characters-rollback.md)
  (the fixed-step clock)
- Research: [docs/research/gpu-scene.md](../research/gpu-scene.md)

## Context

- `ForwardRenderer::add_passes` took a fresh `&[DrawItem]` every frame and wrote 64 bytes
  per instance into the upload ring: 0.87 ms of CPU and 6.1 MiB of upload per frame at
  100k instances, **even when nothing moved**.
- Occlusion flags (ADR 0035) and last frame's transforms for motion vectors (ADR 0043)
  were known by draw-list index: their quality depended on the caller keeping the order
  stable, and motion vectors needed a per-view copy of every instance each frame ("remember
  instances": 25 µs on the RTX 4060 Ti, **500 µs on the Radeon iGPU** at 100k).
- The ECS was not connected to the renderer: the editor pushed a static list, the stress
  tool rebuilt a list from a query every frame. `engine_runtime` (CLAUDE.md §2) did not
  exist.
- `TransformPropagation::run` recomputed every `GlobalTransform` when anything moved, which
  also marked every chunk changed, so change detection downstream saw everything.
- The team decided (ADR 0019 addendum): singleplayer is one world and one simulation; the
  player's camera follows input every rendered frame, not every tick.

## Options

**Instance storage**

1. *Per-frame draw list* (as before): simple, no bookkeeping; CPU and upload grow with the
   number of instances every frame.
2. *Persistent GPU scene with stable slots* (UE5 GPUScene, Unity BRG; chosen): instances
   live in device buffers; only changes are uploaded; per-view state keys off slots.
3. *Retained render-world entities* (Bevy 0.15): a second entity world on the render
   side. Our renderer has no entity concept; slots are enough.

**Upload of changes** (option 2)

- a. *Copy regions* (`vkCmdCopyBuffer` with one region per changed slot): no shader, but
  last frame's value must be copied first (two dependent copies per slot) and a driver
  walks every region on the CPU.
- b. *Scatter compute pass* (UE5's `FScatterUploadBuffer`; chosen for few changes): a
  16-byte update per slot plus 64 bytes of data; one thread per update copies the current
  value into last frame's buffer and writes the new one.
- c. *Whole-array copy with swapped buffers* (chosen when at least half of the slots
  changed): the buffer that held the frame before last receives a full copy and becomes
  this frame's; last frame's becomes "previous" without a copy. Measured below: the
  scatter alone moved 1.4× the memory of a draw list at 100 % moving.

**Removed instances**

- *A hole check in the culling shader*: one branch in `cullMain`, but it changes the
  committed forward shader for both paths.
- *Compaction* (move the last instance into the hole): no holes, but slots stop being
  stable.
- *A sink batch* (chosen): holes are parked in batch 0, whose command nothing draws and
  whose box is inside out, so every frustum plane rejects it; a freed slot is reused by the
  next insert (lowest first), so churn leaves no holes. The forward shaders are unchanged.

**Extraction from the ECS**

- *Rebuild every frame* from a query (the old path): cost follows the world.
- *Change detection + removal events* (chosen): `Or<(Changed<GlobalTransform>,
  Changed<MeshRenderer>)>` chunks for additions and edits, an opt-in per-component removal
  log in `World` for despawns and removals, and a resync epoch for clears and snapshot
  restores. Cost follows changes, at chunk granularity.
- *Generational sweep* of all mirrored entities each frame: no ECS change, O(n).

**Transform propagation**

- *Full recompute when anything changed* (ADR 0027).
- *Dirty bits propagated up the hierarchy* (Bevy 0.16): a marker component per entity.
- *Chunk-version candidates, recomputed top-down by depth, walk below value changes only,
  write only changed values* (chosen): no new component, uses the change versions the ECS
  already keeps.

**Interpolation of rendered motion**

- *Write blended poses into `Transform`* (`PhysicsSync::interpolate`, ADR 0066): needs
  propagation every rendered frame and writes simulation state.
- *Blend at extraction* (chosen): the instances that moved in the last tick are blended
  every rendered frame from their last two world transforms; simulation state is never
  written.

**Frame loop**

- *Sequential* (chosen): wait for the frame slot and image, sample input, run the due ticks
  (systems, propagation, extraction), update the camera from that input, interpolate,
  record.
- *Pipelined* (Destiny, Naughty Dog: render N while simulating N+1): hides simulation
  cost, adds a frame in flight. Not taken now (below).

## Measurements

ref-1 (`docs/perf/reference-hw.md`), release, `VK_LOADER_LAYERS_DISABLE=~implicit~`, branch
`feat/gpu-scene`. Medians.

**`cargo run -p engine_render --release --example gpu_scene_bench`**: a cube field,
1920 × 1080 HDR, frustum culling, motion vectors on. *app*: the application's CPU to hand
the frame its instances (building the list, or `set_transform` / `insert` / `remove`);
*renderer*: `add_passes` (list) or `GpuScene::add_passes` + `add_scene_passes` (scene);
*upload*: bytes written into the ring; GPU in µs (*upload*: the scatter or full copy;
*cull* includes waiting for it; *remember*: the draw list's copy of last frame's
instances).

RTX 4060 Ti:

| instances | motion | path | app µs | renderer µs | upload KiB | GPU upload | GPU cull | GPU remember |
|---:|---|---|---:|---:|---:|---:|---:|---:|
| 10k | static | draw list | 13 | 88 | 626 | – | 5.7 | 5.0 |
| 10k | static | GPU scene | 0 | **2** | **1** | – | 5.8 | – |
| 10k | 1 % moving | GPU scene | 2 | **7** | 11 | 5.8 | 11.6 | – |
| 10k | 100 % moving | GPU scene | 47 | 167 | 626 | 5.5 | 7.5 | – |
| 10k | 1 % removed + added | GPU scene | 1 | **8** | 9 | 5.9 | 11.6 | – |
| 100k | static | draw list | 123 | 870 | 6251 | – | 14.6 | 24.6 |
| 100k | static | GPU scene | 0 | **2** | **1** | – | 14.7 | – |
| 100k | 1 % moving | GPU scene | 14 | **46** | 95 | 7.4 | 21.7 | – |
| 100k | 100 % moving | GPU scene | 473 | 1670 | 6251 | 25.1 | 15.7 | – |
| 100k | 1 % removed + added | GPU scene | 20 | **50** | 79 | 6.1 | 19.8 | – |

Radeon iGPU (2 CUs):

| instances | motion | path | app µs | renderer µs | upload KiB | GPU upload | GPU cull | GPU remember |
|---:|---|---|---:|---:|---:|---:|---:|---:|
| 10k | static | draw list | 12 | 68 | 626 | – | 24.4 | **336** |
| 10k | static | GPU scene | 0 | **1** | **1** | – | 24.4 | – |
| 10k | 1 % moving | GPU scene | 2 | **6** | 11 | 2.6 | 26.4 | – |
| 10k | 100 % moving | GPU scene | 46 | 145 | 626 | 19.7 | 49.2 | – |
| 100k | static | draw list | 141 | 596 | 6251 | – | 160 | **501** |
| 100k | static | GPU scene | 0 | **2** | **1** | – | 157 | – |
| 100k | 1 % moving | GPU scene | 20 | **69** | 95 | 19.4 | 179 | – |
| 100k | 100 % moving | draw list | 129 | 601 | 6251 | – | 378 | 501 |
| 100k | 100 % moving | GPU scene | 509 | 1316 | 6251 | 280 | 644 | – |
| 100k | 1 % removed + added | GPU scene | 21 | **64** | 79 | 11.8 | 171 | – |

The draw list's rows are the same for every motion (it rebuilds everything). Shadow and
forward passes are unchanged (same instances, same shaders).

- **Static and mostly static scenes** (the common case): the renderer's CPU per frame
  falls from 870 µs to 2 µs at 100k on the RTX (600 → 2 on the Radeon), the upload from
  6.1 MiB to under 1 KiB, and the GPU no longer copies every instance for motion vectors
  (−25 µs on the RTX, **−0.5 ms** on the Radeon).
- **1 % moving** (1000 instances): 46 µs on the RTX against 870; 95 KiB uploaded.
- **Churn** (1 % removed and added every frame): 50 µs; no holes, since freed slots are
  taken over at once. The first version freed slots only after the upload; the ~1000
  holes broke the culling pass's one-atomic-per-wave path and cost +140 µs of culling on
  the Radeon.
- **Everything moving** is the scene's worst case: 1.67 ms of renderer CPU against
  0.87 ms on the RTX (1.32 against 0.60 on the Radeon), plus 0.47 ms of `set_transform`
  calls. Per moved instance the scene recomputes its world box, batch and centroid
  (~11 ns) and then copies the whole array into the ring (the same PCIe-bound write the
  draw list does). The GPU side is even (full copy 25 µs vs remember 25 µs on the RTX;
  upload + cull 924 µs vs cull + remember 879 µs on the Radeon). Upload modes measured on
  the RTX at 100k, 100 % moving: scatter 145 µs of GPU (27 MB moved), full copy with
  swapped buffers 23 µs.
- Bit-exactness: a scene and the same draw list render identical pixels and motion
  vectors in every pipeline state (opaque, masked, double-sided, mirrored, blended), with
  and without the depth prepass and occlusion culling, and through every upload
  transition, on both GPUs (`crates/engine_render/tests/gpu_scene.rs`).

**Transform propagation** (`cargo bench -p engine_ecs --bench ecs`, 100k entities; main
→ this branch, µs):

| scenario | main | incremental |
|---|---:|---:|
| 1000 roots × 9 × 10, one root moved | 1376 | **13** |
| same, 1 % of the roots moved (spread) | 1346 | **137** |
| same, every root moved | 1366 | **935** |
| same, nothing changed | 0.1 | 0.1 |
| 100k flat, 1 % movers in their own archetype | 260 | **11.5** |

The first incremental version was 12 % slower than main when every root moved (column
lookups per entity, a compare per write); cached archetype columns and writing without a
compare below a changed parent made it 32 % faster.

**Extraction** (`crates/engine_runtime/tests/extract.rs`, counts): 10,100 renderable
entities, 10 movers in their own archetype: a static frame visits 0 entities; the moving
frame visits 100 (one chunk) and moves 10.

**Stress tool** (12k entities, every one moving every frame; `--gpu-scene` against the
default draw list; µs median): RTX extraction 104 vs list 25, record 374 vs 204, frame
1521 vs 1253 (657 vs 798 FPS); Radeon GPU-bound at 8.4 ms either way. Propagation: 130 on
main → 107. A fully dynamic set is cheaper as a per-frame draw list, so the stress tool
keeps that path by default.

**Game loop** (`cargo run -p engine_runtime --release --example game_loop`, 21k entities,
5 % in spinning hierarchies, 64 Hz ticks, interpolated):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| propagation (frames with a tick) | 32 µs | 51 µs |
| extraction (frames with a tick) | 12.5 µs | 24 µs |
| interpolation of ~1000 movers | 14.5 µs | 21 µs |
| scene upload (CPU) | 28 µs | 50 µs |
| **input sample → camera written for the GPU** | **44 µs** | **109 µs** |

Sampling input before waiting for the frame slot and swapchain image instead gave 226 µs
and 6.9 ms. A model of the alternative (`per_frame_camera_latency_against_a_tick_camera`:
the camera as simulation state, interpolated between 64 Hz ticks, at 240 FPS): median
18.6 ms (worst 20.8) against 2.1 ms (worst 4.2) for the per-frame camera.

**Zero allocations**: 100 recorded frames with 1 % moves and churn through
`GpuScene::add_passes` and `add_scene_passes`: 0 heap allocations
(`tests/gpu_scene_zero_alloc.rs`). ECS: unchanged zero-allocation test passes.

## Decision

**`engine_render::gpu_scene::GpuScene`**

- `insert(SceneInstance) -> InstanceId` (slot + generation), `remove`, `set_transform` /
  `set_affine` (a bit-identical transform is a no-op), `teleport` (no motion this frame),
  `set_mesh`, `set_material`, `get`, `clear`. Slots are reused lowest first, at once.
- Two device buffers (this frame's and last frame's instances, the forward renderer's
  64-byte `Instance`). `add_passes` (once per frame, before the views) resolves pending
  changes and uploads them: scatter (16-byte update + 64 bytes) when fewer than half of
  the slots changed, otherwise a full copy into the buffer that held the frame before last,
  with the roles swapped. Slots that stop moving are settled once (16 bytes). Growth
  copies the old buffers in a graph pass and retires them after the frames in flight.
- Materials and mesh boxes are resolved on insert or edit; `MaterialTable::revision`
  (kind changes, removals) resolves every instance again once.
- Batches (mesh × pipeline state; blended instances alone) are kept incrementally, with a
  running centroid for front-to-back order. Scene bounds (for the shadow cascades) grow
  incrementally and are recomputed (vectorized, per-axis arrays: 19 µs at 100k) when a
  bounding instance moves inward or goes.
- Holes are parked in sink batch 0: identity basis, inside-out box, a command nothing
  draws.

**`ForwardRenderer::add_scene_passes`**: `add_passes` is split into a layout step (draw
list or scene) and one shared declaration; the draw-list path is unchanged. Per frame the
scene path lays out batches and commands (cost per batch). `Occlusion` flags and previous
transforms key off slots; a `Temporal` view provides the camera history and jitter, its
own instance copy is not used. No shader changed except the new `scene.slang`.

**ECS** (`engine_ecs`)

- `TransformPropagation::run` is incremental and writes only changed values; `run_full`
  keeps the full pass as the reference. A property test (random edits, reparenting,
  broken chains, despawns, hand-written world transforms; 40 seeds in CI, 1000 checked
  once) asserts byte-identical results and state hashes.
- `World::despawn` now takes the entity out of its parent's list (a dead entity in the
  list made walks skip its later siblings).
- `World::track_removals::<T>()` / `drain_removed`, `resync_epoch`, `get_mut_untracked` /
  `mark_changed`, and the `Or<(..)>` filter. Bookkeeping only: not snapshotted, not
  hashed; determinism is unchanged.

**`engine_runtime`** (new): `MeshRenderer { mesh, material, visible }`, `RenderExtract`
(change detection + removal logs, interpolation at extraction), `Runtime` (one world, the
clock `engine_physics::ecs::FixedStep` capped at 4 ticks per frame, ticks → presentation →
interpolation). The editor draws its scene from a `GpuScene`; the stress tool can
(`--gpu-scene`).

**Responsiveness** (ADR 0019 addendum, CLAUDE.md §7): the loop waits for its frame slot
and image, then samples input, runs the ticks (which consume that input), then updates
the camera from the same input in the per-frame presentation step, then records. The
camera never goes through tick interpolation.

**Not pipelined yet**: simulation and rendering run in sequence. The example's whole CPU
frame is 0.12 ms on the RTX; pipelining would hide nothing yet and adds a frame in flight.

## Consequences

- The cost of a frame follows what changed, not what exists, on the CPU and in uploads;
  per-view state no longer depends on any draw order (ADR 0035's and ADR 0043's caveats
  are gone on the scene path).
- **Fully dynamic sets** (everything moving every frame) cost about twice the CPU of a
  draw list. Until parallel extraction and upload exist, keep such sets (particles, the
  stress scene) on per-frame draw lists; a view draws either a list or a scene.
- `PhysicsSync::interpolate` and `RenderExtract`'s interpolation must not both be used.
- `GlobalTransform` written by hand on an entity that has a `Transform` is still
  overwritten by the next run (the write marks its chunk, which makes it a candidate); a
  write through `get_mut_untracked` is not seen. Write `Transform`, or call `run_full`.
- Code that despawned children with `World::despawn` and relied on the dead entity
  staying in the parent's list (nothing in the tree does) sees it gone.
- Revisit:
  - parallel extraction and `apply` on the job system (fully dynamic sets);
  - per-batch slot pages if mixed-mesh scenes show divergent culling waves;
  - one view drawing a scene and a per-frame list together;
  - pipelined simulation/rendering when simulation cost warrants it;
  - a presentation offset for the locally controlled character (outside the tick).

## Addendum (2026-10-10): moving instances placed on the GPU, the tick stages on the job system

**Context.** The second core review (`docs/research/core-audit-2.md` §3.5) measured about
70 ns per moving instance per frame on one thread across four stages (propagation,
extraction, interpolation, scene upload): 60,000 movers made the RTX CPU-bound at 5.2 ms.
The CPU blend and the whole-array upload ran every frame although the simulation moves
things 64 times a second, and the tick's stages ran on one thread.

**Options.**
1. *The CPU blend, in parallel:* the blend and the upload of every moving instance stay per
   frame (3.8 MB at 60,000), only spread over threads.
2. *Blend on the GPU, the tick's stages in parallel (chosen):* each tick uploads a mover's
   new transform once; every frame a compute pass places it. The per-frame CPU follows the
   tick rate, not the frame rate.
3. *Blend in the vertex shaders:* no pass, but every draw of every view (prepass, shadows,
   local shadows, forward) reads two transforms and blends per vertex.

**Design.**
- `GpuScene::step_motion(id, to)` gives a tick's transform (the GPU holds the one before),
  `set_motion(id, from, to)` both; `set_alpha` the frame's factor. `set_affine`, `teleport`
  and `remove` stop the motion. `RenderExtract` uses them when its sink interpolates
  (`SceneSink::interpolates`; other sinks keep the CPU blend).
- Per slot, the two transforms as six planes of 16-byte rows by slot and a word saying
  which is the last tick's: a tick writes only the new one, over the older (`sceneStep`:
  52 bytes uploaded per mover); `sceneMotion` writes both for a start, two ticks between
  uploads, or a new or teleported instance; `sceneInterpolate` places every moving instance
  each frame. Loads and stores through buffer addresses are declared 16-byte aligned
  (`loadAligned<16>`; Slang declares plain pointer accesses 4-byte aligned).
- The two instance buffers swap roles every frame (before: only for whole-array copies):
  last frame's placements are the previous ones without a copy, so the placement pass
  writes one buffer. A slot changed last frame and not this one carries its values forward
  (16 bytes); holes are written to both buffers.
- A handedness flip between ticks (a mirroring scale crossing zero) is not blended through
  the flat, inside-out transforms between, which its batch's culling would draw from the
  back (`SNAP`: both transforms are the new one). The instance changes batch, and culling
  reads the batch from the instance data: the batch word is patched in place
  (`vkCmdUpdateBuffer`, 16 bytes), since the ring's room is counted before anyone knows.
- The boxes of moving instances over their whole move invalidate cached local shadows and
  probes every frame (`SceneFrame::moving_boxes`).
- The tick's stages spread over a pool, with the same results as on one thread:
  `TransformPropagation::run_parallel` (changed roots in jobs of 128 rows, each with the
  subtrees below it: disjoint, a child has one parent; column versions are atomic);
  `RenderExtract::sync_on` (the changed chunks' entities compared in runs of 1,024 rows;
  additions and edits told to the scene on the calling thread in the runs' order; the moves
  through `SceneSink::step_motions`, unsafe: no instance twice, which a `GpuScene` spreads
  over its pool); and `GpuScene::add_passes` (slots whose only change is their motion
  applied in parallel: their box, moving box and record; bounds merged per job; slots that
  change batch take the full path). `Runtime::set_jobs` and `GpuScene::set_jobs` take the
  pool.

**Measurements** (ref-1, `flight` with the Sponza pack, 60,000 movers in spinning
hierarchies of 10 and 64 skinned characters, 900 frames of 1/60 s (`--fps 60`) or 1/240 s
(`--fps 240`) of game time; ms):

| | main | GPU placement only | and the tick's stages on the pool |
|---|---:|---:|---:|
| RTX, 60 fps: CPU frame median / p99 | 4.94 / 6.99 | 3.99 / 6.72 | 1.63 / 2.93 |
| RTX, 60 fps: frame interval median / p99 | 4.96 / 7.00 | 4.01 / 6.74 | 2.18 / 3.81 |
| RTX, 240 fps: CPU frame median / p99 | 2.90 / 5.29 | 0.49 / 4.30 | 0.45 / 1.95 |
| RTX, 240 fps: interval median / p99 / worst | 2.92 / 5.30 / 5.63 | 2.05 / 6.20 / 6.88 | 2.12 / 3.87 / 4.07 |
| Radeon, 60 fps: CPU frame median / p99 | 6.69 / 8.96 | 5.92 / 9.32 | 2.89 / 4.36 |
| Radeon, 60 fps: interval median (GPU-bound) | 40.97 | 41.05 | 41.39 |
| Radeon, 240 fps: CPU frame median / p99 | 3.59 / 6.54 | 1.28 / 5.89 | 1.26 / 2.98 |
| Radeon, 240 fps: interval median / p99 | 40.94 / 52.82 | 41.54 / 54.84 | 41.35 / 52.41 |

With the GPU placement alone, the RTX's tick frames got worse at 240 fps: the frames
between ticks became GPU-bound, the CPU ran ahead, and a tick's 3.9 ms of single-threaded
work landed on top of the wait (worst frame 3.4× the median; CLAUDE.md's budget is 2×). On
the pool the tick's stages cost (RTX, 240 fps, p99 µs): propagation 1,374 → 341,
extraction 1,251 → 578, scene upload 1,296 → 551.

GPU passes (µs): the RTX places 60,000 movers in 30–42 per frame and steps them in 5 per
tick; the Radeon in 454–529 and 178–260; main's whole-array copy took 13.5 and 145–152. The
first layout (a 144-byte record per slot, a tick rewriting both transforms) took the Radeon
570 and 950, the RTX about 140 and 107: per-slot records leave lines partly written, which
the iGPU's shared memory pays for (its CPU propagation ran 0.66 ms slower beside them). On
the Radeon, writing whole 64-byte instances (the material and batch word with them) would
save another 85 µs per frame (measured 425 → 344 with the word left out).

**Decision.** Option 2: GPU placement, with the tick's stages on the job system.

**Consequences.**
- The per-frame CPU of moving instances no longer grows with them; a tick's grows, spread
  over the pool. On the GPU-bound Radeon the frame is 1 % longer at 60,000 movers (GPU
  +0.5 ms: placement and steps against the copy), its CPU frame 2.3× shorter; at the
  default 2,000 movers the passes cost about 20 µs.
- Culling reads an instance's batch from its data on the GPU: whatever changes a slot's
  batch must reach the GPU in the same frame. A stale word dropped instances, or another
  batch's (an overflowing range), at random; `moving_instances_apply_the_same_on_one_thread_and_on_a_pool`
  checks the per-batch counts after culling.
- Propagation of non-root candidates (children whose own transforms change) stays on one
  thread, by depth.
- The fully dynamic sets of the original consequences need no per-frame draw list any more.
- Revisit:
  - whole-line instance writes on the iGPU (the material and batch word beside the planes,
    kept by every writer of that word);
  - the remaining serial passes over the dirty list (classification, about 0.1 ms at
    60,000 on the RTX) if tick frames need it.
