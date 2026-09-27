# 0041. Texture compression: BC7 and BC5 on the CPU, a derived data cache

- Status: Accepted
- Date: 2026-09-27
- Builds on: [ADR 0040](0040-materials-and-textures.md) (materials and textures)
- Research: [docs/research/meshes-and-materials.md](../research/meshes-and-materials.md)

## Context

ADR 0040 left textures as RGBA8:

- Sponza's took 363 MB.
- Texturing cost 4.5 ms of the Radeon iGPU's 8.1 ms forward pass, a memory-bandwidth
  bill.
- GPUs sample block-compressed formats natively at a quarter of that size or less:
  - **BC7:** 16 bytes per 4 × 4 texels, RGB or RGBA;
  - **BC5:** two channels at full precision, made for normal maps;
  - **BC4:** one channel;
  - BC1 and BC3: older, lower quality.

Compressing is slow and only depends on the source and the settings, so results belong
in a derived data cache (CLAUDE.md §13).

## Options

**Encoders**, measured by `bench/texture-compress` (a research harness outside the
workspace) on Sponza's and FlightHelmet's textures, level 0, one thread:

1. **block_compression 0.10 (MIT) (chosen for now).**
   - A Rust port of Intel's ISPC Texture Compressor kernels (MIT): BC1-5 and BC7, with
     the ISPC presets.
   - Pure Rust (with `bytemuck`), no build tools; its GPU part (wgpu) is left out.
   - The port is scalar, so it runs slower than ISPC's SIMD.
2. **bc7e (Binomial, ISPC) and Intel's ISPC compressor.**
   - The best quality per time in Aras Pranckevičius' 2020 comparison: 10–20 Mpix/s on
     12 threads above 45 dB.
   - They need the ISPC compiler at build time, and a vendored binary is not allowed.
3. **bc7enc (Geldreich, C++).** "Behind the curve in all aspects" in the same comparison.
4. **A GPU encoder in Slang.**
   - Ported from the same ISPC kernels, as block_compression's WGSL shows it can be.
   - Orders of magnitude faster on the editor's GPU; the next step (below).

| block_compression, Sponza | Mtexels/s, one thread | mean / worst PSNR dB |
|---|---:|---:|
| color, BC7 ultra fast | 23.2 | 44.50 / 31.51 |
| color, **BC7 very fast (chosen)** | 1.0 | 46.22 / 35.31 |
| color, BC7 basic | 0.3 | 46.44 / 35.81 |
| color, BC1 | 49.3 | 36.47 / 28.67 |
| color with alpha, **BC7 alpha very fast** | 0.7 | 45.12 / 43.55 |
| color with alpha, BC3 | 54.2 | 40.91 / 39.44 |
| data, **BC7 very fast** | 1.0 | 49.12 / 39.13 |
| data, BC7 basic | 0.3 | 52.28 / 40.59 |
| normal maps, **BC5** | 119.4 | 43.82 / 33.95 |
| normal maps, BC7 fast | 0.7 | 38.56 / 26.08 |

- **BC7 "very fast"** is within 0.2 dB of "basic" on color, at three times the speed.
- **Data textures** gain 3 dB from "basic"; that waits for the GPU encoder.
- **BC5** beats BC7 on normal maps by 5 dB, at 100 times the speed.
- **BC1 and BC3** lose 5–10 dB.
- **FlightHelmet** (smooth PNG sources) gives the same order at higher PSNR: BC7 very
  fast 53.65 dB on color, BC5 58.11 dB on normals.

**Cache:**

- a. Cache nothing: every load compresses again (6 s for Sponza).
- b. **A content-addressed directory (chosen),** like the shader cache (ADR 0008):
  - the key hashes the source bytes, the usage (and mask cutoff), the mip filter and the
    encoder with its settings;
  - entries are written to a temporary name and renamed, so a crash never leaves a torn
    entry and several processes can share the directory.

  The entry format is internal to the cache. It is versioned through the key and a magic
  number, and it is not the cooked runtime format, which remains a team decision.

## Measurements

ref-1, release, 1920×1080 HDR, 2048² shadow map, GPU µs (medians), the interior view
of ADR 0040. RGBA8 → BC7/BC5:

| | texture memory | RTX forward | Radeon forward |
|---|---:|---:|---:|
| Sponza (69 textures) | 363 → 91 MB | 390 → 283 | 8,109 → 6,892 |
| FlightHelmet (15 textures) | 272 → 68 MB | 69 → 67 | 2,066 → 1,782 |

- **Anisotropic filtering,** Sponza with BC textures:

  | Filtering | RTX forward | Radeon forward |
  |---|---:|---:|
  | trilinear | 249 | 5,613 |
  | 4× | 289 | 6,688 |
  | 8× | 299 | 6,818 |
  | 16× | 283–303 | 6,892 |

  - Most of the cost comes with 4×. The default stays 16× (quality); a graphics setting
    can lower it on integrated GPUs.
  - What remains on the iGPU is texture fetches times overdraw. A depth prepass takes the
    overdraw away.
- **Import on the 16 threads of the job system** (texture by texture, and a large
  texture's rows of blocks in parallel):
  - Sponza (71 Mtexels): 5.8 s with an empty cache, 13 ms from the cache (87 MB of
    entries);
  - FlightHelmet (53 Mtexels): 4.5 s, 12 ms.
- **Correctness:**
  - `tests/materials.rs` renders a BC7 texture within 4 steps of its texels, and a BC5
    normal map shading exactly like its RGBA8 original (744 = 744, 741 = 741), on both
    GPUs with 0 validation errors;
  - unit tests cover every level's size (odd sizes, levels smaller than a block), the
    same blocks with and without threads, PSNR bounds, cache entries round-tripping and
    rejecting garbage, and keys changing with content and settings.

## Decision

- **`engine_assets`:**
  - `compress` turns a `TextureImage` into an `EncodedTexture`: BC7 sRGB for color
    (alpha modes when any texel is not opaque), BC7 linear for data, BC5 for normal maps.
    The block_compression "very fast" presets run on the job system.
  - `DerivedDataCache` (`get`, `put`) and `import_texture` (decode, mips, compress, or
    load from the cache).
- **`engine_render::texture_pool`:** `TextureLevels::format` is RGBA8 (sRGB or linear),
  BC7 (sRGB or linear) or BC5. Staging offsets are aligned to 16 bytes.
- **Device:** `textureCompressionBC` joins the required features; every desktop GPU has
  it (ADR 0006).
- **Shaders unchanged:** normal maps already rebuild Z from X and Y, which BC5 needs.
- `scene_bench` imports through the cache (`--ddc`, `--uncompressed` for comparisons).

## Consequences

- **Next: a GPU BC7 encoder in Slang** (from the ISPC kernels, as block_compression's
  WGSL shows). Import time should drop by orders of magnitude, which makes the "basic"
  and "slow" presets affordable (+3 dB on data).
- **The cache is local.** A team-shared cache (a network directory or a service) comes
  with the project system.
- **Kept out for now:**
  - BC4 for single-channel textures: glTF packs occlusion with metallic-roughness;
  - BC6H: no HDR textures yet (environment maps come with IBL).
- **All textures stay resident.** Streaming mip levels by distance can now read blocks
  from the cache.
