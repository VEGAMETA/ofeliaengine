# Physics: bodies, gravity, queries, characters, rollback

The engine simulates rigid bodies through one interface, `engine_physics::PhysicsBackend`.
Today the backend is Jolt Physics; the engine's own solver will plug into the same
interface. Your game code does not change when the backend does.

## 1. A world

```rust
use engine_physics::*;
use engine_physics_jolt::JoltBackend;
use engine_core::math::Vec3;

let mut world = JoltBackend::new(&PhysicsSettings::default())?;
let ground_shape = world.create_shape(&ShapeDesc::Box { half_extents: Vec3::new(50.0, 0.5, 50.0) })?;
world.create_body(&BodyDesc::fixed(ground_shape, Vec3::new(0.0, -0.5, 0.0)))?;

let ball_shape = world.create_shape(&ShapeDesc::Sphere { radius: 0.5 })?;
let ball = world.create_body(&BodyDesc::dynamic(ball_shape, Vec3::new(0.0, 5.0, 0.0)))?;

// Every fixed update (for example 60 times per second):
world.step(1.0 / 60.0, 1)?;
let (position, rotation) = world.transform(ball).unwrap();
```

- **Shapes** are created once and shared by any number of bodies: sphere, box, capsule,
  cylinder, convex hull, triangle mesh (for static level geometry), compounds of them
  (`ShapeDesc::Compound`) and scaled ones (`ShapeDesc::Scaled`; `is_valid_scale` says which
  scales a shape takes; colliders from a pack take any, section 11).
- **Bodies** are `Static` (never move), `Kinematic` (moved by you, push others) or
  `Dynamic` (simulated).
- `MotionQuality::Continuous` stops fast, small bodies (bullets) from tunneling.

## 2. Layers

`PhysicsSettings::layers` and `collides` define up to 32 layers and which pairs collide.
The default has layer 0 for static geometry and layer 1 for moving objects. Queries take a
`QueryFilter` with a layer mask: for example, ignore the player's own layer when casting
from the player.

**Name them** in `layer_names` (the default names its two `Static` and `Moving`). Scenes
put their bodies on layers by name: the colliders' rules file (section 11) and glTF's
collision filters say `"Props"`, and your settings decide what number that is and what it
collides with. Renumbering layers needs no new cook.

```rust
let settings = PhysicsSettings {
    layers: 3,
    collides: vec![0b110, 0b111, 0b111], // static and static do not collide
    layer_names: ["Static", "Moving", "Props"].map(String::from).to_vec(),
    ..PhysicsSettings::default()
};
```

## 3. Gravity

Gravity is part of the API, not a hack:

| `GravityMode` | Effect |
|---|---|
| `World` | the world's gravity (`set_gravity`) |
| `Scaled(k)` | world gravity × k: 0 floats, −1 falls up |
| `Custom(v)` | a fixed vector for this body, for example "down" toward a planet's surface |
| `Fields` | the sum of the world's gravity fields at the body |

Fields (`set_gravity_fields`):
- `GravityField::Point { center, strength, min_distance, radius }` pulls toward a point
  with inverse-square falloff: planets, black holes;
- `GravityField::Box { min, max, acceleration }` is constant inside a box: gravity lifts,
  wind tunnels.

Change a body's mode at any time with `set_body_gravity`.

## 4. Queries

Rays:
- `raycast(&Ray { origin, direction }, filter)`: the closest hit, with point, normal,
  fraction and the body's `user_data`. The direction's length is the range.
- `raycast_all(&ray, filter, &mut hits)`: every body along the ray, nearest first
  (penetrating bullets, line of sight through glass).
- `raycast_batch(&rays, filter, &mut hits)`: many rays at once (AI line of sight,
  sensors).

Swept shapes ("does this fit?", melee swings, thick projectiles, camera collision):

```rust
let probe = world.create_shape(&ShapeDesc::Capsule { half_height: 0.5, radius: 0.3 })?; // once
let cast = ShapeCast {
    shape: probe,
    position: from,
    rotation: Quat::IDENTITY,
    direction: to - from, // its length is the range
};
if let Some(hit) = world.cast_shape(&cast, QueryFilter::default()) {
    let safe = from + (to - from) * hit.fraction; // where the capsule first touches
}
```

