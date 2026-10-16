# 0103. Navigation: Recast and Detour, baked by the cook

- Status: Accepted
- Date: 2026-10-16
- Related:
  - [ADR 0056](0056-cooked-packs-and-streaming-reads.md): packs;
  - [ADR 0099](0099-player-crate.md): the player;
  - [ADR 0101](0101-mesh-colliders.md): the colliders whose level geometry is baked;
  - research note `docs/research/navigation.md`;
  - core audit 3 §4, queue item 4.

## Context

- **Nothing navigates.** The third review (core audit 3 §4) lists navigation among the
  core's gaps for a game; CLAUDE.md §2 names `engine_nav`, §13 points at Recast/Detour.
- **A game needs** (the research note):
  - a walkable surface made from the level for an agent of a height, radius, step and
    slope;
  - paths from A to B as corners to walk between;
  - the nearest point on the mesh, and a ray along it ("can I walk straight there?");
  - later: crowds that steer around each other, doors and moving obstacles, jumps and
    ladders (off-mesh links), area costs.
- **The engine's rules:** no allocation per frame; a feature costs nothing where unused;
  the heavy part happens in the cook, behind its one button.

## Options

1. **Recast and Detour, vendored (C++, Zlib) behind a C shim, built by `cc`** (as Jolt and
   Box2D are).
   - **For:** the industry's standard (Unreal and Godot bake with Recast); complete
     (baking, paths, queries, crowds, obstacles, off-mesh links); no dependencies;
     Detour's tiles are flat blobs a pack keeps as they are.
   - **Against:** a C++ shim to write and keep.
2. **Rust crates:** rerecast (a Recast port, 0.4) to bake, landmass (0.9) for paths and
   avoidance.
   - **For:** pure Rust.
   - **Against:** both young; landmass pulls `geo` and its dependencies; nothing measured
     against Detour; no tile cache or crowds of Detour's maturity.
3. **Our own, in Rust.**
   - **For:** fully ours.
   - **Against:** weeks to reach Recast's robustness on messy levels, for the same result.

The owner's rule for vendored C++ (2026-10-16: our own Rust port when the C++ brings
BSL, MPL or LGPL code or heavy dependencies) does not apply: Recast is Zlib and depends on
nothing.

## Measurements

ref-1 (Ryzen 7 7700), 2026-10-16. The engine's crates at `opt-level = 1` (the dev
profile); Recast and Detour are always built with `-O3`.

| Scene | Level geometry | Tiles | Size | Bake | Load | A path |
|---|---|---|---|---|---|---|
| Sponza (glTF sample) | 1 static body, 262,267 triangles | 6 | 23.6 KiB | 24–30 ms | 0.05 ms | across the courtyard, 17.86 m, complete, 8–11 µs |
| Intel Sponza | 115 static bodies | 6 | 34.4 KiB | 0.21 s | — | — |

- **The cook:** the bake is 0.03 s of Sponza's 2.3 s and 0.21 s of Intel Sponza's 46 s
  (its meshes take 15.9 s, writing the pack 30.4 s).
- **The library** (Recast, Detour, DetourCrowd and the shim) builds from clean in 1.8 s
  (in parallel), once; 1.0 MB of objects.
- **Tests** (`engine_nav/tests/nav.rs`, levels with known answers):
  - around a wall: a path through its gap of 24–26.5 m; the straight ray blocked at 40–50 %;
  - 40 m of floor, 16 tiles baked in parallel: a straight path across their seams within
    2 %;
  - an island out of reach: a partial path to the shore facing it;
  - nothing walkable (a wall too thin to stand on): no tiles.

## Decision

- **Recast and Detour, vendored** at commit `9f4ce64` (after release 1.6.0;
  `third_party/recastnavigation`: Recast, Detour, DetourCrowd, their `License.txt`), built
  by `cc` as C++17 with 64-bit polygon references (`DT_POLYREF64`: no limit on tiles times
  polygons).
- **`engine_nav`:**
  - `BakeSettings`: the agent (1.8 m tall, 0.4 m radius, 0.4 m step, 45° slopes), cells of
    0.15 × 0.1 m, tiles of 80 cells (12 m), Recast's region and detail parameters;
  - `bake(vertices, triangles, settings, jobs)`: tiles baked in parallel on the job pool,
    each from the triangles Rust buckets into it (with a border of the agent's radius plus
    three cells, so tiles meet without seams); `NavMeshData` holds Detour's tile blobs;
  - `NavMesh`: `nearest` (`&self`), `path` (corners into the caller's `Vec`, complete,
    partial or none) and `raycast` (`&mut self`, the query's search state); nothing is
    allocated per query once the `Vec` has room; `Send` and `Sync`.
- **The cook bakes by default** (`CookSettings::navigation`, `ofeliaengine cook
  --no-navmesh` for none): the level geometry is every static, non-sensor body's colliders
  as placed, meshes as they are and boxes, round shapes and hulls as their hulls' faces;
  a mirroring placement keeps faces facing out. Moving bodies are not the level. Without
  colliders (`--no-colliders`) nothing is baked.
- **The pack:** entry kind `NavMesh` (code 12), pack version 8; packs without one keep
  their version. `meta[0]` is the tile count; the data is the grid's origin and tile width,
  then each tile's column, row, byte count and bytes. A scene's loading skips it.
  `engine_assets::navigation::{level_triangles, load_navigation, read_nav_entry}`.
- **The player:** `Player::load_navigation()` reads the entry and puts the
  `engine_nav::NavMesh` into the world as a resource; a game that does not navigate never
  calls it, and the entry is not read.

## Consequences

- A game finds paths on its level with no set-up beyond `load_navigation`; agents walk
  through moving props (they are not baked) until obstacles come.
- **Later, in this order:** crowds (DetourCrowd is vendored already: steering and
  avoidance), temporary obstacles (DetourTileCache, for doors and props), off-mesh links,
  area types and costs, agents of several sizes (one navmesh each), the debug draw of the
  mesh and paths in the editor.
- **Not measured yet:** what a game's executable grows by. The shipping profile's LTO
  should drop all of it from a game that never calls `load_navigation`; check at the
  next size measurement.
- **Determinism:** Detour's queries are deterministic for the same mesh and inputs; the
  bake is too (tiles are independent and each is baked in one thread).
