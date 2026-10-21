# Blockout: brushes for greyboxing a level

Checked: 2026-10-21. Products were studied as products: their manuals, release notes,
well-known tutorials and forum threads. TrenchBroom (GPL): its manual and key bindings
only, no code. Godot (MIT): the CSG gizmo's source was read to confirm its colors and
handles; no code was carried over. Input for [ADR 0107](../../adr/0107-editor.md) §7
(phase E2b) and [ADR 0110](../../adr/0110-blockout-brushes.md). The technique (planes, CSG, colliders, navigation):
[../blockout.md](../blockout.md).

The phase as the owner set it (ADR 0107 §7, 2026-10-17): brushes for greyboxing a level,
like Godot's CSG: box, wedge and ramp, stairs, cylinder, sphere; add, subtract, intersect;
face editing (extrude a face, move an edge); colliders and the navigation mesh made from
them, so Play runs on the greybox at once; a grid material in world space; bake to a
mesh. Convex brushes held as planes, in our own Rust.

CLAUDE.md §12: immediate feedback, keyboard-first, minimal modes, errors next to their
cause, nothing blocks the UI.

## How seven editors do it

### Making, sizing, operations

| Editor | Making a brush | Sizing it | Add, subtract, intersect |
|---|---|---|---|
| Godot 4.7 (CSG nodes) | a node from the add dialog: box, cylinder (or cone), sphere, torus, a polygon extruded, spun or swept along a path, a mesh | the inspector; a handle on each face: one face moves, the opposite stays (since 4.2; Alt moves both); snapped by the move step when snapping is on | a field per node: union, intersection, subtraction, applied to the earlier siblings in tree order (the first child's is ignored); a combiner groups; every shape's edges drawn in its operation's color (union blue, subtraction the inverse, intersection near-white), the selected one's faces translucent |
| TrenchBroom 2026.2 | the default tool: drag on a face or the grid in 3D; the height is one grid step (Alt changes it; Shift makes x = y, Shift+Alt a cube); shapes: cuboid, stairs (2026.1), arch (2026.2), cylinder, cone, two spheroids | Shift over a face outlines it, a drag moves it along its normal; grid 1–256 units, + and −; snapping on | commands that rewrite the brushes: convex merge (Ctrl+J), subtract (Ctrl+K: an arch becomes eight brushes), intersect (Ctrl+L), hollow (Ctrl+Shift+K: six brushes, walls one grid step thick) |
| Hammer (Source) and Hammer++ | block tool (Shift+B): drag a box in a 2D view, the height in another, Enter; block, wedge, cylinder, spike, sphere, arch, torus | handles in the 2D views; [ and ] change the grid; Shift+W toggles snapping, Alt frees a drag | carve (Ctrl+Shift+C) cuts the selection out of the brushes it touches; make hollow (Ctrl+H; a thickness is asked, 32 by default, negative for walls outside) |
| Unity ProBuilder 6.0 | New Shape (Ctrl+Shift+K): press and drag the base, release, move for the height, click; Shift shows a copy at the pointer, a click stamps it; cube, sphere, plane, cylinder, cone, prism, stairs, torus, pipe, arch, door | the Create Shape panel: X, Y, Z typed, the pivot at the first corner or the center; Unity's grid snapping, Ctrl by increments; the settings stay editable until the topology is edited | Boolean (experimental): a window, two meshes, an operation; Apply makes a new mesh, the inputs stay |
| Unreal 5.8, BSP brushes | dragged in from Place Actors > Geometry: box, cone, cylinder, linear, curved and spiral stairs, sphere | Brush Settings: X, Y, Z; Hollow with Wall Thickness | Brush Type: additive or subtractive; "The order in which Brushes are placed is critical"; additive edges bluish, subtractive orange (a forum post) |
| Unreal 5.8, Modeling Mode | box, sphere, cylinder, cone, torus, stairs, capsule and more; Extrude Path for walls and ramps; Cube Grid (beta): click and drag cells, E pulls, Q pushes, Ctrl+E and Ctrl+Q change the cell size, Z takes corners for slopes | tool properties; the grid's gizmo (R) | Boolean and Merge make new meshes |
| Blender 5.2 (reference) | the Add Cube tool: drag the base, release, move for the height, click; on the surface under the cursor, aligned to its normal | Ctrl snaps, Alt draws from the center, Shift keeps the aspect; then the Adjust Last Operation panel | the Boolean modifier, live: intersect, union, difference; solvers Exact (slow, sure), Float (fast), Manifold (fastest, closed meshes only) |

### Editing, playing, what works, what annoys

| Editor | Faces, edges, corners | To Play | What works | What annoys |
|---|---|---|---|---|
| Godot 4.7 | none: no editing of polygons, no UVs | `use_collision` on the root (off by default) makes a static triangle collider; a NavigationRegion3D reads CSG as meshes, baked by a button; since 4.4, Bake Mesh Instance and Bake Collision Shape | the tree shows the order; always editable; face handles | no face editing, no stairs; slow with big trees ("Keep CSG geometry relatively simple"; a frozen level starts about 4× faster, says CSG Blockout's author); collision and navigation set up by hand; the bake leaves the CSG visible; add-ons fill the gaps (FuncGodot, Cyclops, CSG Blockout) |
| TrenchBroom 2026.2 | vertex (V), edge (E), face (F) tools: handles, a lasso; moves that would make a brush concave are refused; corners that meet fuse; Ctrl at the start of a face drag makes a new brush instead (split); clip tool (C): two or three points, Enter cuts, Ctrl+Enter picks the side kept; UV lock (U) | compile profiles run outside tools (qbsp, vis, light), then launch the engine; leaks drawn from point files; navigation is the game's own | drawing on faces in 3D; Shift+drag on faces with no tool switch; shapes; repeating the last commands; an issue browser | compiling before playing; units in powers of two; vertex editing "can sometimes create invalid brushes and microleaks" (its manual); subtract leaves many fragments; three tools and many modifiers to remember |
| Hammer and Hammer++ | vertex tool (Shift+V), Ctrl+F splits a face; clipping (Shift+X: a line, Shift+X again picks the side, Enter); texture lock (Shift+L). Hammer++: the vertex tool updates live, invalid faces red; clipping by three points in 3D, the part cut away red. Source 2's Hammer moved to meshes: blocks drawn in 3D, Shift+move extrudes, keys 1 to 6 pick what a click takes, from vertices to groups | Run Map (F9): vbsp, vvis, vrad, then the game; a leak stops it | quick boxes; Enter to create; hollow rooms | three 2D views to draw in; carve leaves "extra and invalid brushes, rounding errors" and leaks (TWHL), so mappers avoid it; compile times |
| ProBuilder 6.0 | G toggles object and element editing, H cycles vertex, edge, face; Shift+drag extrudes, Ctrl+E too; loops (Alt+L), edge loops (Alt+U) | a mesh collider by default (4.x docs); static flags off by default; the navigation mesh baked by hand | the base-then-height gesture; typed sizes; stairs and doors; Shift+drag to extrude | the boolean outside the scene's flow; modes; forum reports of broken faces, crashes and slowness in levels of many small rooms |
| Unreal BSP | Brush Editing mode, "a very simplified 3D modeling application"; vertices kept on the grid | solid brushes block players; a NavMeshBoundsVolume builds the navigation mesh at once and after changes; P shows it in green; Create Static Mesh converts | stairs; hollow as a setting; collision and navigation at once; a world grid material by default | the order; "not recommended as a final method of level design" |
| Unreal Modeling Mode | PolyGroup Edit: extrude, bevel, edge loops; push and pull | collision from the project's settings; navigation as above | Cube Grid's push and pull from the keyboard | many tools, marked beta; each result an asset; new shapes came without simple collision after a default changed (UEFN 34.30, April 2025, a forum thread) |
| Blender 5.2 | Tab for edit mode; 1, 2, 3 for vertex, edge, face; E extrudes; G, R, S with axis keys and typed values | none: exported to an engine | typed values; live booleans | modes; booleans want closed meshes ("Only Manifold meshes are guaranteed to give proper results") |

Also looked at:
- **Roblox Studio:** Union (Ctrl+Shift+G), Negate (Ctrl+Shift+N; the part "becomes
  translucent with a reddish tint"), Intersect (Ctrl+Shift+I), and Separate (Ctrl+Shift+U)
  takes a union apart again. Play puts the avatar in at once. The navigation mesh is
  rebuilt by itself, and the viewport's visualization options show it.
- **SketchUp:** Push/Pull (P): click a face, move, click, or type the distance and Enter;
  a double-click on another face repeats it; a face pushed through a parallel one cuts a
  hole.
- **Godot add-ons (MIT),** which show what Godot users miss:
  - Cyclops Level Builder: blocks drawn base first, then the height; every block
    collides; a clip tool; merge, subtract and intersect commands; conversion to Godot
    meshes.
  - CSG Blockout 3.0 (2026-10-03, Godot 4.6+): box, room and cut tools ("drag a base on
    any surface, release, done"); rooms with a wall thickness; door and window presets
    aligned to a clicked wall; arrows that push one face while the opposite stays;
    dimension labels; a player reference and a jump check; Play From Here with a
    first-person test character; freeze into a mesh with collision as one undo step;
    world-aligned grid materials.

## Scenarios

Our proposal (below). A key or a chord counts one; each typed digit counts one. Godot's
counts are estimates.

| The developer wants to… | Ours | Keys | Godot 4.7 today |
|---|---|---|---|
| a room 10 × 8 m with walls 3 m high and a doorway 1 × 2.1 m | B, click a corner on the floor, 10 Tab 8 Enter, 3 Enter, Ctrl+H (hollow: 0.2 m walls outside the drawn space); Shift+B, click the doorway's corner on the wall, 1 Tab 2.1 Enter, Enter (through the wall) | 17 keys, 2 clicks | two CSGBox3D from the add dialog, sizes and the door's place typed in the inspector, Flip Faces, Subtraction, Use Collision: about 13 clicks and 40 keys; the walls have no thickness |
| stairs from the floor to a ledge 1.2 m up, 1 m wide over 1.8 m | B B B (stairs), click the floor at the foot, 1 Tab 1.8 Enter, 1.2 Enter: six steps of 0.2 m, rising away from the first click; the hint says the agent can climb them | 13 keys, 1 click | no stairs: a box per step, or a polygon drawn point by point |
| a window 1.2 × 1 m with its sill 1 m up | Shift+B, move over the wall until the label says 1 m above the floor, click, 1.2 Tab 1 Enter, Enter | 8 keys, 1 click | as the doorway: about 8 clicks and 25 keys |
| a ramp 2 m wide rising 1 m over 4 m | B B (wedge), click the foot's corner, 2 Tab 4 Enter, 1 Enter (14°); or a box, then Tab, click its top edge at the low end, Q Y −1 Enter, Tab | 8 keys, 1 click (from a box: 14 keys, 2 clicks) | a polygon of three points with a depth, or a box turned into the floor |
| the room 2 m longer at one end | click a wall, drag the end wall's handle until its label says 12 m; the other end stays | 1 click, 1 drag | the same, with the box's handle |
| a corridor 6 × 2 m, 2.5 m high, to a second room | B, click the floor at the first room's wall, 6 Tab 2 Enter, 2.5 Enter, Ctrl+H: the corridor opens into both rooms (question 2) | 10 keys, 1 click | a third box and two cuts |
| find the cut that made the doorway and widen it | click the doorway's side (the faces a cut makes belong to it), drag its side's handle | 1 click, 1 drag | click its edges, or find it in the tree |
| see where an agent can walk | Navigation in the viewport's overlay menu; it follows each edit a moment later | 2 clicks | a NavigationRegion3D with a NavigationMesh, then Bake, again after every change |
| walk it | F5 (E3): its colliders and navigation mesh exist already; Shift+F5 from the camera, with a test walker if the game has no player yet (question 3) | 1 | a player scene of one's own first |
| turn the greybox into a mesh | Ctrl+Shift+P, "bake", Enter: a mesh and its collider replace the brushes as one undo step; the brushes are kept, hidden | 6 keys | CSG menu > Bake Mesh Instance, hide the CSG, Bake Collision Shape, a StaticBody3D for it: about 6 clicks, 12 keys and a drag |

## What we take

1. **Brushes** (ADR 0107 §7: convex brushes held as planes):
   - each brush is convex; a shape makes one brush or several that act as one object
     (stairs: one brush per step);
   - shapes: box, wedge (a ramp), stairs, cylinder, sphere; arch and cone later
     (TrenchBroom, ProBuilder);
   - a box or a cylinder can be hollow: walls of a set thickness (0.2 m, a guess) outside
     the drawn space, so the drawn size is the room's inside (Unreal's Hollow, CSG
     Blockout's rooms). Each side can be left open (a courtyard's top);
   - a brush's origin is the middle of its base, and its height runs along the base's
     normal, so End drops it and a turn keeps its footprint;
   - scaling a brush (E, the gizmo) changes its size, not a scale: the walls' thickness
     and the steps' height stay (guess);
   - the inspector shows its parameters: the shape (switching it keeps the box, as in
     ProBuilder), the size, the steps' height, the sides, hollow, collides, a tint;
   - brushes are a kind of their own, with their own icon in the tree, the add menu and
     the palette (E1's rule).
2. **Drawing a brush** (Blender's Add Cube tool, ProBuilder, Cyclops, SketchUp):
   - **B** draws a box. B again before the first click goes to the next shape (wedge,
     stairs, cylinder, sphere), as our tools' keys do when pressed again. The tool bar's
     shape button and the palette ("Blockout: Draw Stairs") offer every shape.
     **Shift+B** draws a cut the same way;
   - before the first click, a mark shows where the corner will land: on the surface
     under the pointer (ADR 0109's surface pick, E2-5), or on the ground plane. The base
     lies in that surface's plane. From the first click on, the plane is fixed, so the
     rest follows the pointer in the same frame;
   - a click sets the first corner, the base follows the pointer, and a second click
     keeps it (press, drag and release works too). Then the height follows the pointer
     along the normal, and a click keeps it. Enter keeps what the pointer gives. Escape
     or a right click drops it all. The whole drawing is one undo step;
   - **typed sizes** take the pointer's place: the first side, Tab, the second, Enter;
     then the height, Enter (Blender's Tab between parts, left open in E2-2). The side
     being typed is highlighted on the preview. A cut with no height typed goes through
     the brush it was drawn on (guess);
   - **R** turns the shape a quarter about the base's normal while drawing (stairs,
     wedges). Stairs rise away from the first corner;
   - **snapping:** corners and sizes snap to the grid in the surface's plane, by the tool
     bar's position step (question 1). A corner also snaps to brushes' corners and edges
     within a few pixels, so a new brush meets the old ones (guess);
   - **labels** on the preview's edges give its sizes. Over a surface, the mark's
     distances to the face's edges show too, to place a window's sill (CSG Blockout's
     labels, TrenchBroom's selection dimensions);
   - **the hint** by the pointer names the shape and its keys ("Stairs 1 × 1.8 m, 6 steps
     of 0.2 m · walkable"). A slope or a step the navigation agent cannot climb (45°,
     0.4 m: `docs/dev-guide/navigation.md`) says so;
   - B, then Enter before any click, puts a brush of the last size at the pointer
     (ProBuilder's stamp);
   - **[ and ]** step the position step down and up through its menu (Hammer's keys);
   - Ctrl+A's add menu gets a Blockout section: a brush at the camera's target, on the
     grid (a 2 m box, a guess).
3. **Sizing afterwards:**
   - the selected brush shows a small handle in the middle of each face (Godot, CSG
     Blockout). A drag moves that face along its normal and the opposite face stays. A
     cylinder or a sphere has a radius handle too. Only the active brush shows them
     (guess);
   - Alt+drag on a handle extrudes instead: a new brush grows from the face (the
     duplicate gesture `viewport-tools.md` takes), the same as in editing below. Godot
     gives Alt to moving both faces; here both sides grow by scaling (E) or by the
     inspector's size, which grows about the base's middle;
   - the owner's snapping rules hold; labels show the size while dragging;
   - typed values and expressions in the inspector.
4. **Add, cut, intersect:**
   - **a field on each brush.** B draws adds and Shift+B cuts. **Minus** (and the keypad's)
     turns the selected brushes from add to cut and back (Roblox's Negate). Intersect is
     set in the inspector or the palette;
   - **the order is Godot's, which the team knows:** within its group, a brush acts on the
     brushes above it in the tree. New brushes go to the end of their group, so a new cut
     cuts everything already there. Dragging rows reorders. Rooms merge (question 2);
   - **groups:** brushes live under a "Blockout" group, made with the first brush
     (Godot's CSGCombiner3D). Another group is a separate piece (a building; later a door
     that moves). A brush drawn while a brush or a group is selected joins that group;
   - **shown:** adds in the grid material; a cut's edges orange and dashed, with a faint
     fill; an intersect's white and dotted. Both are drawn whenever overlays are on
     (Godot draws every shape's edges in its operation's color; Roblox tints negative
     parts red). The dashes tell them apart without color (CLAUDE.md §12);
   - the tree marks each brush's icon with +, − or ∩. A cut that cuts nothing (above
     everything in its group) is marked in the tree, and the inspector says why;
   - **picking:** the faces a cut makes belong to the cut, so a click on a doorway's side
     selects the doorway (each face lies on one brush's plane; ADR 0109's ids);
   - the result follows a drag frame by frame (immediate feedback). The ADR measures
     how big a group can be and still keep that within the frame;
   - nothing is split into fragments while editing: fragments exist only in the result
     and in a bake.
5. **Editing corners, edges and faces:**
   - **Tab** on the selected brushes starts it; Tab or Escape ends it (Blender's Tab). A
     click on anything else ends it and selects what was clicked;
   - **no separate vertex, edge and face modes:** the pointer highlights the nearest
     corner within a few pixels, else the nearest edge, else the face under it (Unreal's
     PolyGroup Edit). Click, Shift+click, and a box takes corners. The brushes' geometry
     is on the CPU, so the highlight is exact and in the same frame;
   - **a drag** moves what is picked: a face along its normal, an edge or a corner in the
     plane that faces the camera most. Q, E, R work with axis keys and typed values, as
     for objects. A face moved without an axis goes along its normal (guess);
   - **extrude:** Ctrl+E (ProBuilder's key), or Alt+drag on a face as on the handles,
     grows a new brush from the face in the same group. Inward, it makes a cut of the
     face's outline (TrenchBroom's split, SketchUp's push through) (guess);
   - a move that would make a brush concave stops at the limit, and the status bar says
     why (TrenchBroom refuses such moves). Corners that meet merge (TrenchBroom), so an
     edge moved onto another turns a box into a wedge;
   - a shape edited this way becomes plain brushes (ProBuilder's shapes stop being
     parametric too). The status bar says so, and undo brings the shape back;
   - the same tool later edits hull colliders (`docs/dev-guide/physics.md` §11, "Editing
     them later").
6. **Play at once:**
   - every add brush collides by default (Cyclops, ProBuilder; Godot's `use_collision` is
     off). "Collides" turns one off, for decoration;
   - the navigation mesh is rebuilt from them in the background a moment after an edit
     (Unreal, Roblox), never blocking, with the project's agent. The viewport's overlay
     menu shows it and the colliders, one by one (E2-7 left both for now);
   - Play (E3) copies the edit world with them: no cook, no bake;
   - a game built with brushes runs the same: the cook turns each group into a mesh,
     colliders and the navigation mesh (the ADR decides how);
   - a test walker for a game that has no player yet (question 3).
7. **The grid material:**
   - the brushes' default: a grid in world space (Unreal's world grid material, CSG
     Blockout's world-aligned grid), so faces line up and sizes read at a glance: 1 m
     squares with thinner lines at 25 cm (guess). Floors, walls and ceilings are shaded
     apart by the way they face (guess). A tint per brush multiplies it;
   - while drawing, the snapping grid shows on the surface's plane around the mark
     (TrenchBroom projects its grid onto faces);
   - its own pipeline, made only when a scene has brushes: a view without them pays
     nothing (CLAUDE.md §5).
8. **Bake to a mesh:**
   - "Blockout: Bake to Mesh" (the selected groups, or all of them) makes each group's
     result a mesh asset beside the scene (glTF, so Blender opens it; a guess) and puts a
     model with its collider in the group's place, as one undo step;
   - the brushes stay in the scene, hidden and switched off, and "Blockout: Unbake" brings
     them back (CSG Blockout's freeze). They are not left visible beside the mesh, as
     Godot's bake leaves them;
   - UVs come from the world projection, so the grid looks the same; lightmap UVs later.
9. **Keys** (ours; free in Blender's and Godot's maps too): B, Shift+B, Tab, Minus,
   Ctrl+H, Ctrl+E, [ and ]. Every action is a "Blockout:" command in the palette,
   rebindable. While drawing or editing, typed digits belong to the tool (no bookmark
   jumps), as during keyboard transforms.

## What we do not take

- **Subtract and carve that rewrite brushes into fragments** (TrenchBroom, Hammer):
  carve's slivers and off-grid corners cause leaks, and the hole cannot be edited again.
  Ours stay live until baked.
- **A compile or bake before playing** (TrenchBroom, Hammer; Godot's navigation Bake
  button).
- **Drawing in 2D views as the main way** (Hammer): brushes are drawn on surfaces in 3D.
  Orthographic views can come later.
- **Collision off by default** (Godot's `use_collision`).
- **An order hidden from view** (Unreal BSP's placement order, "To First", "To Last"):
  ours is the tree's, visible and draggable.
- **Booleans in a separate window or a modifier stack** (ProBuilder's experimental
  Boolean, Blender): the operation is a field of the brush itself.
- **Three tools for corners, edges and faces** (TrenchBroom, Hammer, ProBuilder's H,
  Source 2's 1–6): one editing state. Digits are bookmarks in our map anyway.
- **Units in powers of two** (TrenchBroom, Hammer): metres and the tool bar's steps.
- **Not now:**
  - a clip tool: cuts and edge moves cover what a greybox needs, and planes make it
    cheap to add later;
  - free polygons (ProBuilder's Poly Shape, TrenchBroom's brush tool);
  - a voxel grid (Unreal's Cube Grid);
  - texture alignment tools: the world grid needs none, and materials per face come
    with E10;
  - trigger and collision-only brushes (ProBuilder's Set Trigger and Set Collider,
    Quake's clip brushes): later, with gameplay volumes.

## Questions for the team

Answered on 2026-10-21 (team decisions, ADR 0110): 1 b as the coordinator recommended (the
owner's rule with inference), 2 a (rooms merge), 3 for E3; the shapes the owner wants:
box, cylinder, sphere, torus, cone and truncated cone, besides the wedge and stairs.

1. **Should brushes snap by default?** The owner's rule for everything else is free by
   default, with Shift for steps and Y to change the default.
   - a) Brushes snap to the grid by default; Shift frees them (TrenchBroom, Hammer,
     ProBuilder). Cost: one more default to learn, and Shift means the opposite for
     brushes and for everything else.
   - b) Brushes follow the same rule as everything else (Y once, the session keeps it),
     and a corner being drawn or moved always lands on another brush's corner or edge
     within a few pixels (SketchUp's inference, which has no grid by default). Cost: a
     freehand wall that meets nothing is off the grid until Ctrl or Y.
   - The note's author recommended a); the coordinator recommends b): one rule for
     Shift, and the gaps and slivers a free default would leave come from brushes that
     do not meet, which the inference closes.
2. **Should rooms merge?**
   - a) Where a room's inside meets another room's walls, they open: a corridor drawn up
     to a room opens into it, with no doorway cut. Other cuts follow the tree's order.
     Cost: one more rule, for rooms only.
   - b) A room is a box with walls; every opening is cut by hand. Cost: two cuts for
     every corridor (scenario 6).
   - Recommended: a).
3. **What walks a greybox in Play when the game has no player yet?** (For E3's UX
   note.)
   - a) Shift+F5 starts a test walker: a capsule of the navigation agent's size on the
     physics character, with a first-person camera (CSG Blockout's Play From Here). Cost:
     a small built-in game object in the editor.
   - b) Only the game's own player. Cost: no walking until there is game code.
   - Recommended: a), so a level can be walked in its first minutes.

## Not verified

- **Valve's wiki was down (HTTP 503) all through 2026-10-21.** Hammer's keys come from an
  older Hammer 4 tutorial (math.vu.nl), search excerpts of the wiki, and TWHL. Hammer++'s
  list comes from its features page. Not checked: whether Hammer++ draws blocks in the 3D
  view; Hammer's handle cycling (click again for resize, rotate, skew); how Source games
  make navigation (nodes placed by hand in Half-Life 2, `nav_generate` for bots); Source
  2's navigation build.
- **Godot:** handles moving one side since 4.2 (Alt for both) comes from a forum reply
  (2024-09-30). The CSG colors and the handles were checked in the gizmo's source, not in
  the docs.
- **Unreal:** the colors of additive and subtractive brushes come from a forum post. The
  world grid material as BSP's default is from memory. The cost of rebuilding BSP was not
  checked.
- **ProBuilder:** the mesh-collider default comes from the 4.2 docs; the 6.0 preferences
  page was cut off.
- **TrenchBroom:** the key for repeating commands is not in the manual's text.
- **Roblox:** that the navigation mesh rebuilds by itself comes from a 2017 announcement
  and the current pathfinding page.
- **Godot's counts in the scenarios are estimates.** Our design's guesses are marked
  "(guess)".

## Sources (checked 2026-10-21)

- Godot 4.7: [Prototyping levels with CSG](https://docs.godotengine.org/en/stable/tutorials/3d/csg_tools.html),
  [CSGShape3D](https://docs.godotengine.org/en/stable/classes/class_csgshape3d.html),
  [NavigationMesh](https://docs.godotengine.org/en/stable/classes/class_navigationmesh.html),
  [Using navigation meshes](https://docs.godotengine.org/en/stable/tutorials/navigation/navigation_using_navigationmeshes.html);
  the CSG gizmo's source (`modules/csg/editor/csg_gizmos.cpp`, master, MIT); forum threads
  [84989](https://forum.godotengine.org/t/why-is-scaling-and-snapping-in-3d-editor-not-working-as-shown-to-me/84989)
  (2024-09-30), [140033](https://forum.godotengine.org/t/alternative-to-the-scale-tool-designing-3d-levels-in-godot-4/140033)
  (2026-06-12), [CSG Blockout 3.0](https://forum.godotengine.org/t/csg-blockout-3-0-level-prototyping-with-godots-own-csg-nodes-draw-measure-play-freeze/144940)
  (2026-10-03); [Cyclops Level Builder's docs](https://github.com/blackears/cyclopsLevelBuilder/blob/master/doc/index.md) (MIT).
- TrenchBroom: [the manual](https://trenchbroom.github.io/manual/latest/) (2026.2) with
  its Markdown source and `shortcuts.js`; [releases](https://github.com/TrenchBroom/TrenchBroom/releases)
  2026.1 (2026-06-18: stairs) and 2026.2 (2026-08-21: arch, sweep tool).
- Hammer: [Hammer 4 tutorial](https://www.math.vu.nl/~eliens/masterclass/documents_hammer.html);
  [Hammer++ features](https://ficool2.github.io/HammerPlusPlus-Website/features.html);
  the Valve wiki's Block Tool, Hotkey Reference and Source 2 Mesh Editing pages (search
  excerpts only); TWHL's "VERC: Carving responsibly" (search excerpt).
- ProBuilder: [Shape tool 6.0](https://docs.unity3d.com/Packages/com.unity.probuilder@6.0/manual/shape-tool.html),
  [Boolean 6.0](https://docs.unity3d.com/Packages/com.unity.probuilder@6.0/manual/boolean.html),
  [Creating a shape 5.0](https://docs.unity3d.com/Packages/com.unity.probuilder@5.0/manual/workflow-create-predefined.html),
  [Shape tool 5.2](https://docs.unity3d.com/Packages/com.unity.probuilder@5.2/manual/shape-tool.html),
  [Hotkeys 5.2](https://docs.unity3d.com/Packages/com.unity.probuilder@5.2/manual/hotkeys.html),
  [Preferences 4.2](https://docs.unity3d.com/Packages/com.unity.probuilder@4.2/manual/preferences.html);
  Unity forum threads ("Pro Builder is totally useless", "connect vertices rarely seems to
  work").
- Unreal 5.8: [Geometry brushes](https://dev.epicgames.com/documentation/en-us/unreal-engine/geometry-brush-actors-in-unreal-engine),
  [CubeGrid](https://dev.epicgames.com/documentation/en-us/unreal-engine/cubegrid-tool-in-unreal-engine),
  [Modeling tools](https://dev.epicgames.com/documentation/en-us/unreal-engine/modeling-tools-in-unreal-engine),
  [Basic navigation](https://dev.epicgames.com/documentation/en-us/unreal-engine/basic-navigation-in-unreal-engine);
  forum threads [Modeling Mode](https://forums.unrealengine.com/t/modeling-mode/2454479)
  (2025-04-13) and [Wireframe color](https://forums.unrealengine.com/t/wireframe-color/33491).
- Blender 5.2 LTS manual: [Add Cube](https://docs.blender.org/manual/en/latest/editors/3dview/toolbar/add_cube.html),
  [Boolean modifier](https://docs.blender.org/manual/en/latest/modeling/modifiers/generate/booleans.html).
- Roblox: [Solid modeling](https://create.roblox.com/docs/parts/solid-modeling),
  [Pathfinding](https://create.roblox.com/docs/characters/pathfinding).
- SketchUp: [Pushing and pulling shapes](https://help.sketchup.com/en/sketchup/pushing-and-pulling-shapes-3d).
