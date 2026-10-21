# 0110. Blockout: convex brushes held as integer planes, evaluated in Godot's order over convex pieces

- Status: Proposed (2026-10-21): the core is built (E2b-1); three questions to the team
  (below)
- Date: 2026-10-21
- Related:
  - [ADR 0107](0107-editor.md) §7, phase E2b (the owner, 2026-10-17): brushes for
    greyboxing a level, like Godot's CSG; our own Rust over convex brushes held as planes;
  - [ADR 0039](0039-geometry-pool.md), [ADR 0064](0064-gpu-scene-extraction-runtime.md)
    meshes and the GPU scene; [ADR 0092](0092-custom-materials.md) material types;
    [ADR 0101](0101-mesh-colliders.md) hulls; [ADR 0103](0103-navigation.md) the
    navigation mesh; [ADR 0108](0108-scene-format-2.md) scenes; [ADR 0109](0109-viewport-picking.md)
    picks;
  - research: `docs/research/blockout.md`; UX: `docs/research/ux/blockout.md`.

## Context

- **The phase** (the owner): box, wedge and ramp, stairs, cylinder, sphere; add, subtract,
  intersect; face editing (extrude a face, move an edge); colliders and the navigation mesh
  made from the brushes, so Play runs on the greybox at once; a grid material in world
  space; bake to a mesh. Our own Rust over convex brushes held as planes (the Quake, Hammer
  and TrenchBroom way), not Godot's C++ library.
- **What the sources show** (research note):
  - float planes with epsilons (qbsp, TrenchBroom, Hammer, Godot up to 4.3) are where the
    invalid brushes, microleaks, holes and cracks their users report come from;
  - Godot since 4.4 rebuilds a whole CSG tree's mesh booleans (Manifold) on the main
    thread at every change, every frame of a drag; its collider is a hollow triangle mesh;
  - destructive carve (Hammer, TrenchBroom) turns a hole into fragments no one can edit
    again;
  - plane-based geometry can be exact (Nehring-Wirxel, Trettner and Kobbelt 2021; Chisel's
    exact path, 2026-10-12): a point is the three planes it lies on, and every decision is
    the sign of an integer expression.
- **The UX** (`ux/blockout.md`): brushes drawn on surfaces, edited by their faces, their
  operation a field, their order the tree's; the result follows a drag frame by frame;
  Play with no bake.
- **Constraints:** an edit answered within the frame (CLAUDE.md §12); nothing paid by
  scenes without brushes, not even shader registers (§5); everything undoable; scenes as
  text (ADR 0108).

## Options

### A. The geometry

