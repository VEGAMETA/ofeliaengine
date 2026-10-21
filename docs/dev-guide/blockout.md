# Blockout

Brushes for greyboxing a level (ADR 0110): boxes, wedges, stairs, cylinders and spheres that
add to, cut out of or intersect each other, drawn with a grid in metres. They are exact: two
brushes that meet meet without gaps, a cut makes clean walls, and the result is the same on
every machine.

## Brushes and groups

- A **`Brush`** is a shape of a size (metres, along x, its height y, z), its origin the middle
  of its base, placed by its entity's `Transform`:
  - `BrushShape::Box`, `Wedge` (a ramp rising towards +z), `Stairs` (`segments` steps rising
    towards +z), `Cylinder` (`segments` sides around y), `Cone` (`segments` sides; `top`, the
    top's share of the base: 0 pointed, 0.5 a truncated cone), `Sphere` (`segments` sides
    around y), `Torus` (lying on its base: x and z its outer diameters, y the tube's;
    `segments` convex parts around, the tube of half as many sides);
  - `op`: `Add`, `Cut` (takes itself out) or `Intersect` (keeps only what lies inside it);
  - `hollow`: a room's walls, metres thick (0: solid; [rooms](#rooms));
  - `tint`: its color under the grid.
- A **`BrushGroup`** holds brushes: its descendants act on each other in the scene tree's
  order, each on what the brushes before it made. A cut cuts what is above it in the tree,
  not what comes after it. Brushes of different groups do not touch each other: make a
  building one group, a door that will move another.
- The faces a cut makes belong to the cut, so selecting a doorway's side selects the doorway.
- A brush that is off (`Disabled`, or below a disabled entity; the eye in the editor's tree)
  acts on nothing: a cut turned off leaves its wall whole, a group turned off shows nothing.

## Rooms

A brush with `hollow` walls is a **room**: its size is its inside, and its walls stand
around it, the shape's every side moved out by their thickness (a box's walls, floor and
ceiling; a cylinder's round wall). Rooms open into each other (team decision, 2026-10-21):

- **A room's inside takes itself out of every room in its group,** wherever the two are
  in the tree. A corridor drawn from a hall out through its wall opens the wall where
  their insides meet, and the corridor's end inside the hall goes; no doorway cut.
- **Only rooms:** a crate, a pillar or stairs inside a room stay, and cuts and intersects
  keep the tree's order, so a cut after a room still cuts its wall (a door to the
  outside).
- **Only an adding brush is a room;** a cut with `hollow` cuts its whole size.

```rust
let hall = Brush::new(BrushShape::Box, Vec3::new(6.0, 3.0, 6.0)).with_hollow(0.2);
let corridor = Brush::new(BrushShape::Box, Vec3::new(2.0, 2.4, 6.0)).with_hollow(0.2);
// The corridor's inside runs from z = 2 to 8: from the hall's inside out through its wall.
world.spawn((SceneId::new(), Name::new("Hall"), Transform::IDENTITY, hall));
world.spawn((SceneId::new(), Name::new("Corridor"), Transform::from_xyz(0.0, 0.0, 5.0), corridor));
```

```rust
let group = world.spawn((SceneId::new(), Name::new("Blockout"), Transform::IDENTITY, BrushGroup::default()));
let room = world.spawn((SceneId::new(), Name::new("Room"), Transform::IDENTITY,
    Brush::new(BrushShape::Box, Vec3::new(6.0, 3.0, 6.0))));
let inside = world.spawn((SceneId::new(), Name::new("Inside"), Transform::from_xyz(0.0, 0.2, 0.0),
    Brush::new(BrushShape::Box, Vec3::new(5.6, 2.6, 5.6)).with_op(BrushOp::Cut)));
let door = world.spawn((SceneId::new(), Name::new("Door"), Transform::from_xyz(0.0, 0.2, 3.0),
    Brush::new(BrushShape::Box, Vec3::new(1.2, 2.0, 0.6)).with_op(BrushOp::Cut)));
for e in [room, inside, door] {
    world.set_parent(e, Some(group))?;
}
```

## Colliders

Brushes collide: each brush group becomes one static body, its brushes' pieces as convex
hulls, when the game asks its physics for them (`ScenePhysics::sync_blockout`, physics.md
§11). `collides: false` leaves a brush out (decoration). Bodies that roll or slide over a
blockout set `internal_edges`, and the seams between pieces are as smooth as one floor.
In the editor, **View: Colliders** (the palette) draws the pieces that collide.

