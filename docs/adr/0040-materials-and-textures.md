# 0040. Materials and textures: a bindless material table, filtered mip chains

- Status: Accepted
- Date: 2026-09-27
- Builds on: [ADR 0039](0039-geometry-pool.md) (geometry pool),
  [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU-driven draws),
  [ADR 0006](0006-vulkan-baseline-and-device.md) (bindless set)
- Research: [docs/research/meshes-and-materials.md](../research/meshes-and-materials.md)

## Context

After the geometry pool, real scenes had their meshes but not their surfaces:

- a material was three numbers per instance (base color, metallic, roughness);
- there were no textures, normal maps, alpha masks, double-sided surfaces or emission;
- nothing fought specular aliasing.

The team's priority is quality and speed on real content. The target is glTF 2.0's
metallic-roughness model, the format artists export and the Khronos samples use.

## Options

**Where materials live:**

1. **Factors per instance** (as before), textures bound per draw.
   - Impossible with one indirect draw call for many batches: nothing can be bound
     between them.
2. **A descriptor set per material,** the classic way.
   - Same problem: a set has to be bound for every material, so draws cannot be merged.
3. **A bindless material table (chosen):** Bevy 0.16's bindless `StandardMaterial`,
   niagara, The Forge's visibility buffer.
   - Every material is 80 bytes in one GPU buffer; an instance carries its slot.
   - Textures are indices into the bindless set, each with its sampler index in the top
     8 bits.

**Missing textures:**

- a. Sample 1 × 1 white default textures: branch-free, but four extra fetches per pixel.
  - With a flat default normal map it also tilts normals: 8-bit UNORM cannot store 0,
    and 128 decodes to +0.0039, 0.3°.
- b. **A `NO_TEXTURE` index, and the sample skipped (chosen).**
  - The branch is uniform across a quad (a quad belongs to one primitive, so to one
    material), so derivatives stay defined.
  - Measured on cubes against (a), RTX: city 1k 94 → 84 µs, inside the field 10k
    69 → 65 µs.

**Pipeline states:** alpha (opaque or masked) × face culling (single- or double-sided) ×
winding (mirrored transforms flip it, glTF requires it):

- a. A pipeline per combination: 8 forward and 8 shadow pipelines.
- b. **Two pipelines per pass, opaque and masked (chosen).**
  - Cull mode and front face are dynamic state (`vkCmdSetCullMode`, `vkCmdSetFrontFace`,
    core in Vulkan 1.3).
  - Batches are ordered by state, so each state is one indirect draw.
  - Masked pipelines are separate because a shader that can discard loses the early
    depth test.

**Order of batches within a state:**

- As listed, or **front to back by the first instance's box (chosen)**, so the depth test
  rejects hidden fragments before shading.

**Scalarizing the shading per material within a wave:**

- A loop over `WaveReadLaneFirst` makes the material load and texture indices uniform.
- Measured worse on the RTX and rejected: 10k cubes of 64 materials 52 → 80 µs, Sponza
  390 → 401 µs.
- A warp reading one address is already a single transaction.

**Texture import:**

- Decoders: `png` (image-rs, the fastest PNG decoder in Rust) and `zune-jpeg` (as fast as
  libjpeg-turbo). Both are pure Rust with permissive licenses (research note).
- Mip levels:
  - a 2 × 2 box, or a **Kaiser-windowed sinc (chosen,** NVTT's parameters: half-width 3,
    α = 4), which keeps the detail a box blurs away;
  - filtered in linear light with premultiplied alpha;
  - coverage-preserving alpha for masked materials (Castaño 2010);
  - renormalized normal maps.

## Measurements / arguments

ref-1, release, 1920×1080 HDR, 2048² shadow map, GPU µs (medians). `scene_bench`: the
camera inside, down the scene's long axis (Sponza's atrium); textures RGBA8 with mips.

**Real scenes:**

