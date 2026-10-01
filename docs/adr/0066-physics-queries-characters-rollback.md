# 0066. Physics for games: shape queries, characters, kinematic moves, rollback snapshots, ECS sync

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0018](0018-physics-interface-and-jolt.md) (the interface, Jolt),
  [ADR 0028](0028-physics-scope.md) (Jolt stays; 2D on Box2D),
  [ADR 0017](0017-ecs.md) (ECS, snapshots)
- Research: [`docs/research/physics-queries-and-characters.md`](../research/physics-queries-and-characters.md)

## Context

A core audit found what a game needs from physics beyond stepping bodies, and what the
interface of ADR 0018 lacked:
- swept shapes ("will this capsule fit through?", melee and projectile sweeps);
- every hit along a ray, not just the closest;
- overlaps of arbitrary shapes;
- a character controller;
- kinematic bodies moved by target (lifts, doors, moving platforms);
- saving and restoring the world for rollback netcode (ADR 0028's first trigger for
  reopening our own solver is "Jolt's save/restore too slow for the tick budget": this
  ADR measures it);
- helpers to tie bodies to ECS entities with interpolation from the fixed step.

ADR 0018 listed shape casts, the character controller and save/restore as follow-ups.
The 2D layer (ADR 0028) had no queries at all.

## Options

### Shape casts: what the caller passes

1. **Inline shapes** (`Sphere { radius }`, `Capsule {..}`…) per call. Convenient, but Jolt
   needs a `Shape` object: one would be created per call (an allocation), or a cache
   would be kept per size.
2. **A `ShapeId` created once** (chosen), as Jolt, PhysX and Unity's shape assets do. Any
   convex shape, including hulls; no allocation per cast.

### Character controller

1. **Jolt's `Character`** (a rigid body): it moves only with the solver, floats and
   bounces on stairs (Jolt's own documentation recommends `CharacterVirtual` for
   players).
2. **Our own controller over shape casts** (Godot-style): runs on any backend, but it is
   months of tuning (stairs, slopes, platforms, pushing) that Jolt already has.
3. **Jolt's `CharacterVirtual` behind a backend-agnostic interface** (chosen):
   - the mechanism (collide and slide, slope limit, stairs, sticking to the floor,
     pushing) in the backend: `move_character(id, velocity, dt)`;
   - the policy (walk, jump, gravity, air control, platforms) in shared Rust:
     `CharacterMotor`.

### How to reach what JoltC does not expose

`CharacterVirtual` is in JoltC, but `StateRecorder` is not. JoltC's character update also
uses its process-wide temporary allocator, which our step lock guards (ADR 0018).

1. **Patch the vendored JoltC.** Breaks "third_party is unmodified", and every JoltC update
   becomes a merge.
2. **Our own C++ extension** against Jolt's C++ API (chosen): `csrc/ext.cpp`, about 400
   lines.
   - It reaches JoltC's physics system through a copy of JoltC's private four-pointer
     struct. World creation checks the copy against JoltC's own accessor and fails
     cleanly if they disagree.
   - It is built as a separate small library with the same defines and flags as Jolt, so
     editing it does not rebuild Jolt.

### Saving state

1. **Jolt's `StateRecorderImpl`** (a `std::stringstream`): allocates per save, and the
   bytes would be copied out.
2. **A `StateRecorder` over the caller's buffer** (chosen).
   - `PhysicsSnapshot` owns a `Vec<u8>` and Jolt writes straight into it.
   - On overflow only the size is counted; the buffer grows with headroom and saving runs
     again. In steady state nothing is allocated on the Rust side.
   - Characters are saved after the system, in id order.
   - A header (magic, body, constraint and character counts) refuses a foreign snapshot
     before Jolt reads it.

### ECS integration

1. **Physics owns the transforms** (entities read physics directly when rendering).
   Stutters whenever a frame runs 0 or 2 fixed steps.
2. **Fixed step plus interpolation of the last two poses** (chosen; Unity, Godot and Bevy
   do the same):
   - plain-data components: `RigidBody`, `Character`, `PhysicsPose`, `KinematicTarget`;
   - a `FixedStep` clock;
   - `PhysicsSync`: `push` kinematic targets, `pull` poses (one batched read for
     bodies), `interpolate` root `Transform`s.

### 2D

1. **Leave 2D without queries.**
2. **Box2D 3.1's queries and its mover toolkit** (chosen):
   - `b2World_CastRay`, `CastShape`, `OverlapShape`, `SetTargetTransform`;
   - the mover loop from Box2D's own sample;
   - what the sample left out: a slope limit, ground detection, snapping to the ground.

## Measurements

ref-1 (Ryzen 7 7700), `cargo bench` profile, commit of this branch, one thread unless
noted. Benchmarks: `cargo bench -p engine_physics_jolt --bench queries` and
`cargo bench -p engine_physics2d --bench queries2d`.

### Correctness (tests)

