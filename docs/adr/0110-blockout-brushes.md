# 0110. Blockout: convex brushes held as integer planes, evaluated in Godot's order over convex pieces

- Status: Accepted (team decisions, 2026-10-21: the three questions below answered a;
  the shapes the owner asked for: box, cylinder, sphere, torus, cone and truncated cone)
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
- **The grid material:** the built-in material with a grid texture (a 1 m cell, lines
  every 25 cm, its mipmaps, 16× anisotropy; ADR 0098) over the UVs' metres, its color the
  brush's tint. Golus (2023) finds such a texture "basically as good as a simple texture
  based grid can look"; it needs no pipeline of its own, so nothing is compiled, no
  register moves, and a cooked game draws it as is. A procedural material type (ADR 0092:
  lines antialiased from screen-space derivatives, made with the first brush) replaces it
  if the texture's moiré or blur shows.
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

### The team's answers (2026-10-21)

1. **Rooms merge** (`ux/blockout.md`, question 2: a): a room (a hollow box) whose inside
   overlaps another room's wall opens into it, so a corridor drawn to a room needs no
   doorway cut; other brushes keep the tree's order (E2b-3).
2. **Snapping for brushes** (`ux/blockout.md`'s question 1: b, the coordinator's
   recommendation): the owner's rule as for everything else, and
   corners drawn or moved land on other brushes' corners and edges within a few pixels
   (SketchUp's inference) (E2b-3).
3. **The format above** (a): the shape's parameters, and integer planes in millimetres once
   edited.
4. **The shapes** (the owner): besides the box, the cylinder, sphere, torus, and the cone,
   pointed and truncated; the wedge and stairs stay.

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

## As built (E2b-2a, 2026-10-21): brushes in the player

- **`engine_brush`:**
  - `shape`: `Shape` (box, wedge rising towards +z, stairs of a part per step, a
    cylinder's prism around y on an ellipse, a sphere's polyhedron inscribed in an
    ellipsoid) and `parts` in a brush's own grid, its base's middle at the origin;
  - `Plane::from_f64` (normals of whole numbers kept, others scaled to 2^22 and rounded),
    `Plane::transformed` (a part's plane in its group's grid: moves and quarter turns
    exact, turns rounded, the plane moved less than a unit's 2^-22);
  - `mesh::triangles`: fans of the polygons, each grown 0.05 mm in its plane (the mitre
    limited to ten times) so T-junctions show no cracks; normals from the planes; UVs in
    metres on the nearest axis plane.
- **`engine_app::blockout`:**
  - the components, as the format proposed (question 3): `BrushGroup`, and `Brush` with
    `op`, `shape`, `size` (metres), `segments` (stairs' steps, the sides of cylinders and
    spheres), `planes` (`BrushPlane`: part, normal, offset in millimetres, for edited
    brushes), `collides`, `tint`;
  - `Blockout::sync` in the player's presentation step after the models: nothing unless a
    brush, a group, a brush's or a parent's transform or a parent changed, or brushes
    came or went; then each group's brushes (its descendants in the tree's order, not
    those of a group within it; a brush outside any group alone) are compared with what
    made their planes (the component and the transforms up to the group, in
    millimetres), the group rebuilt when its brushes changed, otherwise `Group::set` for
    the changed ones;
  - a mesh per brush that shows anything, in the brush's own space (mirrored brushes turn
    their triangles), with a `MeshRenderer` hidden from scenes; a material per tint;
  - the grid texture (256 × 256 texels a metre: dark lines on the metres, lighter ones
    every 25 cm, its smaller levels averaged) made after the first brush's frame and put on
    the brushes' materials; until then they are plain.
