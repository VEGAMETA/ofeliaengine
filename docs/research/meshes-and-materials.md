# Real content: mesh storage, vertex format, materials, textures

Checked 2026-09-27. Sources at the end. No code was copied from any of them.

## Where the renderer stands

- One host-visible buffer per mesh, pulled in the vertex shader through its device
  address.
- No index buffer: three vertices per triangle, so a vertex shared by six triangles is
  stored and shaded six times.
- 32-byte vertices (position and normal as `float4`), no texture coordinates or
  tangents.
- Materials are three numbers per instance (base color, metallic, roughness). No
  textures, no normal maps, no alpha masks, no double-sided surfaces, no anti-aliasing.
- Every measurement so far is on cubes that cost almost nothing to draw. The occlusion
  culling numbers showed how that misleads: a loss on the RTX only because a cube is
  cheaper than the pass that culls it.

## Mesh storage in other engines

- **Bevy (MIT/Apache, `bevy_render::mesh::allocator`):**
  - Vertex and index data of many meshes packed into shared buffers ("slabs"), one
    vertex layout per slab.
  - Allocation requests are batched; a slab grows by allocating a larger buffer and
    copying on the GPU.
  - Reason: meshes drawn back to back without rebinding buffers, which multi-draw
    indirect needs.
- **niagara (Arseny Kapoulkine, MIT) and meshoptimizer's documentation:** one vertex and
  one index buffer for the scene; per-mesh offsets in the draw data; quantized vertex
  attributes.
- **Godot (MIT):** octahedral normals and tangents: 16 bits per axis ("Oct32") or 8
  ("Oct16") instead of 12-16 bytes per vector.

## Vertex quantization (meshoptimizer's guidance)

- **Positions:** half floats, or 16-bit normalized relative to the mesh's box.
  - Box-relative 16-bit: steps of 1.5 mm on a 100 m mesh.
  - Neighboring meshes quantized to different boxes no longer meet exactly: cracks along
    shared edges.
  - Half floats step by 3 cm at 50 m from the mesh origin.
- **Normals:** 8 or 10 bits per component; octahedral (Cigolle et al. 2014) is the best
  error per bit. At 16 bits per axis the error is far below what shading shows.
- **Texture coordinates:** half floats, or 16-bit normalized relative to the mesh's UV
  range.
  - Precision is range / 65535: fine for 0..1, but a floor tiled 64 times loses about
    2 texels of a 2048 texture.
  - Half floats step by 1/32 at 64.
- **Optimization order:** index → vertex cache → (overdraw) → vertex fetch → quantize →
  filter degenerate triangles; optionally a position-only index buffer for depth passes.
- **Meshlets:** 64 vertices and 124-126 triangles, cone weight around 0.25 when
  cluster cone culling is used.

## Materials

- **glTF 2.0 metallic-roughness:**
  - base color (sRGB texture × linear factor, alpha);
  - metallic (B) and roughness (G) in one texture;
  - tangent-space normal map with a scale; occlusion (R) with a strength;
  - emissive (sRGB × factor × `KHR_materials_emissive_strength`);
  - alpha opaque, mask with a cutoff, or blend; double-sided.
- **The BRDF stays the one already in use** (Filament's documentation: GGX,
  height-correlated Smith, Schlick, Lambert). glTF's reference BRDF is the same family.
