# 0101. Colliders made from meshes, cooked into Jolt's binary form

- Status: Proposed (2026-10-16; the owner chose a Rust decomposition on 2026-10-16; the rules
  file's shape is the owner's question 1). Built: 8c-1 to 8c-4; left: the rules file and
  `RayHit`'s surface (8c-2b)
- Date: 2026-10-16
- Related:
  - [ADR 0018](0018-physics-interface-and-jolt.md) and [ADR 0066](0066-physics-queries-characters-rollback.md):
    the physics API and the Jolt backend;
  - [ADR 0028](0028-physics-scope.md): Jolt stays the 3D physics;
  - [ADR 0056](0056-cooked-packs-and-streaming-reads.md): asset packs;
  - [ADR 0092](0092-custom-materials.md): `<scene>.materials.ron`, the precedent for a file
    beside a scene;
  - [ADR 0099](0099-player-crate.md): the player;
  - research note `docs/research/mesh-colliders.md`;
  - queue item 8c.

## Context

- **Nothing makes colliders from a game's meshes.** The physics API (ADR 0066) builds shapes
  from descriptions, so a game writes every collider in code. The cook should make them
  ("one button"):
  - level geometry as triangle meshes;
  - props as a fitted box, sphere or capsule, a convex hull, or convex parts for a concave
    prop;
  - authored colliders taken as they are.
