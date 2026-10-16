# Navigation

Your characters find their way around your level on a **navigation mesh**: the surface an
agent of human size can walk on, made from the level's colliders when you cook the scene
(ADR 0103). The engine uses Recast to build it and Detour to find paths on it.

## Making it

`ofeliaengine cook scene.gltf -o scene.pack` makes it. There is nothing to set up:
- **What is walkable** comes from the level geometry: every static collider of the scene
  (see the physics page: by default every mesh is level geometry). Floors, stairs, ramps
  and roofs the agent can step onto become walkable; walls and props block. Moving bodies
  (props that fall and roll) are not part of the level, so agents walk through them for
  now.
- **The agent** is 1.8 m tall with a 0.4 m radius, climbs steps up to 0.4 m and slopes up
  to 45°.
- **It is cheap:** Sponza's is 24 KB and takes 0.03 s of the cook, Intel Sponza's 34 KB
  and 0.2 s.
- The cook prints its size: `a navigation mesh of 6 tiles, 23.6 KiB, baked in 0.03 s`.
- `--no-navmesh` leaves it out. `--no-colliders` leaves it out too: it is made from them.

## Finding paths

```rust
use engine_core::math::Vec3;
use engine_nav::{NavMesh, PathStatus};

player.load_navigation()?;  // after Player::new: the world's NavMesh resource

// In your logic (a reused Vec allocates nothing once it has room):
let extent = Vec3::new(1.0, 2.0, 1.0);  // how far from the mesh a point may be
let nav = world.resource_mut::<NavMesh>().unwrap();
let mut corners = Vec::new();
match nav.path(from, to, extent, &mut corners) {
    PathStatus::Complete => { /* walk from corner to corner */ }
    PathStatus::Partial => { /* `to` is out of reach: this gets as near as it can */ }
    PathStatus::None => { /* `from` or `to` is not near the mesh */ }
}
```

- **`path`** gives the corners to walk between: the start, every turn, the end. At most
  256 corners; a path takes microseconds (17.86 m across Sponza's courtyard in 8 µs).
- **`nearest(point, extent)`**: the walkable point nearest `point`, to place a spawn or
  snap a click to the floor.
- **`raycast(from, to, extent)`**: can the agent walk straight there? `Reached`, or
  `Blocked(t)` with how far along it got (0 to 1), or `OffMesh`.
- **The extent** is how far from the mesh, on each axis, `from` and `to` may lie: the
  agent's position is usually its feet, on the floor; a click on a wall needs more.
- **A game that does not navigate** never calls `load_navigation`, and the mesh is not
  even read.

## What is not there yet

- **Crowds:** agents steering around each other.
- **Doors and moving obstacles:** carving them out while the game runs.
- **Jumps, ladders and teleports** (off-mesh links), areas that cost more to cross (mud,
  water), agents of other sizes.
- **Seeing it:** the mesh and paths drawn in the editor.

They come in that order (ADR 0103).
