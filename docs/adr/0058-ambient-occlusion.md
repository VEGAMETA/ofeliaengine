# 0058. Ambient occlusion: GTAO from the depth prepass, in XeGTAO's structure

- Status: Accepted
- Date: 2026-10-01
- Research: [docs/research/ambient-occlusion.md](../research/ambient-occlusion.md)
- Related: [ADR 0043](0043-motion-vectors-and-taau.md) (TAA/TAAU, which averages the noise),
  [ADR 0046](0046-image-based-lighting.md) (the ambient and environment light it darkens),
  [ADR 0052](0052-compute-passes.md) (compute passes)

## Context

Ambient and environment light reached every surface as if nothing hid the sky: creases,
corners, the undersides of arches and the folds of cloth were as bright as open floors.
Material occlusion textures cover only what one mesh hides from itself, not what the
scene hides. Ambient occlusion was item 4 of the team's priorities (2026-09-30), with
subsurface scattering.

The forward renderer has no G-buffer: whatever AO reads must be ready before the shading
pass, and the shading pass applies it.

## Options

1. **GTAO** (Jimenez et al. 2016), in the structure of Intel's XeGTAO (MIT, 2021)
   (chosen): per pixel, a few slices around the view vector; along each slice the two
   horizons; the cosine-weighted visibility between them integrated exactly. It is
   "ground truth" in the sense that with enough samples it converges to the ray-traced
   answer for a height field, which our test checks.
2. **SSAO / HBAO+ / ASSAO / CACAO** (sample-based or horizon-based, older): cheaper per
   sample or tuned for looks, but not converging to a reference; HBAO+ is closed source,
   CACAO (AMD, MIT) is a tuned ASSAO.
3. **Visibility bitmasks** (Therrien et al. 2023): GTAO with a thickness per sample, so
   thin objects stop occluding everything behind them. Better on thin geometry, same
   structure; a later step on top of this one.
4. **Ray-traced AO:** the reference itself; needs ray tracing and a denoiser, not on the
   Radeon iGPU.

Where it runs:

- **Between the depth prepass and the shading pass** (chosen): the forward pass reads the
  result per pixel. AO forces the depth prepass on (it costs 58 µs on Sponza, RTX).
- After shading, as a multiply of the lit image: wrong, it would darken direct light and
  specular too.

## Numbers

Sponza, 1920×1080, `scene_bench --ao <preset>` (release, `VK_LOADER_LAYERS_DISABLE=~implicit~`),
µs of the three AO passes together (depths + main + one denoise pass):

| Preset (slices × samples per side) | RTX 4060 Ti | Radeon iGPU (2 CU) |
|---|---:|---:|
| Low (1 × 2) | 231 | 5,935 |
| Medium (2 × 2) | 312 | — |
| **High (3 × 3), default** | **460** | **12,104** |
| Ultra (9 × 3) | 1,055 | — |

- Of High on the RTX: depths 28, main 386, denoise 42. The forward pass, Sponza with the
  sky light: RTX 321 → 318 µs with AO; Radeon 8,664 → 9,854 µs (+14 %: the shading
  variant that reads AO takes more registers; only views with AO use it).