- `cast_shape` gives the first hit; `cast_shape_all` every body along the way, nearest
  first; `cast_shape_batch` many casts at once.
- Any shape except triangle meshes can be swept: spheres, capsules, boxes, cylinders,
  convex hulls. Create it once and reuse it.
- A shape that starts inside a body reports it at `fraction` 0 with the `penetration`
  depth (the deepest first). One that starts touching a body and moves away from it does
  not report it.

Overlaps:
- `overlap_shape(shape, position, rotation, filter, &mut bodies)`: everything touching a
  shape (area triggers, "is this spot free?").
- `overlap_sphere(center, radius, filter, &mut bodies)`: the same for a sphere, without a
  shape.

Filters (`QueryFilter`):
- `QueryFilter::layers(mask)`: only these layers;
- **what a hit is made of:** `physics.surface_of(hit.body, hit.part)`, for a ray's or a
  shape cast's hit. For a scene's colliders it is the glTF material of the triangle hit, or
  of a prop's mesh (pick footstep sounds and impact effects by the material's name); for
  your own shapes, what `set_shape_surface` gave them. It costs about 7 ns a call, and
  nothing when not called;
- `.with_sensors()`: sensors too (`BodyDesc::sensor` bodies report overlaps but do not
  collide; queries skip them by default);
- `.ignoring(body)`: skip one body, typically the one casting (or your character's proxy
  body).

Results are written into vectors you keep: clear and reuse them every frame, and queries
do not allocate.

## 5. Kinematic bodies: lifts, doors, moving platforms

A `Kinematic` body moves only where you put it, and pushes dynamic bodies out of the way.
Move it with a target per fixed update:

```rust
// Before every step: where the lift should be at the end of it.
world.move_kinematic(lift, Vec3::new(0.0, height_at(time + dt), 0.0), Quat::IDENTITY, dt);
world.step(dt, 1)?;
```

- `move_kinematic` sets the body's velocity so it arrives exactly at the end of the
  step. Things on it are carried and things in its way are pushed.
- `set_transform` teleports: nothing on it moves along. Use it to place, not to move.
- Without a new target the body keeps its last velocity.

## 6. Characters

A character is a capsule that your code moves, not the solver. It walks up stairs, stops
at steep slopes, stays on the ground when walking down, rides moving platforms and pushes
crates.

```rust
use engine_physics::{CharacterDesc, CharacterMotor};

let hero = world.create_character(&CharacterDesc {
    max_slope: 45f32.to_radians(),
    step_height: 0.35,
    ..CharacterDesc::capsule(spawn_feet, 1.8, 0.3) // feet position, height, radius
})?;
let motor = CharacterMotor { jump_speed: 6.0, ..CharacterMotor::default() };

// Every fixed update, before world.step:
let state = world.character(hero).unwrap();
let walk = input_direction * 4.0; // m/s along the ground
let velocity = motor.velocity(&state, walk, input.jump_pressed, dt);
let state = world.move_character(hero, velocity, dt).unwrap();
world.step(dt, 1)?;
if state.is_grounded() { /* footsteps, coyote time... */ }
```

- **Position is the feet** (the bottom of the capsule).
- **You decide the velocity**, gravity included. `CharacterMotor` does the usual: walk on
  the ground, jump only when grounded, fall, steer in the air (`air_control`), ride
  platforms (`follow_ground`). Write your own for anything else: swimming, ladders,
  dashes.
- `CharacterDesc` controls:

  | field | default | what |
  |---|---|---|
  | `max_slope` | 50° | steeper slopes are walls |
  | `step_height` | 0.4 m | steps climbed without jumping; 0 off |
  | `snap_to_ground` | 0.5 m | how far it is pulled down to stay on the ground; 0 off |
  | `mass`, `max_push_force` | 70 kg, 100 N | pushing crates, pressing on platforms |
  | `layer` | 1 | what it collides with (`PhysicsSettings::collides`) |
  | `proxy_layer` | none | a body that follows it, so rays, sensors and bullets see it |

- `CharacterState` tells where it stands:
  - `ground`: `OnGround`, `OnSteepGround` (slides), `NotSupported`, `InAir`;
  - the ground's normal, point, body and velocity;
  - the velocity left after sliding along walls.
