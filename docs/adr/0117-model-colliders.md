# 0117. Colliders of a scene's models

- Status: Accepted (2026-10-22). It carries the team decision of 2026-10-16 (ADR 0101:
  every mesh collides as level geometry unless its file's rules say otherwise) from the
  cook to scene files, for the owner's request of 2026-10-22 that the editor show what
  collides.
- Date: 2026-10-22
- Builds on: [ADR 0101](0101-mesh-colliders.md) (colliders made from meshes, the rules
  file), [ADR 0108](0108-scene-format-2.md) (scene files, `Model`, assets loaded in the
  background), [ADR 0110](0110-blockout-brushes.md) (the blockout's bodies,
  `sync_blockout`), [ADR 0116](0116-view-modes.md) (the colliders drawn in the editor)

## Context

- **A pack's meshes collide.** The cook makes their colliders by the scene's rules
  (`<scene>.colliders.ron`). By default every mesh is static level geometry, its triangles
  as drawn; props get fitted primitives, hulls or convex parts; `UCX_` meshes and glTF
  physics are taken as authored. A game loads them with `Player::load_physics`.
- **A scene file's models do not.** A scene the editor saves names its meshes by asset
  (`Model.mesh = asset("scenes/yard.glb#Yard 1", …)`). They load in the background, without
  a pack. Only the blockout collides (`ScenePhysics::sync_blockout`): a prop dragged into a
  level from the FileSystem dock is drawn, and the player walks through it.
- **The editor shows only the brushes' colliders** (ADR 0116 V2a): nothing else has any.

## Options

1. **Cook scene files into packs to play them.** Colliders would exist only after a cook.
   The editor would show none while editing, and Play would wait for the cook. That goes
   against ADR 0107 E3: Play runs on the scene's world, at once.
2. **Collider components** (Unity's and Godot's way): a `Collider` added by hand to each
   model that should collide, its shape chosen there. Explicit, but every model must be
   set up. It also contradicts ADR 0101's default: nothing to set up for a level to collide.
3. **A model's collider made from its mesh by its file's rules**, as the cook makes it, in
   the background with the file. The editor draws it, and a game's physics gives the model
   a body. The default is Unreal's: a placed mesh collides. A component overriding the
   rules per entity can come later, with ADR 0101's edited colliders.

## Decision

Option 3.

- **A model's collider** is what the cook makes for its mesh, in the mesh's space:
  - **a glTF mesh:** the cook's collider for the first node that draws it. It follows the
    file's rules (`<file>.colliders.ron` beside it, read as the cook reads it) and the
    file's own colliders (`UCX_`, glTF physics). It takes that node's body settings
    (motion, sensor, layer, friction, restitution, mass) and the file's import scale (E4-9);
  - **a built-in mesh:** its own shape, static: the cube a box, the sphere a sphere, the
    cylinder a cylinder, the plane its two triangles;
  - **none** for a mesh the rules turn off, a skinned mesh, and a mesh no node draws.
- **Made only when asked**, by `Assets::collider`:
  - the first time a model's collider is asked for, its glTF file is cooked on the
    loader's workers, all of its meshes at once, and read first if it is not in memory;
  - a file written anew is cooked again when next asked;
  - the convex parts are cached in the derived data cache already (ADR 0101); the rest of
    a cook is a copy of triangles or a fit, and is not cached.
- **A game's bodies:** `ScenePhysics::sync_models`, called from `Game::tick` as
  `sync_blockout` is:
  - a static model gets a fixed body at its entity's place, made again when the entity
    moves or its collider changes, and gone with the entity or when it is turned off;
  - a dynamic or kinematic model gets `RigidBody` and `PhysicsPose`: the physics moves the
    entity, as a pack's props move theirs;
  - shapes are shared by every model of the same mesh and placed at any scale
    (`engine_physics::colliders::placed`).
- **The editor** draws every model's collider where its entity is, over any view mode
  (ADR 0116), in the color of its motion: static cyan (as the blockout's), dynamic orange,
  kinematic violet, sensors yellow; hidden parts faint. In Play the entities are where their
  bodies are, so the lines follow them.
- **Shapes' lines:** `engine_collision::Shape::edges` gives a shape's edges in its own
  space:
  - a box's twelve, a sphere's three great circles, a capsule's and a cylinder's rims and
    sides;
  - a hull's edges between faces that are not coplanar;
  - a mesh's open borders and creases (edges between triangles that turn by more than a
    degree).

## Consequences

- **Easier:**
  - a level built in the editor from models collides as drawn;
  - a prop marked dynamic in its file's rules falls and rolls in Play;
  - the editor shows which.
- **Costs:**
  - a level mesh's collider is a copy of its triangles;
  - a prop's parts take 0.02–3 s the first time, and are cached after;
  - a game that never syncs the models, and an editor that never shows colliders, cook
    nothing;
  - building a large mesh shape for Jolt takes time at load (Intel Sponza: 3.9 s, where a
    pack restores its baked form in 5.5 ms). The baked form can go into the derived data
    cache too, as a later step, if a measurement asks for it.
- **To revisit:**
  - a component on an entity that overrides its mesh's rules (another shape, no
    collision, another layer), with the editor's collider editing (ADR 0101);
  - a rules file written anew is not watched: the colliders follow it when their glTF file
    is next read.

