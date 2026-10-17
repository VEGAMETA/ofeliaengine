# 0029. Scene text format: line-oriented blocks, one field per line

- Status: Accepted (format chosen by the team, 2026-09-26). Version 2 (strings and
  containers, assets by id and path, prefab instances): [ADR 0108](0108-scene-format-2.md)
- Date: 2026-09-26
- Research: [docs/research/reflection.md](../research/reflection.md) ("What stays open
  for the team")
- Follows: [ADR 0022](0022-reflection.md), [ADR 0017](0017-ecs.md)

## Context

CLAUDE.md §13: source assets are text, VCS- and merge-friendly, with stable ordering
and stable GUIDs. The on-disk format is an irreversible decision, so it was the
team's call.

The options were:
1. our own line-oriented format (in the spirit of Godot's `.tscn`);
2. RON;
3. TOML or JSON.

The team chose option 1.

## Decision

```text
[scene version=1]

[entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
Transform.rotation = (0, 0, 0, 1)
Transform.translation = (10, 0, 0)
Transform.scale = (1, 1, 1)
Unit.team = Blue
Unit.target = @0b12e4f8c3a94d5e9f7a6b1c2d3e4f50

[entity 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50]
parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
Transform.translation = (1, -0.5, 0)
Marker = {}
```

- **Identity.** `SceneId` is a component with 128 random bits, written as 32 hex
  digits. Entities without one are runtime-only and are not saved.
- **Order is deterministic:**
  - roots are sorted by id;
  - children follow their parent depth-first, in the parent's child order (so
    sibling order survives a round trip);
  - components are sorted by reflected name;
  - fields keep declaration order.
- **One field per line:** `Component.path = value`.
  - Nested structs use dotted paths.
  - Vector-like structs (all `f32` fields named from `x y z w`: `Vec2`/`Vec3`/`Vec4`/
    `Quat`) are written as `(a, b, c)`.
  - Arrays of numbers are written as `[a, b, c]`.
  - Enums are written by variant name.
  - Entity fields are written as `@<id>` or `@none`.
  - Fields marked `#[reflect(skip)]` are not written.
  - A component without saved fields is written as `Name = {}`.
- **Floats** are written in their shortest round-trip form and parsed at their own
  width, so values survive a round trip bit for bit.
- **Loading:**
  - Missing fields keep `Default`; unknown fields become warnings with a line number
    (schema changes do not break old scenes).
  - Unknown components, bad values and unknown parents are errors with a line number;
    on error nothing is added to the world.
  - A newer `version` than the engine's is refused.
- **Registry.** `SceneRegistry::register::<T: Component + Reflect + Default>()`.
  `require::<T, C>()` adds derived runtime components on load, for example
  `GlobalTransform` for `Transform`.

## Measurements

`cargo bench -p engine_scene --bench scene`, container (4 threads, not reference),
bench profile. 10,000 entities with `Transform` in 100 hierarchies of 100:

| operation | time | per entity |
|---|---:|---:|
| save | 9.2 ms | 0.92 µs |
| load | 13.0 ms | 1.30 µs |

The text is 1.9 MB, 59,901 lines. Loading moves each entity between archetypes once
per added component. Spawning with the final component set in one step is the obvious
next optimization when scenes get large.

## Consequences

- Diffs and merges are line-local:
  - two people editing different fields of one entity merge cleanly;
  - adding an entity adds one block.
- Formats for prefabs, settings and input maps can reuse the same line grammar.
- Not yet supported:
  - prefab instances with overrides;
  - strings (reflection has no string type yet);
  - non-plain-data components.
- Version 1 is frozen once scenes exist in projects. Later changes bump `version` and
  ship a migration.
