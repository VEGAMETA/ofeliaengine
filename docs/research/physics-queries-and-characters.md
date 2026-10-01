# Physics for games: shape queries, character controllers, kinematic bodies, rollback

Checked on 2026-10-01. Basis of [ADR 0066](../adr/0066-physics-queries-characters-rollback.md).
The physics core (ADR 0018, ADR 0028) had rays, sphere overlaps and stepping. Real games
also need swept shapes, a character controller, scripted (kinematic) motion, and for
networked games, saving and restoring the world every frame. This note covers how Jolt (our
3D backend), Box2D v3 (our 2D backend) and the big engines do it, and what we take.

No engine code was copied. Two small pieces were adapted from MIT-licensed samples, with
attribution in `THIRD_PARTY_NOTICES.md`:
- the velocity logic of Jolt's `CharacterVirtualTest` sample (`CharacterMotor`);
- the move loop of Box2D 3.1's mover sample (`e2d_mover_move` in the 2D shim).

## Sources

| Source | Version | License | Read |
|---|---|---|---|
| Jolt Physics | v5.6.0 (vendored, `e77f175595e6`) | MIT | `NarrowPhaseQuery.h`, `ShapeCast.h`, `CharacterVirtual.h/.cpp`, `Character.h`, `StateRecorder.h`, `PhysicsSystem.cpp` (`SaveState`/`RestoreState`), `BodyManager.cpp`, `ContactConstraintManager.cpp` (manifold cache save), `BodyInterface::MoveKinematic` |
| Jolt documentation, "Architecture" | [jrouwe.github.io/JoltPhysics](https://jrouwe.github.io/JoltPhysics/), checked 2026-10-01 | MIT | "Character Controllers", "Deterministic Simulation", "Rolling Back a Simulation" |
| Jolt sample `CharacterVirtualTest` | v5.6.0 (GitHub, `Samples/Tests/Character/`) | MIT | how velocity is composed before `ExtendedUpdate` |
| JoltC | `886e088675ba` (vendored) | MIT | which of the above it exposes (casts: yes; `CharacterVirtual`: yes; `StateRecorder`: no) |
| Box2D | v3.1.1 (vendored) | MIT | `b2World_CastRay`, `b2World_CastShape`, `b2World_OverlapShape`, `b2World_CastMover`, `b2World_CollideMover`, `b2SolvePlanes`, `b2ClipVector`, `b2Body_SetTargetTransform`; `src/world.c`, `src/shape.c` |
| Box2D 3.1 mover sample | `samples/sample_character.cpp` at v3.1.1 | MIT | the mover loop (after Quake's `pmove.c`) |
| Box2D documentation, [character mover](https://box2d.org/documentation/group__character.html) and the [3.1 release post](https://box2d.org/posts/2025/04/box2d-3.1/) | checked 2026-10-01 | — | the mover's design |
| E. Catto, ["Determinism"](https://box2d.org/posts/2024/08/determinism/) | 2024-08-27 | blog | cross-platform determinism in Box2D v3; no rollback support |
| Box3D [character mover](https://box2d.org/documentation3d/md_character.html) | 0.1.0, checked 2026-10-01 | — | the same mover design in 3D (experimental) |
| Godot `CharacterBody3D` | [4.x docs](https://docs.godotengine.org/en/stable/classes/class_characterbody3d.html), checked 2026-10-01 | MIT | `move_and_slide` and its parameters |
| Unity `CharacterController` / PhysX character controllers | [Unity scripting API](https://docs.unity3d.com/ScriptReference/CharacterController.html), [PhysX 5 CCT guide](https://nvidia-omniverse.github.io/PhysX/physx/5.4.1/docs/CharacterControllers.html), checked 2026-10-01 | PhysX: BSD-3 (docs read only) | `Move`, slope limit, step offset, skin width, non-walkable mode, pushing via callbacks |
| Unreal Engine `CharacterMovementComponent`, Mover plugin, Chaos resimulation | [UE 5 documentation](https://dev.epicgames.com/documentation/en-us/unreal-engine/understanding-networked-movement-in-the-character-movement-component-for-unreal-engine), checked 2026-10-01 | proprietary (docs read only) | floor finding, step-up, based movement, saved moves; rewind history |
| GGPO | [github.com/pond3r/ggpo](https://github.com/pond3r/ggpo), `doc/DeveloperGuide.md` | MIT | `save_game_state` / `load_game_state` / `advance_frame` |
| Rapier | [serialization guide](https://rapier.rs/docs/user_guides/rust/serialization) | Apache-2.0 | whole-world snapshots via serde |
| G. Fiedler, ["Fix Your Timestep!"](https://gafferongames.com/post/fix_your_timestep/) | 2004 | article | accumulator, interpolation between the last two states |

## 1. Shape casts and overlaps

**Jolt** (`NarrowPhaseQuery`):
- `CastRay`, `CastShape` (a convex shape swept along a direction), `CollideShape` (overlap
  with contact info), `CollidePoint`.
- Results go to a *collector*: closest hit, any hit, all hits (an `Array`, which
  allocates) or a user collector. A collector's early-out fraction prunes the broad phase
  as hits come in.
- Filters at three levels: broad-phase layer, object layer, body (and sub-shape).
- `ShapeCastSettings`:
  - back-face modes;
  - `mReturnDeepestPoint`: for a shape that starts inside a body, compute the penetration
    (otherwise only a fraction 0);
  - `mUseShrunkenShapeAndConvexRadius`.
- Shapes are placed by their center of mass. `RShapeCast::sFromWorldTransform` converts
  from the shape's origin; `CollideShape` takes the center-of-mass transform directly.
- Hits arrive in no guaranteed order (Architecture.md, "Deterministic Simulation": narrow-
  phase queries are consistent but may report in different orders).
- Triangle meshes cannot be cast (only cast against).

**Box2D v3.1:**
- `b2World_CastRay`, `b2World_CastShape` (a `b2ShapeProxy`: up to 8 points plus a
  radius), `b2World_OverlapShape`, `b2World_OverlapAABB`.
- One callback per shape. Its return value: -1 skip the shape, 0 stop, a fraction clip,
  1 continue.
- Casts stop at contact within the linear slop (5 mm).
- Filters by category and mask bits.

**PhysX / Unity / Godot:**
- PhysX: `PxScene::raycast/sweep/overlap` with hit buffers sized by the caller, and
  "touching" versus "blocking" hits.
- Unity: `Physics.SphereCast`, `CapsuleCast`, `BoxCast`, the `NonAlloc` variants into
  caller arrays (added because the allocating ones caused GC spikes).
- Godot: `PhysicsDirectSpaceState3D.cast_motion` (safe and unsafe fractions),
  `intersect_shape`.

**What we take:**
- Casts take a `ShapeId` created once: zero allocations per cast, any convex shape.
- Closest and all-hits variants.
- Results into caller-owned `Vec`s, reused (the `NonAlloc` lesson).
- Sorted output (fraction, then body), so results do not depend on Jolt's traversal
  order.
- A filter with a layer mask, sensors on or off, and one ignored body (the caster or a
  character's proxy). Most "filter by body" uses are "not myself"; arbitrary predicates
  would need a callback into game code from inside the query, which we avoid.
- Initial overlaps reported at fraction 0 with the depth (`mReturnDeepestPoint`, which
  only costs time in that case).

## 2. Character controllers

### Jolt: `Character` versus `CharacterVirtual`

- **`Character`:**
  - a rigid body locked to translation, simulated with everything else;
  - reacts to bodies naturally;
  - movement only takes effect at the next step, and the solver's restitution, friction
    and penetration recovery make it float, bounce on stairs and slide on slopes;
  - needs `PostSimulation` for ground contacts.
- **`CharacterVirtual`** (what Jolt recommends for players):
  - not a body: collision queries only, run whenever the game calls `Update`;
  - collide and slide with a small constraint solver over the contacts;
  - ground states: `OnGround`, `OnSteepGround`, `NotSupported`, `InAir`;
  - slope limit; `WalkStairs`; `StickToFloor`; `ExtendedUpdate` combines the three;
  - ground velocity for moving platforms;
  - pushes dynamic bodies with impulses limited by `mMaxStrength`, and is pushed by
    kinematic ones;
  - character-versus-character collision through an interface. The provided
    `CharacterVsCharacterCollisionSimple` tests every pair, after a bounding-box check;
  - an optional inner rigid body (`mInnerBodyShape`) so rays, sensors and fast bodies see
    it;
  - its own `SaveState`/`RestoreState`: `PhysicsSystem::SaveState` does not include it.
- **Gravity:** the game sets the whole velocity, gravity included. The gravity argument of
  `Update` is only used to press on the ground body.
- **The sample `CharacterVirtualTest`:**
  - on walkable ground and not moving away from it, the new velocity is the ground's
    velocity plus input (plus a jump);
  - in the air, the old vertical velocity is kept;
  - gravity × dt is added in both cases;
  - `UpdateGroundVelocity` refreshes the platform's velocity after the physics step.

### The others

- **Box2D 3.1 mover** (experimental; Box3D has the same):
  - a capsule outside the simulation;
  - each iteration: `CollideMover` gathers planes, `SolvePlanes` finds a translation
    satisfying them, `CastMover` sweeps it ("can encroach" up to the linear slop), five
    iterations at most;
  - `ClipVector` removes velocity into the planes;
  - "soft" planes (push limits) for other movers;
  - no built-in ground state, slope limit or step-up: the sample finds the ground with a
    "pogo" spring cast;
  - the collision point of a plane is reported in the shape's body space (3.1.1).
- **Unity / PhysX CCT:**
  - `Move(displacement)` returns collision flags (sides, above, below);
  - `slopeLimit`, `stepOffset`, `skinWidth`, `minMoveDistance`;
  - no forces, and it does not push rigid bodies: games do it in `OnControllerColliderHit`;
  - PhysX's "non-walkable mode" either prevents climbing or prevents climbing and forces
    sliding.
- **Godot `move_and_slide`:**
  - the game sets `velocity`;
  - parameters: `floor_max_angle`, `floor_snap_length`, `max_slides`,
    `wall_min_slide_angle`, `floor_stop_on_slope`, `floor_constant_speed`;
  - moving platforms through `platform_floor_layers`, and `platform_on_leave` (keep the
    platform's velocity when leaving);
  - `is_on_floor()`;
  - no pushing of rigid bodies.
- **Unreal `CharacterMovementComponent`:**
  - movement modes (walking, falling, swimming, flying, custom);
  - capsule sweeps; floor finding by a sweep down;
  - `MaxStepHeight` step-up; walkable floor angle; perching on ledges;
  - "based movement" on moving bases;
  - client prediction with saved moves, replayed on correction.
  - UE 5.4+ adds the experimental Mover plugin, built for rollback with the Network
    Prediction plugin.

### What we take

- **Mechanism in the backend, policy in shared code** (Godot's split, Jolt's mechanism):
  - `PhysicsBackend::move_character(id, velocity, dt)`: collide and slide, slope limit,
    stairs, stick to floor, pushing; returns the state;
  - `CharacterMotor`: the velocity policy (walk, jump, gravity, air control, platform
    following), plain Rust and stateless, so it works with any backend and needs nothing
    extra for rollback.
- **Jolt backend:** `CharacterVirtual` + `ExtendedUpdate`, through our own small C++
  extension, because JoltC does not expose `SaveState` and its update uses JoltC's global
  temporary allocator (which needs our step lock).
  - A temporary allocator of our own per world, so character moves need no lock.
  - Character ids set per world, so snapshots are identical across worlds: Jolt's
    default ids come from a process-wide counter.
  - `CharacterVsCharacterCollisionSimple` per world.
  - The ground velocity refreshed after every step.
- **The capsule stands on its position** (the feet), as in Jolt's sample; total height
  as in Unity and Godot.
- **The proxy body is opt-in, with its own layer.**
  - Jolt's inner body is kinematic: on a layer that collides with dynamic bodies it
    pushes them with infinite mass, beside the character's own pushing limited by
    `max_push_force`.
  - Making it opt-in, on a layer the game chooses, keeps "push force" meaningful.
- **2D:** Box2D's mover loop, plus what the sample left to the game:
  - a slope limit: steep upward-facing planes are turned into vertical walls with the
    same push, so the solver slides the capsule down instead of up;
  - ground detection from the most upward-facing walkable plane;
  - snapping down to the ground;
  - the plane point converted to world space.
  - No step-up (the rounded bottom rolls over small ledges) and no pushing. The mover is
    plain data, so a 2D game rolls it back by copying it.

## 3. Kinematic bodies

- **Jolt:** `BodyInterface::MoveKinematic(target position, rotation, dt)` sets linear and
  angular velocity so the body arrives at the end of the step. Unlike
  `SetPositionAndRotation` (a teleport), contacts see the velocity: pushed bodies, and
  characters riding the body, move with it.
- **Box2D 3.1:** `b2Body_SetTargetTransform`, the same idea. Box2D integrates rotation
  approximately (normalized first order): a target angle is reached within about 2%
  per step.
- **PhysX / Unity:** `setKinematicTarget` / `Rigidbody.MovePosition`: the same idea.
- **What we take:** `move_kinematic(body, position, rotation, dt)` in both backends, and a
  `KinematicTarget` component that the ECS sync applies before each step.

## 4. State save/restore and rollback

**Rollback netcode** (GGPO):
- every frame the game saves its state (`save_game_state`) and advances;
- when a late input arrives, it loads the state of the frame the input belongs to
  (`load_game_state`) and re-simulates up to the present, typically up to 8 frames, all
  within one frame's budget.
- Requirements:
  - save and restore fast and without allocation;
  - a checksum for desync detection;
  - bit-identical re-simulation.

**Jolt's `StateRecorder`:**
- `PhysicsSystem::SaveState(recorder, EStateRecorderState, filter)` writes:
  - global state (previous step's delta time, gravity);
  - per body: id, active flag, position, rotation, velocities, sleep timers;
  - the contact cache (body pairs and manifolds with their accumulated impulses);
  - constraint state.
- `RestoreState` reads it back and updates the broad phase's bounds.
- Only state that the simulation changes is saved (Jolt's docs): configuration (friction,
  motion quality, layers…) and the set of bodies are not.
- Restoring after adding or removing bodies fails. Body ids must stay the same; Jolt
  recommends `CreateBodyWithID` instead of destroy and re-create.
- The recorder is a stream interface (`WriteBytes`, `ReadBytes`, `IsEOF`, `IsFailed`), so
  the caller's buffer can back it.
- A *validation* mode compares the state byte by byte while restoring, to find
  non-determinism.
- **Determinism:**
  - same binary + same API calls in the same order → identical results, regardless of
    thread count;
  - `JPH_CROSS_PLATFORM_DETERMINISTIC` extends this across compilers and CPUs (about 8%
    slower, Jolt's figure);
  - the saved bytes are deterministic (contact pairs and manifolds are sorted before
    writing), so a hash of the snapshot is a strong desync check.
- `CharacterVirtual::SaveState`/`RestoreState` cover a character (position, rotation,
  velocity, ground state, contacts that collided).
- **Cost** (read in the source, measured in ADR 0066): saving the contact cache sorts all
  body pairs and makes one temporary array per pair. A settled pile has many persistent
  contacts (sleeping bodies keep their manifolds), so the snapshot and the time grow with
  contacts, not bodies.

**Box2D v3:** cross-platform deterministic since 3.1, but "does not have rollback
determinism": no save/restore (Catto, "Determinism"). Its replays re-simulate from the
start. A 2D game can roll back by rebuilding the world, or later through our own snapshot
of Box2D's bodies (not done; Box2D's internal solver state, such as warm-starting
impulses, would be lost, so the result would differ slightly).

**The others:**
- **Unreal (Chaos):** resimulation keeps a rewind history of particle states inside the
  solver (`FRewindData`) for a fixed number of frames.
- **Rapier:** snapshots the whole pipeline with serde, which allocates.
- **PhysX:** has no snapshot.

**What we take:**
- `save_state(&mut PhysicsSnapshot)` / `restore_state(&PhysicsSnapshot)`, with the
  snapshot owning a reusable byte buffer:
  - a ring of snapshots, one per frame of the rollback window, allocated once;
  - Jolt's recorder writes straight into it. When it would overflow, the size is
    counted, the buffer grows with headroom, and saving runs again;
  - steady state: zero Rust allocations.
- Our header (magic, body, constraint and character counts) refuses another world's
  snapshot before Jolt touches anything.
- `PhysicsSnapshot::hash()` is the 64-bit `StateHasher` over the bytes (about 15–18 GB/s).
  It covers more than `state_hash()`: sleep state, contacts and constraints too.
- Jolt's own temporary allocations inside `SaveState` are counted (a wrapper around
  Jolt's allocation hooks) and reported in ADR 0066, not hidden.
- Jolt's rules apply: the set of bodies and characters must be the same, and
  configuration is not restored.

## 5. ECS integration and the fixed step

- **Fixed step with interpolation** (Glenn Fiedler, "Fix Your Timestep!", gafferongames.com,
  2004; the same in Unity's `FixedUpdate` with Rigidbody interpolation, Godot's physics
  interpolation, and Bevy's `FixedUpdate`):
  - the simulation runs at a fixed rate from an accumulator;
  - rendering blends the last two physics states by the leftover fraction;
  - the number of steps per frame is capped, so a long frame does not snowball into
    more steps the next frame (the "spiral of death").
- **What we take:**
  - `FixedStep` (accumulator, step count, `alpha`);
  - plain-data components: `RigidBody`, `Character`, `PhysicsPose` (the last two poses),
    `KinematicTarget`;
  - `PhysicsSync` with `push` (kinematic targets before the step), `pull` (poses after
    the step, one batched read for bodies) and `interpolate` (root `Transform`s each
    frame; normalized lerp of rotations).
- Every component is plain data, so ECS snapshots (ADR 0017) carry them next to the
  physics snapshot: the test rolls both back together.