- **Tested:**
  - `engine_brush`: the shapes' volumes (the box's, the wedge's half, the stairs'
    steps, the cylinder within 1 % of the ellipse, the sphere inscribed), moves and quarter
    turns exact (with f32's rounding of the turn), a 30° turn's volume within 10⁻⁶; a box's
    12 triangles grown, facing out, UVs in metres;
  - on the player, both GPUs (`tests/blockout.rs`): a scene file with a group (a room, its
    inside cut, a door cut through a wall) and a wedge, stairs and a cylinder, saved and
    loaded; six meshes; the room's inside darker through the door than the lit wall
    beside it, the wall's metre line on the grid; the door moved, its mesh made again and
    the old hole wall again; the cut taken out, its mesh gone (`ENGINE_BLOCKOUT_PNG` writes
    the frame);
  - no allocation in frames with a group of brushes that stays as it is
    (`zero_alloc_models`).

## As built (E2b-2b, 2026-10-21): brushes in the editor, cones and tori

- **`engine_brush::shape`:** `Shape::Cone(sides, top)` (the top as thousandths of the base:
  0 pointed, between truncated, 1000 the cylinder), its sides through the base's edges and
  the top's; `Shape::Torus(around, tube)`, lying on its base: a convex prism per segment
  around the ring (the tube's sides parallel to the segment's chord, the first flat on the
  ground), its ends the planes halfway between it and its neighbours', shared turned
  around, so the faces there go.
- **`mesh::triangles` leaves slivers out:** polygons thinner than 1 µm (twice their area
  over their perimeter). Where rounded planes meet (a torus's segments: their sides,
  rounded to 2^22, meet the shared ends some 25 nm apart) the exact surface keeps strips
  nanometres wide that no pixel would show and growing by 0.05 mm would.
- **`Brush`:** `BrushShape::Cone` and `Torus`; `top` (a cone's top, 0 to 1).
- **The editor** (ADR 0107's notes of the same day): **+** → **Blockout** adds a box, wedge,
  stairs, cylinder, cone, truncated cone, sphere or torus where the camera looks, into the
  active entity's group (itself, or the group above it), else the scene's first, else a
  new "Blockout" group, at its end, all one undo step; the tree shows each brush's shape
  (cube, slope, stairs, cylinder, cone, tire, sphere) and its operation ("Brush", "Cut",
  "Intersect"), a group with a wall.
- **Tested:** cones' volumes (pointed, truncated, full as the cylinder) within 10⁻⁵; a
  torus of 24 parts within 3 % of its volume showing exactly its 288 tube sides; a refused
  torus with no hole; the add menu's Blockout group opened beside it by the pointer, a click
  on Torus making a group and the brush, the next brush joining it, the tree's kinds, undo
  taking both back.

## As built (E2b-3a, 2026-10-21): drawing brushes

- **`engine_editor::draw`:** B draws a brush on the surface under the pointer (ADR 0109's
  surface pick, the selection included), Shift+B a cut. B again before the first click
  goes through the add menu's shapes; Shift+B and B switch the operation.
  - **The stages:** a click sets the first corner in the surface's plane (`u` along x on
    floors and ceilings, `v` up walls and slopes, `u × normal = v`); the base follows the
    pointer and a click or a dragged press's release keeps it; the height follows the
    pointer along the normal, either way; a click makes it.
  - **Typed sizes:** the sides with Tab between, Enter, the height; a cut's typed height
    goes into the surface.
  - **R** turns where wedges and stairs rise (away from the first corner along the second
    side). Drawn stairs get steps of about 0.2 m.
  - **Normals:** a surface normal within 3° of an axis is that axis.
  - **Snapping:** the owner's rules, the first corner rounded to the grid in the plane;
    free corners and sizes on the brushes' millimetre grid.
- **Live:** the brush exists from its first height on, in the drawing's undo step (with
  the group it makes when the scene has none), changed as the pointer moves and
  reverted by Escape or a right click; the blockout evaluates it in the same frame, so a
  cut shows its hole as it is drawn. The overlay draws the box's edges (a cut's dashed,
  orange) and its sizes; the mark before the first click is a square and a cross in the
  surface's plane.
