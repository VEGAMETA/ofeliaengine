# Blockout: convex brushes held as planes, CSG, colliders and navigation

Checked: 2026-10-21 (every fact; sources at the end, cited as [G1] etc.). Input for the
blockout's ADR and phase E2b of [ADR 0107](../adr/0107-editor.md) §7. The tools' UX:
[ux/blockout.md](ux/blockout.md).

- **The phase** (ADR 0107 §7, the owner, 2026-10-17): brushes for greyboxing a level, like
  Godot's CSG: box, wedge and ramp, stairs, cylinder, sphere; add, subtract, intersect;
  face editing (extrude a face, move an edge); colliders and the navigation mesh made from
  them, so Play runs on the greybox at once; a grid material in world space; bake to a
  mesh. Our own Rust code over convex brushes held as planes, not Godot's C++ library.
- **How the sources were studied:**
  - read as code (permissive): Godot's CSG module (MIT), Chisel (MIT), func_godot (MIT),
    bevy_trenchbroom and jackdaw (MIT/Apache-2.0); Jolt (MIT) and Recast (Zlib) as vendored
    in `third_party/`; Manifold (Apache-2.0) through its design notes and Godot's use of it;
  - TrenchBroom (GPL-3.0) and ericw-tools (GPL-2.0): read for ideas only. **No code was
    carried over;**
  - Valve Hammer, Unity ProBuilder and Unreal Engine: studied as products (public docs).
    No engine code was read. The Valve Developer Community wiki returned 503 when fetched;
    what it says is known from search snippets only;
  - papers: as listed, full text where noted.
- Guesses are marked **(guess)**.

## 1. Brushes held as planes

### The Quake and Half-Life `.map` format

- **A face is a plane through three points** [T1]:
  `( 32 32 -3824 ) ( -32 32 -3824 ) ( -32 96 -3824 ) mmetal1_2 0 0 0 1 1`. The order of the
  points orients the plane. The brush is the intersection of the half-spaces behind its
  planes: "all brushes are automatically convex".