- **Bindless material tables** (Bevy 0.16's bindless `StandardMaterial`, niagara, The
  Forge's visibility buffer sample):
  - materials in one GPU buffer, instances refer to a material index, textures by
    bindless index;
  - no per-material descriptor sets and no rebinding between draws.
- **Specular aliasing:**
  - normal maps and curved surfaces alias in highlights even with good texture
    filtering;
  - geometric specular anti-aliasing (Kaplanyan et al. HPG 2016; Tokuyoshi and Kaplanyan
    I3D 2019; Filament ships it) widens roughness by the variance of the normal over the
    pixel, a few instructions per pixel;
  - Toksvig's factor (2005) does the same for normal-map mip levels at import.
- **Alpha-tested foliage** thins out in the distance because averaging alpha into mip
  levels lowers coverage. Castaño's coverage-preserving mip generation (2010) rescales
  each level's alpha so the share of texels above the cutoff stays that of level 0.

## Textures

- **Decoding:**
  - `png` (image-rs, MIT/Apache) is now the fastest PNG decoder in Rust (411 Mpix/s on
    average in image-rs' 2026 benchmark, ahead of `zune-png`; Chromium's default since
    M139).
  - `zune-jpeg` (MIT/Apache/Zlib) matches libjpeg-turbo.
- **Mip levels:**
  - filter in linear light: sRGB decoded first;
  - a better kernel than a 2×2 box for minification sharpness;
  - coverage preservation for masked materials;
  - normalization plus Toksvig for normal maps.
- **Compression (next step):**
  - BC7 for color and color with alpha, BC5 for normal maps (two channels, z rebuilt),
    BC4 for single channels.
  - Aras Pranckevičius' 2020 comparison: bc7e (ISPC, Richard Geldreich, MIT/Unlicense)
    is the best BC7 encoder on quality per time (over 45 dB at 10-20 Mpix/s per core);
    ISPC Texture Compressor (MIT) for BC1/BC3.
  - GPU compute encoders (Compressonator, DirectXTex, MIT) were not measured there.
  - Measure against each other on our textures before choosing; results go into a
    derived data cache keyed by content.

## Test content (downloaded into a local cache, never committed)

| Scene | License | Use |
|---|---|---|
| Intel Sponza (Intel Graphics Research samples) | CC-BY 4.0 | main performance scene; numbers and images publishable with attribution |
| Sponza (Khronos glTF-Sample-Assets) | Cryengine Limited License | local comparisons only (the reference scene of many engines); not committed, no published images |
| FlightHelmet, Corset, Lantern (Khronos) | CC0 | materials |
| ABeautifulGame, MetalRoughSpheres, AlphaBlendModeTest, EmissiveStrengthTest (Khronos) | CC-BY 4.0 | materials, conformance |

## Conclusions

1. **Geometry first (ADR 0039):**
   - one pool of vertices and one of 32-bit indices, suballocated, growing by GPU
     copies;
   - positions as `f32 × 3` in their own stream (depth-only passes fetch 12 bytes);
   - normal and tangent octahedral at 16 bits per axis, texture coordinates as
     `f32 × 2`: 16 bytes of attributes;
   - indexed indirect draws.
   - Box-relative positions and range-relative UVs save 10 more bytes, but cost cracks
     and precision on tiled surfaces; they wait for measurements on real scenes.
2. **Materials and textures (next ADR):** a bindless material table, default textures
   instead of branches, masked and double-sided buckets, geometric specular AA,
   coverage-preserving mip levels.
3. **Compression and the derived data cache:** after measuring the encoders.
4. **meshoptimizer** (MIT, C source, vendored like Box2D) for vertex cache and fetch
   order, then meshlets and LODs.

Measured afterwards (ADR 0039):

- **Indexed vs. triangle soup** on Sponza: forward pass half as long on the RTX, 37% less
  on the Radeon.
- **Layout "position + normal in one aligned 16-byte load"** changed nothing on the RTX
  and cost up to 7% on the Radeon's bandwidth-bound shadow pass. The 12-byte position
  stream stays.

Measured afterwards (ADR 0040):

- **Bindless material table:**
  - 80 bytes per material, textures by bindless index with the sampler index packed in;
  - a missing texture is an index that skips the sample, not a default texture: fewer
    fetches, and flat normals stay exact (UNORM8 cannot store 0).
- **Front-to-back batch order within each pipeline state:** Sponza's forward pass
  halves (RTX 712 → 376 µs, Radeon 17.4 → 8.1 ms).
- **Texturing Sponza with RGBA8:** 225 µs on the RTX, 4.5 ms on the Radeon iGPU (memory
  bandwidth). That makes compression the next step, then a depth prepass for the
  overdraw that remains.
- **Scalarizing the shading per material within a wave:** slower on the RTX; not taken.
- **Texture import:** decoding at 157–184 Mtexels/s per thread; Kaiser mip levels at
  45–86 Mtexels/s. Sponza's 69 textures take 0.2 s on 16 threads.

Compression (ADR 0041), measured with `bench/texture-compress`:

- **BC7 very fast** (block_compression, a Rust port of the ISPC kernels): 46.2 dB on
  Sponza's color, 0.2 dB below "basic" at three times the speed (1 Mtexel/s per thread).
- **BC5 for normal maps:** 5 dB better than BC7 and 100 times faster.
- **Sponza's textures:** 363 → 91 MB; forward RTX 390 → 283 µs, Radeon 8.1 → 6.9 ms.
- **Import:** compressing takes 5.8 s on 16 threads; the derived data cache brings it to
  13 ms. A GPU encoder is the next step.

## Sources

- [Bevy: mesh allocator](https://github.com/bevyengine/bevy/blob/main/crates/bevy_render/src/mesh/allocator.rs)
- [meshoptimizer README (v1.3)](https://github.com/zeux/meshoptimizer)
- [Godot: octahedral normal/tangent compression (PR #60309)](https://github.com/godotengine/godot/pull/60309)
- Cigolle, Donow, Evangelakos, Mara, McGuire, Meyer: "A Survey of Efficient Representations for Independent Unit Vectors" (JCGT 2014)
- [glTF 2.0 specification: materials](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#materials)
- [Filament: Physically Based Rendering in Filament](https://google.github.io/filament/Filament.html) (specular anti-aliasing)
- Kaplanyan, Hill, Patney, Lefohn: "Filtering Distributions of Normals for Shading Antialiasing" (HPG 2016); Tokuyoshi, Kaplanyan: "Improved Geometric Specular Antialiasing" (I3D 2019)
- Toksvig: "Mipmapping Normal Maps" (2005); Castaño: "Computing Alpha Mipmaps" (2010)
- [image-rs: the Rust PNG crate gets even faster (2026)](https://blog.image-rs.org/2026/06/18/png-adoption.html)
- [zune-jpeg](https://docs.rs/crate/zune-jpeg/latest)
- [Aras Pranckevičius: Texture Compression in 2020](https://aras-p.info/blog/2020/12/08/Texture-Compression-in-2020/)
- [bc7enc_rdo (bc7e.ispc)](https://github.com/richgel999/bc7enc_rdo)
- [block_compression 0.10 (docs.rs, checked 2026-09-27)](https://docs.rs/block_compression)
- [bc7enc (C++ BC1-5 and BC7)](https://github.com/richgel999/bc7enc)
- [Khronos glTF-Sample-Assets: model list and licenses](https://github.com/KhronosGroup/glTF-Sample-Assets/blob/main/Models/Models.md); [Sponza README](https://github.com/KhronosGroup/glTF-Sample-Assets/blob/main/Models/Sponza/README.md)
- [Intel Graphics Research samples (Intel Sponza)](https://www.intel.com/content/www/us/en/developer/topic-technology/graphics-research/samples.html)