1. **Mesh booleans** (Godot's Manifold, manifold-rust, csgrs, ProBuilder): any closed mesh
   in. But the team declined Godot's C++ library; manifold-rust brings clipper2-rust
   (BSL-1.0, not on our list); csgrs decides by epsilon. The result is a hollow triangle
   surface, with nothing to edit as planes and no convex pieces for physics.
2. **Convex brushes as float planes with epsilons** (qbsp, TrenchBroom): simple, the
   known way, and the known failures.
3. **Convex brushes as integer planes with exact predicates.** A plane is n·x = d with
   integer normals of at most 2^22 and offsets through points of a 1 mm grid within
   ±2^26 mm (±67 km); a point is where three planes meet, in homogeneous `i128`
   coordinates; which side of a plane a point lies on is the sign of one `i128`
   expression below 2^121. No epsilon decides anything; rounding happens only where an
   edit makes a plane (a turned face's normal, rounded within 2·10⁻⁷ rad).

### B. Evaluation

1. **Destructive:** a cut rewrites the brushes it touches. Rejected by the UX: holes stay
   editable.
2. **A BSP tree of the whole level** (qbsp, Unreal): Quake needs one for visibility and
   leaks; we need only the surface and the solid, local to an edit.
3. **Godot's order over convex pieces, kept through edits:** each brush acts on what the
   brushes before it in its group made. Each additive brush keeps its pieces (its parts
   cut by the cuts after it and clipped by the intersects after it); each piece's face
   keeps what of it is visible (in 16 m tiles when larger, so a ground under a level is
   redone only around an edit); a grid of 4 m cells finds what is near.

### C. Where brushes act on each other

1. One scene-wide CSG: every brush against every other.
2. **Groups** (Godot's `CSGCombiner3D`): a `BrushGroup` entity's brushes, its descendants
   in the tree's order, act on each other; groups do not. A building is a group; a door
   that moves later is another.

## Measurements

`cargo bench -p engine_brush --bench csg`, ref-1 (Ryzen 7 7700), bench profile, one
thread, the E2b-1 branch:

| Operation | |
|---|---|
| a point's side of a plane (exact) | 3.4 ns |
| the point where three planes meet | 10.6–11.4 ns |

A greybox: rooms of 10 × 8 m inside, walls 0.2 m, 3 m high, on one ground, each a box,
its inside cut, a door, a window and a pillar:

| Rooms | Brushes | Pieces | Surface polygons | Triangles | From nothing | Of it the surface |
|---|---|---|---|---|---|---|
| 1 | 6 | 14 | 63 | 126 | 0.15 ms | 0.06 ms |
| 9 | 46 | 118 | 513 | 1,026 | 1.28 ms | 0.58 ms |
| 100 | 501 | 1,301 | 5,616 | 11,232 | 13.9 ms | 6.5 ms |
| 400 | 2,001 | 5,201 | 22,426 | 44,852 | 56.7 ms | 27.4 ms |

In the 400 rooms, kept evaluated (`Group`; one move a millimetre along x, the changed
brushes' polygons gathered for their meshes; medians of 31):

| Edit | Redone | Time |
|---|---|---|
| a door (a cut) dragged | 1 additive brush, 79 tiles | 0.58 ms |
| a room's box (an add) dragged | 1 additive brush, 79 tiles | 0.57 ms |
| a pillar dragged | 1 additive brush, 8 tiles | 0.025 ms |
| everything from nothing, tiles and cells kept | 2,001 brushes | 123 ms |

- **Exactness, tested:** surfaces closed (the volume they enclose the same seen from
  anywhere) for a hole, touching and overlapping boxes, the order of cuts and adds,
  intersects and tilted cuts; the cut owns the hole's walls; 40 random edits kept
  incrementally end where a rebuild from nothing does, to the area on every brush.
- **Not measured yet:** the work spread over the job system (load, large edits); meshes,
  hulls and the navigation's bake made from the result (their steps measure them).

## Decision

### 1. Brushes and groups

- **A brush is convex parts held as planes:** each part the integer planes that bound it
  (option A.3), in the brush's own grid (1 mm). Vertices, edges and polygons are derived:
  a large square on each plane clipped by the others. Any set of planes is convex, so a
  part can only fail by being empty or open, and is then reported, not drawn.
- **A shape keeps its parameters** (box, wedge, stairs, cylinder, sphere) until a face
  edit turns it into plain planes (ProBuilder's and Chisel's rule). Stairs are one part
  per step.
- **Its placement is its entity's `Transform`,** so the gizmo, keyboard transforms,
  surface drops and undo work on brushes unchanged. The group's grid gets each part's
  planes through the transform: exact for moves on the grid and quarter turns, rounded
  otherwise (a turned plane's normal to 2^22, through its first corner on the grid).
- **Operations:** add, cut, intersect, each acting on what the brushes before it in the
  group made (Godot's rule; a cut first cuts nothing). New brushes go to the end of their
  group.
- **Groups** (option C.2): a `BrushGroup` entity; its descendant brushes in tree order.

### 2. Evaluation

- **Option B.3,** in `engine_brush` (no dependencies beyond the core):
  - `exact`: planes and points, every side exact;
  - `polygon`, `solid`: convex polygons and solids split by planes; a convex solid less
    another as convex pieces (axis planes first, so the pieces are slabs), one clipped to
    another;
  - `csg::evaluate`: a group from nothing;
  - `group::Group`: kept evaluated through edits; `set` redoes the additive brushes an
    edit touches and the tiles around them, `take_changed` and `polygons_of` give what the
    meshes need.
- **The surface:** each piece's faces less what the other pieces cover. On a plane two
  pieces share, faces turned towards each other both go, and of two facing the same way
  the earlier piece's stays (qbsp's rule, exact here). A polygon belongs to the brush face
  whose plane it lies on, so a cut owns its hole's walls and a click on a doorway's side
  selects the doorway.
- **When:** in the player's frame, after the game's ticks and before extraction (where
  models turn into renderers), so a drag shows its result in the same frame.

### 3. What the game gets

- **Meshes: one per brush** (its polygons, fans of triangles, normals from its planes, UVs
  by Quake's projection on the axis plane nearest the normal, in metres in the group's
  space), added to the geometry pool and drawn through a `MeshRenderer` on the brush's
  entity, hidden from scenes. Picks then return the brush (ADR 0109). A changed brush
  swaps its mesh. T-junctions are split before the triangles are made, so no cracks show
  once coordinates are rounded.
- **The grid material:** a material type (ADR 0092) drawing lines in the UVs' metres,
  antialiased from screen-space derivatives (Golus 2023), tinted per brush, floors, walls
  and ceilings shaded apart by their normals. Its pipelines are made with the first brush:
  a scene without brushes pays nothing.
- **Colliders: the pieces as convex hulls,** one static compound body per group (solid,
  unlike a triangle mesh: casts starting inside find it). They are made when an edit ends
  and before Play, not during a drag (the editor runs no physics while editing). The
  convex radius is measured in their step (Jolt's 0.05 m rounds edges).
- **Navigation: the visible surface's triangles** go to `engine_nav::bake` in the
  background after an edit, the tiles the edit touched only; the hulls stay out (Recast
  would keep islands inside solids).
- **A cooked game** gets the same: the cook evaluates each group into its meshes, hulls
  and navigation tiles.
- **Bake to Mesh** (a later step): a group's surface as a mesh asset with its hulls as
  colliders (ADR 0101), the brushes kept hidden.

### 4. In scenes

Proposed (question 3):

- **`BrushGroup`**: marks the group (its settings later).
- **`Brush`**: the operation; the shape and its parameters (sizes in metres, steps, sides),
  or, once edited, its parts' planes as integers in millimetres (`[nx, ny, nz, d]`, exact
  through text); whether it collides (on for adds); a tint. Text round-trips exactly.

### 5. Steps

1. **E2b-1, the core** (this ADR): `engine_brush`, tests, the bench above.
2. **E2b-2, brushes in the player and the editor:** the components, the group evaluated in
   the player's frame, meshes per brush, the grid material, Blockout in the add menu (each
   shape at the camera's target), the inspector's parameters, undo.
3. **E2b-3, drawing:** B and Shift+B on the surface under the pointer, typed sizes, the
   face handles, hollow rooms (question 1), the shapes' keys.
4. **E2b-4, colliders and navigation:** hulls per group, the background bake, both in the
   overlays.
5. **E2b-5, faces, edges and corners:** Tab, picking the nearest corner, edge or face,
   moves and extrusions, refused when a part would stop being closed.
6. **E2b-6, bake to a mesh,** and the cook's path.

### Questions to the team

1. **Rooms merging** (`ux/blockout.md`, question 2): a room (a hollow box) whose inside
   overlaps another room's wall opens into it, so a corridor drawn to a room needs no
   doorway cut; other brushes keep the tree's order. Or every opening is cut by hand.
2. **Snapping for brushes** (`ux/blockout.md`, question 1): the owner's rule as for
   everything else, with corners drawn or moved landing on other brushes' corners and
   edges (the coordinator's recommendation), or snapped to the grid by default.
3. **The format above:** the shape's parameters, and integer planes in millimetres once
   edited.

## Consequences

- **Exact geometry costs little:** a side is 3.4 ns; the arithmetic is ours, small
  (3×3 determinants in `i128`) and dependency-free, like QuickHull (ADR 0101).
- **Determinism:** the same brushes make the same meshes, hulls and navigation on every
  machine and thread count.
- **Edits are local:** a door dragged in 2,001 brushes redoes one brush and 79 tiles in
  0.58 ms. An intersect redoes every additive brush before it (it clips everything
  earlier), so it costs what a rebuild does.
- **Harder:**
  - a group's extent is bounded (±67 km at 1 mm);
  - turned brushes are rounded where they meet others (micrometres), so pieces a
    micrometre thin can exist; physics will skip pieces below a size;
  - the plane table grows with edits (each move adds planes) until the group is rebuilt,
    which saving and loading do;
  - removing or reordering brushes redoes the whole group (123 ms for 2,001 brushes on one
    thread; to be spread over the job system).
- **Revisit:** if large groups' rebuilds need it, Chisel's routing tables (the research
  note) against these per-brush caches; the job system for loads.

## As built (E2b-1, 2026-10-21)

- **`engine_brush`** (no dependencies beyond `engine_core` and `engine_profiler`):
  - `exact`: `Plane` (reduced, so equal planes are equal values; `Plane::through` rounds
    a normal to 2^22 and keeps axes and diagonals exact), `Point::meet` (Cramer's rule in
    `i128`), `Point::side`, `Point::same` (256-bit cross products);
  - `polygon`: `Planes` (each plane once, both orientations; a box of half-size 2^27 for
    polygons to start from), `Polygon` (edges as planes, corners as points), `split`,
    `clip`, `reversed`;
  - `solid`: `Solid::from_planes`, `split`, `subtract` (axis planes first), `intersect`;
    `Owner` (brush, face);
  - `csg`: `Brush` (an operation and parts of planes), `evaluate`, `surface`;
  - `group`: `Group` with `push`, `set`, `insert`, `remove`, `rebuild`, `take_changed`,
    `polygons_of`, `for_each_polygon`, `pieces`;
  - `shape`: `box_planes`.
- **Tested:** planes reduced and turned around, exact sides at the bounds and where floats
  would round, wide products; squares facing their planes, clipping through corners and
  along edges, splits and reversed polygons; the closed surfaces and volumes above; open
  and empty brushes reported; a group equal to evaluating from nothing (its ground tiled);
  40 random edits equal to rebuilds.
- **The bench** (`benches/csg.rs`): the tables above.