- `character_contacts(hero, &mut contacts)` lists what it touched in its last move
  (walls, crates, other characters).
- **Proxy body.** Without it, rays and other bodies pass through the character. With it,
  shots hit the proxy (`state.proxy` is its `BodyId`, with the character's `user_data`).
  It is a kinematic body: on a layer that collides with crates it shoves them with
  infinite strength. Give it a layer that only collides with what should feel it that
  way, and cast your own rays with `.ignoring(proxy)`.
- **Characters block each other.** Every character tests every other one: fine for
  dozens, not crowds of hundreds (1.9 µs per move alone, 7 µs among 128).
- `set_character_transform` teleports (respawns, portals).

## 7. Events

After each `step`, `contact_events()` lists `Begin` and `End` of contacts:
- `a < b`;
- sorted, so the order is the same every run;
- `sensor` set when either body is a sensor.

Read them in your fixed update. Your code never runs on physics threads.

## 8. Determinism and rollback

Same build, same inputs, same order of API calls: same results, on any number of threads
(tested in CI). `state_hash()` hashes every body's and character's state; compare it
between runs or between a client and a server to detect divergence. For identical results
across compilers and CPUs, build with the `cross-platform-deterministic` feature (about 8%
slower).

**Rollback netcode** saves the world every frame and, when a late input arrives, restores
an earlier frame and re-simulates:

```rust
use engine_physics::PhysicsSnapshot;

// Once: a ring of snapshots for the rollback window.
let mut ring: Vec<PhysicsSnapshot> = (0..8).map(|_| PhysicsSnapshot::new()).collect();

// Every frame, before simulating it:
world.save_state(&mut ring[frame % 8])?;

// A late input for frame `f`:
world.restore_state(&ring[f % 8])?;
for g in f..now {
    // the same calls as the first time, with the corrected input
}
```

- Saving reuses the snapshot's memory: no allocations once the ring has warmed up.
- After a restore, the same calls give bit-identical results (same build).
- `snapshot.hash()` hashes the saved bytes: sleep state, contacts and joints too. Send it
  to the other players for desync checks.
- **What a snapshot holds:** everything a step changes. Positions, velocities, sleep
  state, contacts, joint state and characters.
- **What it does not hold:**
  - which bodies, shapes, joints and characters exist: restore only into the world it was
    saved from, with the same objects. Creating or destroying one in between makes
    `restore_state` fail with `SnapshotMismatch`;
  - settings you change (materials, gravity modes and fields): restore those yourself.
- If `restore_state` fails on damaged bytes, the world may be partly restored: restore a
  good snapshot.
- **Cost** (ref-1, one thread):
  - 1 000 bodies resting on each other: save 0.27 ms, restore 0.21 ms, 0.5 MiB;
  - 10 000: save about 6 ms, restore 2.7 ms, 5.6 MiB. Contacts dominate.
  - Fine for typical rollback games (tens to hundreds of moving bodies). Ask before
    building rollback over thousands.
- 2D (Box2D) has no snapshots. `Mover2d` is plain data: copy it.

## 9. Physics in the ECS

Physics runs at a fixed rate; frames come at any rate. Draw the bodies between their
last two steps, and motion looks smooth whatever the frame rate:

```rust
use engine_physics::ecs::{FixedStep, KinematicTarget, PhysicsPose, PhysicsSync, RigidBody};

let mut clock = FixedStep::new(60.0);
let mut sync = PhysicsSync::new(&mut ecs);
let crate_entity = ecs.spawn((
    RigidBody::new(body),
    PhysicsPose::at(position, rotation), // the spawn pose: no blending from the origin
    Transform::from_translation(position),
));

// Every rendered frame:
for _ in 0..clock.advance(frame_seconds) {
    // game logic, characters (move_character)...
    sync.push(&mut ecs, &mut physics, clock.dt()); // KinematicTarget → move_kinematic
    physics.step(clock.dt(), 1)?;
    sync.pull(&mut ecs, &physics);                 // new poses into PhysicsPose
}
sync.interpolate(&mut ecs, clock.alpha());         // Transform between the last two steps
```

- `RigidBody` and `Character` tie an entity to a body or a character. `PhysicsPose`
  keeps their last two poses. `KinematicTarget` moves a kinematic body.
- `interpolate` writes the translation and rotation of root entities (no `Parent`); scale
  stays yours. Keep entities with physics at the root of the hierarchy.
- `FixedStep` runs at most 8 steps per frame (`max_steps`) and drops the rest, so a long
  hitch does not make the next frames slow too.
- All components are plain data: ECS snapshots save them for rollback next to the physics
  snapshot.

## 10. Performance

- Keep shapes simple: spheres, capsules and boxes are cheapest; convex hulls next;
  triangle meshes only for static geometry.
- Let bodies sleep (`allow_sleeping`, on by default): sleeping bodies cost almost nothing.
- Use `raycast_batch` and read transforms in bulk with `transforms` instead of one call
  per body.
- Threads are picked for you before every step:
  - fewer than `PhysicsSettings::single_thread_below` awake bodies (default 300) run on
    the calling thread: handing out jobs would cost more than the work;
  - more run on every logical core (`PhysicsSettings::threads`, default
    `PhysicsSettings::all_cores()`). Worker threads start on the first large step.
  - `stats().step_threads` shows what the last step used.
  - The result does not depend on the choice, so switching is safe for replays and
    networking.
  - `threads: 0` keeps the simulation on the calling thread always; `single_thread_below:
    0` always uses the workers.
  - **Give it the engine's thread pool:** `world.set_jobs(Some(jobs.clone()))` (an
    `Arc<engine_core::jobs::ThreadPool>`) runs the physics jobs on the threads the rest of
    the frame uses, instead of threads of Jolt's own that compete with them for the cores.
    As fast on ref-1, faster on small steps, same results; do it whenever your game has a
    job pool.
