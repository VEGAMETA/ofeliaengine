# Brixelizer GI

Research for the bar of [ADR 0084](../adr/0084-global-illumination.md): our probe GI must
cost less GPU time and memory than AMD's Brixelizer GI at comparable quality, on both
reference GPUs, and Brixelizer GI, built from the FidelityFX SDK, is the reference
measurement and the fallback. 2026-10-11. Decision: [ADR 0090](../adr/0090-brixelizer-gi-reference.md).

## Sources (checked 2026-10-11)

| Source | License | What we read |
|---|---|---|
| AMD FidelityFX SDK v1.1.4, commit `c6efa6bf7f2027b3ec94f28578bb5965eabb9e55` (2025-05-08): Brixelizer 1.0.0 (`sdk/src/components/brixelizer`, `sdk/include/FidelityFX/host/ffx_brixelizer*.h`, `sdk/include/FidelityFX/gpu/brixelizer`), Brixelizer GI 1.0.1 (the same under `brixelizergi`), their Vulkan GLSL passes (`sdk/src/backends/vk/shaders/brixelizer*`), the Vulkan backend (`sdk/src/backends/vk/ffx_vk.cpp`) | MIT | The code we build (ADR 0090), the same SDK and commit as FSR 3.1 (ADR 0060) |
| The SDK's `docs/techniques/brixelizer.md` and `brixelizer-gi.md` | MIT | The algorithm overviews, the resource tables |
| The SDK's sample, `samples/brixelizergi` (`brixelizergirendermodule.cpp`, `shaders/lightinggi.hlsl`, `lightinggicommon.h`, `config/brixelizergiconfig.json`) | MIT | How AMD configures and composites it |
| Lou Kramer, "Real-time Sparse Distance Fields for Games" (GDC 2023, gpuopen.com) | talk | The distance field builder's design |
| Boissé, Guidoux, Tanner et al., "GI-1.0: A Fast Scalable Two-Level Radiance Caching Scheme for Real-Time Global Illumination" (AMD, 2023, gpuopen.com) | paper | The GI that Brixelizer GI simplifies (screen probes over a world-space cache) |

## How it works

### The sparse distance field (Brixelizer)

- **Cascades.** A cascade is a cube of 64³ voxels centered on a point (the camera), and
  each voxel holds at most one brick of 8³ unsigned distances (`R8_UNORM`). All cascades
  share one 512³ atlas: at most 2^18 bricks. A cascade's voxel size is its brick size; AMD's
  sample uses 8 cascades from 0.2 m, doubling (12.8 m to 1.6 km across).
