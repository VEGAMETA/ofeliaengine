# Colliders made from meshes

Checked 2026-10-16. Sources at the end.

- **Code read:**
  - Godot 4's scene importer and glTF physics extension (MIT);
  - Jolt Physics 5.6.0's shapes, their scaling rules and binary state (MIT);
  - the V-HACD 4.1.0 header (BSD-3-Clause), built and run;
  - CoACD's sources (MIT), its 1.0.14 wheel run;
  - O3DE's primitive shape fitter (Apache-2.0 OR MIT);
  - Parry's shape scaling (Apache-2.0).
- **Known from their documentation:** Unreal Engine 5's collision and its `UCX_` naming, Unity's
  mesh colliders and their baking, the Khronos drafts `KHR_physics_rigid_bodies` and
  `KHR_implicit_shapes`, and OMI's `OMI_physics_body` and `OMI_physics_shape`.
- **No code was copied.** What a later step ports from CoACD (MIT) is credited in
  `THIRD_PARTY_NOTICES.md` with it. CoACD's `sobol.cpp` is LGPL; it is not used and not
  ported (only its header was looked at, for the license).

## Why

Queue item 8c. The physics API (ADR 0066) builds shapes from descriptions, but nothing makes
those descriptions from a game's meshes, so today a game writes every collider in code.
The cook should make them, as it makes detail levels and meshlets ("one button").

What a game needs:

- **Level geometry** (walls, floors, stairs) collides exactly where it is drawn: triangle meshes.
- **Props that move** need convex shapes: a box, sphere or capsule where one fits, a convex
  hull, or several hulls for a concave prop. Jolt (and every engine read) collides moving
  bodies through convex shapes; a dynamic triangle mesh collides with nothing else dynamic.
- **Authored colliders**, where an artist knows better, are taken as they are.
- **Any scale** (the owner, 2026-10-16): a scene or a model with its colliders can be scaled up
  or down, uniformly or not, after the colliders are made, concave ones included, with no
  errors. In Godot such scaling gives warnings and broken collisions.
- **Editable later** (the owner, 2026-10-16): in the editor, one should be able to grab a
  generated collider's faces, edges or corners and move them where the generator got it
  wrong. The editor comes later; the data should allow it now.

## How other engines do it

### Unreal Engine 5 (documentation)

- Each static mesh has **simple** collision and **complex** collision:
  - simple: boxes, spheres, capsules and convex hulls ("aggregate geometry");
  - complex: the render mesh's triangles, or another mesh or detail level.
- "Use complex as simple" lets static meshes collide through their triangles.
- **Authored** colliders come from the source file, by name: `UBX_<mesh>` (box), `USP_` (sphere),
  `UCP_` (capsule) and `UCX_` (convex hull), with `_NN` for several.
- **Generated** colliders come from the mesh editor:
  - fitted boxes, spheres and capsules;
  - k-DOPs (10, 18 and 26 sides);
  - "Auto Convex Collision" (V-HACD), with a hull count, a vertex limit per hull and a precision.
- **Cooking:** the physics data is cooked into the backend's binary format, cached in the DDC,
  and loaded as is at runtime.

### Unity (documentation)

- Box, sphere and capsule colliders, and the `MeshCollider`:
  - a triangle mesh for static geometry;
  - with "Convex" set, a hull of at most 255 triangles, which dynamic bodies need.
- No decomposition is built in. Asset store tools do it, most with V-HACD.
- **Cooking:** PhysX cooks mesh colliders. "Prebake Collision Meshes" bakes them into the
  build; otherwise they are cooked at load. `Physics.BakeMesh` bakes off the main thread.
- **Scale:** a mesh collider bakes a non-uniform scale into its mesh. A sphere collider stays a
  sphere, sized by one axis.

### Godot 4 (code, `master` 4d10419, 2026-10-15)

- **Import options per mesh:**
  - a body type: static, rigid or area;
  - a shape: decompose convex (V-HACD), single convex, trimesh, box, sphere, cylinder, capsule
    or automatic;
  - **automatic** gives a trimesh for static bodies and a convex decomposition for dynamic ones
    (`ResourceImporterScene::get_collision_shapes`).
- **Name suffixes:**
  - `-col` and `-convcol` add a collider beside the mesh;
  - `-colonly` and `-convcolonly` make the mesh only a collider;
  - `-rigid` makes the node a rigid body.
- **glTF:** imports `OMI_physics_body` and `OMI_physics_shape`, and the Khronos drafts
  (`modules/gltf/extensions/physics`).
- **Shapes are resources:**
  - `ConcavePolygonShape3D` holds a face list;
  - `ConvexPolygonShape3D` holds points;
  - the physics server builds its own structures from them at load.
- **Scale:**
  - the editor warns that a non-uniformly scaled `CollisionShape3D` "will probably not function
    as expected";
  - a backend that cannot take a scale approximates it.

### O3DE (code)

- **PhysX mesh assets** are cooked as triangle meshes, convex hulls or primitives, and can be
  decomposed with V-HACD. The cook writes PhysX's binary.