| | RTX shadow | RTX forward, textured / factors only | Radeon shadow | Radeon forward, textured / factors only |
|---|---:|---:|---:|---:|
| Sponza (262k triangles, 25 materials, 69 textures) | 76 | 390 / 165 | 763 | 8,109 / 3,612 |
| FlightHelmet (95k triangles, 6 materials, 15 textures) | 36 | 69 / 43 | 284 | 2,066 / 1,158 |

- **Front-to-back batches,** Sponza forward, textured:
  - RTX 712 → 376 µs; Radeon 17,350 → 8,102 µs.
  - Factors only: RTX 313 → 165 µs; Radeon 6,911 → 3,621 µs.
- **Texturing costs** 225 µs on the RTX and 4.5 ms on the Radeon iGPU.
  - The iGPU reads uncompressed RGBA8 (363 MB for Sponza) from system memory.
  - Compression to BC7/BC5/BC4 cuts that data by 4 and is the next step.

**Cubes (`draw_bench`), main → this:**

| | RTX add_passes | RTX forward | Radeon add_passes | Radeon forward |
|---|---:|---:|---:|---:|
| 10k, overview | 88 → 80 | 48 → 51 | 55 → 66 | 1,142 → 1,316 |
| 10k, inside the field | 88 → 80 | 44 → 65 | 55 → 66 | 1,030 → 1,272 |
| 10k, city blocks | 89 → 81 | 63 → 97 | 55 → 67 | 1,792 → 2,262 |
| 100k, overview | 989 → 812 | 269 → 269 | 576 → 598 | 5,949 → 6,424 |
| 100k, city blocks | 991 → 813 | 82 → 119 | 585 → 607 | 1,730 → 2,207 |

- **CPU (`add_passes`):**
  - 64-byte instances instead of 80 save 18% on the RTX, which writes the upload ring
    through the PCIe BAR.
  - On the iGPU the ring is cached memory, so the saving is smaller than the new
    per-draw work (the material lookup, the mirror test): +1 ns per draw.
- **GPU, untextured cubes filling the screen:** the full material model costs
  +12…57% on the forward pass. On the RTX, 10k cubes inside the field, 44 → 65 µs:
  - about 15 µs is the per-pixel material load and its branches (44 µs with a constant
    material);
  - about 6 µs is specular antialiasing (59 µs without it).

  Both are the price of the material model on untextured pixels. Real content pays for
  its texture fetches instead (above). The shadow pass is unchanged.

**Texture import** (`texture_bench`, `scene_bench`):

- One thread:
  - decoding runs at 184 Mtexels/s for JPEG and 157–176 for PNG;
  - mip levels at 45–51 Mtexels/s for color, 74–76 for normal maps and 83–86 for data.
- Sponza's 69 textures (71 Mtexels):
  - 1.51 s on one thread, 195–199 ms on the 16 threads of the job system (7.4×);
  - the first run reads cold files from disk: 1.66 s.
- FlightHelmet's 15 textures (2048², 53 Mtexels) take 188–192 ms, bound by the largest
  texture.
- A block-compression encoder will dominate these times, so the filters stay scalar for
  now.

**Correctness** (`tests/materials.rs`, under synchronization validation, on both GPUs;
0 validation errors):

- **Textures:**
  - every texel of an sRGB texture comes back within one step through an sRGB target;
  - repeat, clamp and mirrored wrap modes sample the texels they should.
- **Alpha masks:**
  - a masked quad shows what is behind it only where alpha is below the cutoff;
  - its shadow lets light through there (231 vs 34 on the ground; an opaque quad shadows
    both halves).
- **Faces:**
  - single-sided back faces are culled; double-sided ones are drawn and lit from their
    side;
  - a mirrored instance keeps its front faces and shows its texture mirrored.
- **Normal maps (glTF convention):**
  - red tilts along +u, green up the image;
  - lit 60° off the normal, brightness goes from 555 flat to 744 when the tilt leans
    towards the light and 0 when it leans away;
  - mirrored, +u points along −X.
- **Streaming and edits:**
  - with a budget below one texture, textures arrive one per frame, and materials show
    their factors until then;
  - removing a texture reverts its materials to their factors;
  - material edits apply the next frame, and a removed material's id draws the default
    material.

