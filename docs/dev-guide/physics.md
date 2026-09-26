# Physics: bodies, gravity, queries, determinism

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
  cylinder, convex hull, triangle mesh (for static level geometry).
- **Bodies** are `Static` (never move), `Kinematic` (moved by you, push others) or
  `Dynamic` (simulated).
- `MotionQuality::Continuous` stops fast, small bodies (bullets) from tunneling.

## 2. Layers

`PhysicsSettings::layers` and `collides` define up to 32 layers and which pairs collide.
The default has layer 0 for static geometry and layer 1 for moving objects. Queries take a
`QueryFilter` with a layer mask: for example, ignore the player's own layer when casting
from the player.

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

- `raycast(&Ray { origin, direction }, filter)`: the closest hit, with point, normal,
  fraction and the body's `user_data`. The direction's length is the range.
- `raycast_batch(&rays, filter, &mut hits)`: many rays at once (AI line of sight,
  sensors).
- `overlap_sphere(center, radius, filter, &mut bodies)`: everything touching a sphere
  (explosions, pickups).

Sensors (`BodyDesc::sensor`) report overlaps but do not collide. Queries skip them unless
`QueryFilter::sensors` is set.

## 5. Events

After each `step`, `contact_events()` lists `Begin` and `End` of contacts:
- `a < b`;
- sorted, so the order is the same every run;
- `sensor` set when either body is a sensor.

Read them in your fixed update. Your code never runs on physics threads.

## 6. Determinism

Same build, same inputs, same order of API calls: same results, on any number of threads
(tested in CI). `state_hash()` hashes every body's state; compare it between runs or
between a client and a server to detect divergence. For identical results across
compilers and CPUs, build with the `cross-platform-deterministic` feature (about 8%
slower).

## 7. Performance

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
- On ref-1 (8 cores, 16 threads) a 5 000-body pile gains another 27% from the second
  hardware thread of each core; a 1 000-body pile stops gaining at 8 threads. Measure
  your scene with `cargo bench -p engine_physics_jolt --bench physics`, which sweeps 1 to
  16 threads and shows the automatic choice.
