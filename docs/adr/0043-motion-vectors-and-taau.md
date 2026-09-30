# 0043. Motion vectors, projection jitter and the engine's own TAAU

- Status: Accepted
- Date: 2026-09-30
- Builds on: [ADR 0040](0040-materials-and-textures.md) (instances and the forward pass),
  [ADR 0035](0035-occlusion-culling.md) (per-view state kept across frames),
  [ADR 0042](0042-depth-prepass.md) (the passes motion vectors are written in)
- Research: [`docs/research/temporal-aa.md`](../research/temporal-aa.md)

## Context

With materials and textures in place, aliasing is the most visible artifact:
- geometric edges;
- alpha-tested foliage;
- specular highlights that geometric specular AA only softens;
- shadow-map edges.

CLAUDE.md §6 asks for an `Upscaler` interface (color, depth, motion vectors, exposure,
jitter, masks). The engine's own TAAU is always available behind it, as the quality
reference and the fallback for FSR 3.1, DLSS and XeSS. All of them need the same two
things from the renderer:

- **motion vectors**: per pixel, where the surface was last frame, without jitter;
- **a sub-pixel jitter of the projection** that changes every frame.

Motion vectors are also what motion blur, temporal denoisers and reprojection effects
consume. They are a renderer output with a documented contract, not a TAA detail.

## Options

**Motion vectors:**

1. **Written by the forward pass into a second target (chosen).**
   - The vertex shader also transforms the vertex by last frame's instance transform and
     last frame's unjittered camera.
   - The fragment shader writes the texture-coordinate delta.
   - A full-screen triangle at the far plane (depth test *equal* to 1) fills the pixels
     no geometry covered with the camera's motion.
   - Last frame's instances are kept by a per-view `Temporal` state, copied on the GPU
     from this frame's instance upload, known by draw index like `Occlusion`'s flags.
2. **Camera motion reconstructed from depth, object motion only for moving objects.**
   - Cheaper when most of the scene is static: static geometry skips the extra vertex
     work and the second target.
   - Needs "moved this frame" per instance on the CPU side, and a separate full-screen pass.
   - Kept as the optimization to measure next (see Consequences).
3. **A separate velocity pass over the geometry.** A third pass over the vertices, after
   the shadow pass and, optionally, the depth prepass. Rejected: the vertex cost is what
   ADR 0042 measured as the most expensive part of a second pass.

**Resolve:**

- a. **One compute pass at output resolution (chosen)**, following Karis 2014 and Salvi
  2016. For each output pixel:
  - the current frame is reconstructed from its 3 × 3 jittered samples (Blackman-Harris
    approximation);
  - the velocity is taken from the nearest depth in the neighbourhood;
  - the history is fetched with a 5-tap Catmull-Rom filter;
  - the history is clipped to the variance box (YCoCg, reversibly tonemapped colors);
  - the accumulated weight is kept in history alpha, capped lower for moving pixels;
  - history is ignored off screen and after a reset.
- b. **FSR 3.1 only.** Multi-pass, larger, and not ours to tune. It comes later behind
  the same inputs; the own TAAU stays as the fallback and the reference.
- Variants of (a) measured and not taken (numbers below):
  - a groupshared tile of the neighbourhood;
  - a bilinear history fetch.

## Measurements

ref-1, release, GPU µs (medians).

**Quality:** `crates/engine_upscale/tests/taa.rs`, PSNR of tonemapped colors against 4 × 4
supersampling. The scene is slanted cubes on a ground slab at 96 × 96.

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| one sample per pixel | 35.63 dB | 35.63 dB |
| TAA, 24 frames | **39.79 dB** | **39.82 dB** |
| half resolution, bilinear stretch | 30.70 dB | 30.70 dB |
| TAAU from half resolution, 48 frames | **33.09 dB** | **33.05 dB** |
| camera panning, one sample | 35.68 dB | 35.68 dB |
| camera panning, TAA | **38.42 dB** | **38.61 dB** |

**Motion vectors:** `scene_bench` with and without `--motion`, 1920 × 1080. Forward pass,
plus "motion background" and "remember instances" together:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Sponza, textured | 292 → 317 + 2 (+9%) | 6,956 → 7,508 + 1 (+8%) |
| Sponza, factors only | 166 → 211 + 2 | 3,620 → 4,470 + 1 (+23%) |
| FlightHelmet, textured | 67 → 73 + 2 | 1,796 → 2,062 + 1 (+15%) |
| FlightHelmet, factors only | 47 → 54 + 2 | 1,166 → 1,440 + 1 |

The cost is the second color target and the second transform per vertex; the background
and the copy are negligible.

**Resolve:** `cargo run -p engine_upscale --release --example taau_bench`. 576 cubes on
a ground slab with a camera circling them, about a pixel of motion per frame. The forward
pass at the render resolution is shown for scale.

