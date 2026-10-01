# Skeletal animation and skinning

Date: 2026-10-01. Input for [ADR 0068](../adr/0068-skeletal-animation-and-skinning.md). We
read papers, talks, documentation and permissively licensed code for ideas only; **no
code was copied** into the engine. ACL was built and run, unmodified, by a research
harness outside the workspace (`bench/acl-compare`) to compare against.

## Sources

| Source | Version / date checked | License | What we looked at |
|---|---|---|---|
| [ozz-animation](https://github.com/guillaumeblanc/ozz-animation), Guillaume Blanc | 0.17.0 (2026-08-01), read 2026-10-01 | MIT | runtime split into jobs (`SamplingJob`, `BlendingJob`, `LocalToModelJob`, `TwoBoneIKJob`, `AimJob`); SoA transforms (4 joints per SIMD lane); skeleton joints sorted depth first; key-frame reduction and per-track caches for sequential sampling |
| [Animation Compression Library](https://github.com/nfrechette/acl) (ACL), Nicholas Frechette; with [Realtime Math](https://github.com/nfrechette/rtm) | ACL 2.1.0 (2023-12-07), RTM 2.3.1 (2024-03-16), read and run 2026-10-01 | MIT (both) | uniform sampling, segments of about 16 samples, range reduction per clip and per segment (segment ranges as 8-bit fractions), variable bit rates per track and segment, `quatf_drop_w_variable`, the `qvvf_transform_error_metric` (object-space error on a shell of virtual vertices around each joint), default and constant sub-track removal; docs `algorithm_uniformly_sampled.md`, `range_reduction.md`, `error_metrics.md`, `compressing_raw_tracks.md` |
| N. Frechette's blog, "[Animation Compression Library](https://nfrechette.github.io/)" series (Sign of the times, Range reduction, Variable bit rate, Pitfalls of linear sample reduction) | read 2026-10-01 | — | why uniform sampling with variable bit rates beats linear key reduction in decompression speed at similar size; measuring error in object space, not per track |
| Unreal Engine 5 animation documentation: [Animation Blueprints / AnimGraph](https://dev.epicgames.com/documentation/en-us/unreal-engine/animation-blueprints-in-unreal-engine), [Motion Matching](https://dev.epicgames.com/documentation/en-us/unreal-engine/motion-matching-in-unreal-engine), [Animation compression](https://dev.epicgames.com/documentation/en-us/unreal-engine/animation-compression-in-unreal-engine) | docs read 2026-10-01 | proprietary (source **not read**) | AnimGraph as a per-character graph evaluated on worker threads with a fast path; additive layers, per-bone blends ("layered blend per bone"); ACL is a shipped codec plugin |
| Unity [Mecanim / Animator](https://docs.unity3d.com/Manual/AnimationOverview.html), avatar masks, root motion | docs read 2026-10-01 | proprietary | humanoid retargeting, avatar masks per bone, root motion taken from the root's motion and handed to the transform or a callback |
| S. Clavet, "Motion Matching and The Road to Next-Gen Animation", GDC 2016 (Ubisoft); D. Holden et al., "Learned Motion Matching", SIGGRAPH 2020 | talks / paper | — | pose search over a database by trajectory and pose features; learned variant compresses the database with networks. **Patents:** EA and Ubisoft hold patents or applications on motion-matching variants (e.g. US20230310998A1, a periodic autoencoder); not implemented here, to be checked before any motion matching work |
| J. Gregory, *Game Engine Architecture* (3rd ed.), ch. 12 | book | — | the pipeline: sampling, blending, global pose, post-processing (IK), skinning matrices; additive blending, partial (masked) blending |
| L. Kavan, S. Collins, J. Žára, C. O'Sullivan, "[Skinning with Dual Quaternions](https://users.cs.utah.edu/~ladislav/kavan07skinning/kavan07skinning.html)", I3D 2007; "Geometric Skinning with Approximate Dual Quaternion Blending", TOG 2008 | papers | — | linear blend skinning (LBS) collapses joints under twist ("candy wrapper"); dual quaternion skinning (DQS) keeps volume, needs rigid transforms (scale separately), bulges at bends. **Patents:** none found on the 2007 method; US9613456B2 ("Enhanced dual quaternion skinning with scale non-compensating joints and support joints", active to 2035) covers a variant |
| A. Aristidou, J. Lasenby, "FABRIK: A fast, iterative solver for the Inverse Kinematics problem", Graphical Models 73(5), 2011 | paper | — | forward and backward reaching on joint positions, then rotations from the new bone directions. No patent found (2026-10-01) |
| CCD (cyclic coordinate descent), Wang & Chen 1991; analytic two-bone IK by the law of cosines (ozz `TwoBoneIKJob`, Daniel Holden's "Simple Two Joint IK", 2017) | papers / articles | MIT (ozz) | two-bone IK in the plane of the root, the target and a pole; CCD as the cheap iterative alternative to FABRIK |
| W. Engel (ed.), GPU Pro / GPU Zen chapters on compute skinning; Frostbite "Rendering Battlefield 3", GDC 2011; AMD "Compute-based skinning", Unreal's GPU skin cache docs | talks / docs | — | pre-skinning in a compute pass once per frame so every pass (depth, shadows, forward, motion) reads skinned vertices; the previous frame's positions kept for motion vectors (UE's skin cache keeps them for velocity) |
| Guerrilla Games, "The Animation of Horizon Zero Dawn" (GDC 2017); Naughty Dog, "Animation Bootcamp" talks (GDC 2016-2017) | talks | — | layered state machines, additive layers for breathing and leans, IK for feet on slopes and hands on props as a post-process on the sampled pose |

## How they work

### ozz-animation

- **Data.** A `Skeleton` holds joint names, parents (sorted so every parent precedes its
  children) and the rest pose, all as SoA transforms: 4 joints per SIMD register per
  component. An `Animation` holds key frames per track, reduced at build time (linear
  key reduction against a tolerance) and quantized (16-bit quaternions, half floats for
  translation and scale).
- **Jobs.** Sampling reads keys through a per-instance `SamplingJob::Context` that caches
  the keys around the last time (cheap when time moves forward). Blending, local-to-model
  and IK are separate jobs over plain arrays; parallelism is left to the caller (one job
  per character).
- **IK.** `TwoBoneIKJob` (analytic, pole vector, weight, soften) and `AimJob` (aim an
  axis at a target, with an up vector) return corrective quaternions to multiply into
  the local pose.

### ACL

- **Uniform sampling.** Every track is sampled at the clip's rate; sampling time `t`
  decodes the two samples around it and interpolates. No key search, no per-instance
  cache: decoding cost is independent of where playback jumps.
- **Removals.** Tracks equal to the default (identity, or a supplied bind pose) are not
  stored; constant tracks store one value.
- **Range reduction** per clip and per segment (16 samples): values are mapped to
  [0, 1] over their range before quantization; segment ranges are stored as 8-bit
  fractions of the clip range.
- **Variable bit rates** per track per segment (from a table of rates up to 19 bits, or
  raw), chosen by an optimizer that measures the error in object space: the error metric
  transforms virtual vertices placed at a shell distance around each joint through the
  whole hierarchy, so a parent's rotation error is measured where it shows, at its
  children. The default precision is 0.01 cm with a 3 cm shell.
- **Rotation format:** quaternion with w dropped (made positive), xyz quantized.

### Unreal Engine 5 and Unity (products)

- Animation is a per-character graph (state machines, blend spaces, layered blends with
  per-bone masks, additive layers) whose output pose goes through post-process IK and
  then to the skinned mesh. UE evaluates graphs on worker threads; the "fast path" is
  plain data access without blueprint calls.
- Root motion: the root bone's motion is extracted from the pose and either applied to
  the character's transform or handed to the movement component (Unity: an
  `OnAnimatorMove` callback).
- Compression: UE ships ACL as a codec plugin; Unity offers key reduction and "optimal"
  (curve fitting) modes.

### Skinning on the GPU

- **Vertex-shader skinning** in every pass that draws the mesh: no extra memory, but the
  blend is repeated in the depth prepass, each shadow cascade and the forward pass, and
  every one of those shaders must know about skinning (a variant per pass).
- **Pre-skinning in a compute pass** (UE's GPU skin cache, Frostbite): once per frame,
  into a vertex buffer that every pass then draws as an ordinary mesh. Costs memory for
  the skinned copy per instance; keeping last frame's positions gives exact per-vertex
  motion vectors.
- **LBS vs DQS:** LBS is one weighted 3 × 4 matrix per vertex; DQS needs rigid joint
  transforms (scale handled apart) and costs more ALU, but avoids LBS's volume loss under
  twist. Most shipped engines default to LBS and offer DQS per mesh.

## Measurements (this repository, ref-1, release)

From `crates/engine_assets/examples/anim_bench.rs` and `bench/acl-compare` (see ADR 0068
for the full tables). Error bound: 0.01 % of each model's size, ACL's metric with each
joint's shell at its farthest skinned vertex.

- **Our format against ACL 2.1** (bytes; ACL at its own precision exceeds the bound by
  our measure by 3-45 %, so it is also shown with its precision lowered until it meets
  it):

  | clip | ours | ACL medium, at equal error |
  |---|---:|---:|
  | Fox Survey (3.4 s, 24 Hz) | 5,541 | 7,013 |
  | Fox Walk (0.7 s, 24 Hz) | 2,901 | 3,587 |
  | Fox Run (1.2 s, keys off a 24 Hz grid: 120 Hz) | 11,849 | 7,489 |
  | CesiumMan (2 s, 24 Hz) | 5,572 | 5,503 |
  | RiggedSimple | 420 | 416 |

- **Key reduction** (16-bit values, Douglas-Peucker per track with the tolerance halved
  until the hierarchy meets the bound): smaller on sparse or redundant sources (Run
  5,880 B; RiggedSimple 160 B), larger on dense ones (Survey 9,720 B, CesiumMan 9,872 B),
  and decoding needs a binary search per track: 0.70 µs for two Fox poses against our
  0.66 (single thread).
- **Decoding** a whole 25-joint pose: ours and ACL within measurement noise of each other
  (0.3-0.6 µs on a loaded machine). An earlier version of our format without segments
  and with one width per track decoded twice as fast (0.30 µs for two poses against
  0.66, measured interleaved) and took up to 37 % more memory (Survey 7,320 B, Run
  16,248 B; Walk and CesiumMan about the same).

## What we take

- **ozz's runtime structure:** skeleton sorted depth first (local-to-model is one forward
  loop; a subtree is a contiguous range, so masks are ranges), separate sampling,
  blending, local-to-model and IK steps over flat arrays, no allocation per frame,
  parallelism per character on the engine's own job system. Two-bone IK with a pole and
  an aim (look-at) solver, plus FABRIK for longer chains.
- **ACL's compression approach, implemented in Rust:** uniform sampling, default and
  constant removal, per-clip and per-segment range reduction with 8-bit segment ranges,
  variable bit rates chosen against ACL's object-space error metric on virtual vertices.
  Two changes, both measured: a bit width **per component** (0 for components that do
  not move, common in single-axis joints), and the dropped quaternion component is the
  largest one rather than always w (recomputing a w near 0 lost so much precision that
  Fox's Walk missed the bound 3× even with raw floats).
- **Pre-skinning in compute** into the geometry pool, last frame's positions kept for
  motion vectors.

## What we do not take, and why

- **Vendoring ACL** (MIT, header-only C++): at equal error it is within 1-26 % of our
  format on four of five clips (ours smaller on three), decodes at the same speed, and
  would bring a C++ build, a C shim and its own error-metric types into the engine. Its
  clear advantage is the high-rate clip (Run, 1.6× smaller), which better per-segment
  bit allocation should close (ADR 0068, next steps). Revisit if it does not.
- **Key reduction** as the format: smaller on sparse sources, but larger on dense
  (mocap-like) clips and slower to sample at random times; ozz needs a per-instance
  cache to make it cheap, which is per-character state we would rather not keep.
- **Vertex-shader skinning:** it would put skinning into the forward, prepass and shadow
  shaders (the register rule: features must not cost views that do not use them) and
  repeat the blend per pass and cascade.
- **DQS as the default:** more ALU per vertex (on the 2-CU Radeon skinning is already
  ALU-bound at 8 influences), needs scale handled separately; LBS artifacts are an art
  problem first (twist joints). Can be added as a second entry point per mesh.
- **Motion matching and AnimGraph-style state machines:** outside this task; motion
  matching variants are patent-encumbered (above) and need a team decision.