## Steps

1. `Shape::edges`; `Assets::collider`, the models' colliders cooked when asked; the
   editor draws them (ADR 0116 V2b).
2. `ScenePhysics::sync_models`: the models' bodies, static and moving; the physics guide.

## As built (step 1, 2026-10-22): the models' colliders, drawn

- **`engine_collision::Shape::edges`** (and `Collider::edges`): the lines of the Decision,
  a round shape's circle in 32 segments; `mesh::crease_edges` for hulls and meshes (a
  mesh's vertices welded first, as render meshes split them at seams).
- **`engine_assets::colliders::cook_node_colliders`:** the cook as before, and for each
  body the node it was made for with its collider in that node's object space (before a
  shear is baked into it). `cook_colliders` is it without the nodes.
- **`Assets::collider(mesh) -> ColliderState`** (`Made(Arc<ModelCollider>)`, `Coming`,
  `None`), `Assets::collider_epoch`, `Assets::cooking`:
  - a glTF file's colliders cook on a third thread of the loader (`engine-cook`, lowest
    priority), so a frame that waits for a scene's assets never waits for them; a file
    asked for while it is read cooks once it is;
  - `ModelCollider` holds the collider in the mesh's space (the file's import scale
    applied) and the body's settings (motion, sensor, friction, restitution, mass,
    gravity factor, layer);
  - a file read again (`Assets::reload`, its file written anew) forgets its colliders and
    raises the epoch.
- **The editor** draws every active model's collider with the blockout's (View:
  Colliders, Alt+C), in four batches by motion. Each mesh's lines are made once in its own
  space; the world's are made again when a model moves, comes or goes, or the epoch
  changes.
- **Measured** (ref-1, release, `view_modes` `measure_the_model_colliders --ignored`,
  10,000 built-in cubes, 120,000 edges, framed at 1920 × 1080, medians of 100 frames):

  | colliders shown | RTX 4060 Ti | Radeon iGPU |
  |---|---|---|
  | the `lines` pass | 69 µs | 253 µs |
  | composing the frame (CPU), hidden → shown | 149 → 413 µs | 124 → 278 µs |
  | the editor's frame (`sync_assets`), hidden → shown | 2 → 99 µs | 1 → 98 µs |

  The composing's share is the lines' copy into the frame's ring (2.9 MB, into the RTX's
  memory over PCIe); the editor's is the check of every model's place each frame.
- **Tests:** the shapes' edges (a box's twelve, round shapes' on their surfaces, a hull's
  and a mesh's without their faces' diagonals); the cook's nodes under a shear; in the
  player on both GPUs, a built-in cube's box, a glTF crate's triangles by default, a box
  fitted to it with `(bodies: Dynamic)` beside it, none with `(collides: false)`, cooked
  again each time the file is read again; in the editor on both GPUs, a floor brush, a
  built-in cube and a dynamic crate drawn in their batches.
- **Left for later:**
  - the colliders' overlay checks every model each frame and copies its lines into the
    ring each frame: change detection and lines kept on the GPU while they do not change
    would take both costs away;
  - a glTF physics body whose colliders are on other nodes than the one drawing the mesh
    gives that mesh no collider.