- **Shading without AO must not pay for it.** The first version read AO in every forward
  shading pipeline (a load and a multi-bounce fit, skipped when no AO was bound). On the
  Radeon the environment variant went from 64 to 77 VGPRs, fewer waves in flight, and
  the forward pass got 9 % (procedural scene) to 13 % (Sponza) slower with AO off; the
  RTX did not change. The golden-image gate (ADR 0062) caught it when the pull requests
  were merged together, and a bisection named this change. Measured fixes, environment
  variant VGPRs (main: 64):
  - the load moved next to its use: 76;
  - a scalar multi-bounce fit (the albedo's luminance) instead of per channel: 72;
  - AO folded into the material's occlusion where that is read: 72;
  - all of it under a specialization constant set to 0: still 72 (70 with only the fit,
    72 with only the load): the AMD driver does not remove code under a specialization
    constant before allocating registers;
  - **separate entry points** (`fragmentMainAo`, `fragmentMainMotionAo`) with `ao` a
    literal argument of `shade`, which the SPIR-V optimizer removes from the others: 64,
    and the Radeon's forward pass back to 12,206 µs (main 12,196).
- Optimizations kept (High main pass 462 → 386 µs, Low 179 → 164): the slice directions
  rotated rather than recomputed (no `sin`/`cos` per slice), `rsqrt` instead of `length`
  and a division per sample, the depth level from `firstbithigh` instead of `log2`, the
  atlas levels' offsets as push constants instead of a loop, edges packed into one byte
  (XeGTAO's packing), gathers in the denoise pass (46 → 41 µs).
- Tried and dropped: an R16F depth atlas (−2 %, and coarser depths), compile-time slice and
  step counts (−1 %), gathers in the main and depths passes (no gain), 16 × 16 groups (no
  gain), the depth level rounded to the nearest (no gain). With no depth reads at all the
  High main pass still takes 258 µs: it is bound by arithmetic (transcendentals), as
  XeGTAO is; its authors quote 0.56 ms for High at 1080p on an RTX 2060 using fp16 math,
  which Turing runs at twice the fp32 rate and Ada does not.
- The Radeon iGPU is about 28 times slower than the RTX here, as for the other passes;
  at 1080p AO costs it about twice its forward pass. It is a correctness machine
  (reference-hw.md); a half-resolution mode is the answer for weak GPUs (Not done).

Accuracy against a numerically integrated reference (a floor meeting a wall,
`tests/ao.rs`, row means):

| d from the wall (radius 1) | 0.1 | 0.2 | 0.4 | 0.8 | 1.2 |
|---|---:|---:|---:|---:|---:|
| reference at the effect radius (× 1.457) | 0.544 | 0.587 | 0.673 | 0.831 | 0.957 |
| 9 slices × 32 samples | 0.557 | 0.594 | 0.679 | 0.841 | 0.964 |
| reference at the requested radius | 0.564 | 0.626 | 0.748 | 0.948 | 1.0 |
| High | 0.611 | 0.662 | 0.751 | 0.935 | 0.995 |
| Low | 0.625 | 0.667 | 0.813 | 0.941 | 0.993 |

With many samples GTAO converges to the reference at the effect radius. The presets find
lower horizons (three samples per side rarely land at the radius's edge); XeGTAO tuned the
multiplier and the falloff so that they match a ray-traced reference at the *requested*
radius, which they do. An independent CPU copy of the shader (a review, in numpy) matched
the GPU to about 0.005.

## Decision

- **Algorithm:** GTAO as in XeGTAO: effect radius = radius × 1.457, falloff over the last
  61.5 %, R2 + Hilbert-curve noise per pixel (rotating per frame only under TAA),
  samples denser near the pixel (`s²`), far samples from coarser depth levels
  (`2^(level + 3.3)` pixels), normals from depth with the edge-free neighbours, the
  arc integral with the cosine weight and the `0.05` blend towards the full hemisphere.
  Visibility is raised to a power (2.2) and never below 0.03. Formulas adapted from XeGTAO
  (MIT, credited in THIRD_PARTY_NOTICES.md); the code is ours, in Slang.
- **Passes** (`engine_render::ao`, `shaders/ao.slang`):
  1. `ao depths`: view depths and four coarser levels in one R32F atlas (one dispatch,
     through group shared memory), each coarser texel the falloff-weighted average of its
     2 × 2 towards the farthest;
  2. `ao`: the visibility (R8, stored ÷ 1.5 before the blur so noisy values above 1
     survive) and the edges (four 2-bit weights in an R8);
  3. `ao denoise` (0–3 passes, 1 by default): an edge-aware 3 × 3 blur; the last pass
     restores the scale.
- **Use in shading:** the view's AO multiplies the material's occlusion where that is
  read; the ambient and environment diffuse light take the combined occlusion through
  Jimenez's multi-bounce fit for the albedo's luminance (brighter surfaces lose less), the
  specular Lagarde's specular occlusion from it. Sun and local lights are untouched: their
  shadows are their occlusion.
- **Only views with AO pay for it:** the shading that reads it has its own entry points,
  with 8 pipelines (4 lighting variants, with and without motion vectors) made when AO is
  first turned on, like its 3 compute pipelines; shading without AO has no code for it.
- **API:** `ForwardRenderer::set_ambient_occlusion(&Device, &Bindless,
  Option<AoSettings>)`, off by default; `AoQuality::{Low, Medium, High, Ultra, Custom {
  slices, steps }}`, High by default. Perspective views only: orthographic ones draw
  without it.
- **Tests:** the floor-and-wall reference for the Custom 9 × 32 (±0.04), every preset
  against the requested-radius reference (±0.05 for three samples per side, ±0.08 for
  two), with two-phase occlusion culling and with a temporal state; on both GPUs, under
  the validation layer with synchronization validation.

## Consequences

- Scenes get contact shadows in ambient light for about 0.46 ms at 1080p on the RTX
  (High), or 0.23 ms (Low). Under TAAU AO runs at the render resolution, so upscaling
  lowers its cost with the rest.
- AO forces the depth prepass: 58 µs on Sponza that a renderer without it would not pay.
- A rule for every shading feature from here on: code a view does not use must not be in
  its shaders. On the Radeon a specialization constant is not enough; a literal argument
  of separate entry points is.
- Screen-space limits remain: nothing off screen or behind the first depth layer
  occludes, and thin objects occlude everything behind them within the radius (visibility
  bitmasks would fix the latter).
- To revisit: a half-resolution mode for weak GPUs; bent normals (XeGTAO's directional
  term, about +25 %) for directional ambient; visibility bitmasks; fp16 math on AMD (RDNA
  runs packed fp16 at twice the rate); a Hilbert index lookup table (the index costs about
  16 µs of the main pass).

## Addendum (2026-10-13): bent normals and cone specular occlusion (GTSO)

Queue item 3d. Indoors without GI, diffuse ambient came from the sky's harmonics along the
shading normal, and dielectric reflections kept the sky's brightness under arches: they
looked too bright in shade next to metals. Research:
[docs/research/bent-normals-gtso.md](../research/bent-normals-gtso.md).

**Options.**

1. **Always with AO.** No new variants. Costs every view with AO (below).
2. **A switch, `AoSettings::bent_normals`, with its own entry points (chosen; the owner,
   2026-10-13).** Off, everything is exactly as before; on by default on discrete GPUs,
   off on integrated ones.
3. **Diffuse only (no GTSO).** Fewer registers (lights + environment 83 VGPRs on the
   Radeon instead of 93), but without the half of the benefit that matters most here: the
   reflections under the arches.

**Measured** (Sponza 1920 × 1080, `scene_bench --sky --ao [--lights 32] [--no-bent]`,
main and the branch alternated, 3 rounds; ref-1):

| | RTX 4060 Ti | Radeon iGPU |
|---|---|---|
| AO passes, High | 518–544 → 565–568 µs | 13.5–15.0 → 16.3–20.2 ms (bimodal, as main) |
| denoise (perf gate) | 42 → 58 µs | 0.94 → 2.70 ms |
| forward, sky | 323 → 330 µs | 15.2 → 15.6 ms |
| forward, sky + 32 lights | 343 → 363 µs | 17.3 → 21.0 ms |
| GPU frame, High | +2.5–7 % (+0 with async compute in the gate's scenes) | +10–25 % |
| AO Low | AO 313 → 313 µs, forward +10 µs | AO +1 ms, forward +1.9 ms |
| `--no-bent` | = main | = main |

Radeon VGPRs after the prepass with AO (plain / lights / environment / both): 48 / 63 /
72 / 80 without the bent normals (unchanged), 48 / 63 / 72 / 93 with them. Every shading
pipeline without them is byte-identical to main's SPIR-V.

**Decision.**

- **AO pass:** `aoMainBent` also integrates the bent normal (the paper's Algorithm 2, as
  XeGTAO writes it), with `t0` and `t1` written without trigonometry from the horizons'
  cosines (per side `4 s³ cos n + (4 c³ − 12 c) sin n` and `4 s³ sin n − 4 c³ cos n`);
  turns it to world space with the camera's rotation (a quaternion in the push
  constants); writes RGBA8 with the **visibility in R** and the bent normal × 0.5 + 0.5
  in GBA. `aoDenoiseBent` blurs all four with the same edge weights (from loads) and
  renormalizes. Without the switch, `aoMain` and `aoDenoise` write R8 as before.
- **Shading:** the AO is read early (its latency hides behind the material); with the
  bent normal the sky normal is `normalize(n + bent − n_geometric)` (the shading normal
  bent as the geometry's), used for the harmonics and for GTSO. GTSO: the share of the
  specular cone (aperture `exp2(−3.32 α²)`) inside the visible cone (`cos = √(1 − V)`),
  from all of the smaller cap to none along a smoothstep, taken in the cosines of the
  angles (`cos(θv ∓ θs)`), not their arc cosines; faded out below roughness 0.1
  (Filament's form of Jimenez et al.'s GTSO with Oat and Sander's intersection).
- **Pipelines:** 36 entry points `fragmentMainAoBent…`, `fragmentReflectAoBent…`,
  `fragmentSubsurfaceBent…` (237 → 275 `.spv`). They are not described with the
  renderer's other pipelines: each pipeline that reads AO gets a twin the first time a
  frame draws with it under the switch (`forward/bent.rs`: the same description with the
  bent entry point, `LazyPipeline::with_fragment_entry`), made by the device's warm-up
  thread through `update_material_types`. Until the twin exists the frame draws with the
  pipeline it derives from, which reads R: right visibility, no hitch. Material types
  follow (a twin is a role like any other). The golden harness waits for twins as for
  material types. Each entry keeps the pipeline it derives from, so that its id (the
  map's key) is never reused while the entry lives, as material types keep their roles.
- **Without the warm-up thread** (`Device::set_pipeline_warm_up(false)`, `flight
  --no-warm-up`) a derived pipeline (`LazyPipeline::with_fragment`, `with_fragment_entry`)
  is now created when it is described: nothing else would make it. Before, material types'
  variants were never made in that mode (their fallback drew for good).
- **Tried, not taken** (lights + environment on the Radeon stayed at 89–95 VGPRs): the
  bent normal in view space turned in the shading (+3 registers and 6 uniform loads per
  pixel); reading AO late (+0.9 ms: the latency shows); bent and sky normals both live;
  half precision for the sky normal; GTSO computed early as one scalar; ambient after
  the local lights (raised the pipelines without AO: 80 → 84).

**Consequences.**

- On discrete GPUs ambient light comes from the unoccluded direction and reflections fade
  where the visible cone does not cover them: the golden shots with AO changed (FLIP mean
  0.027–0.062), metals' lower halves no longer mirror the sky, shaded stone and cloth
  under Sponza's arches got darker.
- Integrated GPUs keep AO without them by default: with local lights their registers cost
  the Radeon about 20 % of its shading.
- Cooked packs compile 36 more entry points per material type.
- To revisit: the denoise of RGBA8 from four gathers per channel (×2.9 on the Radeon);
  the lights + environment variant's registers.