| output ← render | RTX forward | RTX taau | Radeon forward | Radeon taau |
|---|---:|---:|---:|---:|
| 1920×1080 ← 1920×1080 (TAA) | 124 | 174 | 3,226 | 4,518 |
| 1920×1080 ← 1280×720 | 51 | 135 | 1,313 | 4,517 |
| 1920×1080 ← 960×540 | 34 | 133 | 1,097 | 4,517 |
| 2560×1440 ← 2560×1440 (TAA) | 193 | 294 | 5,035 | 8,029 |
| 2560×1440 ← 1707×960 | 83 | 244 | 2,179 | 8,028 |
| 2560×1440 ← 1280×720 | 56 | 237 | 1,664 | 8,027 |

**Where the resolve's time goes** (1080p TAA, shader variants built only for measuring):

| variant | RTX | Radeon |
|---|---:|---:|
| writing the output only | 32 | 259 |
| no history (current frame only) | 89 | 2,290 |
| first version: weight read by a sixth history fetch | 185 | 4,520 |
| **chosen: weight from the 5 Catmull-Rom taps** | **174** | **4,518** |
| a groupshared tile of the 3 × 3 neighbourhoods (16 × 16 groups) | 172 | 4,957 |
| the same, loading only the texels a group needs | 186 | 5,473 |
| bilinear history instead of Catmull-Rom | 148 | 3,866 |

- **About half of the resolve is the history fetch.** The weight folded into the
  Catmull-Rom taps removes one of six fetches at no loss of quality: 38.45 → 38.42 dB
  panning on the RTX, 38.62 → 38.61 dB on the Radeon. It is 6-11% faster on the RTX and
  unchanged on the Radeon.
- **The neighbourhood tile is not a win.**
  - It fetches each input texel once per group instead of up to nine times.
  - Against the first version, on the RTX it is 2-7% faster at 1:1 and 4-17% slower
    when upsampling.
  - On the Radeon it is 10% slower, at every scale.
  - Not taken.
- **A bilinear history is 15% faster and blurs motion.** Panning TAA drops 1.2 dB
  (38.45 → 37.24 on the RTX). That trades quality for speed, so it is a question for the
  team, not a default.
- **The Radeon's time does not depend on the render resolution or the fetch count** the
  way the RTX's does. Explaining that needs a Radeon GPU Profiler capture (next step).
- **TAAU pays on both GPUs, and more on the Radeon.** Upsampling 720p to 1080p costs
  forward + resolve 186 µs on the RTX against 298 µs for native 1080p with TAA, and
  5.8 ms against 7.7 ms on the Radeon. On the iGPU the resolve is still more expensive
  than the forward pass of this scene.
- **Correctness,** under synchronization validation with 0 errors on both GPUs:
  - `tests/motion.rs` checks a moving object's exact displacement, camera motion
    including the background, that jitter never leaks into the vectors, resets, objects
    without a previous transform, an empty draw list, and all four combinations of depth
    prepass and occlusion culling;
  - `tests/taa.rs` checks the table above;
  - the committed SPIR-V matches its source (Slang 2026.18.3).

## Decision

- **Motion vectors in `engine_render`:**
  - `ForwardView::temporal: Option<&Temporal>`, one `Temporal` per view, created with the
    number of draw items to remember;
  - with it, `ForwardPasses::motion` is a `MotionOutput`:
    - `velocity`: RG16F at the render resolution, the unjittered texture-coordinate delta
      current − previous (`uv − motion` is where the surface was);
    - `jitter`: this frame's offset in pixels;
    - `reset`: drop accumulated history;
  - the projection is jittered through Halton(2, 3), with the phase count set by
    `Temporal::set_jitter_phases` (0: none);
  - `Temporal::reset()` after camera cuts;
  - items without last frame's transform (new, reordered, beyond the capacity) count as
    still for one frame;
  - an empty draw list still reports zero motion and moves the state on.
- **`engine_upscale` crate** with `Taau`, one per view:
  - `add_pass` declares the resolve into its history image of the output size and
    returns it, HDR, ready for tonemapping;
  - the history is RGBA16F with the accumulated weight in alpha, and is recreated when
    the output size changes;
  - `Taau::jitter_phases(render, output)` gives ⌈8 · scale²⌉ phases, at least 8.
- **Resolve:** option (a) with the weight read from the Catmull-Rom taps.
- **Measurement tools:**
  - `scene_bench --motion` measures the cost of motion vectors;
  - `taau_bench` measures the resolve;
  - `engine_upscale`'s tests run in `tools/check.ps1 -Gpu` and in `gpu.yml` on both GPUs.

## Consequences

- **Games and the editor opt in per view.** Without a `Temporal` nothing changes: no
  jitter, no second target, the same pipelines as before.
- **FSR 3.1, DLSS and XeSS plug into the same inputs** (`MotionOutput`, depth, jittered
  color). The `Upscaler` trait over them is written with the second implementation, when
  there is something to abstract over.
- **Not done yet:**
  - a negative texture LOD bias while upsampling, log2(render / output): textures are
    blurrier than native until then;
  - reactive and transparency masks (with the transparency pass);
  - sharpening;
  - locks for thin features;
  - exposure input (the resolve works on HDR colors as rendered);
  - the editor's viewport does not use TAA yet.
- **Revisit:**
  - motion vectors only for moving objects (option 2) when a scene benchmark shows the
    +8-23% forward cost matters;
  - the Radeon's resolve after a profiler capture;
  - the bilinear history if the team takes that trade.
