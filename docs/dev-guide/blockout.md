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
brushes rooms with walls 0.2 m thick, or solid again. The tree shows each brush's shape
and what it does ("Brush", "Cut", "Intersect"); the inspector edits its fields, every change
one undo step; the eye turns it off.

## Coming

A brush's faces dragged by handles, sides of a room left open (a courtyard), corners
landing on other brushes' corners and edges, faces and edges edited by hand,
colliders and the navigation mesh made from them, and baking a group into a mesh: ADR
0110's steps.