- **Primitive fitting** (`Gems/PhysX/Core/Code/Source/Pipeline/PrimitiveShapeFitter`):
  1. a first guess from the eigenvectors of the vertices' covariance;
  2. then BFGS minimizes the vertices' mean squared distance to the primitive's surface plus a
     volume term;
  3. the best of sphere, box and capsule wins.

### Bevy with Rapier and Parry (code, Parry `master` b3c3584, 2026-10-06)

- Parry has triangle meshes with a BVH, convex polyhedra, compounds, and a convex decomposition
  of its own: a Rust port of the older voxel V-HACD.
- **Scaling** (`Shape::scale`, `Ball::scaled`):
  - a ball under a non-uniform scale becomes a convex polyhedron (a subdivided sphere, then
    scaled);
  - capsules and cylinders likewise;
  - meshes and polyhedra scale their points exactly.
- Parry shapes serialize with serde, BVH included.

### Jolt 5.6.0 (code)

- **Shapes:**
  - sphere, box, capsule, tapered capsule, cylinder, tapered cylinder;
  - convex hull (at most 256 points; the builder drops the rest);
  - mesh (triangles with per-triangle materials, in a BVH of compressed triangle blocks);
  - height field and plane;
  - static and mutable compounds;
  - rotated-translated, scaled, and offset centre of mass.
- No decomposition and no fitting.
- **Valid scales** (`IsValidScale`):

  | shape | scales it takes |
  |---|---|
  | sphere, capsule | uniform (any sign) |
  | cylinder | uniform in X and Z |
  | box, convex hull, mesh | any non-zero scale, negative included (`IsInsideOut` flips the windings) |
  | a compound's child, a rotated child | only a scale that does not shear it: uniform, or a rotation that maps axes onto axes (`CanScaleBeRotated`) |

  `MakeScaleValid` replaces an invalid scale by an average. This is the silent approximation
  engines end up with.
- **Convex radius under scale:** `ScaleConvexRadius` gives `min(r × min|s|, 0.05 m)`. Scaling a
  hull up never rounds its edges by more than Jolt's default 5 cm.
- **Binary form:** `SaveWithChildren` and `sRestoreWithChildren` save a shape and restore it.
  Jolt warns that the form is not stable across versions: after an update, the shape is built
  again from its settings and saved again.

## Authored colliders in glTF

### `KHR_implicit_shapes` (draft, KhronosGroup/glTF#2370, updated 2026-10-03)

- **Shapes**, at document level:
  - a plane, infinite or sized, single- or double-sided;
  - a sphere and a box;
  - a cylinder with a radius per end (a cone when one is 0);
  - a capsule with a radius per end (tapered).
- Degenerate shapes are not allowed.

### `KHR_physics_rigid_bodies` (draft, KhronosGroup/glTF#2424, updated 2026-10-03)

- **On a node:**
  - `motion`: kinematic or not, mass, centre of mass, inertia, initial velocities, gravity factor;
  - `collider`: `geometry`, either an implicit shape or a mesh node, with a `convexHull` flag;
    plus a physics material and a collision filter;
  - `trigger`: a geometry, or a set of nodes;
  - `joint`: limits and drives.
- **At document level:** physics materials (static and dynamic friction, restitution, how they
  combine), collision filters (named systems) and joints.
- **Scale:**
  - a zero scale disables a shape;
  - a negative scale applies its absolute value to an implicit shape;
  - the draft does not forbid non-uniform scales.
- **Tools:** the draft's authors' Blender exporter writes them; Babylon.js and Godot read them.

### `OMI_physics_body` and `OMI_physics_shape` (KhronosGroup/glTF#2258)

The Open Metaverse Interoperability group's earlier pair, with the same ideas. Godot reads both.
The Khronos drafts are where the work goes on now.

### Names

Unreal's prefixes (`UCX_`, `UBX_`, `USP_`, `UCP_`) are what DCC exporters and store assets
carry. Godot's suffixes are Godot's own.

## Convex decomposition

### V-HACD 4.1.0 (BSD-3-Clause; header built and run)

1. Voxelize the mesh (about 400,000 voxels by default) and fill its inside.
2. Split the voxels recursively along axis planes where a part's hull differs most from it,
   until the volume error is below a percentage or a depth is reached.
3. Merge hulls, the smallest growth in volume first, down to `maxConvexHulls`.
4. Shrink-wrap the hull vertices onto the source mesh, and limit vertices per hull (64 by
   default).

One header, its own threads, no dependencies.

### CoACD (MIT; `main` 131010c, 2026-09-23; the 1.0.14 wheel run)

Wei et al., "Approximate Convex Decomposition for 3D Meshes with Collision-Aware Concavity and
Tree Search", SIGGRAPH 2022.

1. **Preprocess:** a mesh that is not a manifold is remeshed into one (an OpenVDB level set,
   resolution 50).
2. **Concavity "aware of collisions":** the larger of two terms, both on the mesh scaled into a
   unit box:
   - the Hausdorff distance between the part's surface and its hull's (sampled points, a k-d
     tree);
   - a volume term.