- On ref-1 (8 cores, 16 threads) a 5 000-body pile gains another 27% from the second
  hardware thread of each core; a 1 000-body pile stops gaining at 8 threads. Measure
  your scene with `cargo bench -p engine_physics_jolt --bench physics`, which sweeps 1 to
  16 threads and shows the automatic choice.
- `PhysicsSettings::threads` sets the worker threads (0 keeps the simulation on the
  calling thread).
  - Small worlds (a few hundred bodies) are fastest on the calling thread: handing out
    jobs costs more than the work.
  - Large piles scale with threads, but usually not past the number of physical cores;
    the second hardware thread of a core (SMT) rarely helps.
  - Measure your scene with `cargo bench -p engine_physics_jolt --bench physics`, which
    sweeps 1 to 16 threads.
- Queries on ref-1, one thread, over 1 000 bodies: a ray 0.15 µs, a sphere or capsule
  cast 0.36–0.39 µs, a box or hull cast 0.47–0.49 µs, a capsule overlap 0.65 µs
  (`cargo bench -p engine_physics_jolt --bench queries`, which also measures characters
  and save/restore).

## 11. Colliders made from your meshes

The cook makes colliders from a scene's meshes and puts them into the pack, baked for Jolt
(ADR 0101). Your game loads them with one call and steps them from its tick.

### What collides

1. **A file with glTF physics** (`KHR_physics_rigid_bodies` with `KHR_implicit_shapes`; the
   Khronos drafts, which Blender's glTF physics exporter writes) collides as it says:
   - a node with a `motion` is a body, with the colliders of its subtree;
   - a collider elsewhere is static;
   - a trigger is a sensor;
   - physics materials give friction and restitution;
   - a dynamic body's mesh collider (a mesh without `convexHull`) becomes its convex parts
     (below), since a simulated body cannot be a triangle mesh. Static and kinematic ones
     keep their triangles.

   Nothing else in such a file collides. A collider's collision filter names its body's
   layer: the filter's first collision system (section 2). Joints are not read yet.
2. **Collision meshes you model** next to a mesh, named the Unreal way, give that mesh its
   collider and are not drawn:

   | name | becomes |
   |---|---|
   | `UCX_Chair`, `UCX_Chair_01` | the convex hull of that mesh |
   | `UBX_Chair_02` | a box fitted around it |
   | `USP_Chair_03` | a sphere around it |
   | `UCP_Chair_04` | a capsule around it |

   The name after the prefix is the node's (or its mesh's) name. A collision mesh whose
   name matches nothing is a static collider of its own.