## Decision

- **`engine_render::material::MaterialTable`:**
  - glTF metallic-roughness `MaterialDesc`s:
    - base color and alpha, metallic-roughness, normal map with scale, occlusion with
      strength, emission;
    - each factor times an optional `TextureRef` (texture and wrap modes);
    - alpha opaque or masked; double-sided.
  - `add`, `set` and `remove`, with generational ids. Slot 0 is the default material;
    stale ids draw with it.
  - `begin_frame`:
    - resolves texture indices again when the texture pool changes;
    - uploads changed materials through a staging buffer;
    - imports the table (`MaterialFrame`) into the frame's graph.
- **`engine_render::texture_pool::TexturePool`:**
  - RGBA8 textures (sRGB or linear) with full mip chains;
  - images created at `add`, their data uploaded oldest first within a budget of bytes
    per frame;
  - registered in the bindless set once uploaded; until then materials shade without
    them;
  - removals are freed after their frames.
- **Forward renderer:**
  - `DrawItem { mesh, material: MaterialId, world_from_object }`, with 64-byte
    instances.
  - Batches are runs of one mesh and one pipeline state (masked, double-sided,
    mirrored). They are ordered by state, then front to back, and drawn with one
    indirect draw per state.
  - Opaque and masked pipelines for the forward and shadow passes, with cull mode and
    front face as dynamic state (`GraphicsPipelineDesc::dynamic_culling`).
- **Shading** (`forward.slang`):
  - normal mapping in the MikkTSpace frame, the per-pixel bitangent from unit vertex
    vectors;
  - back faces of double-sided surfaces flip the whole tangent frame;
  - mirrored instances flip the bitangent sign;
  - geometric specular antialiasing (Tokuyoshi and Kaplanyan 2019, Filament's variance
    0.15 and threshold 0.2);
  - occlusion on the ambient term, emission added;
  - a surface facing away from the light stays in its own shadow whatever its normal map
    says.
- **Alpha-tested materials discard with `OpDemoteToHelperInvocation`**, so derivatives
  after the test stay defined. `shaderDemoteToHelperInvocation` joins the required device
  features: Vulkan 1.3 requires every device to support it.
- **Samplers:** nine material samplers, 16× anisotropic trilinear, one per pair of wrap
  modes (`Bindless::material_sampler`).
- **`engine_assets`:**
  - `decode_image` (PNG, JPEG) and `build_mips` (per `TextureUsage`);
  - the glTF importer now keeps each texture's image (embedded bytes or a relative path,
    percent-decoded) and wrap modes, and every material map with its scale or strength;
  - `read_texture` loads a texture's encoded image.
- `scene_bench` loads a glTF scene's textures in parallel, measures textured and
  factors-only shading, and saves screenshots. `texture_bench` measures import stage by
  stage.

## Consequences

- **Every renderer user owns a `TexturePool` and a `MaterialTable`,** and calls their
  `begin_frame`s before drawing. The old `forward::Material` is gone.
- **Next, from these measurements:**
  - **Texture compression and the derived data cache:** BC7 for color, BC5 for normal
    maps, BC4 for single channels. The iGPU spends 4.5 ms of Sponza's frame reading
    RGBA8 textures.
  - **A depth prepass,** measured against front-to-back order alone. It removes the
    remaining overdraw, and clustered lights, ambient occlusion and TAA need depth first
    anyway.
- **glTF gaps:**
  - `BLEND` materials draw opaque until the transparency pass;
  - `KHR_texture_transform`, a second set of texture coordinates, vertex colors and
    `KHR_materials_transmission` are not supported yet.
- **Normal-map variance in roughness** (Toksvig or LEAN, baked at import) waits for the
  derived data cache. Geometric specular antialiasing covers curved geometry, and TAA
  will cover the rest.
- **All textures stay resident;** Sponza takes 363 MB. Streaming mip levels by
  distance comes with compression.
- **Draw order:** with occlusion culling, batches keep being known by their instances'
  positions in the draw list, which front-to-back ordering does not change.
