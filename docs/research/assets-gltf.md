# glTF import

Checked: 2026-09-26. Sources were read for design ideas only; no code was carried over.

| Option | Version | License | Notes |
|---|---|---|---|
| `gltf` crate | 1.4.1 (2024-05) | MIT OR Apache-2.0 | document model, validation, accessor readers; optional image and URI import |
| cgltf (C) | 1.14 | MIT | single-header C parser used by many engines; would need bindings |
| Godot's importer | 4.x | MIT | read for how nodes map to scene nodes and how missing normals are generated |
| Bevy's `bevy_gltf` | 0.18 | MIT/Apache-2.0 | built on the `gltf` crate; maps nodes to entities |

## What we take

- The `gltf` crate for parsing and validation (Bevy uses it too); our own buffer
  resolution, so no URL or image code comes along.
- Node hierarchy to entities with local transforms (Godot, Bevy).
- Smooth normals when missing (Godot's default), and generated indices for
  non-indexed primitives.

## Not yet

meshoptimizer processing, texture decoding and KTX2, skins and animations, morph
targets, cameras and lights (`KHR_lights_punctual` is enabled in the parser but not
mapped yet).
