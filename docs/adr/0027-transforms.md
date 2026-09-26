# 0027. Transforms: local and world components, propagation from roots

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/transforms.md](../research/transforms.md)
- Follows: [ADR 0017](0017-ecs.md) (next step 5), [ADR 0026](0026-system-scheduler.md)

## Context

Rendering, physics sync, audio and the editor need world-space transforms of entities
in a hierarchy (`Parent` / `Children`, ADR 0017).

Requirements:
- plain-data components (snapshots, rollback, state hashes);
- no per-frame allocation;
- a static scene should cost nothing.

## Options

1. **Global transform computed on demand with dirty flags** (Godot). Cheap for sparse
   changes. Needs notifications down the subtree on every change, and hidden work when
   something reads a world transform.
2. **Derived `GlobalTransform` component, propagated by a pass from the roots**
   (Bevy; chosen). World transforms are ordinary component data, readable by any system
   or the GPU upload without side effects.
3. **Contiguous per-root hierarchy buffers** (Unity). Fastest linear pass, but it
   changes the storage model; reparenting copies buffers.

## Measurements

`cargo bench -p engine_ecs --bench ecs`, container (4 threads, not reference), bench
profile:

| scenario | µs |
|---|---:|
| propagate 100k entities (1 000 roots × 9 children × 10 grandchildren), one root moved | 2,126 (21 ns per entity) |
| same world, nothing changed | 0.1 |

The first version called `World::get` / `get_mut` per child: 2,863 µs. Every such call
hashes the component's type. One cached query `get` per child (`Parent`, `Transform`,
`GlobalTransform`, `Option<Children>`) is 26% faster. The remaining time is mostly
memory traffic: about 14 MB of transform and link data for 100k entities.

## Decision

- `Transform { rotation: Quat, translation: Vec3, scale: Vec3 }` (48 bytes, padding
  explicit and hidden from reflection). Constructors: `IDENTITY`, `from_xyz`,
  `from_translation`, `from_rotation`, `from_scale`, `with_rotation`, `with_scale`.
- `GlobalTransform(Affine3A)`, not reflected (derived data).
- `TransformPropagation::run(&mut World) -> bool`:
  - returns false without work if no `Transform`, `Parent` or `Children` changed
    (chunk versions);
  - otherwise it sets roots from their locals and walks each root's children depth-first
    with an explicit stack.
  - A child without both components cuts its subtree off.
- In a schedule it is an exclusive step (`add_exclusive`), after game logic and before
  anything reads world transforms.
- `engine_core` enables glam's `bytemuck` feature so glam types can be fields of
  plain-data components.

## Consequences

- One moved root recomputes the whole world. Follow-ups:
  - per-subtree dirty bits;
  - parallel propagation over roots, which needs a shared-world `QueryState::get`.
- Scale then rotation then translation; non-uniform parent scale shears rotated
  children (as in Bevy, Unity and Godot).
