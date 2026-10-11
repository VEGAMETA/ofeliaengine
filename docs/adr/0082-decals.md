# 0082. Decals: a decal buffer before lighting

- Status: Accepted (option 2, the owner, 2026-10-11)
- Date: 2026-10-11
- Research: [docs/research/decals.md](../research/decals.md)
- Related: [ADR 0045](0045-local-lights.md) (z-bins and tile bitmasks),
  [ADR 0058](0058-ambient-occlusion.md) (the register rule), [ADR 0079](0079-pipelines-on-first-use.md)
  (variants made on first use), queue items 2 and 7c (forward.rs split by feature)

## Context

Queue item 2 asks for clustered decals. The engine has none. Decals change a surface's
material before it is lit (albedo, roughness, metalness, normal). Each feature of the
forward shading is a literal parameter of `shade_surface` with entry points of its own,
so a view without the feature pays nothing, not even registers (CLAUDE.md §5).

## Options

1. **Clustered decals in the shading** (DOOM 2016, Godot 4, Bevy 0.16):
   - decals are binned with the lights' z-bins and tile bitmasks, and each shading
     invocation walks its pixel's decals before lighting;
   - every surface takes them, transparent ones included, with or without the prepass;
   - **cost:** a `decals` literal doubles the 32 fragment entry points and the forward
     renderer's pipeline sets (plain and multisampled).
     - Made on first use since ADR 0079, so start-up does not grow; shader build time
       and forward.rs's size do.
     - forward.rs (5,600 lines) would best be split by feature (queue item 7c) first.
2. **A decal buffer before lighting** (deferred decals, as in Killzone 2, Frostbite and
   Unreal's DBuffer):
   - after the depth prepass, a compute pass evaluates each pixel's decals (the same
     bins) into one or two render-size textures: albedo with coverage, roughness and
     metalness, a normal;
   - the shading reads them at its pixel: a variant still, but a texel load and a blend
     instead of a loop;
   - **cost:** the prepass whenever decals draw (it is on with AO and in the iGPU
     defaults), about 8 to 12 bytes per pixel, and transparent surfaces get no decals.
3. **Mesh decals** (a box or a projected mesh drawn after the opaque surfaces, blended
   into the lit color): no shading change, but the decal's albedo is not lit as the
   surface's, and normal and roughness cannot change. Rejected.

## Recommendation (to confirm)

Option 1, after the forward.rs split (7c), because it is what the clustered renderers it
is modelled on do, it decals everything, and ADR 0079 removed its start-up cost. Option 2
if the owner prefers a smaller change to the shading now. Measurements (decal cost per
pixel on both GPUs, registers of the decal variants) come with the prototype.

## Decision

**Option 2, a decal buffer before lighting** (the owner, 2026-10-11: "a decal buffer is
necessary; without it the corners would come out wrong"). After the depth prepass, a
compute pass evaluates each pixel's decals, binned like the lights (z-bins and tile
bitmasks), from the pixel's position and normal in the depth buffer, into render-size
targets (albedo with coverage, a normal, roughness and metalness); the forward shading's
decal entry points read them at their pixel and blend before lighting.

## Consequences

- Decals draw only with the depth prepass (on with AO and in the iGPU defaults); a view
  with decals turns it on.
- 12 bytes per pixel of transient targets, written and read only in the 16-pixel tiles
  some decal reaches; nothing without decals (the register rule: entry points of their
  own, made on first use, ADR 0079).
- Transparent surfaces, subsurface materials, multisampled views and probe captures get
  no decals (the last two for now).
- Measured below (Implementation).

## Implementation (2026-10-11)

- `engine_render::decals`: `Decal` (a box `[-0.5, 0.5]³` projected along its −z onto
  surfaces facing its +z; an albedo texture with coverage in alpha, a tangent-space normal
  map, a tint and opacity, roughness and metalness replacing the surface's by `surface`,
  `angle_fade`, `order`), `DecalPasses` (`prepare`, `add_prepared`, or `add_passes` for
  both), `DecalTargets`.
- Culling on the CPU: the frustum, then each box's corners give its screen rectangle in
  16-pixel tiles and its depth range along the view axis (a box across the near plane:
  its corners in front and where its edges cross it; a bounding sphere's rectangle is
  several times larger for flat boxes). Decals are kept in drawing order (`order`, then
  list order), so the z-bins are bitmasks (256 bins × a bit per decal), not the lights'
  index ranges, which need depth order. The tiles' bitmasks are set on the GPU (a group
  per decal, as the lights'); the occupied tiles (a bit per tile) and their list come from
  the CPU.
