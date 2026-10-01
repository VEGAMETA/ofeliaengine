# 0068. Skeletal animation and skinning: an ozz-style runtime, ACL-style compressed clips in Rust, pre-skinning in compute with a depth-equal motion pass

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0002](0002-job-system.md) (job system), [ADR 0033](0033-gltf-import.md)
  (glTF import), [ADR 0039](0039-geometry-pool.md) (geometry pool),
  [ADR 0043](0043-motion-vectors-and-taau.md) (motion vectors),
  [ADR 0056](0056-cooked-packs-and-streaming-reads.md) (packs),
  [ADR 0064](0064-gpu-scene-extraction-runtime.md) (GPU scene)
- Research: [docs/research/animation.md](../research/animation.md)

## Context

CLAUDE.md §2 lists `engine_anim` (skeletal animation, blending, IK); §13 names
ozz-animation as the reference. Nothing animated existed: glTF skins and animations were
not imported, and the renderer had no skinned meshes. Constraints:

- no allocation per frame (§5), parallel over characters on our job system;
- clips compressed with a **measured** error bound;
- skinned meshes must reach every pass (depth prepass, shadow cascades, forward shading,
  motion vectors) without adding code to the forward shaders of views that draw no
  skinned mesh (the register rule: forward shading sits at exactly 64 VGPRs on the
  Radeon);
- the persistent GPU scene (ADR 0064) and the per-frame draw list must both work.

## Options

**Runtime structure**

1. *ozz-style steps over flat arrays* (chosen): a skeleton sorted depth first, poses as
   one array per component (translations, rotations, scales), separate sampling,
   blending, local-to-model, IK and skinning-matrix steps; parallel per character.