3. **Everything else** is level geometry: a static body with the mesh's triangles, exactly
   as drawn.
   - All of a mesh's parts are in one collider. Each triangle remembers its material (the
     *surface*, for footsteps and impacts).
   - Skinned meshes get no collider.
   - **The rules file** beside the scene changes that (below).

### The rules file: `<scene>.colliders.ron`

Put `sponza.colliders.ron` next to `sponza.gltf`. Every field is optional; a scene without
the file is all level geometry.

```ron
// sponza.colliders.ron
(
    bodies: Static,            // what a mesh becomes: Static (level) or Dynamic (a prop)
    dynamic_shape: Auto,       // a prop's collider: Auto, Box, Sphere, Capsule, Hull or Parts
    parts: (threshold: 0.05, max_parts: 32), // how concave props are split (below)
    no_collision: ["Leaves"],  // glTF materials whose triangles collide with nothing
    layer: "Static",           // the bodies' collision layer, by name (section 2)
    meshes: {                  // a glTF mesh by its name, or "#3" (the fourth): what differs
        "Chair": (bodies: Dynamic, dynamic_shape: Parts, layer: "Props"),
        "Barrel": (bodies: Dynamic),
        "Banner": (collides: false),
    },
)
```

- **`bodies: Dynamic`** makes props: each mesh is split into convex parts first (below). A
  mesh convex enough to be one part gets a box, sphere, capsule or cylinder when one is
  within 10 % of its hull, else its hull; a concave one (a chair, a mug, a U-shaped table)
  gets its parts. `dynamic_shape: Parts` asks for parts outright, `Hull` and the primitives
  for one shape.
- **`collides: false`** turns a mesh (or, at the top, every mesh) off.
- **`no_collision`** excludes materials by name: foliage cards, decals. Nothing is guessed:
  a masked material may be a grating one walks on.
- **`layer`** puts the bodies on a named layer; without it, static bodies take layer 0 and
  moving ones layer 1. A name your game's `PhysicsSettings::layer_names` lacks is an error
  when the pack's physics loads.
- A misspelled field is an error with its line and column, when you cook.

`ofeliaengine cook scene.gltf -o scene.pack` does all of this.
- `--no-colliders` leaves colliders out (and the navigation mesh made from them:
  `navigation.md`).
- The cook prints how many colliders and bodies it made, how many meshes it split into
  convex parts (and how many of those came from the cache), and how long it took. Sponza
  takes 0.3 s more and 2.4 MB of its 75 MB; Intel Sponza 2 s more and 37 MB of 1.3 GB.

### Convex parts

A simulated body has to be made of convex shapes, and a concave prop's hull would fill
it: a ball would bounce off the air under a chair or inside a mug. The cook splits such
meshes into convex parts with CoACD's method (Wei et al., SIGGRAPH 2022), rewritten in Rust
for the engine:
- **The parts follow the shape:** a U in 3 parts, a table in 5, a chair in 25, a duck in 11.
  How concave a part may stay is the rules file's `parts: (threshold: …)`, 0.05 of the
  mesh's size by default; lower makes more, tighter parts. `max_parts` caps the count.
- **Each part is at most 64 points.**
- **Time:** 0.02-3 s per mesh the first time on 16 threads (the sample models in
  `docs/research/mesh-colliders.md`), all meshes of a scene at once. The parts go into the
  derived data cache, so the next cook of an unchanged mesh loads them.
- **Open and messy meshes are fine:** a mesh that is not closed, or made of several pieces
  (a chair of boards overlapping where they join), is first rebuilt as one closed surface
  about half a percent of its size outside it.
- **See it:** `cargo run -p engine_app --release --example props_drop -- sponza.pack frames`
  drops 21 concave props, each scaled another way, into Sponza's courtyard and writes the
  frames. One limit: a dynamic prop under a non-uniformly scaled parent with a turned child
  collides with the shear, but is drawn without it (ADR 0101, 8c-4).
- **A scene of level geometry** (the default rules) splits nothing and costs nothing more.

### Models in scene files