- The decal pass: a group of 64 per occupied tile, each thread a 2 × 2 quad of its 16 × 16
  pixels. Per pixel the depth's z-bin first (one row of the inverse projection), then the
  wave walks the union of its pixels' decals (scalar loads of each decal's 144 bytes).
  A pixel in a decal's box gets its position, its normal from the depth (on each axis the
  closer neighbour, so edges do not bend it; turned towards the pixel's ray) and its
  texture gradients from the neighbours' positions. A decal's normal map is laid in the
  surface's plane (the decal's axes projected onto it), so a decal across a corner or a
  slope keeps its relief the right way up.
- Targets: albedo (`R8G8B8A8_UNORM`: the square root of rgb premultiplied by coverage,
  and the coverage) and surface (`R32G32_UINT`: the normal's change, three signed 10-bit
  components premultiplied by coverage, added to the shading's normal, so the surface's own
  normal map stays under a decal; roughness, metalness and their coverage, 10 bits each).
- The shading: 16 decal entry points (with ambient occlusion, contact shadows, local
  shadows; the reflections' too), `DecalShading` with the variants of the features in use
  (up to 36 pipelines, made on first use). `apply_decals` tests the occupied tile, reads
  two texels and blends before lighting. The forward push grew to 120 bytes (two indices);
  the frame's padding holds the occupied tiles' address.
- `ForwardRenderer::set_decals`, `ForwardView::decals`, `SceneInput::decals` (the frame
  renderer), `ForwardPasses::decals`; `scene_bench --decals <count> [--decal-size f]`; the
  golden shot `procedural_decals` and the perf scene of the same name.

### Measurements (ref-1, 1080p, release)

| Case | RTX 4060 Ti | Radeon iGPU (2 CUs) |
|---|---:|---:|
| Golden scene, five decals: the decal pass | 34 µs | 0.63 ms |
| ... the scene's frame without decals → with them (decals turn the prepass on) | 400 → 443 µs | 13.84 → 12.26 ms |
| Sponza with the prepass, frame without decals | 0.50 ms | 13.5 ms |
| ... one decal covering the screen (decal pass; frame) | 145 µs; +0.16 ms | 4.09 ms; +4.9 ms |
| ... 64 decals of 1-3 m on the floor, one around the camera | 130 µs; +0.13 ms | 2.80 ms; +3.6 ms |
| ... 256 such decals | 253 µs; +0.26 ms | 5.66 ms; +6.5 ms |

- The Radeon's cost is instructions: ~300 per pixel inside a decal (normal from depth,
  gradients, two samples, normal map frame, packing) on 128 lanes. Found on the way: 8 × 8
  groups of one pixel each cost ~50 ns apiece to launch on the iGPU (1.7 ms for an empty
  pass over 1080p, 28 µs on the RTX), hence a group per occupied tile and 4 pixels per
  thread (4.58 → 4.09 ms for the screen-covering decal, 3.73 → 2.80 ms for 64).
- Registers of the shading (AMD's fragment stage, VGPRs): plain 48, decals 48; local
  lights 64 → 58; environment 64 → 64; lights + environment 80 → 90. NVIDIA: 56 → 56,
  63 → 56, 63 → 62, 64 → 61. The decal pass: 65 VGPRs on gfx1103 (RGA).
- **Option 1 measured too** (branch `exp/decals-clustered`, not merged): the same culling,
  the decal loop in the shading (position and normal from the interpolants, gradients from
  `ddx`/`ddy`). Its frame cost over the same Sponza cases: RTX +0.085 / +0.071 /
  +0.156 ms (better than the buffer), Radeon +6.4 / +6.2 / +10.4 ms (worse: the loop
  raises the registers of the whole forward shading, the iGPU's largest pass). The decal
  buffer stays.
- Determinism: the golden shot is byte-identical run to run on both GPUs; 0 heap
  allocations per frame with 24 moving decals (`zero_alloc::local_shadows`).
- A driver quirk on the way: on AMD's driver (26.8.1) a buffer address read out of a
  struct value that was itself loaded through an address read as zero; access chains
  (`push.frame.decals.bins[i]`) work. The shading's decal code does not need it now.
- A bug on the way: the normal from depth was turned towards the eye along the view axis;
  a camera looking slightly up turned floors upside down (no decals on them). It is turned
  against the pixel's ray now.

Left: decals in multisampled views and probe captures, emissive decals, decals on
transparent surfaces (option 1's shading for them alone), half-precision math and a normal
shared by a 2 × 2 quad for the iGPU.
