# Transform hierarchies

Checked: 2026-09-26. Sources were read for design ideas only; no code was carried over.

## Sources

| Project | Version | License | What we looked at |
|---|---|---|---|
| Bevy (`bevy_transform`) | 0.18 | MIT/Apache-2.0 | `Transform` / `GlobalTransform`, `propagate_transforms`, static-scene optimization (dirty-bit tree) |
| Godot (`Node3D`) | 4.x | MIT | local transform, lazily computed global transform with a dirty flag, notifications |
| Unity (`Transform`, `TransformAccessArray`) | product docs | proprietary (docs only) | hierarchies stored contiguously per root, jobs parallel over roots |
| flecs | 4.x | MIT | `cascade` queries (breadth-first by depth) for hierarchical systems |

## How each one works

- **Bevy:**
  - `Transform` (translation, rotation, scale) and `GlobalTransform` (`Affine3A`).
  - A system walks from roots down in parallel, one task per root.
  - Since 0.16 it tracks which subtrees contain changes and skips clean ones ("static
    scene" optimization).
  - Children are stored as a `Vec<Entity>` component.
- **Godot:**
  - A node's global transform is computed on demand and cached.
  - A change marks the subtree dirty through notifications.
  - Cheap for sparse changes; the recursive notification is costly for large moving
    hierarchies.
- **Unity:**
  - Each root hierarchy is one contiguous buffer in depth-first order.
  - Updating it is a linear pass.
  - Jobs split work by root.
  - Reparenting between roots copies the buffers.
- **flecs:**
  - `cascade` iterates entities ordered by hierarchy depth.
  - A system can then run a flat loop where every parent is done before its children.

## What we take

- **Two components** (Bevy): `Transform` is written by game code and the editor;
  `GlobalTransform` (`Affine3A`, 64 bytes) is derived. Both are plain data, so they are
  snapshotted and hashed like everything else.
- **Walk from roots over the intrusive child list** that `engine_ecs` already has
  (`Parent` / `Children`, ADR 0017). The walk uses an explicit stack and no allocation
  in steady state.
- **Skip when nothing changed:** three chunk-version scans (`Changed<Transform>`,
  `Changed<Parent>`, `Changed<Children>`) decide whether any work is needed.

## Not yet (follow-ups, with the numbers that justify them)

- **Per-subtree dirty tracking** (Bevy, Godot). Our change versions are per chunk, so a
  single moved root marks a whole chunk. Per-subtree skipping needs a per-entity dirty
  bit.
- **Parallel over roots** (Bevy, Unity). Roots are independent. This needs a shared-world
  random access path in `QueryState` (`get` through `&World`, unsafe, like
  `for_each_shared`).
- **Depth-ordered storage** (Unity, flecs cascade). This would turn the walk into linear
  passes. It changes the storage model, so it is not attempted without a profile that
  demands it.