- **The tree.** A three-level AABB tree per cascade (the root, 4³ nodes, 16³ leaves over the
  bricks' bounds) skips empty space.
- **Building.** It is built from triangles by compute passes, from jobs (an instance's world
  bounds overlapping the cascade):
  1. culling the voxels already built;
  2. voxelizing triangles into per-voxel references;
  3. distances per brick from the references;
  4. compressing into the atlas;
  5. an eikonal pass filling the rest.
- **Updates.** The main API updates one cascade per frame (cascade 0 every second frame,
  cascade 1 every fourth, and so on). Static cascades rebuild only voxels that scrolled in
  or were invalidated. A dynamic cascade is rebuilt from its instances at every update and
  merged with the static one into the cascade that is traced.
- **Tracing.** A ray steps through the cascades from the most detailed, walks each
  cascade's tree and sphere-traces the bricks it enters. A hit gives a distance and a brick:
  no triangle, no material.

### The GI (Brixelizer GI)

- **The radiance cache.** It holds 4³ texels per brick (a 256³ `R11G11B10` 3D texture).
  Every frame it takes the previous frame's lit image, reprojected onto the bricks under
  1/16 of the pixels (a rotating 4 × 4 pattern). Only surfaces that were on screen get
  radiance. Off screen, a surface keeps what it last had, or stays black.
- **The screen probes.** One probe per 8 × 8 tile of the internal resolution, each with
  8 × 8 octahedral directions: one ray per internal pixel and frame, traced through the
  distance field. Hits are lit from the radiance cache, misses from the environment cube
  (times an intensity). The probes are reprojected from frame to frame.
- **The irradiance cache.** Second-order harmonics (9 coefficients) per brick, fed by the
  screen probes and spread to neighbouring bricks by a pass over the 64³ voxels of one
  cascade per frame. Each pixel's diffuse light is the nearest brick's harmonics on its
  normal (the docs: "resolved by projecting the nearest SH probe"); the screen probes'
  own result is kept only where no brick is near (`FfxBrixelizerGIInterpolateScreenProbes`
  overwrites it, and its blend weight `total_sh_cache_weight` is computed but unused).
- **The outputs.** Diffuse and specular light (the specular from a quarter-resolution
  pre-trace of brick ids, then a trace). They are computed at an internal resolution
  (native, 75, 50 or 25 %), denoised (temporal, then two blur passes) and upsampled to the
  display size.
- **The sample's composite.**
  - Light: `diffuseGI × baseColor × 1.5` and `specularGI × (F0 · A + F90 · B) × 3.0`. The
    factors are ad hoc, the "GI factors" of its UI.
  - Lit history: direct diffuse light plus diffuse GI. With the GI in it, the light gains
    a bounce every frame.

### What it needs from the renderer

| Input | AMD's sample | Our forward renderer today |
|---|---|---|
| Depth and last frame's depth | G-buffer depth, copied each frame | Depth after the prepass; no history |
| Normals and last frame's | G-buffer normals, copied | None before the shading: the probe GI's resolve takes them from the depth; the reflections' normals are written by the shading, too late |
| Roughness | G-buffer | In the shading only |
| Motion vectors | G-buffer | Written by the prepass (current − previous, texture coordinates) |
| Last frame's lit image | Deferred lighting's diffuse output, copied | The view's HDR image, transient |
| Blue noise | 16 textures of 128² RG (Christoph Peters', loaded from files), one per frame | None |
| Environment | The prefiltered IBL cube, intensity 0.1 | The prefiltered cube and its intensity |
| Matrices | View, projection, last frame's, row-vector convention | Column-vector `glam` matrices, Vulkan's Y-down clip space, depth 0 at the near plane |

### Memory (the SDK's constants)

| Resource | Size |
|---|---:|
| Distance field atlas, 512³ `R8_UNORM` | 128 MiB |
| Brick bounds, 2^18 × 4 B | 1 MiB |
| Per cascade: brick map 64³ × 4 B, tree 18 KB | 1.0 MiB (8 cascades static only: 8.1 MiB; static, dynamic and merged as in the sample: 24.4 MiB) |
| Context buffers (counters, brick lists, eikonal and merge lists, instances, transforms) | a few MiB |
| Build scratch | set by the triangle, reference and brick limits (the sample: 1 GiB) |
| GI radiance cache, 256³ `R11G11B10` | 64 MiB |
| GI brick harmonics: two sets of 2^18 × 9 × 8 B, and a state of 2^18 × 16 B | 40 MiB |
| GI screen-sized targets at 960 × 540 (50 % of 1080p): ping-pong probes, GI, specular, downsampled inputs | ~50 MB |

That comes to about 300 MiB before the build scratch. Our probe GI over Sponza holds 13 MB
of probe maps, plus the acceleration structures its rays need (ADR 0087; measured in ADR
0090).

### Things found reading the code

- **Depth.** `FfxBrixelizerGIDepthCloserOp` (`ffx_brixelizergi_common.h`) tests
  `#ifdef FFX_BRIXELIZER_GI_OPTION_DEPTH_INVERTED`. The permutation always defines it (0 or
  1), so with standard depth it picks the farther depth. AMD's sample uses inverted depth.
  We feed it inverted depth too.
- **CPU sizes.** The contexts are large: `FfxBrixelizerContext` is 23.8 MB,
  `FfxBrixelizerBakedUpdateDescription` 8.4 MB, `FfxBrixelizerGIContext` 0.8 MB. Allocate
  them once on the heap, never on a stack.
- **Normalization.** The diffuse output is irradiance: an open floor under a uniform sky
  of radiance 1 gets 3.19 to 3.21 on both GPUs at 512 × 512 (π). At 128 × 128 it gets 2.19:
  the screen probes need a view larger than a few tiles. The sample's factor 1.5 is a look,
  not a unit. (The screen probes' own result, were it used, would be π/8 of that.)
- **One library.** The SDK's shader blob dispatcher (`ffx_shader_blobs.cpp`) compiles in
  one effect per `#ifdef`. FSR 3.1 and Brixelizer must therefore be one build of the SDK
  in one binary: two builds would define the backend's symbols twice.

Found running it (2026-10-11, both GPUs, under the validation layer), worked around in
`engine_ffx`'s Vulkan layer without touching the SDK:

- **The global descriptor pool** (`ffx_vk.cpp`, `CreateBackendContextVK`) lists six
  descriptor types but is created with `poolSizeCount = 5`, which leaves out the storage
  buffers. FSR 3.1 never needed them; Brixelizer's passes do. Drivers allocated anyway,
  but the layer warns that they may not.
- **The bindless pool** has no `FREE_DESCRIPTOR_SET_BIT`, but its sets are freed one by
  one when an effect is destroyed.
- **Buffers made through the backend** get the transfer usages only if initialized, and
  the SDK copies from the build's scratch to read its counters back. Initializing one races
  with the first copy, so the scratch is created in the copy-source state.
- **Measured sizes.** A distance field context with our limits (1 M references, 16 MB of
  triangle swap, 16,384 bricks a build: 76.6 MB of scratch) holds 255 MiB of GPU memory. A
  GI context at 64 × 64 holds 104 MiB, almost all of it the radiance cache and the brick
  harmonics, which do not depend on the view's size.

Found integrating it (2026-10-11):

- **AMD's compiler breaks the distance field build.** On ref-1's Radeon (RDNA2, driver
  26.8.1) a test scene built 30,054 bricks where the RTX built 38,468: holes through the
  floor and a wall. Every counter, scan and triangle reference matched bit for bit (read
  back from the build's scratch); only `FfxBrixelizerEmitSDF` wrote nothing into some
  bricks. Its conservative edge tests of a triangle's projections take the edges as
  differences of vertices in brick coordinates. A floor's projection onto xy is a segment,
  whose tests pass with no margin, so an edge parallel to an axis must be exactly zero
  along it. AMD's compiler leaves a residue instead (1e-5 between two equal coordinates
  of 1624.5: below the coordinates' own precision, so the subtraction is fused with the
  division's multiply into an FMA), and whole triangles are dropped. `precise` on the
  subtraction does not help, and that part is a bug of AMD's driver: `EmitSDF` computes
  the same differences again without it (`ba`, `ac`, `cb`), and the driver merges the
  NoContraction subtraction with its undecorated twin and fuses it anyway. A minimal
  shader shows it (`engine_rhi`'s `contraction_repro` example, 65,536 lanes; driver
  26.8.1): no `precise`, 6,921 lanes with a residue; the subtraction `precise`, none; the
  same with its plain twin used too, 6,921 again (NVIDIA: none in any variant).
  Fixed in the vendored SDK (`third_party/README.md`): the edges come from the
  world-space vertices. With two smaller fixes (edges stored as halves rounded to the
  nearest, AMD truncating; device-scope fences around the scans' last-group handoffs)
  both GPUs build the same bricks. Ruled out on the way: the wave size (32 and 64 alike),
  memory ordering, the grid alignment of faces, the iteration limit.
- **The GI must wait for the distance field.** The main API builds one cascade per frame
  (the 8th of 8 cascades on the 65th update). Rays that leave the built cascades miss
  everything and take the sky's light, which then lives in the caches, fed back through
  the lit image, and fades by half every 48 frames or so: a closed room under a sky of
  radiance 1 still had 0.74 of irradiance after 64 frames (0.07 after 256), and Sponza
  looked lit by a flat ambient. `engine_brixelizer` gives the first updates the frame
  indices that build each cascade once, the coarsest last, and runs the GI from then on
  (the room: 0.008 / 0.010).
- **Motion vectors are required**: without them the GI does not run. Views without
  temporal antialiasing get the camera's motion, made from the depth by our input pass
  (the same light as with the renderer's vectors within 4 %).
- **Hits read a cosine lobe.** A ray that hits a brick reads its directional harmonics
  (radiance injected along each surface's normal) through a clamped cosine around the ray:
  the radiance times the cosine of the hit, where a Lambertian surface has the same radiance
  every way. A floor point 0.4 m in front of a sunlit wall gets 0.22 to 0.23 after 64
  frames on both GPUs, and 5 m away 0.08. One bounce gives 0.958 and 0.170 (integrated
  over the wall), with the floor's light on the wall about 1.14 and 0.20; probe GI gives
  1.19 and 0.20 on the same scene (`engine_render`'s test). Brixelizer GI's is a fifth.
  The cosine alone explains about half of the loss (0.45 on average there); the rest we
  did not trace. The radiance cache holds the wall's lit radiance (0.71 against 0.71).

Reported to AMD on 2026-10-11, for other developers on SDK 1.1.4: the holes and the
driver's lost NoContraction ([#202](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/issues/202)),
the sky let in before the field is built ([#203](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/issues/203)),
the descriptor pool without storage buffers ([#204](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/issues/204)).
AMD dropped Brixelizer and the Vulkan backend from the SDK's 2.x line (the repository's
main is the FSR SDK 2.3.0, checked 2026-10-11): 1.1.4 is their last version, so no fix
is likely to come. The driver bug goes to AMD's driver team through AMD's Bug Report Tool
with the example's package.

## Measurements (2026-10-11)

`cargo run -p engine_golden --release --example gi_bench` on ref-1: Sponza through the
golden bed at 1920 × 1080 without antialiasing, the courtyard (`sponza_gi`'s view) and the
medallions from above (`sponza_gi_medallions`'s); Brixelizer GI with the sample's settings
(8 cascades from 0.2 m, static and dynamic, 50 % internal resolution on the RTX and 25 %
on the iGPU), warmed up 256 frames; median of 100 frames. Response and build-up at
640 × 360, every frame captured; response with the sun alone (a black sky).

| | probe GI, RTX | Brixelizer GI, RTX | probe GI, Radeon | Brixelizer GI, Radeon |
|---|---:|---:|---:|---:|
| GI passes, ms (courtyard / medallions) | 0.343 / 0.328 | 1.550 / 1.629 | 2.38 / 2.38 | 33.0 / 32.2 |
| GPU frame, ms | 1.03 / 1.06 | 2.19 / 2.36 | 20.9 / 21.3 | 54.0 / 54.0 |
| Memory, MiB | 20.4 (volume 12.8, acceleration structures 7.5) | 594 (distance field 364.2, view 229.8) | 28.7 (12.8, 15.8) | 532 (364.2, 167.8) |
| Start | converged before the first frame: 10 frames, 19.9 ms of GPU (+1.9 for the structures) | field built in 8 frames (about 15 ms); 90 % of its light after 116 frames | 10 frames, 951 ms (+44) | about 0.85 s; 90 % after 138 frames |
| Sun turned off: frames to 50 % / 10 %, left after 400 | 15 / 162, 1.3 % | 180 / > 400, 17 % | 123 / > 400, 35 % | 203 / > 400, 23 % |
| Mean irradiance (courtyard / medallions) | 0.090 / 0.075 | 0.052 / 0.030 | 0.090 / 0.074 | 0.053 / 0.030 |

Brixelizer GI's passes, courtyard, RTX / Radeon, ms: the SDK's GI 1.19 / 22.2, the field's
update 0.19 / 2.3, our input pass (inverted depth, normals and the camera's motion from the
depth) 0.11 / 5.5, the conversion to the shading's irradiance 0.04 / 2.6, the lit image's
copy 0.02 / 0.4. A deferred renderer would have most of our inputs for free; the SDK's own
dispatch alone costs 3.5 / 9.3 times probe GI's passes.

Pictures (`target/gi_bench/`, and the golden shot `sponza_brixelizer_gi` on both
vendors): Brixelizer GI keeps the arcades in shade, but its bounce is weaker and bluer, the
sunlit floor's warm light barely reaching the walls; seen from above, the curtains and the
medallions are dark (its specular is not composited, and reflection probes captured in a
screen GI view see only the sky, ADR 0090). Probe GI on the iGPU: its far probes wait for
their turn of the ray budget (ADR 0089), so a sun turned off leaves a third of its bounce
after 400 frames; a refresh after a large change of light (`ProbeGi::refresh`) converges in
ten frames, as at load.

## Patents (search 2026-10-11)

A web search found no AMD patent or application on Brixelizer's sparse distance fields
(bricks in cascades over a shared atlas) or on Brixelizer GI's and GI-1.0's caches. The MIT
license grants no patent rights. Google Patents' result pages could not be read from here,
so the search is not conclusive. This matters only if Brixelizer GI ships as the fallback
(ADR 0084); a proper search then goes to the humans. (It does not: ADR 0090.) Our blue
noise tile is made by Ulichney's void-and-cluster method (1993; `examples/blue_noise.rs`),
whose patents, filed then, have expired.

## What we take

- **Build.** The SDK's Brixelizer and Brixelizer GI, built as FSR 3.1 is (ADR 0060), with
  three fixes for AMD's compiler (`third_party/README.md`):
  - the C++ compiled with `cc`;
  - the GLSL compiled to SPIR-V by our generator with the SDK's glslang 11.12.0;
  - the shader blobs served from our tables.
- **Inputs.** What our renderer lacks is computed for it, as cheaply as we can:
  - normals from the depth (the same normals our probe GI's resolve uses, so both GIs see
    the same surfaces);
  - inverted depth;
  - a pair of textures for this frame and the last;
  - a copy of the lit image;
  - a generated blue noise tile.
- **Configuration.** AMD's sample, with fair settings:
  - the internal resolution our resolve uses (50 % on discrete GPUs, 25 % on integrated
    ones);
  - static and dynamic cascades, since our GI follows moving geometry too.

- **Start-up.** Every cascade built once over the first updates, the GI running only after.
- **Debug views.** The SDK's views of the distance field (`SdfDebugView`) and of the GI's
  caches (`DebugView`), for tests and the golden bed.

## What we leave

- Specular GI, in a comparison that is diffuse against diffuse. Our specular comes from
  the reflection probes and screen-space reflections.
- The raw API (the main API is what AMD recommends).

## Conclusion

Probe GI meets ADR 0084's bar on both GPUs: 4.5 times (RTX) and 14 times (Radeon) less GPU
time, 29 and 18 times less memory, a bounce of the physical size where Brixelizer GI gives a
fifth (in front of a sunlit wall: 1.19 against 0.23, physically about 1.14), and on the RTX
a response to a change of light twelve times faster. Brixelizer GI
stays as the reference (the golden shot, `gi_bench`); with no ray queries it would be the
only GI of the two that runs.