A scene saved by the editor names each model's mesh in a glTF file (no pack). Its collider
is made the same way: by the rules beside its file (`yard.colliders.ron` next to
`yard.glb`), level geometry by default (ADR 0117).
- **Bodies:** `self.physics.sync_scene(scene)?` in `Game::before_ticks` (above) gives
  every model its body and keeps it in step:
  - a static model's body is where its entity is, made again when the entity moves;
  - a prop (dynamic or kinematic by its rules) gets `RigidBody` and `PhysicsPose` and
    becomes a root of the hierarchy where it is: the physics moves it, its scale kept;
  - a model turned off or despawned loses its body; one whose collider is still being
    cooked gets it once it comes;
  - it visits only the models that changed, and allocates nothing while nothing does.
- `player.assets_mut().collider(model.mesh)` gives the collider itself:
  `ColliderState::Made` with the collider in the mesh's space and its body's settings
  (motion, layer, friction…), `Coming` while the file is cooked in the background
  (convex parts can take seconds), `None` when nothing collides.
- A built-in mesh's collider is its own shape: the cube a box, the sphere a sphere.
- The editor draws them (View: Colliders, Alt+C), colored by motion.

### The player's own physics world

A game that runs no physics of its own (one made of scripts, say) can let the player run
one: `PlayerSettings { physics: Some(PhysicsSettings::default()), .. }`. The player then:
- loads the pack's bodies into it, and keeps the scene's models and brushes in step before
  every frame's ticks (`sync_scene`);
- steps it after each `Game::tick`, and moves its bodies' entities smoothly between steps;
- lets scripts query it (`physics.raycast`), and sound travel through it.

`player.physics_mut()` reaches it, to add bodies of the game's own. A game that owns a
`ScenePhysics` gives it to its scripts instead: `fn physics(&self) -> Option<&ScenePhysics>
{ Some(&self.physics) }` in its `Game`.

**Bodies name their entities.** A model's body, a brush group's and a pack's moving body
carry their entity in their user data: a query's hit gives it back
(`engine_physics::ecs::user_data_entity(hit.user_data)`, or `physics.entity_of(body)`).
Give a body of your own `user_data: entity_user_data(e)` and scripts see it as `e`.

### Loading them

```rust
use engine_app::{Player, ScenePhysics};
use engine_physics::PhysicsSettings;

let mut physics = ScenePhysics::new(&PhysicsSettings::default())?;
let bodies = player.load_physics(&mut physics)?;  // after Player::new
// Game::tick, after your logic:   physics.step(world, dt)?;
// Game::present:                  physics.interpolate(world, alpha);
// Queries, characters, your own bodies:  physics.physics().raycast(...)
```

- **Level geometry loads as a copy.** Shapes are restored from Jolt's binary form: Intel
  Sponza's 3.7 million triangles in 5.5 ms, where building them would take 3.9 s.
- **Props move their meshes.** A dynamic or kinematic body gets an entity (`RigidBody`,
  `PhysicsPose`, `Transform`), and the scene instances it moves become its children.
- **Layers and surfaces come along:** each body on its layer by name (pass the same
  `PhysicsSettings` you named the layers in), and `surface_of` answers for every hit.
- **A game without physics** never calls `load_physics`, and the pack's colliders are not
  even read.
- **A pack baked by another Jolt version** (after an engine update) is refused with "cook
  the pack again". Packs are cooked by the engine that runs them.

### Any scale

Scale a scene, a prop or a level however you like, after the colliders are made: uniform,
non-uniform, mirrored. Nothing is regenerated and nothing errors.
- **Hulls and meshes** take any scale as they are, and so do convex parts: a scaled,
  squashed or mirrored concave prop is still covered exactly by its scaled parts, with
  nothing split again.
- **Boxes** take any scale along their own axes.
- **Shapes Jolt cannot scale are rebuilt** for the scale and shared by every body scaled
  alike:
  - a squashed sphere, capsule or cylinder becomes a hull within 1 mm of the true surface;
  - a turned box stretched across its axes becomes the exact hull of its corners.
- **A shear** (a non-uniform scale on a parent over a rotated child) is baked into the
  collider by the cook.
- **A zero scale** turns the collider off.

### Blockout brushes

A scene's brushes ([blockout.md](blockout.md)) collide through the same world, with its
models (below): one call in `Game::before_ticks`, which the player calls every frame
before the ticks with the scene's parts outside the world:

