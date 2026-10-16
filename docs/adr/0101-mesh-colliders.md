# 0101. Colliders made from meshes, cooked into Jolt's binary form

- Status: Proposed (2026-10-16; the owner chose a Rust decomposition on 2026-10-16; the rules
  file's shape is the owner's question 1)
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
- **A prop**, a mesh marked dynamic, gets the first that fits:
  1. a box, sphere or capsule whose volume is within 10 % of the hull's;
  2. its hull, when that is within 10 % of the mesh's volume;
  3. convex parts.

  Every hull is at most 64 points.
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
- **The decomposition is a sizable port.** Its fallback (vendoring V-HACD) stays open until
  8c-3 meets its goal.
- **The editor gets colliders as plain data**, ready to edit.
- **Packs trust their contents:** Jolt's restore does not validate its bytes the way a fuzzed
  parser would. A pack is part of the game, as its SPIR-V is.