## Exporting a greybox

**Blockout: Export to glTF** (the palette) writes the selected group (or the group of the
selected brush) beside the scene as a binary glTF named after it (`blockout.glb`), over an
earlier one: its visible surface as the editor draws it, in the group's own space (metres,
y up). The brushes of one tint share a part and a material, the tint over the grid
(embedded), so Blender shows the greybox as it looks here, ready for an artist to build
over. From code: `player.blockout().baked(group, &mut brushes)` gives the triangles,
`engine_app::grid_png()` the grid, `engine_app::bake::glb` the file.

## Baking a group

**Blockout: Bake to Mesh** (the palette) bakes the selected groups (or the groups of the
selected brushes; with nothing selected in a group, every group of the scene) as one undo
step:
- **The file:** the group's surface written beside the scene as `<group>.glb`, as the
  export writes it, with its `.meta` (the file's id, kept when the file is baked again).
  Another group's bake of the same name gets `<group>-2.glb`.
- **In the scene:** a model per tint under the group (`Blockout 1`, `Blockout 2`, … named
  after the file's meshes), drawn from the file, and `BrushGroup.baked` naming the file:

  ```text
  BrushGroup.baked = asset("levels/blockout.glb", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
  ```
- **The brushes stay:** not drawn, but they still collide and still make the navigation
  mesh, from their exact hulls (`ScenePhysics::sync_blockout`). The tools leave them alone
  (Tab, Ctrl+H and the side handles say the group is baked); a new brush goes to a group
  that is not.

**Blockout: Unbake** brings the selected baked groups (or every one) back: the brushes
drawn again, the bake's models gone; the file stays. **Bake to Mesh** on a baked group bakes
it again from its brushes over the same file, with the same id, and the player reads the
file again. An artist's version of the mesh belongs in a file of its own: a bake writes
over its file. Undo puts the scene back, not the file (the next bake writes it again).

## Navigation

The visible surface of the brushes that collide bakes into a navigation mesh
(`Blockout::surface_triangles`, then `engine_nav::bake`; navigation.md). In the editor,
**View: Navigation** bakes it in the background a moment after each edit and draws it, so
you see at once where an agent can walk: through a doorway wide enough, up stairs whose
steps are low enough.

## What it costs

- The player evaluates a group again only where something changed: moving a door among
  2,000 brushes takes about 0.6 ms on one core; a frame where nothing changes costs nothing
  and allocates nothing.
- Each brush is one mesh; scenes without brushes pay nothing.
- Rooms cost more, each cut by the insides that reach it: 400 rooms joined by 380
  corridors take 0.12 s from nothing, and dragging one redoes it and the two it opens into
  in about 1.3 ms.

## In the editor

**B** draws a brush on the surface under the pointer, **Shift+B** a cut: a click for the
first corner, one for the base, one for the height, or the sizes typed (`editor.md`,
"Drawing brushes"). A doorway: Shift+B over the wall, a click, `1` Tab `2.1` Enter `0.3`
Enter. Ctrl+H while drawing makes it a room: what you draw is its inside. **+** →
**Blockout** adds each shape where the camera looks. Both go into the selection's group (or
the scene's first, or a new "Blockout" group), at its end. **Ctrl+H** makes the selected
brushes rooms with walls 0.2 m thick, or solid again. While drawing, corners land on other
brushes' corners and edges near the pointer, so brushes meet exactly; the active brush's
side handles size it afterwards, the opposite side staying (`editor.md`, "Sizing a brush by
its sides"). **Tab** edits the selected brushes' faces, edges and corners: a drag moves
one (a box's edge dragged onto another makes a wedge), Alt+drag on a face pulls a new brush
out of it or pushes a cut into it (`editor.md`, "Editing faces"). The tree shows each brush's shape
and what it does ("Brush", "Cut", "Intersect"); the inspector edits its fields, every change
one undo step; the eye turns it off.

## Coming

Sides of a room left open (a courtyard), radius handles for cylinders and spheres, and
the cook's path: a shipped game's blockout evaluated by the cook into meshes, hulls and
navigation tiles, with the one-button build (ADR 0107 E14).
