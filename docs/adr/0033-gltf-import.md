# 0033. glTF 2.0 import into memory with the `gltf` crate

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/assets-gltf.md](../research/assets-gltf.md)

## Context

CLAUDE.md §13: glTF 2.0 is the primary import format; meshes go through
meshoptimizer; textures become KTX2. The engine could render only procedural cubes.
Real scenes, the stress scene and the editor all need models.

The on-disk cooked (runtime) format is an irreversible decision for the team. This ADR
covers only import into memory.

## Options

1. **Own glTF parser** on a JSON parser. Full control, but the spec surface (accessors,
   sparse data, normalized integers, strides, extensions, GLB) is large, and
   correctness takes a long tail of files.
2. **The `gltf` crate** (MIT OR Apache-2.0), without default features (chosen):
   - the document model with spec validation and accessor readers (strides,
     normalized types, sparse);
   - no image decoding or URL handling pulled in.
   - It brings serde, serde_json and a few small crates (all MIT/Apache/Unlicense).
   - Last release May 2024, but widely used (about 2M downloads in 90 days) and
     stable; the importer stays swappable behind `import_gltf`.

## Decision

- **`engine_assets::import_gltf(bytes, base_dir)`** → `ImportedScene`:
  - meshes as indexed triangle lists (positions, normals, UV0, tangents, bounds). Missing
    normals become area-weighted smooth normals; missing indices are generated.
    Non-triangle primitives are skipped.
  - metallic-roughness materials, with emissive strength, alpha mode and double-sided;
  - texture sources (URI or embedded buffer view), not decoded;
  - nodes with local TRS and children; roots of the default scene.
- **Buffers are resolved here:**
  - the GLB binary chunk;
  - base64 data URIs (own decoder);
  - files next to the asset. `..` and absolute paths are refused, so an asset cannot
    read outside its folder.
- **Validation:** out-of-range indices and index counts that are not a multiple of 3
  are errors, so the renderer never reads outside a vertex buffer.
- **`spawn_scene(world, scene, parent)`:**
  - one entity per node with `Transform`, `GlobalTransform` and parent links (cycles
    in malformed files are cut);
  - a `MeshInstance { mesh }` on mesh-bearing nodes.

## Measurements

None yet. Import is a tool-time path; a bench comes with large sample files in the
asset pipeline work.

## Consequences

- Models can be loaded into the ECS today; the renderer draws them through
  `Primitive::triangle_vertices` until indexed meshes arrive.
- Next:
  - meshoptimizer (vertex cache, overdraw, LODs, meshlets);
  - texture decoding and conversion to KTX2 (BC7/BC6H/BC5);
  - the cooked format (team decision) and the DDC;
  - a `cargo-fuzz` target for the importer (§15).