- **Casts against analytic geometry**, `tests/queries.rs`:
  - sphere, box (both orientations), upright and lying capsule, cylinder and an
    off-centre convex hull cast onto the ground give the exact fraction (to 1e-4) with
    normal +Y;
  - sphere against sphere: fraction, normal and contact point of the analytic solution
    (to 1e-3);
  - every hit in order, with layer and ignore filters, rays and shapes;
  - starting inside: fraction 0, depth 0.75 (exact);
  - overlaps of rotated boxes and capsules;
  - meshes can be hit but not cast.
- **Kinematic**: the body is at its target after every step (1e-4); without a new target
  it keeps its velocity; it pushes a crate ahead of it.
- **Characters**, `tests/characters.rs`:
  - landing (feet at ground level within 3 cm; it rests at 0.0);
  - walking 6 m in 2 s (5 cm);
  - eight 0.25 m stairs climbed to 2.0 m; stopped at the first step without step-up;
  - a 30° ramp climbed on its surface; a 60° ramp refused (50° limit);
  - riding a platform that moves 1 m/s sideways and 0.25 m/s up: 2 m in 2 s, standing on
    it, ground velocity exact;
  - pushing a 20 kg crate with 400 N but not with 0 N;
  - characters block each other;
  - the proxy is hit by rays and can be ignored;
  - teleport, destroy, invalid capsules.
- **Rollback**, `tests/rollback.rs`, on a game-like world (300-body pile, 10-link chain, a
  kinematic platform, two characters, scripted input):
  - save → 60 frames → restore → 60 frames: identical `state_hash` and identical snapshot
    bytes, on one thread and on four;
  - a 10-frame rollback from a ring of snapshots reproduces every frame's two hashes;
  - a fresh world agrees too;
  - another world's snapshot is refused (one body more, empty, truncated).
- **ECS**, `tests/ecs_sync.rs`:
  - 50 Hz physics under 144 Hz frames: 49–50 steps in a second;
  - drawn poses always between the last two steps;
  - the kinematic target is followed exactly;
  - ECS and physics rolled back together re-run identically.
- **Zero allocations**, `tests/zero_alloc.rs`: Rust's global allocator and Jolt's own hooks
  (counted by the extension), 100 rounds of seven queries and 100 character moves:
  **0 and 0**.
- **2D**, `tests/queries2d.rs`, `tests/zero_alloc2d.rs`:
  - casts within Box2D's 5 mm slop of the analytic answers;
  - every hit, layers and ignore; overlaps;
  - kinematic targets: exact position; angle within 2%, Box2D's rotation integration;
  - the mover lands, walks, stops a radius before a wall, climbs 30°, refuses 60°, rides a
    platform, snaps down a ledge (4 instead of 11 frames airborne);
  - deterministic;
  - 0 allocations over 240 frames.

### Queries (3D, Jolt, over a settled 1 000-body pile, 4 096 casts 100 m down)

| query | µs per query | queries per µs |
|---|---:|---:|
| ray, closest | 0.145 | 6.9 |
| ray, every hit | 0.156 | 6.4 |
| sphere cast, closest | 0.363 | 2.8 |
| capsule cast, closest | 0.391 | 2.6 |
| box cast, closest | 0.470 | 2.1 |
| convex hull cast, closest | 0.488 | 2.0 |
| capsule cast, every hit | 0.455 | 2.2 |
| capsule overlap | 0.652 | 1.5 |

2D (Box2D, 1 000 boxes and circles): ray 0.34, every hit 1.12, circle cast 0.77, capsule
0.88, box 0.87, overlap 0.94 µs.

### Character moves

| characters | µs per move | µs per frame |
|---:|---:|---:|
| 1 | 1.9 | 1.9 |
| 16 | 3.2 | 51 |
| 128 | 7.0 | 895 |

- The growth is Jolt's `CharacterVsCharacterCollisionSimple`: every character checks
  every other's bounding box, several times per move (about 40 ns each). It is fine up
  to dozens of characters; crowds need a grid (follow-up).
- 2D mover: 1.7 µs per move (p99 3.6).

### Save and restore (Jolt, one thread)

| bodies | state | snapshot | save | restore | ns per body (both) | Jolt allocations save / restore |
|---:|---|---:|---:|---:|---:|---|
| 1 000 | falling (few contacts) | 134 KiB | 42 µs | 54 µs | 95 | 11 / 11 |
| 1 000 | landed (awake, in contact) | 456 KiB | 260 µs | 199 µs | 458 | 2 242 / 11 |
| 1 000 | settled (mostly asleep) | 486 KiB | 272 µs | 213 µs | 485 | 2 590 / 11 |
| 10 000 | falling | 1.3 MiB | 390 µs | 589 µs | 98 | 26 / 15 |
| 10 000 | landed | 5.2 MiB | 5.6 ms | 2.4 ms | 802 | 27 608 / 15 |
| 10 000 | settled | 5.6 MiB | 6.1 ms | 2.7 ms | 877 | 31 277 / 15 |