- **Keys:** the drawing has the frame's keys and the clicks in the view. The camera
  keeps the middle button and the wheel, and the right button before the first click.
- **Tested:**
  - the stages, the planes, snapping, typed sizes and a cut on a wall, R, the shapes'
    cycle, edges and labels (unit tests);
  - in the editor: a box in three clicks as one undo step in a new group; a doorway
    typed into a wall's face, into the wall's group; a right click and Escape dropping
    it; the right button flying the camera before the first click;
  - on both GPUs: the corner lands on a floor 0.5 m below the ground through the
    surface pick, and the brush is meshed as it is drawn.
- **Left for E2b-3b and after:** hollow rooms that merge (Ctrl+H), the face handles,
  corners landing on other brushes' corners and edges, a cut "through the brush it was
  drawn on" with no height given, the stamp (B, Enter), the walkability hint.

## As built (E2b-3b, 2026-10-21): rooms

- **`engine_brush::csg::Brush::inside`:** a room's inside as convex parts, its faces
  numbered after the parts'. An additive brush with one is a room (`is_room`).
- **The team's answer 1 (rooms merge), as evaluated:** a room's pieces are its parts less
  every room's inside whose bounds touch it, its own included and wherever the rooms are
  in the group's order; then the cuts and intersects after it, as for any additive brush.
  Insides take out only rooms. The walls of an opening one room's inside makes in
  another's belong to the room that made it, and a room's inner faces to the room.
  `Group` keeps it through edits: a room's change redoes it and the rooms its old and new
  inside touch.
- **Walls:** `shape::grown` moves every plane of a shape out by the walls' thickness
  (`Plane::offset`, exact for axis planes), so a box's walls, floor and ceiling and a
  cylinder's round wall are all that thick; the inside is the shape at its size.
- **`Brush::hollow`** (metres, 0 solid; scenes without it load solid): `parts()` gives the
  walls, `inside()` the inside; a cut is never a room.
- **The editor:** Ctrl+H makes the selected adding brushes rooms with walls 0.2 m thick, or
  solid again, as one undo step; while drawing, Ctrl+H draws a room, its drawn size its
  inside ("Room", "Cylinder Room"). The tree shows a room as a floor plan, "Room".
- **Measured** (ref-1, one thread, `cargo bench -p engine_brush --bench csg`): 400 rooms of
  10 × 8 × 3 m and 380 corridors between them (781 brushes) evaluate in 121 ms from
  nothing, 248 ms as a `Group` with its tiles; dragging a corridor redoes 3 brushes and
  294 tiles in 1.45 ms, a room 3 brushes and 216 tiles in 1.25 ms. The other numbers
  above are unchanged (a door dragged among 2,001 brushes 0.587 ms).
