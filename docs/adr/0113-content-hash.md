# 0113. The content hash of cache keys: XXH3-128

- Status: Accepted (2026-10-22)
- Date: 2026-10-22
- Builds on: [ADR 0008](0008-shader-compilation.md) (`StableHasher`, never to change),
  [ADR 0041](0041-texture-compression.md) (the derived data cache and its keys),
  [ADR 0108](0108-scene-format-2.md) (E4-8: the editor's textures through the cache)

## Context

The derived data cache names its entries by keys that hash everything they derive from
(ADR 0041). Three keys cover whole source files:
- textures: the image file;
- panoramas: the `.hdr` or OpenEXR file;
- convex parts of colliders: the mesh's vertices and triangles.

They hashed those bytes with `hash128`, 128-bit FNV-1a, which reads a byte at a time:
1.13 GB/s on ref-1. Since E4-8 the editor loads textures from the cache. There the hash
was most of the work:
- FlightHelmet's 15 textures (45 MB of PNG) on one thread: 42 ms of keys and 18 ms of
  entries read;
- a project of a gigabyte of images would spend a second of CPU on keys alone.

ADR 0008 promised `StableHasher` would never change: the shader cache and the pipeline
cache name files by it.

## Options

1. **Keep FNV-1a.** Nothing changes; loads from the cache stay bound by the hash.
2. **Make `StableHasher` faster.** Breaks ADR 0008's promise: every shader, pipeline and
   derived entry everywhere is orphaned, for the small inputs those caches hash.
3. **`StateHasher`, the engine's fast 64-bit hash of simulation state.** Its four lanes
   never mix until the end, and its own documentation says it is not a cache key.
4. **Our own fast 128-bit hash.** A new design for a key that must never change has no
   review behind it. An implementation of a known one (XXH3) is about 300 lines with
   SIMD paths of our own to verify.
5. **XXH3-128 through the twox-hash crate.**
   - The specification has been frozen since xxHash 0.8.0 (2020).
   - twox-hash 2.1.5 is MIT, maintained (2026-10-04), has no dependencies with the
     features used, is tested against xxHash's C reference, and picks AVX2 or SSE2 at
     run time (CLAUDE.md §5).
6. **BLAKE3.** Cryptographic strength nobody needs here. It is slower on one thread and
   a larger crate, with C and assembly by default.

## Measurements

ref-1 (Ryzen 7 7700), release:

| Measurement | `hash128` (FNV-1a) | `content_hash128` (XXH3-128) |
|---|---|---|
| Throughput, 16 MB | 1.13 GB/s | 55–64 GB/s |
| FlightHelmet's keys on one thread (45 MB of PNG) | 42 ms | 1 ms |
| FlightHelmet from the cache, shown in the first frame (it waits) | 34 ms | 31 ms |
| FlightHelmet from the cache, shown when added later (an editor's drop) | 31 ms | 27 ms |

The loads gained less than the keys did: the hash was spread over eight workers. What is
left is reading the entries (18 ms for 68 MB on one thread) and building the meshes.

## Decision

- **`engine_core::hash::content_hash128(bytes)`** is XXH3-128 with seed 0. Like
  `StableHasher`, it never changes. A test pins its values for inputs on every path of
  the algorithm (3, 100, 200, 5,000 bytes, 1 MiB) and xxHash's own value for no bytes, so
  a dependency update that changed them fails.
- **Keys that cover whole source files use it** and feed the result to `StableHasher`
  with the rest of the key. Their version strings move to 2:
  - `engine texture 2`;
  - `engine panorama 2`;
  - `engine collider parts 2`.
- **`StableHasher` stays as ADR 0008 promised:** the shader cache, the pipeline cache and
  every key's small parts (settings, versions).

## Consequences

- **The old entries of those three kinds are orphaned.** Each texture, panorama and
  collider is computed once more on its next use. This covers the editor's `.ofelia/ddc`
  and `test-assets/.ddc`: the golden tests' and the perf gate's first run after this
  change cooks again. The cache has no eviction yet; orphaned entries can be deleted with
  their folder.
- **One more dependency** in `engine_core` (THIRD_PARTY_NOTICES.md).
- **Loads from the cache now pay mostly for reading entries.** `texture_from_bytes` copies
  each level out of the entry; reading straight into the levels is the next step if it
  shows.
