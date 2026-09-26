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
- If nothing moved and the hierarchy did not change, the step costs almost nothing.
  If anything moved, every world transform is recomputed: about 2 ms per 100 000
  entities on the 4-core test machine.
- A child without both components cuts off its own children: they are not updated.