3. **Search:** cutting planes along the three axes. A Monte Carlo tree search looks at sequences
   of cuts (150 iterations, depth 3 by default), then a local search refines the best plane. A
   cut clips the part and triangulates the caps (constrained Delaunay, the CDT library).
4. **Merge** adjacent hulls while the merged hull's concavity stays under the threshold.
5. **Optional:** decimate hulls to a vertex limit, and extrude them.

**Dependencies:**
- OpenVDB (MPL-2.0), Boost (BSL-1.0), CDT (MPL-2.0), spdlog, zlib;
- nanoflann (BSD-2), quickhull (public domain), Bullet's `btConvexHullComputer` (Zlib);
- `src/sobol.cpp`, which is LGPL.

### Measured (ref-1, 2026-10-16)

- **Inputs:**
  - five procedural shapes (a U, a table, stairs, a mug with a handle, a torus);
  - glTF sample models (Khronos glTF-Sample-Assets), each merged into one mesh.
- **Settings:**
  - V-HACD with 8 and 16 hulls (1 % error) and 32 hulls (5 %);
  - CoACD at its default threshold 0.05, alone and with hulls decimated to 64 vertices.
- **Times:** V-HACD on all threads; CoACD's wheel mostly on one (1-2 cores busy, sampled).
- **Judged exactly, without a grid:**
  - the model as a solid: itself when watertight; otherwise its voxels at 1/128 of its size,
    filled and remeshed by marching cubes;
  - **overfill** and **underfill**: the volume the hulls' union adds to the solid, and the
    volume of the solid it leaves out, both as a share of the solid's volume (manifold3d
    booleans);
  - **sticks out**: how far the collider reaches beyond the model's surface, in % of the
    bounds' diagonal. This is what a player sees: an object resting on air.