- **Vertices, edges and faces are not stored.** Editors and compilers compute them ("the
  brush geometry", "the same method as the BSP compilers") [T1].
- **Texture axes:**
  - the Standard format gives a name, two offsets, a rotation and two scales; the axes come
    from the world axis plane nearest the face's normal (TrenchBroom calls it *paraxial*)
    [T2];
  - the Valve 220 format writes them per face:
    `[ ux uy uz offset ] [ vx vy vz offset ] rotation scale_u scale_v` [E3] (TrenchBroom's
    *parallel* [T2]), so a texture can follow a slanted face.
- **Precision:** the points in the example are integers. TrenchBroom offers circles whose
  vertices stay on the grid (12, 24, 48 or 96 sides), because "Quake-like map compilers
  ... usually handle geometry better when it is aligned on the grid" [T1].

### TrenchBroom (v2026.2, 2026-08-21; master `29ef55ae`, 2026-10-19): ideas only (GPL-3.0)

- **Geometry from planes** [T3]: a polyhedron as large as the world bounds is clipped by
  each face's plane in turn. A plane that cuts nothing gets no face and is dropped. Then
  vertex positions are corrected and short edges healed. Epsilons [T5]: 0.0001 for a point's
  side of a plane ("this is what tyrbsp uses"), 0.001 for correcting values ("this is what
  QBSP uses").
- **Editing** [T1][T3][T4]:
  - *extrude:* a face moves along its normal, snapped to the grid distance or to grid
    planes. A move that would make another face vanish is refused. With Ctrl the brush is
    split instead of grown; *stamping* makes a new brush from a face and its moved copy;
  - *vertex, edge and face tools:* the brush becomes the convex hull of its moved vertex
    set. A vertex pushed inside is deleted; faces are cut into triangles to stay convex.
    After a move, a face's plane is taken from three of its vertices; with UV lock on, the
    texture keeps its place;
  - *clip:* a brush split by a plane through two or three points.
- **CSG** [T1][T6]:
  - *convex merge:* the hull of the selected brushes' vertices;
  - *subtract:* each minuend is cut into convex brushes by the subtrahend's planes: the part
    in front of each plane is a piece, the rest goes on to the next plane. The subtrahend is
    first clipped to the minuend ("might result in better quality subtractions"), and the
    planes nearest the x, then y, then z axis are used first. The manual's arch comes out as
    eight brushes;
  - *hollow:* "a shortcut for subtracting a smaller version of a brush from itself", walls as
    thick as the grid;
  - *intersect:* one brush with all the inputs' planes;
  - a new face takes the material of an input face on the same plane, else the current one.
- **Shapes** [T1]: cuboid, stairs, arch, cylinder (possibly hollow), cone, UV sphere,
  icosahedral sphere; circles aligned by edges, by vertices, or "scalable" (on the grid).
- **Pros:** brushes stay convex by construction; one representation for drawing, editing and
  the compiler. **Cons:** float planes and epsilons; the manual warns that vertex editing "can
  sometimes create invalid brushes and microleaks"; CSG results are many brushes, no longer
  editable as one shape.

### ericw-tools qbsp (2.0.0-alpha11, 2026-06-05; main `36eec1da`, 2026-09-12): ideas only (GPL-2.0)

- **Planes** [E1]: the normal is the normalized cross product of two edges of the three
  points, in doubles. Near-equal planes share one entry (`add_or_find_plane`: normals within
  1e-6, distances within 1e-4). A normal with a component of exactly ±1 gets its other
  components zeroed, and every plane gets a type by its nearest axis, for fast tests.
- **CSG of faces** (`CSGFaces`, `ClipInside` [E2]): each brush's faces are clipped by every
  overlapping brush of the same contents; the parts inside are dropped. A face exactly on a
  clipping plane: opposite-facing ones are always clipped off (touching brushes); a
  same-facing one is kept for one brush only, by precedence. A BSP is then built from the
  surviving faces.
- **Epsilons** [E1]: "The quality of the bsp output is highly sensitive to these epsilon
  values": on a plane 0.1, equal points 0.05, normals 1e-6, distances 1e-4. Vertices are
  welded through a hash at 0.05; T-junctions are fixed in a pass of their own (`tjunc.cc`).
- **Collision:** bevel planes are added to brushes "to allow the brush to be expanded against
  axial bounding boxes" (box traces against brush planes) [E1].

### Valve Hammer and VMF (products; the wiki unreachable)

- **A side** [V1]: three plane points, a material, `uaxis` and `vaxis` (an axis, an offset and
  a scale, 0.25 by default), a rotation, a lightmap scale, smoothing groups, and
  displacement data (left aside here).
- **Carve** (search snippets of [V2]) subtracts the selected brush from every brush it
  touches. The wiki advises against it: it "will produce messy geometry, including invalid
  faces, invalid brush volumes, and possibly corrupted brushwork"; intricate shapes should
  be a static mesh or a displacement. Hollow turns a brush into walls of a given thickness
  **(product knowledge; not verified on 2026-10-21)**.
- **Source 2's Hammer left brushes:** construction is in faces, edges and vertices of meshes;
  "Source 2 no longer uses BSP brushes" (search snippets of [V3]).
- **Collision in Source 1:** convex solids made from the world brushes **(guess, from a
  search snippet of a VBSP code mirror: "shrink BSP brushes by this much for collision";
  the code's license is not open, so it was not read)**.

### Godot and Bevy ports of the `.map` way

- **func_godot** (MIT; 2025.12, main `267ed438`) [F1]:
  - a face's polygon: a square on the plane (512 units by default) clipped by every other
    plane of the brush, vertices snapped to an epsilon;
  - faces covered by an opposite face of another brush are culled;
  - collision by default "a Convex CollisionShape3D for each brush"
    (`Geometry3D.compute_convex_mesh_points(planes)`, margin 0.04), or one concave shape;
  - tool textures: clip, skip, origin.
- **bevy_trenchbroom** (MIT OR Apache-2.0; 0.14.0, 2026-08-13; main `408bde57`) [B1]:
  vertices from every triple of planes, intersected in f64 and kept if inside all planes
  (an O(n⁴) loop); duplicates within 1e-4 merged; UVs from the map's texture settings (with
  Valve 220's axes when given); colliders: one convex hull per brush (a cuboid when it is
  one), or a triangle mesh per mesh.

## 2. CSG over meshes and over brushes

### Godot 4 (master `67906672`, 2026-10-20; 4.7.2-stable, 2026-08-18): mesh booleans on Manifold since 4.4

- **Nodes** [G1][G2]: `CSGBox3D`, `CSGCylinder3D` (and cones), `CSGSphere3D`, `CSGTorus3D`,
  `CSGPolygon3D` (a 2D outline extruded, spun or swept along a path), `CSGMesh3D`,
  `CSGCombiner3D`. Each has an operation: union, intersection or subtraction, "between this
  node and the previous child of this nodes parent" (ignored for the first child). A
  combiner hands its children's result to its parent.
- **Evaluation** (`CSGShape3D::_get_brush` [G3]):
  - a shape is a triangle soup (`CSGBrush`: per triangle three vertices, UVs, a smooth flag,
    an invert flag and a material);
  - the root packs its own triangles and each child's into Manifold meshes
    (`tolerance = 2 * FLT_EPSILON`, then `Merge()`), and runs `Manifold::BatchBoolean` over
    each run of children with the same operation; only the root draws;
  - a change marks the node and its parents dirty. The root's booleans run again; a clean
    child's result is reused.
- **What the game gets** [G1][G3]:
  - one mesh per root, a surface per material; UVs as each primitive makes them: a box gives
    each face the whole 0–1 square, so a texture stretches with the size (#19143, open since
    2018);
  - `use_collision`: a static body with a `ConcavePolygonShape3D` of the result's triangles;
  - navigation: a source-geometry parser adds the root's mesh to the bake (as mesh, or as a
    static collider when `use_collision` is on and the layer matches);
  - since 4.4: "Bake Mesh Instance" and "Bake Collision Shape", `bake_static_mesh()`,
    `bake_collision_shape()`, or the scene exported as glTF.
- **Before 4.4** (4.3 [G4]): its own boolean of triangle soups (a face BVH, intersection
  segments, faces re-triangulated in 2D, inside tests along rays), vertices snapped to
  `snap` (0.001 by default, "so that the faces of two meshes can be perfectly aligned").
  Reported: holes and missing triangles at some translations (#43755, #41140); holes with
  coplanar faces and a freeze while moving them (#58637).
- **The switch** (PR #94321, "Fix mesh corruption of CSG by using elalish/manifold", merged
  2024-11-29, milestone 4.4 [G5]): the proposal (#9711 [G6]) says "CSGMesh corrupts
  trivially" and quotes Manifold's promise: "as long as all the input meshes are manifold,
  they will stay manifold". `snap` is deprecated: "The CSG library no longer uses snapping."
  Manifold 3.5.2 is vendored [G7].
- **What people report:**
  - the docs: "CSG nodes are only intended for prototyping as they have a significant CPU
    performance cost"; "Moving a CSG node within another CSG node also has a significant
    CPU cost" [G1][G2]; the tutorial advises separate trees for separate objects [G8];
  - #4448 (2022, open): "the CSG mesh is regenerated every time a change is made. This can
    occur every frame when dragging"; it asks for a debounce;
  - #15484 (2026-09, open): CSG nodes lock the main thread;
  - #119543 (4.7 beta 2): moving CSG children at run time fell to 2 frames per second; fixed
    by #119551 (2026-05-19, in autosmoothing);
  - since the switch, non-manifold inputs (planes, quads) fail ("Manifold creation from mesh
    failed"), tracked in #100164;
  - #98189 (open): "collision tests starting inside CSG shapes do not show a result";
  - open proposals for what is missing: stairs (#2166, 2021), per-face materials (#937), a
    face editor (#14464, 2026; groundwork in PR #118630, open), TrenchBroom's convex merge
    (#8688), UVs that follow the size (#14133), colliders per surface (#14751).
- **Pros:** any closed mesh is an input; booleans hold since 4.4. **Cons:** the whole tree's
  booleans run on the main thread at every change; collision is a hollow triangle mesh; no
  face editing, stairs or world-space UVs; inputs must be manifold.

### Manifold (v3.5.4, 2026-09-25; master `d056b8fb`; Apache-2.0)

- **The boolean** [M1]: from Julian Smith's dissertation (Smith and Dodgson [P8]). It accepts
  floating-point error: each computation builds on earlier ones so that "the same question
  is never asked in two different ways". Symbolic perturbation breaks exact ties: for a
  union the first mesh moves out along its normal, the other way for difference and
  intersection. Touching cubes merge; a mesh minus itself is empty.
- **Guarantees:** manifold output from manifold input. It "does not account for any
  tolerance"; geometric validity is not guaranteed; degenerate triangles may remain.
- **Properties:** float channels per vertex, interpolated; an `originalID` per input and a
  `faceID` per triangle, which Godot uses for materials.

### Unreal Engine 5 (docs 5.8): BSP brushes, then Modeling Mode

- **Geometry brushes** [U1]: additive ("solid, filled-in spaces") and subtractive ("a hollow,
  carved-out space"); "The order in which Brushes are placed is critical"; solid,
  semi-solid (blocks, does not cut) and non-solid. Primitives: box, cone, cylinder, curved,
  linear and spiral staircases, sphere. A brush converts to a static mesh. The docs: "not
  recommended as a final method of level design", and "If you don't set the edges or corners
  of Brushes on the grid, errors can occur."
- **Modeling Mode** (Beta) [U2][U3]: the CubeGrid tool "creates blockout meshes using a
  repositionable grid" (faces pushed and pulled by grid cells, a corner mode); PolyGroup
  editing (extrude, push-pull); results become static or dynamic meshes.
- **BSP collision** comes from the BSP tree **(guess; not in the docs read)**.

### Unity ProBuilder (docs 6.0.9)

- **Meshes, not brushes** [Y1]: cube, sphere, plane, cylinder, cone, prism, stairs, torus,
  pipe, arch, door, sprite; "Edit Shape" reopens a shape's parameters.
- **UVs** [Y2]: tiling, offset, rotation, anchor and fill; "World Space" aligns UVs to the
  world: "Textures on same-angle faces always line up, but UVs do not stay put when you move
  the object."
- **Booleans** [Y3]: experimental; union, subtraction and intersection of two meshes into a
  new one, the sources kept. Built on pb_CSG, a C# port of csg.js by ProBuilder's original
  author (MIT) **(guess; the docs do not say)**.

### RealtimeCSG and Chisel (Sander van Rossen; MIT)

- **RealtimeCSG** [C1]: a CSG level editor for Unity whose CSG core is a native binary: the
  C++ code "will not be available to the public".
- **Chisel** (com.chisel `1a404288`, 2026-10-14; C# on Unity's Burst jobs) [C2][C3]:
  - non-destructive: brushes and generators ("if you create stairs, these will remain
    stairs") in a tree with additive, subtractive and intersecting nodes;
  - the surface per brush: its faces are categorized against the brushes that touch it as
    inside, aligned, reverse aligned or outside, through a routing table made from the tree.
    Brushes flagged as changed are rebuilt with those they touch (read in outline);
  - an exact path, added on 2026-10-12 in one commit (`17214c41`): planes quantized to
    integers (the unit normal times 2^20, offsets in steps of 1/2^14, |d| ≤ 2^24), vertices
    as homogeneous Int128 coordinates of three planes, side tests filtered in doubles and
    exact in 192-bit integers ("the sum is below 2^157");
  - colliders: Unity `MeshCollider`s of the result;
  - known issues: "some cases where triangulation fails", "some cases where overlapping
    surfaces create overlapping polygons".
- The talk: "Geometry in Milliseconds: Real-Time Constructive Solid Geometry", GDC [C4].
- Another Unity brush tool, SabreCSG (MIT since 2016), was not studied.

### jackdaw (a Bevy level editor; Apache-2.0, crates MIT OR Apache-2.0; 0.19.0-rc.10, 2026-10-11)

- Brushes began as planes. Since PR #253 (2026-05-17) they are concave, edited as half-edge
  meshes. On 2026-10-12 its booleans moved to manifold-rust (§4), and polygon edges are
  split at T-junctions after a boolean [J1]. Why it left convex brushes is not written
  **(guess: freer modelling)**.

### Summary

| | Brushes | CSG | Precision | Collision | Navigation input |
|---|---|---|---|---|---|
| qbsp (Quake) | convex, planes | faces clipped by brushes, then a BSP | doubles, epsilons, plane hashing | brushes with bevels (Quake 2 on) | — |
| TrenchBroom | convex, 3 points a plane | destructive, convex pieces | doubles, epsilons, correction | the compiler's | — |
| Hammer (Source 1) | convex, 3 points a side | destructive carve | floats | convex solids **(guess)** | — |
| func_godot, bevy_trenchbroom | `.map` brushes | none (a compiler's job) | doubles, epsilon | convex per brush | Godot's bake / **(not studied)** |
| Godot 4.4+ | triangle meshes per node | ordered tree, Manifold | symbolic perturbation | triangle mesh | the result's mesh |
| Unreal BSP | brushes in order | BSP | floats, grid advised | BSP **(guess)** | **(not studied)** |
| ProBuilder | meshes | experimental, csg.js **(guess)** | floats | **(not studied)** | Unity's |
| Chisel | convex, tree | per-brush categories; exact path | exact integer planes (new) | triangle mesh | Unity's |
| jackdaw | concave meshes | manifold-rust | Manifold's | **(not studied)** | **(not studied)** |

## 3. Exact plane-based geometry

- **CSG on BSP trees:** Thibault and Naylor 1987 [P1]; Naylor, Amanatides and Thibault 1990
  [P2] (booleans as merges of trees).
- **Boundary evaluation by classification** (Requicha and Voelcker 1985 [P3]): the faces of
  each input are split by the others, each piece classified (in, out, on with either
  orientation) and kept or dropped. qbsp's `CSGFaces` and Chisel's categories do this.
- **Plane-based representation:** a vertex is the meeting point of three input planes and is
  never stored as rounded coordinates. Every decision is the sign of a determinant of input
  planes, so fixed-size integers can make it exact.
  - Bernstein and Fussell 2009 [P4] (abstract only): BSP booleans on four predicates, a
    convex polygon constructor and a splitter; 16–28× faster than CGAL's Nef polyhedra on
    iterated work, half the speed of Maya's non-robust booleans.
  - Campen and Kobbelt 2010 [P5] (abstract only): plane-based BSPs only in the octree cells
    where intersections occur; exact with fixed precision after quantizing the input.
  - Nehring-Wirxel, Trettner and Kobbelt 2021 [P6] (read: §3.2, §4, Table 1):
    - a point (planes p, q, r) against a plane s is `sign|p q r s| · sign|n_p n_q n_r|`; kept
      as homogeneous coordinates (four 3×3 determinants), a classification is one 4D dot
      product;
    - exact and overflow-free when `48 · n+⁴ · v+ ≤ 2^(b−1)` (n+ the largest normal
      component, v+ the largest coordinate, b the integer width). With planes made from
      mesh vertices (n+ = 8 v+²), 256 bits hold coordinates up to 8.73 × 10⁷ (26 bits);
    - measured (i7-7700K, one thread, CPU cycles): intersecting three planes 23 / 160 / 402,
      classifying a vertex 13 / 103 / 142 with 128 / 192 / 256-bit integers; gmp and CGAL's
      lazy exact numbers took hundreds to thousands;
    - "all results are exact and no epsilon tolerance is required"; up to 2.5 million
      mesh–plane cuts a second on one core.
  - EMBER (Trettner, Nehring-Wirxel and Kobbelt 2022 [P7], summary only): mesh booleans,
    plane-based with homogeneous integer coordinates.
- **Floats with consistent topology:** Smith and Dodgson 2007 [P8], Manifold's basis.
- **Exact rationals:** Zhou, Grinspun, Zorin and Jacobson 2016 [P9] (mesh arrangements),
  manifold-rust's second engine.
- **Filtered point predicates:** Shewchuk 1997 [P10] (the `robust` crate). They take points,
  not planes.

### The budget for brushes (own arithmetic from [P6]'s bound)

- Planes with integer normals (|a|, |b|, |c| ≤ n+) and integer offsets, in grid units of the
  brush group's own space. Two design points fit Rust's `i128`, every product and partial
  sum included:

| | n+ | v+ (grid units) | 48 · n+⁴ · v+ | at a 1 mm grid |
|---|---|---|---|---|
| A | 2^22 | 2^26 | 2^119.6 | ±67 km per group; a rounded normal within about 2·10⁻⁷ rad |
| B | 2^24 | 2^24 | 2^125.6 | ±16.8 km per group; about 5·10⁻⁸ rad |

- A plane is 3 × `i32` + `i64`: 20 bytes, as [P6]'s 128-bit planes. A vertex (A) is four
  integers below 2^96.
- **Equal points** are found by incidence: v₁ equals v₂ when it lies on v₂'s three planes
  (three classifications in `i128`). Cross-multiplying coordinates would need about 165
  bits.
- **Order along an edge** (for T-junctions): the side of the other point against a third
  plane, times the sign of that plane's normal along the edge's direction (a triple product
  of normals). Chisel has the same test.
- **A rounded normal** anchored at a grid vertex tilts the plane by at most about 2·10⁻⁷ rad
  (A): 20 µm over a 100 m face. An f32 coordinate is about 1 mm apart at 8 km, so the output
  cannot show it.

## 4. Rust crates (crates.io, 2026-10-21)

| Crate | Version, date | License | What | For us |
|---|---|---|---|---|
| manifold-rust | 0.18.0, 2026-10-20 (first 2026-07-05) | Apache-2.0 | Manifold 3.5.0 ported ("exact numerical match", it says) plus an exact rational engine (Zhou et al.) | depends on clipper2-rust 1.2 (BSL-1.0, not on our list); young; mesh booleans |
| manifold-csg | 0.4.1, 2026-09-09 | MIT OR Apache-2.0 | bindings to Manifold's C++ | the C++ library the team declined |
| boolmesh | 0.2.1, 2026-10-11 | MPL-2.0 | a Manifold port | MPL needs an ADR |
| csgrs | 0.20.1, 2025-07-24 | MIT | csg.js-style BSP of polygons, epsilon 1e-8 (f64) or 1e-4 (f32); its main branch moved to exact scalars, unreleased | decides by epsilon; pulls nalgebra and geo |
| parry3d | 0.31.1, 2026-09-18 | Apache-2.0 | convex hulls, mesh intersection with tolerances, V-HACD | we have QuickHull (ADR 0101) |
| robust | 1.2.0, 2025-05-10 | MIT OR Apache-2.0 | Shewchuk's point predicates | planes need their own |
| geometry-predicates | 0.3.0, 2021-02-10 | MIT | the same | unmaintained |
| chull | 0.2.4, 2020-12-08 | MIT/Apache-2.0 | convex hull | unmaintained |
| quickhull | 0.1.0, 2026-09-11 | MIT OR Apache-2.0 | QuickHull | new; ours exists |
| truck-shapeops | 0.4.0, 2024-09-20 | Apache-2.0 | B-rep booleans | a CAD kernel |
| fj-core (Fornjot) | 0.49.0, 2024-03-21 | 0BSD | a CAD kernel | 0BSD not on the list as written; stale |
| bevy_trenchbroom | 0.14.0, 2026-08-13 | MIT OR Apache-2.0 | `.map` brushes to meshes and convex colliders | a reference; tied to Bevy |
| quake-util, shalrath | 0.4.0 (2025-05-10), 0.2.6 (2024-04-02) | CC0/MIT/Apache-2.0; MIT/Apache-2.0 | `.map` parsers | a `.map` import, later |
| i_overlay | 9.1.0, 2026-10-14 | MIT OR Apache-2.0 | 2D polygon booleans | convex faces do not need it |

- **None does convex CSG over planes with exact integer predicates.** The arithmetic is small
  (3×3 and 4×4 determinants on `i128`) and ours to write, like QuickHull was (ADR 0101).

## 5. From brushes to what a game needs

### Render meshes and UVs

- **Triangles:** what survives of a brush face is a set of convex polygons, each a fan of
  triangles. An edge must be split where a neighbour's vertex lies on it (a T-junction), or
  cracks show once coordinates are rounded: qbsp has a pass for it [E1], jackdaw added one on
  2026-10-12 [J1].
- **UVs:**
  - Quake's Standard format, TrenchBroom's paraxial mode, ProBuilder's World Space: a
    projection on the world axis plane nearest the normal; neighbouring faces line up, and
    the texture stays in the world when a brush moves; slanted faces stretch (by √2 at 45°)
    [T1][T2][Y2];
  - Valve 220 and TrenchBroom's parallel mode: axes in the face's plane, no stretch [E3];
  - Godot's CSG: per primitive, the whole 0–1 square on each box face [G3];
  - texture lock (TrenchBroom's UV lock and alignment lock): UVs move with the brush.
- **A grid** (Golus, "The Best Darn Grid Shader (Yet)", 2023-10-20 [K1]): a texture with
  rotated-grid supersampling and 16× anisotropic filtering is "basically as good as a simple
  texture based grid can look"; a procedural grid with lines in world units, antialiased
  from screen-space derivatives ("phone-wire AA"), its moiré suppressed, looks better.
  Godot's tutorial points to triplanar mapping, "relatively slow" [G8].
- **Smoothing:** flat faces by default; VMF has smoothing groups [V1], Godot an angle
  (`autosmooth`, `smoothing_angle`) [G1].

### Collision

- **A convex shape per brush** (func_godot's default, bevy_trenchbroom; Quake 2 and later
  trace boxes against brush planes): solid; inside and outside are known.
- **A triangle mesh of the result** (Godot, Chisel): hollow; Godot's #98189 shows casts
  starting inside finding nothing.
- **Jolt** (v5.6.0, vendored):
  - `ConvexHullShape` takes points, at most 256. Its convex radius is "subtracted so the
    total size will not grow"; corners round within `mMaxErrorConvexRadius` (0.05 m); without
    a radius, "collision detection on penetrating shapes is much more expensive" [H1]. Our
    backend passes 0.05 for every hull (`engine_physics_jolt/src/lib.rs`).
  - A mesh shape finds active edges by shared vertex indices: an edge with no neighbouring
    triangle is active [H2]. T-junctions and unwelded seams give ghost contacts.
  - `mEnhancedInternalEdgeRemoval` drops contacts with internal edges ("ghost collisions")
    among all the faces a query returns, after Terdiman's method [H3]. Jolt's comment names
    meshes; that it covers the hulls of a compound the same way is **(a guess from the
    collector's design)**. A `CharacterVirtual` applies it to its whole query,
    across bodies (`CollideShapeWithInternalEdgeRemoval`); our characters turn it on
    (`engine_physics_jolt/csrc/ext.cpp`). Between rigid bodies it works per pair, when
    either body has the flag, so only across the seams within one body's shape.

### Navigation input

- **Recast sees surfaces, not solids.** A rasterized span that overlaps another merges with
  it, and the merged span takes the higher area id when their tops are within the climb
  height [R1] (`addSpan`). So a walkable floor face that runs on under a solid merges with the
  solid's bottom face and stays walkable, and the space up to the solid's top counts as
  clearance. Inside a solid taller than the agent (1.8 m), what is left after the erosion
  (0.4 m from each side) and is larger than the smallest region (8 × 8 cells of 0.15 m,
  about 1.4 m², unless it touches a tile's border) stays as an unreachable island: a 3 × 3 m
  block standing on a floor would hold one **(reasoning from Recast's source and our bake's
  settings [R2]; not tested)**.
- So the input should be the result's visible surface, hidden faces removed, as Godot's
  parser does with the CSG mesh [G3]. Each brush's own hull faces would leave such islands
  where brushes overlap or stand on each other.

### Bake to a mesh

- Godot: a `MeshInstance3D` with an `ArrayMesh` per root, a `ConcavePolygonShape3D`, or a glTF
  export [G1][G8]. Unreal: a brush converted to a static mesh [U1]. ProBuilder's shapes are
  meshes already. TrenchBroom writes the `.map` for a compiler.

## 6. What fits us

- **The GPU scene draws meshes from the geometry pool** (ADR 0039, 0064):
  - a brush's output is plain `MeshData`: positions, normals, UVs, indices; tangents may be
    left empty; no meshlets or detail levels are needed;
  - `add_mesh` copies through staging in the next frame and `remove_mesh` frees after the
    frames in flight: a changed brush swaps its mesh;
  - a mesh per brush keeps an edit local, and ADR 0109's picks return the brush's slot. The
    face under the pointer is then a ray against the brush's planes on the CPU, exact and
    cheap;
  - each mesh has its own bottom-level acceleration structure, and the top level's rebuild
    grows with instances: 103 / 1,030 / 10,300 instances cost 85 / 116 / 181 µs on the RTX
    and 231 / 507 / 3,798 µs on the iGPU (ADR 0087). A level of thousands of brushes may want
    its meshes merged by area in the cook **(guess: to measure)**.
- **Materials** (ADR 0092): a grid can be a material type over `ctx.world_position` and
  `ctx.normal`; only its own pipelines pay for it (the register rule); GI draws a type with
  its glTF fallback. A texture grid works on the built-in material with world-space UVs and
  anisotropic filtering (ADR 0098).
- **Physics** (ADR 0066, 0101): `ShapeDesc::ConvexHull { points }` and static compounds
  exist; `engine_collision` keeps hulls as points with stable ids (QuickHull in f64, planes
  from a hull); the cook bakes Jolt's binary form into packs. A convex piece of a brush is a
  hull with exactly known vertices: no hull search is needed. Making a hull in Jolt took
  7–356 µs, restoring it 0.5–3.8 µs (ADR 0101).
- **Navigation** (ADR 0103): `engine_nav::bake(vertices, triangles, settings, jobs)` takes
  triangles. The cook takes level geometry from static colliders today, hulls as their
  faces: brush hulls would bring their hidden faces. Sponza's 262,267 triangles bake in
  24–30 ms, so a greybox can be baked when Play starts **(guess for a greybox's size)**.
- **Characters** (ADR 0066) climb steps: its test walks up eight of 0.25 m, so stairs
  can be real steps.
- **The editor** (ADR 0107):
  - brushes as components of plain data undo as bytes (ADR 0031);
  - the owner's snapping steps (0.01 to 5 m, E2-6) all sit on a 1 mm grid;
  - the gizmo, keyboard transforms and surface drops move entities;
  - scenes are text (ADR 0108), and integers round-trip exactly;
  - the budgets: input answered within the frame, no allocations per frame while idle.
- **Determinism:** with exact integer predicates the meshes, hulls and navigation are the same
  on every machine and thread count.

## What we take

1. **Convex brushes held as planes** (Quake, Hammer, TrenchBroom): a brush is a list of faces,
   each a plane, a material and UV settings. Vertices, edges and polygons are derived, by
   clipping a large box with each plane in turn (TrenchBroom, qbsp, func_godot), not from
   every triple of planes (O(n⁴), bevy_trenchbroom). Any set of planes is convex, so an edit
   can fail only by leaving a brush empty or open (not closed within the bounds), and is then
   refused (TrenchBroom; Chisel leaves such a brush out).
2. **Exact plane-based predicates** (Bernstein and Fussell, Campen and Kobbelt, Nehring-Wirxel
   et al., Chisel's new path): integer planes within §3's bounds (design point A, unless the
   ADR's measurements say B), `i128` arithmetic, no epsilon in any decision. A vertex is
   always three input planes; coordinates are made only for output, as f32. Rounding
   happens only where the user edits: a rotated plane's normal is rounded and anchored at a
   grid vertex.
3. **Godot's CSG tree** (each brush's operation against its earlier siblings; groups like
   `CSGCombiner3D`), evaluated over convex pieces, non-destructively:
   - the solid's pieces: each additive brush cut by the subtractive brushes after it
     (TrenchBroom's subtraction, axis planes first) and clipped by the intersecting brushes
     after it;
   - the surface: each piece's faces clipped by the other pieces and kept outside them;
     qbsp's rule on a shared plane (opposite faces both go; equal faces stay once, by order);
   - a surface piece takes the material and UV settings of the brush face whose plane it
     lies on (TrenchBroom's rule, exact here);
   - only what a changed brush's bounds touch is redone (Chisel), in parallel on the job
     system.
   Chisel's routing tables over the tree are the alternative, for the ADR to weigh.
4. **Colliders: the pieces as hulls** (func_godot, bevy_trenchbroom), one static compound body
   per group with Jolt's enhanced internal edge removal on, which then applies to every body
   touching it. Whether that hides the seams between pieces, and which convex radius to use
   (Jolt's 0.05 m rounds edges), is to be measured.
5. **Navigation input: the visible surface** (Godot); the hulls stay out of the bake.
6. **Tools as commands** (TrenchBroom): extrude a face (its plane's offset; refused when a face
   would vanish; a split as the option), move vertices or edges (the hull of the moved
   vertices, planes through them), clip, convex merge, hollow (a shrunk subtractive copy, so
   it stays editable), and subtraction into plain pieces only on request.
7. **Primitives** (Unreal, TrenchBroom, ProBuilder, Chisel): box; wedge and ramp (five planes);
   cylinder (an n-sided prism, TrenchBroom's grid-aligned circles as an option); UV and
   icosahedral spheres (one convex brush each); stairs as a generator of step brushes. A
   primitive keeps its parameters until a face edit makes it a plain brush (ProBuilder's
   Edit Shape, Chisel's generators, ADR 0101's generated and edited colliders).
8. **UVs in world space by default** (Quake's paraxial projection, ProBuilder's World Space),
   with offset, scale and rotation per face, face-plane axes (Valve 220) and texture lock as
   options. **A grid material type** with procedural, antialiased lines in world units
   (Golus).
9. **Brush roles** after TrenchBroom's and func_godot's tool textures: drawn, colliding,
   walkable, trigger.
10. **Bake to a mesh** (Godot, Unreal): the group's surface written as a glTF, its pieces as
    `UCX_` meshes, so ADR 0101's cook keeps them as convex colliders **(an idea; the format is
    the ADR's)**.

## What we do not take

- **Mesh booleans** (Godot's Manifold, ProBuilder, Unreal's Modeling Mode, jackdaw): the team
  declined Godot's C++ library; manifold-rust brings BSL-1.0 code; csg.js-style BSPs (csgrs)
  decide by epsilon. They give a hollow triangle surface, not convex pieces, and nothing to
  edit as planes.
- **A BSP tree of the whole level** (qbsp, Unreal's BSP, Naylor et al., Nehring-Wirxel's
  octree BSPs): Quake needs one for visibility and leaks. We need only the surface and the
  pieces, which stay local to an edit.
- **Float planes with epsilons** (qbsp, TrenchBroom, Hammer, func_godot, bevy_trenchbroom,
  Godot up to 4.3): the source of the invalid brushes, microleaks, holes and cracks they
  report.
- **Destructive carve as the subtraction** (Hammer, TrenchBroom): an explicit command at most;
  the default stays non-destructive.
- **A triangle mesh as a brush group's collider** (Godot, Chisel): hollow, and Jolt's active
  edges need welded vertices without T-junctions. Baked meshes keep ADR 0101's level rule.
- **Concave brushes edited as half-edge meshes** (jackdaw, ProBuilder, Source 2's Hammer):
  modelling tools; baked meshes could get such tools later.
- **Lightmap UVs** (Godot's #47766): our GI is probe-based (ADR 0084).
- **Godot's rebuild of the whole tree on the main thread.**

## Open questions for the ADR (to measure)

- The design point (A or B), the grid unit (1 mm or 0.1 mm), a group's extent.
- What a classification costs in Rust's `i128` ([P6]'s 13 cycles were C++ with its own
  128-bit type), and whether a double-precision filter (Chisel) pays.
- A greybox (for example 2,000 brushes) evaluated from nothing, and one brush dragged: the
  target is within the frame **(guess: under 1 ms for a local edit)**.
- The hulls' convex radius (0, 0.01 or 0.05 m): a ball rolled and a character walked across
  piece seams.
- A mesh per brush against meshes merged by area: draws and acceleration structures on both
  GPUs.
- Brushes in one scene-wide root, or only within groups (with the UX note).
- Slivers: exact arithmetic keeps pieces a micron thin where epsilons would merge them;
  physics should drop pieces below a size **(guess)**.

## Patents (searched briefly on 2026-10-21; not a legal opinion)

- **Nothing found on what we take.** Brush planes and face clipping (Quake's tools, the
  1990s), BSP set operations (1987, 1990) and boundary evaluation (1985) are far older than
  any patent term; the plane-based exact predicates are academic work (2009–2022) with no
  patent found.
- **Two active patents near the topic, on what we do not take** (status as Google Patents
  lists it):
  - US8814677B2 (Disney, now Adeia; to 2032-01-11): real-time BSP CSG as a game mechanic,
    paint and thinner thrown at geometry [X1];
  - US8854367B2 (Dassault Systèmes; to 2032-12-21): booleans of triangulated surfaces through
    triangle intersections and winding numbers [X2].

## Sources (all checked 2026-10-21)

Godot paths are at https://github.com/godotengine/godot/blob/679066724352c12673001d692d41dc2dab9314c0/
(MIT; master 2026-10-20; release 4.7.2-stable, 2026-08-18).

- [G1] `modules/csg/doc_classes/CSGShape3D.xml`, `CSGBox3D.xml`. [G2] `…/CSGCombiner3D.xml`,
  `CSGPolygon3D.xml`. [G3] `modules/csg/csg.h`, `csg.cpp`, `csg_shape.cpp` (`_get_brush`,
  `_pack_manifold`, `navmesh_parse_source_geometry`, `_get_brush_collision_faces`,
  `CSGBox3D::_build_brush`), `modules/csg/editor/csg_gizmos.cpp` (the bake menu).
  [G4] the same files at `4.3-stable`. [G7] `thirdparty/README.md`
  (Manifold 3.5.2, `11235e6b`).
- [G5] https://github.com/godotengine/godot/pull/94321 (merged 2024-11-29, milestone 4.4);
  issues #19143, #41140, #43755, #47766, #58637, #98189, #100164, #105619, #119543.
- [G6] https://github.com/godotengine/godot-proposals/issues/9711 ; proposals #937, #2166,
  #4448, #8688, #14133, #14464, #14751, #15484.
- [G8] https://docs.godotengine.org/en/stable/tutorials/3d/csg_tools.html (4.7 docs, "not yet
  updated for Godot 4.7").
- [M1] Manifold (Apache-2.0; v3.5.4, 2026-09-25; master `d056b8fb`, 2026-10-20): README;
  https://github.com/elalish/manifold/wiki/Manifold-Library .
- TrenchBroom (GPL-3.0, ideas only; v2026.2, 2026-08-21; master `29ef55ae`, 2026-10-19), at
  https://github.com/TrenchBroom/TrenchBroom/blob/29ef55ae42f841e61b63bd2282c6db814f1529ef/ :
  [T1] `app/TrenchBroom/resources/documentation/manual/index.md`;
  [T2] `lib/TbMdlLib/include/mdl/BrushFace.h`; [T3] `lib/TbMdlLib/src/Brush.cpp`;
  [T4] `lib/TbMdlLib/src/BrushFace.cpp`; [T5] `lib/VmLib/include/vm/constants.h`;
  [T6] `lib/TbMdlLib/include/mdl/Polyhedron_CSG.h`.
- ericw-tools (GPL-2.0, ideas only; 2.0.0-alpha11, 2026-06-05; main `36eec1da`, 2026-09-12),
  at https://github.com/ericwa/ericw-tools/blob/36eec1da2a194467e6baac2f444c2dd04b57d266/ :
  [E1] `include/common/mathlib.hh`, `qbsp/map.cc`, `qbsp/qbsp.cc`, `qbsp/tjunc.cc`;
  [E2] `qbsp/csg.cc`; [E3] `common/mapfile.cc` (`parse_valve_220`).
- Valve (proprietary products; developer.valvesoftware.com returned 503 on 2026-10-21):
  [V1] srctools 2.6.1 docs (MIT), https://srctools.readthedocs.io/en/v2.6.1/modules/vmf.html ;
  [V2] https://developer.valvesoftware.com/wiki/Reshaping_solids (search snippets);
  [V3] https://developer.valvesoftware.com/wiki/Source_2/Docs/Level_Design/Basic_Construction/Mesh_Editing_1 ,
  https://developer.valvesoftware.com/wiki/Mesh_entity (search snippets).
- [F1] func_godot (MIT; 2025.12; main `267ed438`, 2026-10-12): https://github.com/func-godot/func_godot_plugin ,
  `addons/func_godot/src/core/geometry_generator.gd`, `src/fgd/func_godot_fgd_solid_class.gd`.
- [B1] bevy_trenchbroom (MIT OR Apache-2.0; 0.14.0; main `408bde57`): https://github.com/Noxmore/bevy_trenchbroom ,
  `src/brush.rs`, `src/physics.rs`.
- [J1] jackdaw (Apache-2.0; crates MIT OR Apache-2.0; 0.19.0-rc.10; main `51930147`, 2026-10-21):
  https://github.com/jbuehler23/jackdaw , `crates/jackdaw_csg/src/lib.rs`, its history
  (2026-10-12), PR #253.
- Unreal Engine (proprietary; docs 5.8): [U1] https://dev.epicgames.com/documentation/en-us/unreal-engine/geometry-brush-actors-in-unreal-engine ;
  [U2] https://dev.epicgames.com/documentation/en-us/unreal-engine/modeling-mode-in-unreal-engine ;
  [U3] https://dev.epicgames.com/documentation/en-us/unreal-engine/cubegrid-tool-in-unreal-engine
  (search snippets).
- Unity ProBuilder (proprietary; docs 6.0.9): [Y1] https://docs.unity3d.com/Packages/com.unity.probuilder@6.0/manual/shape-tool.html ;
  [Y2] `…/manual/auto-uvs-actions.html`; [Y3] `…/manual/boolean.html`; pb_CSG (MIT),
  https://github.com/karl-/pb_CSG (README).
- [C1] RealtimeCSG (MIT, the Unity part only): https://github.com/LogicalError/realtime-CSG-for-unity (README).
- [C2] Chisel (MIT): https://github.com/RadicalCSG/com.chisel (`1a404288`, 2026-10-14):
  `Core/2.Processing/Categorization/CategoryIndex.cs`, `Core/2.Processing/Jobs/CreateRoutingTableJob.cs`,
  `…/FindModifiedBrushesJob.cs`, `Core/2.Processing/Exact/ExactPlane.cs`, `ExactInt.cs`,
  `ExactCSGJob.cs`, `Components/Components/Generated/ChiselColliderObjects.cs`; the exact path in
  commit `17214c41` (2026-10-12). [C3] https://github.com/RadicalCSG/Chisel.Prototype (README).
- [C4] S. van Rossen, "Geometry in Milliseconds: Real-Time Constructive Solid Geometry", GDC,
  https://www.youtube.com/watch?v=Iqmg4gblreo (title from YouTube's oEmbed; 2020 according to
  search snippets; not watched).
- Jolt (MIT; v5.6.0 in `third_party/JoltPhysics`): [H1] `Jolt/Physics/Collision/Shape/ConvexHullShape.h`,
  `ConvexShape.h`; [H2] `…/Shape/MeshShape.h`, `MeshShape.cpp` (`sFindActiveEdges`);
  [H3] `Jolt/Physics/Collision/InternalEdgeRemovingCollector.h`, `NarrowPhaseQuery.h`,
  `Jolt/Physics/Character/CharacterVirtual.cpp`, `Jolt/Physics/PhysicsSystem.cpp`,
  `Jolt/Physics/Body/BodyCreationSettings.h`.
- Recast (Zlib; `third_party/recastnavigation`, `9f4ce64`, ADR 0103): [R1]
  `Recast/Source/RecastRasterization.cpp` (`addSpan`), `Recast/Source/RecastRegion.cpp` (small
  regions kept when they touch a tile's border). [R2] ours: `crates/engine_nav/csrc/shim.cpp`
  (the bake's filters), `crates/engine_nav/src/lib.rs` (`BakeSettings`: `region_min_cells` 8).
- [K1] B. Golus, "The Best Darn Grid Shader (Yet)", 2023-10-20,
  https://bgolus.medium.com/the-best-darn-grid-shader-yet-727f9278b9d8 (read through the
  author's RSS feed; its code's license is not stated in the text read).
- Papers:
  - [P1] W. C. Thibault, B. F. Naylor, "Set operations on polyhedra using binary space
    partitioning trees", SIGGRAPH 1987, Computer Graphics 21(4).
  - [P2] B. Naylor, J. Amanatides, W. Thibault, "Merging BSP trees yields polyhedral set
    operations", SIGGRAPH 1990, Computer Graphics 24(4).
  - [P3] A. A. G. Requicha, H. B. Voelcker, "Boolean operations in solid modeling: boundary
    evaluation and merging algorithms", Proceedings of the IEEE 73(1), 1985, pp. 30–44,
    doi:10.1109/PROC.1985.13108.
  - [P4] G. Bernstein, D. Fussell, "Fast, Exact, Linear Booleans", Computer Graphics Forum
    28(5) (SGP 2009), pp. 1269–1278, doi:10.1111/j.1467-8659.2009.01504.x (abstract only).
  - [P5] M. Campen, L. Kobbelt, "Exact and Robust (Self-)Intersections for Polygonal
    Meshes", Computer Graphics Forum 29(2) (Eurographics 2010), pp. 397–406,
    doi:10.1111/j.1467-8659.2009.01609.x (abstract only).
  - [P6] J. Nehring-Wirxel, P. Trettner, L. Kobbelt, "Fast Exact Booleans for Iterated CSG
    using Octree-Embedded BSPs", Computer-Aided Design 135 (2021) 103015, arXiv:2103.02486.
  - [P7] P. Trettner, J. Nehring-Wirxel, L. Kobbelt, "EMBER: Exact Mesh Booleans via
    Efficient & Robust Local Arrangements", ACM Transactions on Graphics 41(4) (SIGGRAPH
    2022) (summary only).
  - [P8] J. M. Smith, N. A. Dodgson, "A topologically robust algorithm for Boolean
    operations on polyhedral shapes using approximate arithmetic", Computer-Aided Design
    39(2), 2007, pp. 149–163, doi:10.1016/j.cad.2006.11.003.
  - [P9] Q. Zhou, E. Grinspun, D. Zorin, A. Jacobson, "Mesh Arrangements for Solid Geometry",
    ACM Transactions on Graphics 35(4) (SIGGRAPH 2016).
  - [P10] J. R. Shewchuk, "Adaptive Precision Floating-Point Arithmetic and Fast Robust
    Geometric Predicates", Discrete & Computational Geometry 18(3), 1997.
- [X1] https://patents.google.com/patent/US8814677 . [X2] https://patents.google.com/patent/US8854367B2
  (the summary of the invention; the claims were not read).
- crates.io's API (user agent `ofeliaengine-research`) for §4's versions, dates, licenses and
  dependencies.
- Ours: ADRs 0031, 0039, 0064, 0066, 0084, 0087, 0092, 0098, 0101, 0103, 0107, 0108, 0109;
  `crates/engine_physics_jolt/src/lib.rs` (the hulls' convex radius),
  `crates/engine_physics_jolt/csrc/ext.cpp` (the characters' edge removal), `crates/engine_nav`.
