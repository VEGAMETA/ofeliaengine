# 0028. Physics scope: Jolt stays, own 3D solver deferred; 2D on Box2D v3

- Status: Accepted (team decision, 2026-09-26)
- Date: 2026-09-26
- Amends: [ADR 0018](0018-physics-interface-and-jolt.md), CLAUDE.md §1, §2, §8, §17

## Context

CLAUDE.md §8 planned our own Rust physics (P2) as a parallel track, measured against
Jolt, which is the reference and fallback. P0 (interface) and P1 (Jolt backend,
benchmark suite, determinism test) are done (ADR 0018).

What P1 showed:
- Jolt is fast. On ref-1 a 5,000-body pile steps in 3.99 ms on 16 threads.
- Jolt is deterministic across thread counts (test).
- Jolt already carries our non-standard gravity (fields, point attractors, per-body
  vectors) through a step listener.
- Matching its stability (stacks, joints, CCD, sleeping) is a multi-month effort. The
  acceptance bar is within 10% on the suite.

CLAUDE.md §1 left 2D physics open: our own solver or Box2D v3.

## Options

### 3D

1. **Continue P2 now.** Months of work before it can replace anything, and it competes
   with the editor, scripting and assets, without which the engine is not usable.
2. **Stay on Jolt; defer P2 with explicit reopen triggers** (chosen).

### 2D

1. **Own 2D solver** in the spirit of Box2D v3. Same cost argument as P2, smaller.
2. **Rust port** (for example Rapier 2D, Apache-2.0). A different design from Box2D v3,
   and a large dependency tree.
3. **Box2D v3 as vendored C source behind a C shim** (chosen):
   - about 40 C files, MIT, no dependencies;
   - builds with the `cc` crate in seconds;
   - the shim exposes integer ids and plain arguments, so Rust never mirrors Box2D's
     structs, which change between versions.

## Decision

- **3D: Jolt is the physics engine.** P2 (`engine_physics_native`) is deferred. It is
  reopened only on one of these triggers, each backed by a measurement:
  1. rollback netcode needs per-tick save/restore of many bodies, and Jolt's
     `SaveState`/`RestoreState` is too slow for the tick budget (to be measured when
     rollback is built);
  2. a gameplay feature Jolt cannot express through its API or a step listener;
  3. a Jolt maintenance problem (license change, abandoned upstream).
- **2D: `engine_physics2d` on Box2D v3.1.1** (vendored in `third_party/box2d`). 2D physics
  is not a priority, so the first cut is small:
  - static, kinematic and dynamic bodies; box and circle shapes;
  - stepping; transforms and velocities; begin-contact events;
  - single-threaded;
  - deterministic for the same binary (test).
  - Joints, polygons, chains, queries, sensors and Box2D's task system on our job
    system come when a game needs them.

## Consequences

- The physics roadmap is now "Jolt features we expose": character controller, queries,
  ragdolls, and debug visualization in the editor.
- The build needs a C/C++ compiler for Jolt and Box2D (already true for Jolt).
- `PhysicsBackend` stays a trait. A second 3D backend remains possible, and the
  benchmark suite and determinism tests stay as the acceptance gate if P2 is reopened.
- The 2D API is a concrete type (`World2d`), not a trait: there is no second 2D backend
  to abstract over.
