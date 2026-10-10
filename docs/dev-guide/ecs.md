# Entities and components

Game state lives in a `World`: **entities** (ids) with **components** (data). Code that
works on many entities at once runs a **query** over every entity with the components
it needs. The same data layout serves the game, the editor, scripts, saving, rollback
and networking.

## 1. Components

A component is a plain Rust type. Mark it with one line:

```rust
use engine_ecs::{bytemuck, component, pod_component};

// Plain data (numbers, vectors, entity handles, fixed arrays): copyable byte-wise.
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Velocity { x: f32, y: f32, z: f32 }
pod_component!(Velocity);

// Anything else (strings, vectors, handles to resources).
struct Name(String);
component!(Name);
```

**Prefer plain data** (`pod_component!`) for everything the simulation touches:
- only plain-data components can be saved for rollback (prediction, lockstep, replays)
  and hashed for desync checks;
- `bytemuck`'s derive checks at compile time that the type has no padding;
- when it complains, add explicit padding fields or reorder the fields.

## 2. Entities

```rust
use engine_ecs::World;

let mut world = World::new();
let player = world.spawn((Position::default(), Velocity { x: 1.0, y: 0.0, z: 0.0 }));
world.insert(player, Name("Ada".into()));       // add (or replace) a component
world.get_mut::<Velocity>(player).unwrap().y = 5.0;
world.remove::<Name>(player);                    // returns the removed value
world.despawn(player);
```

- An `Entity` stays valid until despawned. Using it afterwards returns `None` or `false`,
  never another entity's data.
- Entities with the same set of components are stored together. Adding or removing a
  component moves the entity: cheap, but not free. Do not toggle components every frame
  for flags that change every frame; use a field instead.

## 3. Queries

Create a query once (for example when your system starts) and run it every frame:

```rust
use engine_ecs::{Changed, Entity, QueryState, With, Without};

let mut movement = QueryState::<(&mut Position, &Velocity)>::new(&mut world);

// Every frame:
movement.for_each(&mut world, |(position, velocity)| {
    position.x += velocity.x * dt;
});
```

**Fetches:**
- `&T`, `&mut T`;
- `Entity` (the id);
- `Option<&T>`, for "if it has one";
- tuples of these.

**Filters** go in the second parameter:
- `With<T>` and `Without<T>`;
- `Changed<T>`: only entities in groups written since this query last ran.

**Four ways to run a query:**

| | when |
|---|---|
| `for_each` | the default |
| `iter` | when you need an iterator (`find`, `zip`, early exit) |
| `for_each_chunk` | hot loops: you get whole arrays (`&mut [Position]`, `&[Velocity]`) |
| `par_for_each` | large counts: chunks are spread over the job system's threads |

`par_for_each` gives the same result as `for_each`, as long as your closure only changes
its own entity.

## 4. Changing structure while iterating: commands

Spawning or despawning inside a query is not allowed (the query is walking the storage).
Record the change and apply it afterwards:

```rust
use engine_ecs::Commands;

let mut commands = Commands::new(); // keep it: it reuses its memory
lifetimes.for_each(&mut world, |(entity, life)| {
    if life.0 <= 0.0 {
        commands.despawn(entity);
        let debris = commands.spawn((Position::default(),));
        commands.insert(debris, Velocity::default());
    }
});
world.apply(&mut commands); // in recording order
```

## 5. Parents and children

```rust
let car = world.spawn((Position::default(),));
let wheel = world.spawn((Position::default(),));
world.set_parent(wheel, Some(car))?;     // appended after car's other children
for child in world.children(car) { /* in the order they were attached */ }
world.set_parent(wheel, None)?;          // detach
world.despawn_recursive(car);            // car and everything under it
```

The links are ordinary plain-data components (`Parent`, `Children`): rollback and
saving include them automatically.

## 6. Determinism, rollback and desync checks

The same sequence of calls always gives the same entities, the same iteration order and
the same `world.state_hash()`, on every run and machine with the same build. This is what
networking with prediction and rollback, lockstep multiplayer and replays rely on.

```rust
use engine_ecs::Snapshot;

let mut saved = Snapshot::new();
world.save(&mut saved)?;       // every entity and plain-data component
// ... simulate a few ticks with predicted input ...
world.restore(&saved)?;        // back to exactly that state
// ... re-simulate with the corrected input ...
let hash = world.state_hash(); // compare with the server's to detect desyncs
```

**Rules for deterministic game logic:**
- **Keep simulated state in plain-data components.** Snapshots refuse the others.
- **Do not base simulation decisions on `Changed` filters.** Change tracking is
  bookkeeping, and a restore marks everything changed. Use `Changed` for caching, UI,
  rendering and editors.
- **Do not iterate `HashMap`s in simulation code.** Their order is random. Use queries,
  sorted vectors or ordered maps.