- **Tested:**
  - a room's volume and area exact, both sides its own;
  - a hall and a corridor opening into each other the same in either order (the opening,
    the wall beside it, the corridor's end gone inside the hall, its floor outside);
  - a crate inside a room staying, and a later cut cutting the room's wall;
  - rooms kept through a corridor moved through them, a room made solid and a room again,
    each step as a rebuild from nothing;
  - in the player, on both GPUs: a scene file's hall and corridor drawn, the hall lit
    through the corridor's door; looked at from inside the hall, the opening;
  - in the editor: Ctrl+H, undo, a room drawn.
- **Left:** sides of a room left open (a courtyard), a room's walls of another thickness
  from the editor (the inspector sets `hollow`), arches.

## As built (E2b-3c, 2026-10-21): side handles and landing on other brushes

- **`engine_editor::handles`:** the active brush shows a handle in the middle of each side
  of its size facing the camera (its own +x, −x, top, base, +z, −z). A drag moves that
  side along its normal on the plane holding that line that faces the camera most; the
  opposite side stays (the origin, the base's middle, moves half the change along x or z,
  all of it for the base, none for the top). The owner's snapping rules on the size;
  free on the millimetres, never below one. One undo step per drag, Escape reverting it;
  the handles come before the gizmo's under the pointer and hide it while one is dragged.
- **Landing (the team's answer 2):** while drawing, the corners of every other brush's box
  (a room's walls' box too) and its twelve edges are candidates within 8 px of the
  pointer, corners first. The first corner lands on one (on an edge running along an
  axis, at a step of the grid along it); the base's other corner on one projected into
  the base's plane; the height on a corner's when the pointer is over it. They come
  before the grid. The overlay marks a corner with a green dot and an edge with a red
  ring (SketchUp's colors; the shapes tell them apart), and the hint says which.
- **Tested:**
  - the sides' middles, the handles facing the camera and the hit;
  - a side dragged along a turned brush's normal with the opposite side kept, in steps
    with snapping; the top keeping the origin and the base moving it; the millimetre floor;
  - landing on a corner, an edge at a grid step along it, the grid elsewhere, a height
    from a corner;
  - in the editor: a side dragged as one undo step, before the gizmo, Escape taking
    another back; a doorway's corner landing on the wall's edge, on the grid;
  - on both GPUs: a crate's handles shown, and a drawn corner three pixels off the crate's
    corner landing on it exactly through the real surface pick.
- **Left:** radius handles for cylinders and spheres, Alt+drag pulling a face into a new
  brush, landing on faces' middles and edges' midpoints, the candidates culled to the view
  for very large levels.

## As built (E2b-4a, 2026-10-21): colliders

- **Measured first** (`engine_physics_jolt` tests, `characters.rs`): two floor boxes side by
  side as hulls, the seam at x = 0; a ball of 0.25 m rolled across at 3.8 m/s and a
  character walked across at 3 m/s; how far each strays at the seam from its height
  before it, and the ball's speed lost over 1.2 m (0.06 m/s of it its damping):

  | hulls | bodies | the ball smooths internal edges | ball | speed lost | character |
  |---|---|---|---:|---:|---:|
  | rounded 5 cm (Jolt's default) | two | no | 7.78 mm | 0.157 m/s | 3.25 mm |
  | sharp | two | no | 6.37 mm | 0.179 m/s | 3.79 mm |
  | rounded | one compound | no | 7.78 mm | 0.157 m/s | 2.72 mm |
  | rounded | one compound | yes | 4.38 mm | 0.095 m/s | 2.72 mm |
  | sharp | one compound | no | 6.37 mm | 0.179 m/s | 0.00 mm |
  | **sharp** | **one compound** | **yes** | **0.00 mm** | **0.060 m/s** | **0.00 mm** |

  The rounding is not the knock: the knock is the next hull's edge (a ghost contact),
  which Jolt's enhanced internal edge removal smooths only within one body, and rounding
  leaves a groove it cannot.
- **So:** a static body per brush group, its colliding pieces as sharp hulls (convex
  radius 0) in one compound, in the world (`ScenePhysics::sync_blockout`, the hulls'
  points from the pieces' exact corners); moving bodies set `BodyDesc::internal_edges`.
  `engine_physics` gained `ShapeDesc::ConvexHull::convex_radius` (`HULL_RADIUS`, 5 cm, for
  everything else, as before) and `BodyDesc::internal_edges` (Jolt's
  `SetEnhancedInternalEdgeRemoval`).
- **`Blockout`:** `groups()` (each group's epoch, bumped whenever its pieces may have
  changed), `hulls(group)` (the pieces of brushes that collide), `collider_edges` (for the
  overlay).
- **When:** the game calls `sync_blockout` from its tick; a group is made again when its
  epoch or its place changes, and its body goes with it. The editor makes none (it runs
  no physics while editing): **View: Colliders** draws the pieces' edges, made again when
  a group changes.
- **Tested:** in the player on both GPUs, a ball rolled across the seam between two
  floor brushes stays within a micrometre of its height; a brush with `collides: false`
  is not hit by a ray; an edit makes the group's body again, and a group removed takes
  it. In the editor: View: Colliders on and off; looked at over two brushes.
- **Left for E2b-4b:** the navigation mesh, baked in the background from the visible
  surface, shown in the overlays.

## As built (E2b-4b, 2026-10-21): navigation

- **`Blockout::surface_triangles`:** the visible surface of every group's colliding
  brushes in the world, fans of its polygons (counter-clockwise from outside, as Recast
  wants them); the pieces' insides stay out.
- **`engine_nav::NavMesh::edges`:** the mesh's polygons' edges for drawing, each inner edge
  once and each marked a boundary or not (a new function of the shim, reading Detour's
  tiles).
- **The editor, View: Navigation:** a bake a third of a second after the brushes last
  changed, when no gesture runs, on a background thread and a pool of the editor's own
  (half the cores, at most four, so the player's pool keeps its frames); the edges drawn
  over the viewport, boundaries bright green; the status bar tells the tiles and the time.
- **Measured** (`engine_nav` tests, release, ref-1): 400 rooms of 10.4 × 8.4 m with walls
  and ceilings on a ground of 240 × 200 m, 16,802 triangles, 357 tiles: 612 ms on one
  thread, 299 ms on 2 workers, 158 ms on 4, 92 ms on 8.
- **Decided on that:** every bake bakes every tile. A large level shows its mesh within half
  a second of an edit (the wait and the bake), the editor never stops for it, and baking
  only the tiles an edit touched (ADR 0110's plan) waits until a level makes that half
  second felt.
- **Tested:** in `engine_nav`, a floor's edges on it, its rim a boundary all round, a
  wall's sides boundaries and inner edges between polygons; in the editor on both GPUs, a
  floor and a wall with a gap baked a moment after View: Navigation turns on, the mesh on
  the floor and its boundaries along both sides of the wall, then the wall moved and baked
  again, the boundaries with it; looked at.

## As built (E2b-5a, 2026-10-21): editing faces

- **`engine_editor::faces`:** a brush's faces in the world from its parts' solids (each
  face its part and its plane's place among the part's planes); the face a ray meets
  first (the nearest turned towards it, or from inside the one it leaves through).
  - **A move:** the face's plane offset along its normal (`Plane::offset`, exact for axis
    planes), the distance by the owner's snapping rules, refused when a part would be
    left empty.
  - **A pull:** a new brush between the face (as it shows: a room's wall's outside) and
    its plane moved out, its sides the planes through the face's edges across it (exact
    for a box's faces). Pushed in, a cut of the face's outline that deep, a millimetre
    proud of the face so it opens it.
  - **An edited brush is `BrushShape::Planes`:** a room's planes are its inside's, so it
    stays a room, the walls growing from them.
- **The editor:** Tab with the pointer over the viewport (over the panels Tab stays the
  fields') edits the selected brushes' faces. The gizmo and the side handles hide; the
  faces' edges show, the one under the pointer light blue. A drag moves a face, Alt+drag
  pulls or pushes; each is one undo step, Escape taking it back. Tab, Escape or a click on
  no face ends the editing. A pulled brush goes into the same group and is edited next too.
  Edited brushes show no side handles (their size is not their planes').
- **Tested:**
  - unit tests: a box's six faces and picks from outside and inside; a move along the
    normal, exact, refused through the base; the snapped travel; a room's face moving its
    inside and its wall; a pull out of a room starting at its wall's outside; a pull's
    volume and a push's cut;
  - in the editor: Tab, the top lit, a drag 1 m up making planes as one step and undo
    bringing the box back; Alt+drag pulling an extrusion into the group; a push making a
    cut; Escape ending it;
  - on both GPUs: Tab over a crate, its top lit, dragged up and meshed from its planes;
    looked at.
- **Left for E2b-5b:** edges and corners moved (the brush's corners' hull made again,
  corners that meet merged, so an edge moved onto another makes a wedge), Q, E, R and typed
  values on faces, Ctrl+E.