- **Contacts dominate.**
  - Bodies alone cost about 40 ns each to save (the "falling" rows). The contact cache
    (every touching pair's manifolds with their impulses, sleeping ones included) is
    70–76% of the bytes once bodies touch.
  - Saving it sorts every pair and every pair's manifolds, and makes one temporary array
    per pair: the Jolt-side allocations above.
- **Routing those temporaries to a bump arena** (tried, not kept): 8–14% faster (10 000
  settled 6.0 → 5.5 ms). Not worth an allocator trick inside Jolt's hooks; the sorting
  and the hash-map walk are the cost.
- **Hash of a snapshot:** 19–20 GB/s (25 µs for 1 000 bodies, 0.3 ms for 10 000).
- **Rollback budget:**
  - With 1 000 contacting bodies, saving every frame costs 0.27 ms, and a full 8-frame
    rollback costs one restore (0.2 ms) plus 8 steps. That is comfortably within 60 Hz.
  - With 10 000 contacting bodies, saving costs 5.6–6.1 ms per frame. That is ADR 0028's
    first trigger for such worlds.
  - Rollback games (fighting, sports, small arenas) usually have tens to hundreds of
    dynamic bodies, so this is reported, not acted on (questions in the PR).
- The physics step benchmark is unchanged (1 000 pile 0.827 ms, 5 000 pile 3.83 ms,
  batched ray 0.14 µs; ADR 0018: 0.853, 4.16, 0.14).

## Decision

**`engine_physics`:**
- `QueryFilter` gains `ignore: Option<BodyId>`, with builders: `layers`, `with_sensors`,
  `ignoring`.
- `ShapeCast` / `ShapeHit`; `raycast_all`, `cast_shape`, `cast_shape_all`,
  `cast_shape_batch`, `overlap_shape`. Results go into caller buffers, sorted by
  fraction then body.
- `move_kinematic(body, position, rotation, dt)`.
- `character`:
  - `CharacterDesc` (a feet-anchored capsule, up, slope limit, step height, ground snap,
    mass, push force, layer, optional proxy layer, user data);
  - `CharacterState` (pose, velocity, `GroundState`, ground normal, point, body and
    velocity, proxy);
  - `CharacterContact`;
  - `create_character`, `destroy_character`, `move_character`, `character`,
    `set_character_transform`, `character_contacts`;
  - `CharacterMotor`, a stateless velocity policy after Jolt's sample.
- `PhysicsSnapshot`, `save_state`, `restore_state`, `PhysicsError::SnapshotMismatch`.
  `state_hash` now covers characters too.
- `ecs`: `RigidBody`, `Character`, `PhysicsPose`, `KinematicTarget`, `FixedStep`,
  `PhysicsSync`.
- New dependencies: `engine_ecs` and `bytemuck`, already in the tree (ADR 0017).

**`engine_physics_jolt`:**
- Queries through JoltC's collector callbacks.
- `csrc/ext.cpp`:
  - characters (`CharacterVirtual` + `ExtendedUpdate`, a per-world temporary allocator,
    per-world character ids, character-versus-character collision, ground velocity
    refreshed after each step);
  - state save/restore;
  - a counter of Jolt's heap allocations (`JoltBackend::jolt_allocations`).

**`engine_physics2d`:**
- Capsules, query layers (`add_shape_on_layer`), `QueryFilter2d`, `Hit2d`;
- `raycast`, `raycast_all`, `cast_shape`, `cast_shape_all`, `overlap_shape`,
  `move_kinematic`;
- `Mover2d` with `move_mover`: Box2D's mover loop, a slope limit, ground detection and
  snapping. Plain data, so a game rolls it back by copying.
- Box2D has no world save/restore ("no rollback determinism", Catto 2024): not offered
  in 2D.

## Consequences

**Easier:**
- Character games on the interface, with no backend code in game logic.
- Rollback netcode for small and medium physics worlds: a ring of snapshots, a hash for
  desync checks, bit-identical re-simulation.
- Physics in the ECS, smooth at any frame rate.

**Rules for games** (`docs/dev-guide/physics.md` §§4–9, §11):
- Characters move before the step with the step's `dt`.
- A snapshot restores only into the world it came from, with the same bodies,
  constraints and characters; configuration (materials, gravity modes and fields,
  layers) is the game's to restore. These are Jolt's own rules.
- A failed restore of damaged bytes can leave the world partly restored: restore a good
  snapshot after it.
- The proxy body pushes like a kinematic body: give it a layer.
- Entities with physics components are hierarchy roots.

**Revisit:**
- Character crowds (more than about 50): a grid for character-versus-character pairs,
  instead of Jolt's all-pairs list.
- Large rollback worlds: save only bodies that can change (a `StateRecorderFilter` for
  sleeping islands), or save every second frame and re-simulate more. Measure against
  P2 if a game needs it (ADR 0028, trigger 1).
- Queries run on the owning thread (`&self` with per-world filter state); parallel batched
  queries (AI) need per-thread filter objects: when a game needs them.
- The JoltC struct copy in `ext.cpp` must be rechecked on every JoltC update. The layout
  check fails world creation cleanly if it is wrong.
- 2D movers do not step up or push bodies; 2D save/restore would need our own snapshot
  of Box2D state, with warm-starting lost.