| model | method | hulls | hull vertices | time | overfill | underfill | sticks out |
|---|---|---:|---:|---:|---:|---:|---:|
| U (procedural) | V-HACD 8 hulls, 1 % | 8 | 68 | 0.9 s | 0.3% | 0.0% | 1.59% |
| U (procedural) | V-HACD 16 hulls, 1 % | 16 | 130 | 0.9 s | 0.0% | 0.0% | 0.40% |
| U (procedural) | V-HACD 32 hulls, 5 % | 8 | 69 | 0.8 s | 0.6% | 0.0% | 2.05% |
| U (procedural) | CoACD 0.05 | 3 | 28 | 1.4 s | 0.1% | 0.0% | 0.05% |
| U (procedural) | CoACD 0.05, hulls of ≤ 64 vertices | 3 | 28 | 1.4 s | 0.1% | 0.0% | 0.05% |
| table | V-HACD 8 hulls, 1 % | 8 | 76 | 0.6 s | 9.4% | 0.0% | 1.54% |
| table | V-HACD 16 hulls, 1 % | 16 | 144 | 0.6 s | 1.3% | 0.0% | 1.28% |
| table | V-HACD 32 hulls, 5 % | 32 | 285 | 0.6 s | 0.6% | 0.0% | 1.04% |
| table | CoACD 0.05 | 5 | 44 | 2.7 s | 0.1% | 0.0% | 0.00% |
| table | CoACD 0.05, hulls of ≤ 64 vertices | 5 | 44 | 2.7 s | 0.1% | 0.0% | 0.00% |
| stairs * | V-HACD 8 hulls, 1 % | 8 | 72 | 1.3 s | 0.7% | 4.0% | 2.04% |
| stairs * | V-HACD 16 hulls, 1 % | 16 | 134 | 1.3 s | 0.3% | 4.2% | 1.66% |
| stairs * | V-HACD 32 hulls, 5 % | 32 | 278 | 1.2 s | 0.6% | 4.1% | 2.04% |
| stairs * | CoACD 0.05 | 5 | 344 | 52.1 s | 2.5% | 0.4% | 0.58% |
| stairs * | CoACD 0.05, hulls of ≤ 64 vertices | 5 | 299 | 51.9 s | 2.4% | 0.4% | 0.58% |
| mug with a handle | V-HACD 8 hulls, 1 % | 8 | 286 | 2.7 s | 91.0% | 3.5% | 6.63% |
| mug with a handle | V-HACD 16 hulls, 1 % | 16 | 428 | 2.7 s | 35.9% | 3.6% | 3.32% |
| mug with a handle | V-HACD 32 hulls, 5 % | 32 | 611 | 2.5 s | 16.0% | 3.4% | 3.17% |
| mug with a handle | CoACD 0.05 | 21 | 766 | 20.1 s | 15.6% | 0.0% | 1.19% |
| mug with a handle | CoACD 0.05, hulls of ≤ 64 vertices | 21 | 646 | 19.9 s | 14.8% | 4.8% | 1.19% |
| torus | V-HACD 8 hulls, 1 % | 8 | 445 | 1.5 s | 16.3% | 4.8% | 6.12% |
| torus | V-HACD 16 hulls, 1 % | 16 | 804 | 1.4 s | 5.5% | 3.5% | 3.19% |
| torus | V-HACD 32 hulls, 5 % | 32 | 1265 | 1.2 s | 2.0% | 3.1% | 1.09% |
| torus | CoACD 0.05 | 11 | 2067 | 14.0 s | 5.5% | 0.0% | 1.59% |
| torus | CoACD 0.05, hulls of ≤ 64 vertices | 11 | 704 | 14.1 s | 4.2% | 3.7% | 1.54% |
| Duck | V-HACD 8 hulls, 1 % | 8 | 460 | 2.5 s | 3.5% | 4.1% | 4.37% |
| Duck | V-HACD 16 hulls, 1 % | 16 | 871 | 2.5 s | 3.3% | 3.0% | 4.35% |
| Duck | V-HACD 32 hulls, 5 % | 32 | 1394 | 1.8 s | 1.5% | 3.3% | 1.69% |
| Duck | CoACD 0.05 | 10 | 16499 | 133.8 s | 6.3% | 0.0% | 1.36% |
| Duck | CoACD 0.05, hulls of ≤ 64 vertices | 10 | 640 | 176.3 s | 1.8% | 5.4% | 1.62% |
| Avocado * | V-HACD 8 hulls, 1 % | 8 | 414 | 1.3 s | 2.0% | 6.8% | 3.38% |
| Avocado * | V-HACD 16 hulls, 1 % | 16 | 613 | 1.3 s | 1.0% | 6.7% | 2.68% |
| Avocado * | V-HACD 32 hulls, 5 % | 32 | 1018 | 1.0 s | 0.6% | 6.6% | 2.12% |
| Avocado * | CoACD 0.05 | 3 | 4877 | 48.3 s | 1.3% | 0.4% | 0.68% |
| Avocado * | CoACD 0.05, hulls of ≤ 64 vertices | 3 | 192 | 55.6 s | 0.2% | 11.4% | 0.91% |
| WaterBottle * | V-HACD 8 hulls, 1 % | 8 | 380 | 1.3 s | 2.7% | 5.5% | 3.09% |
| WaterBottle * | V-HACD 16 hulls, 1 % | 16 | 562 | 1.3 s | 2.7% | 5.3% | 3.12% |
| WaterBottle * | V-HACD 32 hulls, 5 % | 32 | 779 | 1.0 s | 0.6% | 8.0% | 2.03% |
| WaterBottle * | CoACD 0.05 | 4 | 5637 | 37.8 s | 3.2% | 0.5% | 0.72% |
| WaterBottle * | CoACD 0.05, hulls of ≤ 64 vertices | 4 | 256 | 43.7 s | 1.2% | 5.0% | 1.77% |
| BoomBox * | V-HACD 8 hulls, 1 % | 8 | 464 | 3.1 s | 4.9% | 6.8% | 2.87% |
| BoomBox * | V-HACD 16 hulls, 1 % | 16 | 948 | 3.1 s | 2.9% | 4.9% | 3.96% |
| BoomBox * | V-HACD 32 hulls, 5 % | 32 | 1030 | 2.3 s | 1.0% | 7.0% | 2.40% |
| BoomBox * | CoACD 0.05 | 26 | 18926 | 195.1 s | 1.2% | 0.3% | 0.73% |
| BoomBox * | CoACD 0.05, hulls of ≤ 64 vertices | 26 | 1631 | 288.3 s | 0.6% | 13.5% | 0.97% |
| GlamVelvetSofa * | V-HACD 8 hulls, 1 % | 8 | 279 | 0.8 s | 31.6% | 7.6% | 8.68% |
| GlamVelvetSofa * | V-HACD 16 hulls, 1 % | 16 | 528 | 0.8 s | 9.4% | 8.1% | 5.92% |
| GlamVelvetSofa * | V-HACD 32 hulls, 5 % | 32 | 771 | 0.6 s | 3.4% | 10.2% | 3.12% |
| GlamVelvetSofa * | CoACD 0.05 | 24 | 5217 | 50.9 s | 7.4% | 0.8% | 0.83% |
| GlamVelvetSofa * | CoACD 0.05, hulls of ≤ 64 vertices | 24 | 1202 | 51.9 s | 6.4% | 4.1% | 0.82% |
| Lantern * | V-HACD 8 hulls, 1 % | 8 | 94 | 0.3 s | 7.0% | 8.7% | 1.90% |
| Lantern * | V-HACD 16 hulls, 1 % | 16 | 176 | 0.3 s | 3.5% | 12.5% | 1.89% |
| Lantern * | V-HACD 32 hulls, 5 % | 32 | 320 | 0.3 s | 1.5% | 16.3% | 1.38% |
| Lantern * | CoACD 0.05 | 8 | 832 | 22.8 s | 25.6% | 1.2% | 2.10% |
| Lantern * | CoACD 0.05, hulls of ≤ 64 vertices | 8 | 505 | 22.8 s | 24.0% | 1.7% | 2.14% |
| SheenChair * | V-HACD 8 hulls, 1 % | 8 | 392 | 1.9 s | 21.3% | 6.3% | 1.66% |
| SheenChair * | V-HACD 16 hulls, 1 % | 16 | 778 | 1.8 s | 13.4% | 7.1% | 1.67% |
| SheenChair * | V-HACD 32 hulls, 5 % | 32 | 1169 | 1.6 s | 5.1% | 9.0% | 3.64% |
| SheenChair * | CoACD 0.05 | 29 | 14213 | 95.3 s | 6.8% | 0.7% | 0.82% |
| SheenChair * | CoACD 0.05, hulls of ≤ 64 vertices | 29 | 1821 | 104.1 s | 5.5% | 5.0% | 0.82% |
| ToyCar * | V-HACD 8 hulls, 1 % | 8 | 283 | 1.5 s | 430.3% | 10.1% | 1.42% |
| ToyCar * | V-HACD 16 hulls, 1 % | 16 | 497 | 1.3 s | 289.5% | 16.6% | 3.38% |
| ToyCar * | V-HACD 32 hulls, 5 % | 32 | 906 | 1.2 s | 181.9% | 24.4% | 2.83% |
| ToyCar * | CoACD 0.05 | 110 | 10091 | 142.6 s | 117.1% | 2.9% | 1.19% |
| ToyCar * | CoACD 0.05, hulls of ≤ 64 vertices | 110 | 6711 | 144.4 s | 113.7% | 3.9% | 1.37% |
| DragonAttenuation * | V-HACD 8 hulls, 1 % | 8 | 326 | 1.3 s | 83.8% | 24.2% | 1.73% |
| DragonAttenuation * | V-HACD 16 hulls, 1 % | 16 | 480 | 1.3 s | 67.2% | 29.5% | 1.75% |
| DragonAttenuation * | V-HACD 32 hulls, 5 % | 32 | 834 | 1.3 s | 45.8% | 34.9% | 1.75% |
| DragonAttenuation * | CoACD 0.05 | 51 | 6034 | 103.2 s | 66.7% | 1.3% | 1.18% |
| DragonAttenuation * | CoACD 0.05, hulls of ≤ 64 vertices | 51 | 3144 | 109.7 s | 60.8% | 2.9% | 0.97% |