- **The owner's requirements (2026-10-16):**
  - the decomposition in Rust, not CoACD's C++ with its MPL, BSL and LGPL parts
    ("конечно лучше на rust");
  - any scale works after the colliders are made: uniform, non-uniform or mirrored, concave
    props included, with no errors (Godot's fail);
  - groundwork for the editor: grabbing a generated collider's faces, edges or corners and
    moving them.
- **Measured** (the research note, ref-1):
  - **Building a level's mesh shape at load costs about 1 µs per triangle:** 207 ms for
    Sponza, 3.9 s for Intel Sponza, on one thread.
  - **Restoring the same shape from Jolt's binary form is 470-880 times faster:** 0.44 ms and
    5.5 ms. The binary form is also smaller than the triangles (16 bytes per triangle).
  - **Hulls:** 7-356 µs to build, 0.5-3.8 µs to restore.
  - **CoACD fits better than V-HACD with 2-10 times fewer hulls:** the table, 3 hulls
    against 8-16 for a U, 5 against 8-32 for a table, sticking out 0-0.6 % of the size against
    1-2 %. It is 20-90 times slower in its own build: 1.4-288 s per model.

## Options

**Where a collider's shapes come from at runtime.**

1. **The engine's own description in the pack, built by Jolt at load:** backend-neutral, but a
   level pays its build at every load (0.2-3.9 s).
2. **Jolt's binary form in the pack** (chosen): what Unreal and Unity do with their backends.
   Loading is a copy. The form changes with Jolt's version, so the pack carries the version:
   a mismatch is a load error that says to cook again. Packs are cooked by the same engine
   that runs them, as the material types' SPIR-V already is (ADR 0092).
3. **Both, the binary form as a cache built on the player's machine at first load:** a
   first-load hitch, and more code, for no gain over 2.

**The convex decomposition.**

1. **CoACD's C++, vendored:** OpenVDB (MPL-2.0), Boost (BSL-1.0, not on the allowed list),
   CDT (MPL-2.0), an LGPL `sobol.cpp`. Out.
2. **V-HACD 4.1, vendored:** one BSD-3 header, no dependencies, 0.3-3.1 s per model. It
   fills its hull budget, fits worse, and with a small budget fills cavities (a mug with 8
   hulls: 91 % overfill). Kept as the fallback.
3. **Our own Rust implementation of CoACD's algorithm** (chosen by the owner): its quality,
   with the time cut by:
   - decimating the remeshed input before the search;
   - fewer tree search iterations;
   - searching in parallel on the job system;
   - hulls decimated to a vertex budget;
   - results cached in the DDC.

   Its goal: CoACD's hull counts within 10 %, in at most 3 s per sample model on ref-1. If it
   misses, option 2 is vendored.

**Scaling.**

1. **Approximate what the backend cannot take** (Jolt's `MakeScaleValid`, Godot): silent
   errors.
2. **Rebuild only what the backend cannot take, exactly where possible** (chosen; Parry does
   the same for round shapes):
   - a turned box under another scale becomes the hull of its 8 corners, exact;
   - round shapes become hulls of their subdivided surfaces, within a tolerance;
   - a shear is baked into the points;
   - rebuilt shapes are cached per scale.

## Decision

### Data

- **A new crate `engine_collision`**, with no backend:
  - **the collider's own form:** a list of shapes with stable ids and a transform each:
    - box, sphere, capsule, cylinder (parameters);
    - hull (its points; faces follow from them);
    - triangle mesh (vertices, triangles, a surface index per triangle);
  - our own QuickHull;
  - primitive fitting;
  - the decomposition;
  - the scale rules: what a shape becomes under a linear map.

  The editor will edit this form: moving a hull's points, faces and edges, and primitives'
  handles, as commands with undo.
- **Generated colliders are derived data** (cached in the DDC by mesh, rules and generator
  version). **Edited colliders are source data**, kept beside the scene. An edit turns a
  generated collider into an edited one, and a later change to its mesh is reported, not
  overwritten. The edited form's file is decided with the editor.

### Rules (the cook)

- **A scene's mesh instances are level geometry by default:** static bodies with the mesh's
  triangles, exactly as drawn.
  - Materials can be listed as colliding with nothing (foliage cards, decals). There is no
    automatic guess: a masked material may be a grating one walks on.
  - Simplification is optional, within an error, with borders locked (meshoptimizer), for
    dense meshes.
  - Skinned meshes get no collider.
- **A prop**, a mesh marked dynamic, is decomposed first (8c-3 as built: neither its hull nor
  a primitive can tell a concave mesh, since a U's hull is a box):
  1. convex enough to be one part: a box, sphere, capsule or cylinder whose volume is within
     10 % of the hull's, else its hull;
  2. otherwise its convex parts.

  Every hull is at most 64 points. A dynamic glTF physics body's triangle mesh collider
  becomes its convex parts too: a simulated body cannot be a triangle mesh.
- **Authored colliders win over generated ones:**
  - `KHR_physics_rigid_bodies` (motion, colliders, triggers, materials, filters) with
    `KHR_implicit_shapes`;
  - meshes named `UCX_<mesh>`, `UBX_`, `USP_`, `UCP_` (with `_NN`).

  Joints are read but wait for joints in the physics API.
- **The rules live in `<scene>.colliders.ron`** beside the glTF, like `<scene>.materials.ron`
  (question 1). The proposal:

  ```ron
  // sponza.colliders.ron: every field optional
  (
      bodies: Static,               // what the instances are: Static (level) | Dynamic (props)
      static_shape: Mesh,           // Mesh | Simplified(0.01) (within 1 cm) | None
      dynamic_shape: Auto,          // Auto | Box | Sphere | Capsule | Hull | Parts(threshold: 0.05)
      no_collision: ["Leaves"],     // glTF materials whose triangles do not collide
      meshes: {                     // per glTF mesh ("#3": the fourth), the same fields
          "Chair": (bodies: Dynamic, dynamic_shape: Parts(threshold: 0.03)),
      },
  )
  ```

### The pack (version 6)

- **A collider entry per mesh**, shared by every instance of the mesh:
  - **Jolt's binary form** of its shape: one shape, or a static compound of them;
  - the engine's form of what the runtime may rebuild: primitives' parameters and hulls'
    points (small); meshes never need rebuilding for a scale;
  - Jolt's format key (its version and build options).
- The scene's instances name their collider, motion, layer and material.
- A pack without colliders stays version 5.

### The physics API

- **New shapes:** `ShapeDesc::Compound` (children with a transform) and `ShapeDesc::Scaled`.
- **On the backend:**
  - baked shapes: bake one into bytes, create one from bytes, the format key;
  - "is this scale valid for this shape".
- **`RayHit` gains the surface** (the triangle's surface index), so a game knows what a ray hit
  (footsteps, impacts).

### Scaling

- A body takes **Jolt's scaled shape over the cooked shape** when Jolt accepts the scale:
  - meshes and hulls take any;
  - boxes take any along their own axes;
  - hulls are stored without a rotation of their own, so inside a compound they take any.
- **What Jolt cannot take is rebuilt** from the entry's own form:
  - a turned box scaled along other axes becomes the hull of its 8 corners (exact);
  - a non-uniformly scaled sphere, capsule or cylinder becomes a hull of its subdivided
    surface (within 0.5 % of its size);
  - a shear (a transform that T·R·S cannot express) is baked into the points. Static instances
    with shear are baked by the cook; the editor warns about them on meshes.
  - Rebuilt shapes are cached per collider and quantized scale.
- **A zero scale disables the collider**, as glTF's draft says.
- **The decomposition is never redone for a scale:** a linear map keeps convex parts convex and
  their union the scaled prop.

### The runtime (the player)

- `PlayerSettings::physics` gives the player a physics world, a resource of the ECS `World`:
  - stepped after the game's fixed tick;
  - synced with `PhysicsSync` (ADR 0066);
  - bodies made from the pack's colliders when the scene loads: shapes restored on the job
    pool, bodies added in one batch, and the broad phase optimized once.
- The game reaches it from its tick, like the rest of the world.

## Steps

1. **8c-1:** `engine_collision`, its data, QuickHull, primitive fitting, the scale rules;
   tests and benchmarks.
2. **8c-2:**
   - the physics API additions and Jolt's baked shapes (`ext.cpp`: save and restore with
     children, the stream fix);
   - the cook's rules, the authored colliders, pack version 6;
   - scaled bodies by the rules above.
3. **8c-3:** the Rust decomposition, measured against CoACD 1.0.14 and V-HACD 4.1 on the
   research note's set.
4. **8c-4:**
   - bodies from the pack in the player;
   - a test scene: props dropped on Sponza, scaled every way;
   - the dev guide;
   - the gate.

## Consequences

- **Level loads restore collision at memory speed.** Intel Sponza's 3.9 s of building
  becomes 5.5 ms.
- **A Jolt update means cooking packs again** (the key says so at load), as an engine change to
  the material types already does. Another backend, should one come (ADR 0028), cooks its own
  form from the same source data.
- **Scale is never an error.** Rebuilt shapes cost memory per distinct scale, and an animated
  non-uniform scale on a round shape rebuilds through the cache.
- **The decomposition is a sizable port** (about 3,000 lines with its tests). It met its goal but for the mug's
  hull count (8c-3 as built), so V-HACD is not vendored.
- **The editor gets colliders as plain data**, ready to edit.
- **Packs trust their contents:** Jolt's restore does not validate its bytes the way a fuzzed
  parser would. A pack is part of the game, as its SPIR-V is.

## 8c-1, 8c-2a and 8c-4a as built (2026-10-16)

### What was built

- **`engine_collision`:**
  - **QuickHull in f64.** Its tolerance comes from the coordinates' magnitude (qhull's rule,
    with f32's epsilon). The next point is the farthest of all faces' outside sets (a heap),
    so a vertex limit stops at the points that matter most. `Hull::error` says how far the
    left-out points lie outside; it is computed exactly over every face. A point whose visible
    faces do not border one simple loop (rounding) is dropped.
  - **Fitting.** Boxes: the world axes, the hull surface's principal axes, and each face's
    normal turned to the smallest rectangle (directions from a hull of at most 64 vertices,
    extents from every point). Spheres: Welzl. Capsules: the radius searched for the smallest
    volume. Cylinders: around the smallest circle seen down each box axis.
  - `Shape::transformed` (any affine map) and `Shape::hull_points`.
  - Mesh helpers: volume, closed, welding.
- **The physics API:**
  - `ShapeDesc::Compound` and `ShapeDesc::Scaled`;
  - `is_valid_scale`;
  - baked shapes: `baked_shape_format`, `bake_shape`, `create_baked_shape`;
  - `optimize_broad_phase`;
  - `engine_physics::colliders`: `build`, `bake`, `placed`.

  Jolt bakes through `ext.cpp`: `SaveWithChildren` and `sRestoreWithChildren`. The format
  key is Jolt's version plus double precision, with a revision byte of our own.
- **The cook (`engine_assets::colliders`, `gltf_physics`):**
  - the order of the decision above: KHR physics, then authored names, then the rules;
  - colliders per glTF mesh (all primitives in one, a surface per triangle), not per cooked
    primitive;
  - shared per mesh;
  - a sheared transform gets its own collider.
- **Pack version 6:**
  - `Collider` entries: the engine's form; a mesh's triangles only when unbaked; the baked
    bytes after a 16-byte boundary.
  - One `Bodies` entry: 128-byte records and the instances they move.
  - `open_scene` skips both; `load_colliders` reads them, and `load_pack` too.
- **`ofeliaengine cook`** bakes in parallel, a Jolt world per collider. `--no-colliders`
  leaves colliders out.
- **The player:**
  - `ScenePhysics` is owned by the game. `JoltBackend` is neither `Send` nor `Sync`, so it
    cannot be a resource of the ECS world; a `Mutex` would put a lock on the tick. The stress
    tool already owned its world this way.
  - `Player::load_physics` loads the bodies; `SceneInfo::instances` lists the instances'
    entities.
  - Moving bodies are entities with their instances as children.

### Measured (ref-1, release)

| | Sponza | Intel Sponza |
|---|---:|---:|
| colliders / bodies | 1 / 1 | 115 / 115 |
| making them in the cook | 0.14 s | 1.58 s |
| baking (parallel) | 0.19 s | 0.24 s (2.24 s on one thread) |
| pack | +2.4 MB of 73.0 MB | +37 MB of 1336 MB |
| loading the bodies (restore, place, add) | 5.5-10 ms | — |

`engine_collision`'s own benchmark (`cargo bench -p engine_collision --bench hull`):

| | median |
|---|---:|
| hull of 1,000 points | 82 µs |
| hull of 10,000 points | 0.62 ms |
| hull of 100,000 points | 5.8 ms |
| hull of 10,000 points on a sphere (all of them vertices) | 6.1 ms |
| a box, sphere, capsule and cylinder fitted around a 64-vertex hull | 0.27-0.31 ms |

### Tested

- **Hulls:**
  - closed, convex, Euler's 2;
  - every input point inside;
  - grids at 10 km;
  - duplicates and degenerate inputs;
  - the limit's exact error.
- **Fitting:** turned boxes, spheres, capsules and cylinders recovered; a tetrahedron keeps
  its hull; flat sets get a thin box.
- **Scale rules:** boxes along and across their axes, ellipsoids within the tolerance,
  capsules, cylinders, mirrors (a mesh keeps a positive volume), convex parts under a shear.
- **Jolt:**
  - compounds keep their children;
  - refused scales are errors, not averages;
  - baked shapes restore the same bytes;
  - damaged bytes are refused.
- **Placement:**
  - a scale Jolt takes is not rebuilt;
  - a squashed sphere and a sheared box hit where they should;
  - a zero scale is off;
  - a mirrored mesh's faces point out.
- **The cook:** shared level meshes with surfaces; excluded materials; props fitted; `UCX_`
  and `UBX_` in the named node's space; an orphan `USP_`; a shear; KHR bodies, materials,
  planes and triggers.
- **The pack:** a round trip of colliders and bodies (version 6), and `open_scene` not
  reading them.
- **Sponza end to end** (`engine_app/tests/colliders.rs`): cooked, baked, packed, loaded and
  scaled by its node. Five rays hit where a brute-force test of the source triangles says,
  within 0.2 % of the size.

### Not done yet

- **The rules file** `<scene>.colliders.ron`: the owner's question 1. Today the rules come
  through `ImportedScene::collider_rules`; the defaults follow the recommendation of
  question 2.
- **`RayHit`'s surface:** the triangles keep it in the engine's form, but Jolt's materials
  are not set from it yet.
- **The cook's collider making runs on one thread** (1.6 s for Intel Sponza's 115 meshes);
  it could run per mesh. The decompositions run in parallel (8c-3).
- **Simplifying level meshes** (`StaticShape::Simplified`) waits for the rules file.

## 8c-3 as built (2026-10-16)

### What was built

- **`engine_collision::decompose`**, CoACD's algorithm in Rust (MIT, credited in
  `THIRD_PARTY_NOTICES.md`), with none of its dependencies:
  - `remesh`: a closed manifold by marching tetrahedra over a distance band (CoACD's 100 cells,
    the surface 0.55 of a cell out), the grid's origin off the mesh's own grid so that
    axis-aligned faces never put grid points on the surface; the band per slab of the grid in
    parallel;
  - `decimate`: the search mesh simplified to about 4,000 triangles by quadric edge collapses
    that keep it closed (the link condition, no flips), within half a cell; a collapse costs
    its mean squared distance plus a little for its length, so flat faces coarsen evenly;
  - `clip`: a closed part cut by a plane, both caps by ear clipping with holes bridged;
  - `cost`: the concavity (the volume term; the boundary Hausdorff distance from R2 samples
    and a triangle BVH);
  - `search`: the tree search (150 iterations, depth 3, 20 planes per axis) and the
    refinement of the first cut. What runs in parallel changes nothing in the result: the
    parts of a round, the root's moves (every iteration expands the next one until all are
    tried), the rollout's three candidate cuts, the refinement's two probes;
  - the merge: pair costs in parallel, kept, and only the merged part's recomputed (as
    CoACD's cost matrix). A merged part keeps its pieces, so the faces where they touch stay
    out of its Hausdorff term (two halves of a box merge back with no concavity);
  - hulls of at most 64 points, the farthest first.
