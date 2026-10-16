# Navigation: navmeshes, paths, avoidance

Research for queue item 4 (core audit 3 §4: "navigation, missing; CLAUDE.md lists
`engine_nav`"). Checked 2026-10-16.

## What a game needs

1. **A walkable surface** made from the level's geometry for an agent of a height, radius,
   step and slope: a navigation mesh (convex polygons with links between them).
2. **Paths** over it from A to B, as a polyline the agent can follow (corners only, string
   pulled).
3. **Queries:** the nearest point on the mesh, a ray along it (can I walk straight there?).
4. **Steering and avoidance** among many agents (crowds).
5. **Changes:** doors, moving obstacles (tile rebuilds or temporary obstacles), off-mesh links
   (jumps, ladders).

## Sources

| Project | Version (checked 2026-10-16) | License | What it is |
|---|---|---|---|
| [Recast & Detour](https://github.com/recastnavigation/recastnavigation) | release 1.6.0 (2023-05-21); `main` 367 commits on, last 2026-02-27 (`9f4ce64`) | Zlib | C++: Recast builds navmeshes; Detour queries them; DetourCrowd steers and avoids; DetourTileCache adds obstacles |
| [Godot](https://github.com/godotengine/godot) 4.x | `NavigationServer3D` | MIT (Recast Zlib, RVO2 Apache-2.0 inside) | Bakes with Recast; its own server for paths (A* over polygons, a funnel) and avoidance (RVO2) |
| [rerecast](https://github.com/janhohenheim/rerecast) | 0.4.0 (2026-08-05) | Apache-2.0 | A Rust port of Recast (baking only), checked against the C++ |
| [landmass](https://github.com/andriyDev/landmass) | 0.9.3 (2026-10-11) | MIT OR Apache-2.0 | Rust: paths on a navmesh you give it, simplification, steering, avoidance (`dodgy_2d`, an ORCA port); depends on `geo`, `kdtree`, `slotmap`, `disjoint` |
| oxidized_navigation | 0.x | (not checked) | Bevy and Rapier only: tiled navmeshes from colliders; tied to both |
| Unreal, Unity | — | closed | Unreal builds on Recast and Detour; Unity's NavMesh is its own |

## How Recast and Detour work

- **Recast**, per tile:
  1. rasterizes the input triangles into a voxel heightfield (cell size, cell height);
  2. marks walkable spans by slope, then filters ledges, low ceilings and spans the agent
     cannot step;
  3. erodes by the agent's radius;
  4. partitions the open spans into regions (watershed, monotone or layers);
  5. traces their contours, simplifies them, and builds convex polygons (up to 6 vertices);
  6. adds a detail mesh for height.

  Tiles bake independently, so in parallel, and a change rebakes only its tiles.
- **Detour:** a tile is one flat blob (`dtCreateNavMeshData`) that a navmesh adds without
  copying (`addTile`, the blob can be the pack's bytes).
  - A* runs over polygons (`findPath`), and the funnel algorithm makes the corners
    (`findStraightPath`).
  - Also: the nearest polygon, raycasts along the mesh, random points, a polygon's height.
  - Queries cost microseconds.
- **DetourCrowd** steers agents along their corridors and avoids each other with RVO-like
  sampled velocities. **DetourTileCache** carves temporary obstacles into compressed tile
  layers.

## Options

1. **Vendor Recast and Detour (C++, Zlib) behind a C shim, built by `cc`,** as Jolt and
   Box2D are.
   - **For:** the industry's standard and Unreal's base; complete (baking, paths, queries,
     crowds, obstacles, off-mesh links); dependency-free; a small library (about 950 KB of
     source for the four parts); Detour's blob loads without copying.
   - **Against:** a C++ shim to write; one more C++ library to build (about 10 s).
2. **Rust crates: rerecast to bake, landmass for paths and avoidance.**
   - **For:** pure Rust.
   - **Against:**
     - both young (0.x; rerecast's API is still moving);
     - landmass pulls `geo` and its dependencies;
     - no measurement against Detour;
     - no tile cache or crowd of Detour's maturity.
3. **Our own, in Rust:** Recast's pipeline and Detour's queries rewritten.
   - **For:** pure Rust and fully ours.
   - **Against:** weeks of work to reach Recast's robustness on messy levels.

The owner's rule for third-party C++ (2026-10-16: a Rust port when the C++ brings
BSL/MPL/LGPL code or heavy dependencies) does not apply. Recast is Zlib and depends on
nothing.

## What we take

- **Recast and Detour, vendored** (`third_party/recastnavigation` at `9f4ce64`: Recast,
  Detour, DetourCrowd), with a C shim in `engine_nav`.
- **The cook bakes** the scene's level geometry (its static colliders' triangles) into
  tiles, in parallel. The pack keeps Detour's blobs.
- **The runtime** loads the blobs into a `dtNavMesh` and queries it, without allocating per
  query.
- **Later:** crowds (DetourCrowd), temporary obstacles (DetourTileCache), off-mesh links, area
  costs.

As built (2026-10-16): ADR 0103 and `docs/dev-guide/navigation.md`.

No code from GPL projects was read or carried over. Recast and Detour are Zlib, and their
notice goes into `THIRD_PARTY_NOTICES.md`.