`*`: not watertight, judged against its voxel solid (the ToyCar's and the dragon's are
thin shells and open in places, so every collider overfills them; compare the methods
with each other there).

- **At the same or a lower hull count, CoACD sticks out less:**
  - 0.05-1.6 % of the size against 1-9 % for V-HACD;
  - it leaves out 0-3 % of the volume against V-HACD's 3-35 % (the dragon: a third).
- **V-HACD fills its budget.** With a small one it fills cavities: a mug with 8 hulls is 91 %
  overfull, a torus 16 %.
- **CoACD finds the natural parts:**
  - the U in 3 hulls, the table and the stairs in 5;
  - the avocado in 3, the bottle in 4.
- **Where V-HACD's 32 hulls stick out less:** the torus (1.1 against 1.6 %, CoACD 11 hulls)
  and the lantern's thin frame (1.4 against 2.1 %, CoACD 8 hulls).
- **Many separate parts need many hulls:** the ToyCar takes 110 from CoACD, the dragon 51.
- **CoACD's own hull decimation** (64 vertices) is an inner approximation: 4-11 % left out.
  Ours limits hulls to 64 points too, the farthest first (below).
- **Time:**
  - V-HACD: 0.6-2.7 s on all threads;
  - CoACD: 1.4-288 s, mostly on one thread.

### Ours: CoACD's algorithm in Rust (8c-3, measured 2026-10-16)

`engine_collision::decompose`, after CoACD's `main` 131010c (MIT; the paper and its source
read side by side), without its dependencies:

| step | CoACD | ours |
|---|---|---|
| a closed manifold | OpenVDB level set | marching tetrahedra over a distance band (six tetrahedra a cell around its diagonal: no ambiguous case), the grid's origin off the mesh's own grid by fractions of a cell; distances per slab of the grid in parallel. A mesh in several closed pieces (boards overlapping) is remeshed too: as it is, it runs through itself |
| the search mesh | the remeshed mesh as it is | simplified to about 4,000 triangles by quadric edge collapses that keep it closed (the link condition, no flipped triangles), within half a cell |
| caps of a cut | constrained Delaunay (CDT) | ear clipping with holes bridged (Eberly) |
| samples | Sobol (LGPL) and random | Roberts's R2 sequence (deterministic) |
| distances | k-d tree over samples | a BVH over the triangles (Ericson's closest point) |
| the tree search | 150 iterations, depth 3, 20 planes per axis | the same; the root's moves tried in parallel (each iteration expands the next one until all are tried, so the result is the same), the rollout's three candidate cuts in parallel, each cut clipped once |
| the merge | cost matrix, the merged row recomputed | the same, pair costs in parallel; a merged part keeps its pieces, so the faces where they touch stay out of its Hausdorff term |
| hulls | optional decimation | at most 64 points, the farthest first |

Same machine, same judge as above, threshold 0.05 (`sticks out` varies by about ±0.06 %
between runs of the judge, from its random samples; our hulls are the same bits every run):

| model | ours: hulls | points | time | overfill | underfill | sticks out | CoACD: hulls | time | sticks out |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| U | 3 | 26 | 0.02 s | 0.1% | 0.0% | 0.05% | 3 | 1.4 s | 0.05% |
| table | 5 | 49 | 0.04 s | 0.1% | 0.0% | 0.00% | 5 | 2.7 s | 0.00% |
| stairs * | 5 | 255 | 0.7 s | 2.1% | 0.7% | 0.54% | 5 | 52.1 s | 0.58% |
| mug with a handle | 24 | 649 | 0.5 s | 20.6% | 0.0% | 1.61% | 21 | 20.1 s | 1.19% |
| torus | 12 | 768 | 0.5 s | 4.0% | 1.6% | 1.29% | 11 | 14.0 s | 1.59% |
| Duck | 11 | 689 | 0.5 s | 0.6% | 3.4% | 1.40% | 10 | 133.8 s | 1.36% |
| Avocado * | 3 | 192 | 0.5 s | 0.4% | 3.3% | 1.46% | 3 | 48.3 s | 0.68% |
| WaterBottle * | 4 | 256 | 0.5 s | 2.2% | 1.7% | 1.84% | 4 | 37.8 s | 0.72% |
| BoomBox * | 25 | 943 | 2.1 s | 0.6% | 5.5% | 0.97% | 26 | 195.1 s | 0.73% |
| GlamVelvetSofa * | 23 | 985 | 0.7 s | 5.1% | 1.2% | 0.79% | 24 | 50.9 s | 0.83% |
| Lantern * | 8 | 402 | 0.5 s | 32.4% | 0.8% | 2.49% | 8 | 22.8 s | 2.10% |
| SheenChair * | 25 | 1189 | 1.3 s | 6.5% | 1.3% | 1.12% | 29 | 95.3 s | 0.82% |
| ToyCar * | 118 | 3491 | 3.0 s | 102.8% | 6.3% | 1.69% | 110 | 142.6 s | 1.19% |
| DragonAttenuation * | 42 | 1436 | 1.9 s | 54.8% | 5.1% | 1.16% | 51 | 103.2 s | 1.18% |

- **The same parts, 30-270 times sooner:** 0.02-3 s on all threads where CoACD's wheel
  (mostly on one thread) takes 1.4-195 s.
- **Hull counts within 10 % of CoACD's** but the mug (24 against 21), fewer on the chair (25
  against 29) and the dragon (42 against 51).
- **Sticks out about as much:** within 0.1 % of CoACD or less on 7 of 14 models (the U,
  the table, the stairs, the torus, the duck, the sofa, the dragon); 0.2-0.5 % more on the
  mug, the boom box, the lantern, the chair and the car; 0.8-1.1 % more on the smooth
  avocado and bottle, where CoACD's hulls keep thousands of points and ours 64.
- **Leaves out 0-6.3 %,** between CoACD unlimited (0-2.9 %) and CoACD at 64 points (up to
  13.5 %).
- **What took the time, and what fixed it:**
  - decimating the stairs' 244,000 remeshed triangles took 6.6 s: on its flat faces every
    collapse cost nothing, so single vertices took in ever more neighbours (fans), and every
    later collapse there scanned them all. Shorter edges first where the errors tie, and at
    most 32 neighbours: 0.3 s;
  - the merge recomputed every pair after each merge (ToyCar: 42 s). Pair costs kept and
    only the merged part's recomputed: 0.3 s;
  - the first rounds of cuts, one to sixteen parts, left most threads idle (0.5 s each).
    The parallel tree search and remesh above: ToyCar 4.9 s to 3.0 s, BoomBox 4.2 to 2.1 s,
    with the same hulls.

## Level geometry: triangle meshes in Jolt

`crates/engine_physics_jolt/examples/mesh_shape_bench.rs`, release, one thread:
- the shape built from the triangles, as the runtime would build it at load;
- the same shape restored from Jolt's binary form;
- 20,000 rays from the bounds' centre outward.

| mesh | triangles | build | Jolt's binary form | restored from it | rays/s |
|---|---:|---:|---:|---:|---:|
| SheenChair | 39,936 | 24.7 ms | 0.63 MB | 0.07 ms | 2.07 M |
| Dragon | 134,995 | 102 ms | 2.11 MB | 0.23 ms | 3.94 M |
| Sponza | 262,267 | 207 ms | 4.18 MB | 0.44 ms | 1.75 M |
| Intel Sponza | 3,747,018 | 3,895 ms | 58.9 MB | 5.5 ms | 1.43 M |

| hull | build | Jolt's binary form | restored from it |
|---|---:|---:|---:|
| 8 points | 7.0 µs | 689 B | 0.52 µs |
| 32 points | 54 µs | 2.6 KB | 1.0 µs |
| 64 points | 118 µs | 4.4 KB | 1.8 µs |
| 256 points | 356 µs | 8.2 KB | 3.8 µs |

