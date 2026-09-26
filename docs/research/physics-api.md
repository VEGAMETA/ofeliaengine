# Physics interface and the Jolt reference backend

Checked on 2026-09-26. CLAUDE.md §8 plans physics in three steps:
- **P0:** a `PhysicsBackend` trait with shared types;
- **P1:** a Jolt backend through the JoltC C wrapper, as reference and fallback;
- **P2:** our own solver.

This note covers P0 and P1. No engine code was copied: the vendored libraries are compiled
as-is, and the bindings are written from `joltc.h`.

## Sources

| Source | Version | License | Read |
|---|---|---|---|
| Jolt Physics | v5.6.0 (`e77f175595e6`, 2026-07-11) | MIT | `Docs/Architecture.md` (bodies, shapes, layers, listeners, determinism, rollback, multiple systems), `Build/CMakeLists.txt` (options, MSVC flags), `PerformanceTest` scenes |
| JoltC | `886e088675ba` (2026-07-13, builds Jolt v5.6.0) | MIT | `joltc.h` (API), `joltc.cpp` (ownership, callbacks, temporary allocator) |
| J. Rouwe, "Architecting Jolt Physics for Horizon Forbidden West" | GDC 2022 | talk | body locking, job system, broad phase |
| Rapier / Parry | docs | Apache-2.0 | handle sets, query pipeline, event collectors |
| Box2D v3 | docs and blog (E. Catto) | MIT | id handles, events as arrays after the step, sub-stepping |
| PhysX 5 | docs | BSD-3 | scene queries, filter shaders |

## What the others do and what we take

1. **Handles, not pointers** (all of them).
   - Jolt: a `BodyID` with a sequence number; Box2D v3: `b2BodyId`; Rapier: generational
     handles.
   - Ours: opaque `BodyId`, `ShapeId` and `ConstraintId` newtypes over `u64`, defined by
     the backend.
2. **Shapes are shared, immutable resources.**
   - Jolt: ref-counted `Shape`; Rapier: `SharedShape`.
   - `create_shape` once, used by many bodies.
3. **Events are arrays read after the step** (Box2D v3), not callbacks.
   - Jolt calls its contact listener from worker threads in no particular order
     (Architecture.md, "Deterministic Simulation").
   - The Jolt backend writes into a preallocated array through an atomic index (no locks,
     no allocation), then sorts by `(kind, a, b)` and removes per-sub-shape duplicates.
   - Game code never runs on physics threads.
4. **Layers.**
   - Jolt: object layers mapped to broad-phase layers, with pair filters. PhysX: filter
     shaders. Box2D: category and mask bits.
   - Ours: up to 32 layers with a symmetric collision matrix. The Jolt backend splits each
     into a static and a moving object layer and maps them to two broad-phase layers, so
     static-static pairs never reach the broad phase (Jolt's recommended setup).
5. **Gravity is first-class** (§8). No reference engine has fields built in:
   - Jolt has world gravity and a per-body factor;
   - Box2D and Rapier have world gravity and a per-body scale;
   - Unity and Unreal leave planets to game code.

   Ours: `GravityMode::{World, Scaled, Custom(vector), Fields}` plus `GravityField::{Point,
   Box}`. The Jolt backend applies custom and field gravity in a step listener at every
   collision step. Jolt calls it before integration, which is correct under sub-stepping.
   The backend changes velocity through the no-lock body interface, and only for awake
   bodies, the same way Jolt's own gravity leaves sleeping bodies alone.
6. **Batched queries** (§8).
   - `raycast_batch` with the same filter; the default loops over `raycast`.
   - A parallel version belongs to the engine side once Jolt's jobs run on our job system.
7. **Determinism.**
   - Jolt: the same binary and the same API call order give the same result; it offers a
     cross-platform option (about 8% slower).
   - Ours:
     - `state_hash` (FNV-1a over every body's bits in id order);
     - the test runs a 300-body pile twice single-threaded, twice with four threads, and
       across thread counts, and gets identical hashes;
     - Jolt's `CROSS_PLATFORM_DETERMINISTIC` is a cargo feature.
8. **Save and restore for rollback** (Jolt `SaveState` / `RestoreState`): not in P0's
   trait yet; it comes with the network work (§13).

## JoltC findings

- **`JPH_PhysicsSystem_Destroy` also deletes** the broad-phase layer interface and both
  layer filters passed at creation. The system owns them. Deleting them again crashed on
  the first run (found with gdb).
- **One temporary allocator for every world.**
  - JoltC passes a single process-wide `TempAllocatorImpl` to every
    `PhysicsSystem::Update`. It is a stack allocator and not thread-safe, so two worlds
    stepping at once on different threads abort.
  - The backend serializes steps with a process-wide lock. With one world it is never
    contended.
  - A per-world allocator needs a JoltC API addition (an upstream contribution candidate).
- **Callbacks use global procedure tables** (`JPH_ContactListener_SetProcs` etc.) with
  per-instance user data. The backend installs them once and passes each world's state
  as user data.
- **Job system.**
  - JoltC offers `JPH_JobSystemCallback_Create` (queue callbacks over Jolt's
    `JobSystemWithBarrier`), but our `engine_core::jobs` is fork-join only, with no
    detached spawn.
  - P1 uses Jolt's thread pool (`threads = 0` runs on the calling thread).
  - Routing Jolt jobs through our pool needs a detached-spawn primitive in
    `engine_core::jobs`: a follow-up with its own measurements.

## Build

The vendored Jolt (153 `.cpp` files, 5.7 MB) and JoltC are compiled by the crate's build
script with `cc`, not CMake:
- the same defines for both: 32-bit object layers (JoltC's requirement), SSE4.1/4.2 (our
  x86-64-v2 baseline; Jolt has no run-time dispatch, so no AVX2);
- no RTTI or exceptions;
- MSVC flags copied from Jolt's CMake (`/Zc:__cplusplus /Zc:inline /fp:except- /fp:fast`,
  or `/fp:precise` with the cross-platform option);
- always optimized, also in dev builds.

Build time: 90 s on 4 container cores. Test scenes and benchmarks are in
`engine_physics::scenes` and `engine_physics_jolt/benches/physics.rs`.