```rust
fn before_ticks(&mut self, scene: &mut SceneParts<'_>) -> Result<(), AppError> {
    self.physics.sync_scene(scene)?; // the brushes' bodies and the models'
    Ok(())
}
```

`sync_blockout(world, blockout)` alone does the brushes' part.

- **A static body per brush group,** its brushes' pieces as sharp convex hulls in one
  compound; a brush with `collides: false` is left out.
- **Made again only when the group changes** (a brush edited, moved, added, turned off)
  or moves, and gone with it.
- **Moving bodies set `internal_edges`** (`BodyDesc { internal_edges: true, .. }`): Jolt
  then smooths their contacts with the edges between the compound's parts, and a ball
  rolls on over the seam between two floor brushes as over one floor. Measured (ADR 0110):
  without it, or with hulls rounded the usual 5 cm, or with each piece a body of its own, a
  ball knocks 4-8 mm at every seam and loses speed; characters 3-4 mm.

### Hulls: rounded or sharp

`ShapeDesc::ConvexHull { points, convex_radius }`: how far the hull's edges and corners
are rounded. `engine_physics::HULL_RADIUS` (5 cm, Jolt's own) for props: robust contacts,
soft corners. 0 for level geometry made of hulls that meet side by side: rounded ones make
a groove at every seam.

### Editing them later

Colliders are stored as the engine's own data: points of hulls, parameters of
primitives, a stable id per shape. The editor will let you grab a hull's corners, edges
or faces, or a primitive's handles, and fix what the cook made. An edited collider is
kept as you left it; when its mesh changes, the editor tells you.

## 12. 2D physics

2D games use `engine_physics2d`, a small layer over Box2D v3:

```rust
use engine_core::math::Vec2;
use engine_physics2d::{BodyKind, Material, Shape, World2d};

let mut world = World2d::new(Vec2::new(0.0, -10.0));
let ground = world.create_body(BodyKind::Static, Vec2::ZERO, 0.0);
world.add_shape(ground, Shape::Box { half_extents: Vec2::new(50.0, 1.0) }, Material::default())?;
let player = world.create_body(BodyKind::Dynamic, Vec2::new(0.0, 4.0), 0.0);
world.add_shape(player, Shape::Circle { radius: 0.5 }, Material::default())?;

world.step(1.0 / 60.0, 4); // in your fixed update; 4 substeps is Box2D's default
let position = world.transform(player)?.position;
for contact in world.contact_begins() { /* contact.a, contact.b */ }
```

- Units are metres, kilograms and seconds. Keep moving objects between 0.1 and 10 m;
  scale pixels to metres, for example 1 m = 32 px.
- It supports boxes, circles and capsules; static, kinematic and dynamic bodies;
  velocities, teleports, kinematic targets (`move_kinematic`) and begin-contact events.
- Queries:
  - `raycast` / `raycast_all`;
  - `cast_shape` / `cast_shape_all` (a `Shape` swept along a translation);
  - `overlap_shape`.
  - A `QueryFilter2d` takes a layer mask and a body to ignore. Put shapes on layers with
    `add_shape_on_layer` (0..64; `add_shape` uses layer 0).
- **Characters** use `Mover2d`, a capsule moved by your code (Box2D 3.1's mover):

  ```rust
  use engine_physics2d::Mover2d;

  let mut hero = Mover2d::new(Vec2::new(0.0, 0.0), 1.8, 0.3); // feet, height, radius
  // Every fixed update, before world.step:
  let mut v = Vec2::new(input_x * 5.0, if hero.ground.is_some() { 0.0 } else { hero.velocity.y });
  v.y -= 10.0 * dt;
  if let Some(ground) = hero.ground { v += ground.velocity; } // ride platforms
  world.move_mover(&mut hero, v, dt);
  ```

  - It slides along walls and does not walk up slopes steeper than `max_slope` (50°).
  - It reports the ground it stands on and snaps down slopes and small steps
    (`snap_to_ground`).
  - It rests within Box2D's 5 mm contact slop of the ground.
  - Unlike 3D characters it does not climb steps (its round bottom rolls over small
    ledges) and does not push bodies.
  - It is plain data: copy it to save it.
- Joints, polygons, sensors and world snapshots come when a game needs them. Ask for
  them.
- It runs on one thread. The same build with the same calls gives the same result.