- A mesh's build costs about 1 µs per triangle. Its restore copies about 16 bytes per triangle
  at memory speed, 470-880 times faster.
- The binary form is smaller than the vertices and indices it is built from: Sponza's are
  4.9 MB (12 bytes per vertex and per triangle) against 4.2 MB.
- Rays barely slow down with size (a BVH), so level meshes need not be simplified for queries.
  Simplifying them saves memory, and pays only for dense scanned or sculpted meshes.
- **Building a level at load is out:** 3.9 s for Intel Sponza on one thread, against 5.5 ms
  restored.

Jolt's restore needed one fix in our stream: `ext.cpp`'s `BufferRecorder` reported the end of
the stream on reaching it, but Jolt expects `std::istream`'s rule, the end only after a read
past it. Jolt reads a shape's empty material list last, sees "end", and fails.

## Fitting primitives

- **Box:** the smallest of several oriented boxes around the hull's vertices:
  - along the principal axes (covariance eigenvectors);
  - along each hull face's normal, with the box turned to the smallest area around it in the
    face's plane (rotating calipers on the projected hull).

  This is close to the smallest box (O'Rourke's exact algorithm is O(n³)). Larsson and
  Källberg's DiTO (2011) is another fast approximation.
- **Sphere:** the smallest enclosing sphere of the hull's vertices (Welzl, 1991, expected
  linear time).
- **Capsule:** the axis along the box's longest side. The radius is the vertices' largest
  distance from the axis; the segment just covers the vertices' projections once capped.
- **Which one:**
  - the decomposition decides first whether the mesh is convex enough to be one part (its
    concavity within the threshold). Neither the hull nor a primitive can tell: a U's hull
    is a box, and comparing volumes needs a closed mesh, which most render meshes are not;
  - one part: a primitive when its volume is within 10 % of the hull's (it then covers the
    hull, so it never cuts into the mesh), otherwise the hull;
  - several: the convex parts.

  O3DE fits by least squares instead, which can cut into the mesh. A collider that covers its
  mesh is the safer default.

## Scaling

The owner's requirement:
- any scale works after the colliders are made: uniform, non-uniform or mirrored;
- on any shape, concave props included;
- with no errors and no rebuild that the developer has to ask for.

- **The decomposition survives every scale.** A linear map keeps convex sets convex and unions
  unions, so a scaled prop's scaled hulls still cover exactly the scaled prop. Nothing is
  decomposed again.
- **What a body can take directly:** Jolt's scaled shape over the cooked shape, when Jolt
  accepts the scale:
  - meshes and hulls take any scale;
  - boxes take any scale along their own axes.
  - Hulls are stored without a rotation of their own (their points in the collider's frame), so
    any scale is valid for them inside a compound.
- **What it cannot take is rebuilt, exactly where possible:**
  - a box turned in its collider and scaled along other axes becomes a parallelepiped: the hull
    of its 8 scaled corners, exact;
  - a non-uniformly scaled sphere, capsule or cylinder becomes an ellipsoid-like shape: a hull
    of its subdivided surface, scaled, within a set tolerance (Parry does the same);
  - a shear (a non-uniform scale above a rotation in the node tree, which T·R·S cannot express)
    is baked into the points: exact for hulls and boxes, approximated for round shapes, exact
    for meshes (rebuilt, slow, so the cook bakes scene instances and the editor warns);
  - a zero scale disables the collider, as the glTF draft says. Very small scales are clamped
    to Jolt's 10⁻⁶.
- **Rebuilt shapes are cached** per collider and scale (quantized), so a thousand barrels
  scaled alike share one shape. An animated scale goes through the cache too.
- Jolt caps the convex radius at 5 cm whatever the scale (above), so scaling up does not round
  edges.

## Editing (groundwork)

The owner's requirement: later, in the editor, one grabs a collider's faces, edges or corners
and moves them.

- **The collider is data in the engine's own form**, not a backend's:
  - shapes with stable ids;
  - primitives as parameters with a transform;
  - hulls as points (their faces follow from the points);
  - meshes as vertices and triangles.
- **Generated colliders** are derived data, made again when their mesh or rules change.
  **Edited colliders** are source data and stay as edited: editing a generated one turns it
  into an edited one. When its mesh changes, the editor says so; it does not overwrite the
  edit.
- **What the editor needs from this step:**
  - moving a hull vertex: move the point, recompute the hull. A point that ends up inside drops
    out, and the editor can show that;
  - moving a face or an edge: move its points together;
  - a primitive's handles change its parameters;
  - every edit is a command with undo (CLAUDE.md §12).
- **None of it needs a backend:** the cook bakes what the data says. An edited collider cooks
  like a generated one.

## What we take

- **Triangle meshes for level geometry**, from the render mesh by default (what you see is what
  collides). Simplification is optional, error-bounded, with borders locked (meshoptimizer),
  for dense meshes. Materials can be excluded by name (foliage cards, decals): no automatic
  guess, since a masked material may be a grating one walks on.
- **For props:** a decomposition, then for a mesh convex enough to be one part a fitted
  primitive, else its hull. This is Godot's "automatic", with fitting added.
- **A convex decomposition of our own, in Rust**, following CoACD's algorithm (the owner,
  2026-10-16: "конечно лучше на rust"). It fits better than V-HACD with 2-10 times fewer
  hulls in the table above, and is 30-270 times faster than CoACD's build (8c-3 above):
  - the remeshed input simplified before searching;
  - CoACD's search as it is (150 iterations), run in parallel on the job system where that
    changes nothing in the result;
  - hulls of at most 64 points;
  - results cached in the DDC.

  None of CoACD's dependencies come along:
  - our own voxel remesh replaces OpenVDB;
  - our own polygon triangulation replaces CDT;
  - our own low-discrepancy sampling replaces the LGPL Sobol;
  - nothing from Boost.