2. *A graph evaluated per character* (UE's AnimGraph, Mecanim): what games author, but
   a layer above the steps; it needs the steps first and an editor (CLAUDE.md §17 puts
   tools last).

**Clip format** (measured, research note)

- a. *Key reduction + 16-bit quantization* (ozz): smallest on sparse or redundant
  sources; larger on dense clips (Fox Survey 9,720 B vs 5,541; CesiumMan 9,872 vs 5,572),
  a binary search per track to sample (0.70 µs for two Fox poses vs 0.66), or a
  per-instance cache.
- b. *Uniform sampling, one bit width per track over the whole clip*: our first version.
  Decodes two poses in 0.30 µs; Survey 7,320 B, Run 16,248 B.
- c. *Uniform sampling with segments, ranges per segment and a bit width per component*
  (ACL's approach plus per-component widths; chosen): Survey 5,541 B (−24 %), Run
  11,849 B (−27 %), Walk and CesiumMan about the same as (b); decodes two poses in
  0.66 µs, about ACL's speed.
- d. *Vendor ACL 2.1* (MIT, header-only C++): at equal error (by our metric) 7,013 /
  3,587 / 7,489 / 5,503 B for Survey / Walk / Run / CesiumMan against our 5,541 / 2,901 /
  11,849 / 5,572; decoding the same within noise; a C++ build and C shim in the engine.

**Skinning**

- *Vertex-shader skinning in every pass*: no extra memory; skinning code in the prepass,
  shadow and forward shaders and the blend repeated per pass and cascade.
- *Pre-skinning in a compute pass into the geometry pool* (chosen): every pass draws the
  skinned instance as an ordinary mesh; costs a mesh copy per instance.
- LBS (chosen) or DQS (more ALU, scale separately; the 2007 method has no patent found,
  an enhanced variant is patented, US9613456B2).

**Motion vectors of skinned meshes**

- *A skinned vertex entry point in the forward shaders* reading last frame's positions:
  free in time, but new pipeline variants and batching changes in `forward.rs` and the
  GPU scene, and forward shader code.
- *A second pass over skinned instances only, depth test equal* (chosen): placement
  bit-identical to the forward shaders' (`precise`, the same operations), so exactly the
  forward pass's pixels of skinned surfaces get their per-vertex motion. Costs a vertex
  pass over skinned meshes in views with motion vectors; nothing elsewhere.

## Measurements

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`. The CPU numbers were taken while
other agents used the machine: medians of repeated runs, A/B comparisons interleaved.

**Compression** (`cargo run -p engine_assets --release --example anim_bench`; bound 0.01 %
of each model's size, shells at each joint's farthest skinned vertex):

| clip | joints | Hz | source keys B | uniform f32 B | ours B | bits/component | error / bound | key-reduced B | compress ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Fox Survey | 25 | 24 | 34,528 | 83,000 | 5,541 | 4.5 | 1.00 | 9,720 | 6 |
| Fox Walk | 25 | 24 | 7,488 | 18,000 | 2,901 | 6.7 | 0.96 | 3,632 | 3 |
| Fox Run | 25 | 120 | 10,400 | 140,000 | 11,849 | 7.2 | 1.00 | 5,880 | 21 |
| CesiumMan | 21 | 24 | 47,424 | 41,160 | 5,572 | 9.2 | 1.00 | 9,872 | 5 |
| RiggedFigure | 21 | 1 | 1,976 | 1,680 | 474 | 5.9 | 0.04 | 2,736 | 0.1 |
| RiggedSimple | 4 | 24 | 2,600 | 8,160 | 420 | 4.2 | 0.73 | 160 | 0.2 |
| SimpleSkin | 2 | 4 | 240 | 1,840 | 314 | 4.5 | 0.96 | 192 | 0.1 |

- Fox's Run is keyed at 24 Hz with keys 0.8 frame off the grid after a gap; the compressor
  picks the smallest rate holding every key (120 Hz). Key reduction stores it in half;
  ACL in 63 %. Better bit allocation per segment (ACL's segment bit-rate search) is the
  next step for this case.
- Every clip of the five models stays within its bound through the runtime decoder, at
  every sample and halfway between samples (`crates/engine_assets/tests/animation.rs`),
  and through cooking into a pack and back.

**Against ACL 2.1** (`bench/acl-compare`, the same uniform samples, bound and shells):

| clip | ours B | ACL medium B (error / bound by our metric) | ACL at equal error B | ours µs/pose | ACL µs/pose |
|---|---:|---:|---:|---:|---:|
| Fox Survey | 5,541 | 6,843 (1.03) | 7,013 | 0.51 | 0.59 |
| Fox Walk | 2,901 | 2,720 (1.18) | 3,587 | 0.55 | 0.57 |
| Fox Run | 11,849 | 7,381 (1.19) | 7,489 | 0.55 | 0.59 |
| CesiumMan | 5,572 | 5,233 (1.42) | 5,503 | 0.53 | 0.35 |
| RiggedSimple | 420 | 416 (0.92) | 416 | 0.05 | 0.14 |

(Pose times at scattered times on a loaded machine: equal within noise.)

**Sampling and blending per character** (Fox, Walk + Run sampled, blended, local to
model, 24 skinning matrices):

| characters | threads | µs per character | µs per frame |
|---:|---:|---:|---:|
| 100 | 1 | 1.07 | 107 |
| 1,000 | 1 | 1.07 | 1,071 |
| 100 | 16 | 0.29 | 29 |
| 1,000 | 16 | 0.13 | 127 |
| 10,000 | 16 | 0.22 | 2,154 (memory-bound: 60 MB of poses and matrices) |

Sampling alone (two clips): 0.66 µs single thread; option (b) 0.30 µs, measured
interleaved with the same binary layout. The segment range and per-component widths cost
the difference; vectorizing four tracks at a time (ozz's SoA) is the next step.

**Skinning on the GPU** (`cargo run -p engine_render --release --example skin_bench`;
tubes of 4,096 vertices on a 32-joint chain, draw list, 1920 × 1080, motion vectors):

| vertices | influences | RTX skinning µs | per 100k | RTX skinned motion µs | Radeon skinning µs | per 100k | Radeon skinned motion µs |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 102,400 | 4 | 13.0 | 12.7 | 50 | 198 | 193 | 500 |
| 102,400 | 8 | 15.8 | 15.4 | 50 | 300 | 293 | 498 |
| 1,003,520 | 4 | 183 | 18.3 | 299 | 1,778 | 177 | 4,662 |
| 1,003,520 | 8 | 184 | 18.4 | 277 | 2,793 | 278 | 4,672 |

- For scale, the same frames' forward pass: RTX 83 µs (100k) / 371 µs (1M), Radeon
  1,975 / 8,656 µs; shadows RTX 48 / 431, Radeon 1,160 / 9,054.
- The skinned motion pass costs about a quarter of the forward pass on the Radeon for a
  scene made of skinned meshes only (it redraws their vertices); views without motion
  vectors, or without skinned meshes, record nothing.
- CPU: posing 25 instances and giving their bounds 8 µs, declaring the pass 3 µs (RTX);
  245 instances 98 / 40 µs.

**Correctness** (both GPUs, validation with synchronization validation, 0 errors):

- `crates/engine_render/tests/skinning.rs`: skinned positions within 1e-5 and normals
  within 1e-3 rad of a CPU reference with 4 and 8 influences; last frame's positions kept
  (and reset after a teleport); bounds containing every skinned vertex; the motion of a
  bending bar against its exact analytic motion within 0.05 px, with and without the
  depth prepass; the GPU scene path giving bit-identical motion to the draw list's.
- `crates/engine_anim/tests/anim.rs`: compression within the bound (linear and cubic
  sources, three bounds), sampling at the keys, additive round trips, two-bone IK reaching
  reachable targets to 1e-5 and stretching straight at unreachable ones, FABRIK to its
  tolerance, look-at within its limit, root motion composed over loops; blending
  identities in unit tests.
- Zero allocations per frame: 256 characters on the job system (sampling, blending with a
  masked additive layer, root motion, local to model, two-bone IK, FABRIK, look-at,
  skinning matrices; `crates/engine_anim/tests/zero_alloc.rs`); 40 skinned instances
  posed, skinned and drawn through the GPU scene with their motion pass
  (`crates/engine_render/tests/skinning_zero_alloc.rs`).
- Golden image `animated_fox` on both GPUs; every existing shot unchanged; forward
  shaders unchanged (SPIR-V identical to main's), so their VGPRs are too.

## Decision

**`engine_anim`** (no dependencies beyond `engine_core`, `engine_profiler`):

- `Skeleton` (depth-first order, parent indices, subtree ranges, rest pose), `Pose`
  (translations, rotations, scales as arrays), `RawClip` (glTF channels with step, linear
  and cubic spline interpolation: the reference), `compress` → `CompressedClip`
  (option c), `CompressedClip::sample` / `sample_joint`, `Blender` (weighted layers,
  per-joint masks, additive layers, rest-pose fill below a weight threshold),
  `local_to_model`, `skinning_matrices`, `root_motion` / `remove_root_motion`,
  `TwoBoneIk`, `Fabrik`, `LookAt`, `for_each_parallel` (characters on the job system).
- Cooked formats: `Skeleton::to_bytes` and `CompressedClip::to_bytes`, validated on
  load.

**`engine_assets`**: glTF skins (joints, inverse bind matrices, every `JOINTS_n` /
`WEIGHTS_n` set) and animations (translation, rotation, scale; morph weights skipped).
A skin's skeleton includes its joints' ancestors, so its model space is the scene's world
space. `cook_scene` compresses every clip that moves a skin (bound
`CookSettings::animation_error`, 0.1 mm by default, shells at each joint's farthest
skinned vertex). **Packs version 2** add `Skin` and `Clip` entries, joint influences on
mesh entries and the skin on instances; a pack without skins is still written as
version 1, and this engine reads both.

**`engine_render::skinning`**: skins (bind pose + 4 or 8 influences) in one device
buffer; each skinned instance owns a mesh in the geometry pool that a compute pass (one
dispatch per skin, instances along y) rewrites every frame with LBS, copying the old
positions into a previous-positions buffer first. Bounds from per-joint bind-pose boxes go
to the pool (`GeometryPool::set_bounds`) and the scene (`GpuScene::refresh_bounds`).
`SkinningFrame::add_motion_pass` fixes the motion vectors of skinned pixels after a
view's forward pass. `MotionOutput` now carries the jittered and the previous camera.
The forward shaders are unchanged.

## Consequences

- Games get animated characters through either the draw list or the GPU scene. A skinned
  instance costs a mesh copy in the pool (28 bytes a vertex) and last frame's positions
  (12 bytes a vertex); a skin's bind pose and influences are stored once (36 or 52 bytes
  a vertex).
- The pack format has a version 2; older engines refuse such packs rather than misread
  them.
- Revisit:
  - **decoding speed**: vectorize sampling over four tracks (SoA, ozz-style) to win back
    option (b)'s speed;
  - **bit allocation on high-rate clips** (Fox Run: ACL 1.6× smaller): ACL's per-segment
    search over bit-rate permutations along a chain;
  - the motion pass's cost on scenes made of skinned meshes: a skinned forward vertex
    entry point if a game needs it (with the register rule's measurement);
  - DQS as a per-skin option; blend shapes (morph targets); a state machine / blend tree
    layer and an animation editor (CLAUDE.md §12) on top of these steps;
  - skinning in the async compute queue (ADR 0065) once a frame has work to overlap.
