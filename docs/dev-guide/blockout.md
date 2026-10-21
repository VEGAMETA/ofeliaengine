# Blockout

Brushes for greyboxing a level (ADR 0110): boxes, wedges, stairs, cylinders and spheres that
add to, cut out of or intersect each other, drawn with a grid in metres. They are exact: two
brushes that meet meet without gaps, a cut makes clean walls, and the result is the same on
every machine.

## Brushes and groups

- A **`Brush`** is a shape of a size (metres, along x, its height y, z), its origin the middle
  of its base, placed by its entity's `Transform`:
  - `BrushShape::Box`, `Wedge` (a ramp rising towards +z), `Stairs` (`segments` steps rising
    towards +z), `Cylinder` (`segments` sides around y), `Sphere` (`segments` sides around y);
  - `op`: `Add`, `Cut` (takes itself out) or `Intersect` (keeps only what lies inside it);
  - `tint`: its color under the grid.
- A **`BrushGroup`** holds brushes: its descendants act on each other in the scene tree's
  order, each on what the brushes before it made. A cut cuts what is above it in the tree,
  not what comes after it. Brushes of different groups do not touch each other: make a
  building one group, a door that will move another.
- The faces a cut makes belong to the cut, so selecting a doorway's side selects the doorway.
- A brush that is off (`Disabled`, or below a disabled entity; the eye in the editor's tree)
  acts on nothing: a cut turned off leaves its wall whole, a group turned off shows nothing.

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

## Coming

The editor's tools (adding brushes from the add menu, drawing them on surfaces with B,
editing faces), colliders and the navigation mesh made from them, and baking a group into a
mesh: ADR 0110's steps.