- **Our own QuickHull** (Barber, Dobkin and Huhdanpaa 1996; Gregorius, GDC 2014), shared by the
  decomposition, the fitting and the runtime rebuilds.
- **Authored colliders:**
  - `KHR_physics_rigid_bodies` with `KHR_implicit_shapes`;
  - Unreal's name prefixes.
- **Jolt's binary form in the pack**, keyed by Jolt's version, as Unreal and Unity ship their
  backends' cooked data. A mismatch is a load error that says to cook again. The engine's own
  description goes beside it only where the runtime may rebuild (primitives and hulls,
  small).
- **Scaling by the rules above**, with nothing approximated silently.

## What we don't take

- **V-HACD as the decomposition:**
  - it fills its hull budget (32 hulls where CoACD finds 3-5);
  - with a small budget it fills cavities (a mug with 8 hulls: 91 % overfill);
  - it was the fallback had our port missed its goals (CoACD's hull counts within 10 %, at
    most 3 s per model). The port meets them but for the mug's hull count (24 against 21,
    and it sticks out 0.4 % more), so it is not vendored.
- **CoACD's C++:** its dependencies (MPL, BSL, LGPL) and its time.
- **Least-squares fitting** (O3DE), which can cut into the mesh.
- **Building level meshes at load**, which is 0.2-3.9 s per level.
- **Godot's suffixes and OMI's extensions** for now: easy to add if users bring such files.

## Sources

- Jolt Physics 5.6.0 (MIT), `third_party/JoltPhysics`: `Shape.h/.cpp`, `ScaleHelpers.h`,
  `ScaledShape.cpp`, `CompoundShape.cpp`, `RotatedTranslatedShape.cpp`, `ConvexHullShape.h`,
  `StreamIn.h`, `StreamUtils.h`, read 2026-10-16.
- V-HACD 4.1.0 (BSD-3-Clause), github.com/kmammou/v-hacd, tag `v4.1.0` (22ec20a),
  `include/VHACD.h`.
- CoACD (MIT), github.com/SarahWeiii/CoACD, `main` at 131010c (2026-09-23), and the `coacd`
  1.0.14 wheel. X. Wei, M. Liu, Z. Ling, H. Su, "Approximate Convex Decomposition for 3D Meshes
  with Collision-Aware Concavity and Tree Search", ACM TOG 41(4), SIGGRAPH 2022.
- Godot (MIT), `master` 4d10419 (2026-10-15): `editor/import/3d/resource_importer_scene.cpp`
  and `.h`, `modules/gltf/extensions/physics/`.
- O3DE (Apache-2.0 OR MIT): `Gems/PhysX/Core/Code/Source/Pipeline/PrimitiveShapeFitter/`.
- Parry (Apache-2.0), github.com/dimforge/parry, `master` b3c3584 (2026-10-06):
  `src/shape/ball.rs`, `capsule.rs`, `cylinder.rs` (`scaled`).
- KhronosGroup/glTF pull requests #2424 (`KHR_physics_rigid_bodies`), #2370
  (`KHR_implicit_shapes`) and #2258 (`OMI_physics_body`, `OMI_physics_shape`), read
  2026-10-16; the drafts' READMEs on the authors' branches (eoineoineoin/glTF `rigidBodyMerge`,
  `collisionShapeMerge`).
- Unreal Engine 5 documentation: "Setting Up Collisions With Static Meshes", "FBX Static Mesh
  Pipeline" (collision naming), "Simple versus Complex Collision".
- Unity documentation: Mesh Collider, Player settings ("Prebake Collision Meshes"),
  `Physics.BakeMesh`.
- C. B. Barber, D. P. Dobkin, H. Huhdanpaa, "The Quickhull Algorithm for Convex Hulls", ACM
  TOMS 1996. D. Gregorius, "Implementing Quickhull", GDC 2014.
- E. Welzl, "Smallest enclosing disks (balls and ellipsoids)", 1991. J. O'Rourke, "Finding
  minimal enclosing boxes", 1985. T. Larsson, L. Källberg, "Fast Computation of Tight-Fitting
  Oriented Bounding Boxes", Game Engine Gems 2, 2011.
- K. Mamou, F. Ghorbel, "A simple and efficient approach for 3D mesh approximate convex
  decomposition", ICIP 2009.