- **The cook:**
  - `DynamicShape::Auto` decomposes, then a mesh of one part gets a primitive or its hull as
    before; `DynamicShape::Parts` asks for parts outright; `ColliderRules::parts` holds the
    settings (CoACD's threshold 0.05, no part limit by default);
  - a dynamic `KHR_physics_rigid_bodies` body's mesh collider (`convexHull` false) becomes its
    parts (or its hull, convex enough); static ones keep their triangles;
  - every mesh to decompose is collected first and done in parallel on the cook's pool;
  - parts go into the DDC under a key of the mesh's welded triangles, the settings and
    `decompose::REVISION`: a second cook loads them (`cook` prints how many);
  - a scene cooked with the default rules (level geometry) decomposes nothing and costs
    nothing more.

### Measured (ref-1, release, 16 threads)

The research note's set, judged exactly (`docs/research/mesh-colliders.md`, "Ours"):

| model | ours: hulls, time, sticks out | CoACD 1.0.14: hulls, time, sticks out |
|---|---|---|
| mug with a handle | 24, 0.5 s, 1.61 % | 21, 20.1 s, 1.19 % |
| torus | 12, 0.5 s, 1.29 % | 11, 14.0 s, 1.59 % |
| Duck | 11, 0.5 s, 1.40 % | 10, 133.8 s, 1.36 % |
| BoomBox | 25, 2.1 s, 0.97 % | 26, 195.1 s, 0.73 % |
| SheenChair | 25, 1.3 s, 1.12 % | 29, 95.3 s, 0.82 % |
| ToyCar | 118, 3.0 s, 1.69 % | 110, 142.6 s, 1.19 % |
| DragonAttenuation | 42, 1.9 s, 1.16 % | 51, 103.2 s, 1.18 % |

- **The goal** (CoACD's hull counts within 10 %, at most 3 s per model) is met but for the
  mug's count: 24 against 21 (+14 %). All 14 models take 0.02-3.0 s.
- **The same hulls on every run, bit for bit,** with or without the pool.
- **What it took to get there:**
  - the stairs' decimation, 6.6 s to 0.3 s: on flat faces every collapse cost nothing and
    vertices took in fans of neighbours; shorter edges first where errors tie, at most 32
    neighbours;
  - ToyCar's merge, 42 s to 0.3 s: costs kept instead of recomputed for every pair after
    every merge;
  - the first rounds of cuts left most threads idle; the parallel search and remesh took
    ToyCar from 4.9 s to 3.0 s and BoomBox from 4.2 s to 2.1 s.

### Tested

- `engine_collision`: the box stays whole, an L splits in two, a U in three (in parallel), with
  the volumes kept; clipping, caps with holes, the BVH against brute force, R2 samples by
  area, the remesh closed (a box and an open box), the decimation closed, within its bound,
  without fans; two halves of a box have no concavity together, apart they have.
- `engine_assets`: a U prop gets three hulls of the U's volume and a crate still its box;
  `Parts` gives the crate its hull, `Hull` the U its full hull; the second cook takes the
  parts from the cache, other settings do not, and a torn entry is not trusted; a dynamic
  glTF physics body's mesh collider becomes three hulls while a static one keeps its
  triangles.

### Not done yet

- **A part limit by default.** The threshold alone decides; a busy mesh (ToyCar with its
  cloth) takes 118 hulls. `max_hulls` exists; a default for props comes with the rules file.
- **Hulls bounded by an error** instead of a count: smooth parts leave out up to 6 % at 64
  points (the avocado and the bottle stick out 0.8-1.1 % more than CoACD's unlimited hulls).
- **The mug's extra hulls** (24 against 21).
- **Editing parts** in the editor (the data is ready: points per hull, stable ids).

## 8c-4 as built (2026-10-16)

### What was built

- **The owner's requirement tested** (`engine_app/tests/colliders.rs`,
  `concave_props_scaled_every_way_land_on_sponza`): Sponza's level collider and nine concave
  props in one `ScenePhysics`: U-shaped props at scale 1, 0.5, 2, (2, 0.5, 1), mirrored
  (−1, 1, 1) and turned under a parent stretched along x (sheared, baked by the cook), an L
  made taller, a crate stretched, a turned crate under a stretched parent (the exact hull of
  its corners). The cook splits the U into three parts and the L into two; each prop lands on
  a spot of bare floor the test finds by rays (the courtyard has planters, vases and arcades
  under roofs), and comes to rest standing, on the floor, asleep. Then a ball dropped into the
  U's notch falls to the floor inside it, where a filled hull would hold it a unit up.
- **A rendered demo** (`engine_app/examples/props_drop.rs`): a cooked Sponza pack and 21
  props of the game's own (U-shaped benches, L-shaped walls, crates, tables and chairs of
  overlapping boards), each turned and scaled its own way (uniform, along its axes,
  mirrored), dropped in three layers into the courtyard on the offscreen player; frames
  written as PNGs. Their meshes are added to the player, their colliders made by
  `cook_colliders`, their bodies placed by `ScenePhysics` and moving their entities.
- **A fix the demo found:** a mesh in several closed pieces (a table of boards, overlapping
  where they join) passed as a closed manifold, every edge once each way, and was cut as a
  solid running through itself: five props still being cut after a minute on 16 threads,
  when the run was stopped.
  A closed mesh now also has to be in one piece to be used as it is; otherwise it is remeshed
  into one surface around all its pieces (a test: two boxes through each other). The 14
  models of the research note decompose to the same hulls, bit for bit.

### Measured (ref-1, release, RTX 4060 Ti)

- The demo's five prop meshes decomposed in 0.53 s (the U in 3 parts, the L in 2, a table in
  5, a chair in 6, the crate a box); Sponza's level collider restored in 6.6 ms; 150 frames of
  1280 × 720 rendered and written in 10-14 s.
- The test: 0.43 s, Sponza's colliders made, nine props cooked and dropped, the ball.

### Not done yet

- **A sheared prop's drawn mesh:** under a non-uniformly scaled parent with a turned child,
  the cook bakes the shear into the collider, but a moving body carries its instance as an
  ECS child whose `Transform` (translation, rotation, scale) cannot hold a shear: drawn
  without it. The collider is right; the picture is not. It needs a local transform with a
  full linear part, an ECS change for its own ADR.
- **The rules file and `RayHit`'s surface** (8c-2b): the owner's question 1.