- **Across different machines,** `sin`, `cos`, `exp` and friends from the standard
  library may round differently (they come from the platform's math library). Same build
  and CPU class is fine. For lockstep between different PCs, use deterministic
  replacements (planned in `engine_core`, CLAUDE.md §8).
- **With `par_for_each`,** only write to your own entity. For totals, use
  `par_for_each_chunk` with one output slot per chunk position, then add them up in order.

## 7. Performance

- **Hot loops:** `for_each_chunk` gives you arrays the compiler turns into SIMD loops.
- **Thousands of entities:** `par_for_each`. Below a few thousand, the sequential version
  is faster (thread start-up costs about as much as the work).
- **Query creation allocates;** iteration does not. Create queries once.
- **Spawning into a world that has had entities before** reuses its memory: no
  allocation.
- **`world.save` copies every entity:** about 1 ms for 100 000 entities with 108 bytes of
  components each, on the 4-core cloud test machine. Every tick is affordable for most
  games. Keep big, rarely-changing data (level geometry) out of the simulated world.

## 8. Systems and schedules

A system declares what it touches once, then runs every tick. A `Schedule` runs systems
in the order you add them; systems that do not conflict run at the same time on the
job system.

```rust
let mut schedule = Schedule::new();
schedule.add(system(
    "move",
    |world, access| (access.query::<(&mut Position, &Velocity), ()>(world), access.read::<Dt>(world)),
    |(query, dt), ctx| {
        let dt = ctx.res(dt).unwrap().0;
        ctx.for_each(query, |(p, v)| p.0 += v.0 * dt);
    },
));
world.insert_resource(Dt(1.0 / 60.0));
schedule.run(&mut world, Some(&pool)); // or None for one thread
```

- **Order is what you write.** The result is always the same as running the systems one
  after another in add order, on any number of threads. Rollback and desync checks
  rely on this.
- **Conflicts** are per component and per resource: one system writes what another
  reads or writes. `schedule.levels()` shows which systems run together.
- **Structural changes** go through `ctx.commands()`. They are applied after the whole
  schedule, in add order. Need them earlier? Put an `add_exclusive` step between the
  systems; it gets `&mut World` and runs alone.
- A system can only use the queries and resources it declared in its own `init`.
  Using another system's handle panics.
- Writing the same component in two systems serializes them, even if they touch
  different entities. Merge them or split the component when it shows in the profiler.

## 9. Transforms

Give an entity a `Transform` (relative to its parent) and a `GlobalTransform` (world
space, computed for you):

```rust
let car = world.spawn((Transform::from_xyz(10.0, 0.0, 0.0), GlobalTransform::IDENTITY));
let wheel = world.spawn((Transform::from_xyz(1.0, -0.5, 0.0), GlobalTransform::IDENTITY));
world.set_parent(wheel, Some(car))?;

let mut transforms = TransformPropagation::new(&mut world);
schedule.add_exclusive("transforms", move |w| {
    transforms.run(w);
});
```

- Write `Transform`; read `GlobalTransform` (rendering, physics sync, audio).
- `GlobalTransform` is updated when the transforms step runs. Add it after the systems
  that move things and before the systems that read world positions.
- The step is incremental (ADR 0064): it looks only at chunks whose `Transform`,
  `Parent` or `GlobalTransform` changed, and writes a world transform only when its value
  changes. 100,000 entities in 1,000 hierarchies: one root moved 13 µs, every root moved
  0.9 ms, nothing moved 0.1 µs. Chunks whose world transforms stayed the same are not
  marked, so `Changed<GlobalTransform>` downstream (rendering, audio) skips them.
- Give things that move every frame a component of their own (a velocity, an AI state):
  they then live in their own chunks, and the chunks of still entities stay untouched. 1 %
  movers in their own archetype among 100,000: 11 µs.
- `run_full` recomputes everything (after writing `GlobalTransform` by hand, say).
- A child without both components cuts off its own children: they are not updated.

## 10. Mirroring the world elsewhere: renderers and removal logs

Systems that keep a copy of part of the world (the renderer's scene, an audio scene, a
network replicator) need what changed since their last run, including what went away:

```rust
world.track_removals::<MeshRenderer>();          // once
let mut changed = QueryState::<(Entity, &GlobalTransform, &MeshRenderer),
    Or<(Changed<GlobalTransform>, Changed<MeshRenderer>)>>::new(&mut world);
// Each frame:
world.drain_removed::<MeshRenderer>(|entity| mirror.remove(entity)); // despawned or removed
changed.for_each_readonly(&world, |(e, g, r)| mirror.update(e, g, r));
if world.resync_epoch() != seen_epoch { /* clear or snapshot restore: rebuild */ }
```

- A removal log grows until drained: track only what something drains every frame.
- `Or<(..)>` matches chunks where any of the filters' components changed.
- `get_mut_untracked` writes without marking the chunk; follow it with `mark_changed`
  when the value really changed (that is how propagation keeps unchanged chunks clean).
- Logs, change ticks and epochs are bookkeeping: snapshots, restores and the state hash
  ignore them, so determinism is unaffected.
- `World::despawn` takes the entity out of its parent's list of children; its own
  children keep a dead parent (use `despawn_recursive` for subtrees).

### Drawing entities: `MeshRenderer` and `engine_runtime`

```rust
use engine_runtime::{MeshRenderer, Runtime};

world.spawn((Transform::from_xyz(0.0, 1.0, 0.0), GlobalTransform::IDENTITY,
    MeshRenderer::new(mesh, material)));
let mut runtime = Runtime::new(world, 64.0); // ticks per second
// Each frame, after waiting for the swapchain image:
let input = sample_input();                  // as late as possible
runtime.frame(seconds_since_last_frame, &input, &mut gpu_scene,
    |world, input, dt| { /* your tick systems: movement, physics, AI */ },
    |world, input, alpha| camera.yaw = input.mouse_x * sensitivity); // every frame
// Then render: gpu_scene.add_passes(..), then the frame renderer (Cameras, below).
```

- **Ticks and frames.** The simulation runs at a fixed rate (64 Hz by default, ADR 0019);
  each frame runs the ticks that are due (at most 4), then transform propagation and
  extraction into the scene after each. Rendering interpolates moving objects between
  their last two ticks, so motion is smooth at any frame rate. Do not also call
  `PhysicsSync::interpolate`: bodies would be blended twice.
- **Moving objects cost per tick, not per frame.** With a `GpuScene`, extraction gives a
  moving object its new transform once per tick, and the GPU places it between its last two
  ticks every frame (ADR 0064's addendum): with 60,000 moving cubes, a frame without a tick
  spends 0.45 ms of CPU in all on the reference RTX machine. An object whose mirroring
  flips between two ticks (a negative scale crossing zero) jumps at the tick instead of
  passing through a flat, inside-out shape.
- **Threads.** Give the runtime and the scene your job pool:

  ```rust
  let jobs = Arc::new(engine_core::jobs::ThreadPool::builder().build()?);
  runtime.set_jobs(jobs.clone()); // propagation and extraction
  gpu_scene.set_jobs(jobs);       // the scene's per-tick work
  ```

  When thousands of things move, a tick's transform propagation, extraction and scene
  work are spread over it, with the same results as on one thread. With both, 60,000
  movers cost the reference RTX machine 1.5 ms of CPU per frame at 60 fps (4.9 ms on one
  thread with the CPU blend). Below a few thousand the work stays on the calling thread.
- **The camera is not part of the tick.** The second closure runs every frame with the
  latest input, after the ticks and right before rendering. A camera driven by ticks and
  interpolated trails the mouse by 18 ms (median at 240 FPS and 64 Hz); this way input
  reaches the frame's command buffer in 44 µs (RTX) / 109 µs (Radeon) in the example. Your
  tick systems still consume the same input for the simulation (aiming, movement).
- **One world, one simulation.** Singleplayer has no client and server (ADR 0019
  addendum).
- **Hide** an entity with `MeshRenderer::visible = 0`; despawning or removing the
  component removes its instance.
- A full example with a window: `cargo run -p engine_runtime --release --example
  game_loop`. It prints where the frame's time goes and the input latency.

### Cameras

A camera is an entity with a `Transform` and an `engine_runtime::Camera` (ADR 0072). A
`CameraView` finds the main one every frame (the active camera with the highest
`priority`) and turns it into what the frame renderer takes
([rendering.md](rendering.md#how-a-frame-is-put-together-the-frame-renderer)):

```rust
use engine_frame::{Antialiasing, Quality};
use engine_runtime::{Camera, CameraView, look_rotation};

let player_camera = world.spawn((
    Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
    Camera::perspective(70f32.to_radians(), 0.1, 1000.0)
        .with_antialiasing(Antialiasing::TAAU, Quality::Quality),
));
let mut cameras = CameraView::new(&mut world, defaults.view_settings());

// Every frame: move the camera in the presentation step, from the latest input…
runtime.frame(dt, &input, &mut gpu_scene, tick, |world, input, _| {
    world.get_mut::<Transform>(player_camera).unwrap().rotation = aim(input);
});
// …and render what it sees, in the same frame.
let main = cameras.resolve(&runtime.world).unwrap();
frame.prepare(&mut view, &main.settings, output, prepare_ctx)?;
frame.add_view(&mut fg, &mut ctx, &mut view, &scene_input, &main.camera, target)?;
```

- **The camera's fresh `Transform` renders**, not its `GlobalTransform` from the last tick:
  what the presentation step wrote reaches this frame (ADR 0019 addendum). Under a parent,
  it is placed under the parent's `GlobalTransform` (the last tick's); for a camera that
  must follow an interpolated or predicted character smoothly, place it yourself in the
  presentation step, as a root.
- **Cuts:** `camera.cut()` (teleports, a new level), or switching the main camera (another
  becomes active or gets a higher priority): the next frame starts its history anew.
- **On the camera:** field of view, near and far planes, exposure (fixed, or with
  `auto_exposure` a compensation in EV), antialiasing and render size, and `cinematic`
  for cutscenes (the only views that may get camera motion blur, ADR 0067). Everything else
  (bloom, tone curve, occlusion culling) comes from the `CameraView`'s base settings.
