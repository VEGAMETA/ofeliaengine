# 0018. Physics interface (P0) and the Jolt reference backend (P1)

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/physics-api.md](../research/physics-api.md)

## Context

CLAUDE.md §8 fixes the plan:
1. **P0:** a `PhysicsBackend` trait so Jolt and our own physics are interchangeable;
2. **P1:** Jolt through JoltC with our own Rust bindings, as the comparison baseline and
   fallback;
3. **P2:** our own solver;
4. a decision by benchmarks.

It also asks for:
- non-standard gravity as a first-class API feature;
- batched queries;
- determinism with a CI test;
- a benchmark suite modeled on Jolt's `PerformanceTest`.

## Options

### Interface shape

1. **Generic, statically dispatched backend.** Zero call overhead, but switching backends
   (benchmarks, the P2 decision, a runtime fallback) means recompiling or monomorphizing
   everything.
2. **An object-safe trait** (chosen). One virtual call per operation is negligible next
   to what each operation does. Bulk paths (`transforms`, `raycast_batch`) take slices,
   so per-body overhead stays out of hot loops.

### Events

1. **Callbacks into game code.** Jolt calls from worker threads in a
   non-deterministic order.
2. **Sorted arrays after the step** (chosen, as in Box2D v3).

### Getting Jolt

1. **`joltc-sys` / `rolt` from crates.io.** Jolt 5.0.0 from 2024, built with CMake.
2. **Vendored Jolt v5.6.0 and JoltC** (chosen). Compiled by `cc` in our build script with
   Jolt's own flags, and bound by hand-written FFI whose struct layouts are checked
   against the C compiler.

### Jolt's worker threads

1. **Jolt's thread pool** (chosen for P1). `threads = 0` runs on the calling thread.
2. **JoltC's callback job system over `engine_core::jobs`.** Needs a detached-spawn
   primitive our fork-join pool does not have yet. Follow-up.

## Decision

- **`engine_physics`:**
  - handles (`BodyId`, `ShapeId`, `ConstraintId`);
  - `ShapeDesc` (sphere, box, capsule, cylinder, convex hull, triangle mesh);
  - `BodyDesc` (motion type, continuous collision, layer, material, damping, gravity mode,
    mass override, sensor, sleeping, user data);
  - `ConstraintDesc` (fixed, point, hinge, distance, slider);
  - `GravityMode` (world, scaled, custom vector, fields) and `GravityField` (point
    attractor, box volume);
  - queries: `raycast`, `raycast_batch`, `overlap_sphere` with a `QueryFilter` (layer
    mask, sensors);
  - sorted `ContactEvent`s, `PhysicsStats`, `state_hash`;
  - `scenes`: ground, pyramid, pile, chain, orbits, ray grid, shared by tests and
    benchmarks of every backend.
- **`engine_physics_jolt::JoltBackend`** implements the trait over vendored Jolt 5.6.0 and
  JoltC:
  - layers split into static and moving object layers, two broad-phase layers;
  - contact events through a lock-free preallocated array, then sorted;
  - custom and field gravity in a step listener each collision step;
  - query filters through JoltC's filter callbacks;
  - steps of different worlds serialized, because JoltC shares one temporary allocator.
- **`third_party/`** holds Jolt (`Jolt/` only, plus its license) and JoltC (header,
  sources, license), unmodified. Versions are in `third_party/README.md`.
- **Cargo feature `cross-platform-deterministic`** enables Jolt's
  `CROSS_PLATFORM_DETERMINISTIC` and `/fp:precise`.

## Measurements

### Correctness

`crates/engine_physics_jolt/tests/jolt.rs`, eight tests, all passing:
- **Free fall** for 1 s: 4.987 m. That is what semi-implicit Euler gives at 60 Hz; the
  analytic value is 4.905 m.
- **Pyramid:** a 10-level box pyramid moves less than 1 cm over 5 s and falls asleep.
- **Queries:**
  - raycasts give the exact hit point, normal, fraction and user data;
  - layer filtering works;
  - a 16 × 16 batch hits the ground everywhere.
- **Events and overlap:**
  - a sensor overlap and a landing are reported as sorted begin events;
  - overlap with and without sensors.
- **Gravity modes over 1 s:**
  - world: −4.9 m;
  - scaled 0: stays;
  - custom up: +4.9 m;
  - custom sideways: +2.5 m in x;
  - 20 orbits around a point field keep their radius within 10% over 10 s.
- **Chain:** 12 links on point joints stay 0.8 m apart while swinging.
- **Determinism:** a 300-body pile over 120 steps hashes identically across repeated runs,
  on one thread and on four, and between one and four threads.
- **Shapes and handles:** convex hull, cylinder, triangle mesh; teleport; destroy;
  rejection of a degenerate hull.

Struct layouts of every mirrored JoltC struct match the C compiler's (`ffi` layout test).

### Performance

`benches/physics.rs`, cloud container (4 vCPUs, not reference), median / p99 step time
in ms at 60 Hz:

| scene | bodies | 1 thread | 4 threads |
|---|---:|---:|---:|
| pyramid, 20 levels | 210 | 0.065 / 1.71 | 0.064 / 1.73 |
| pile of mixed shapes | 1 000 | 3.91 / 6.92 | 3.83 / 6.24 |
| pile of mixed shapes | 5 000 | 18.3 / 37.8 | 19.2 / 28.6 |
| chain, point joints | 200 | 0.58 / 1.43 | 0.41 / 0.78 |
| orbits in a point field | 200 | 0.16 / 0.28 | 0.17 / 0.28 |

- 4 096 batched raycasts over the settled 1 000-body pile: 0.28 µs per ray on one thread.
- ref-1 numbers come from the bench workflow on the PR that adds this ADR.

## Consequences

- Our own solver (P2) implements the same trait and runs the same scenes and tests. The
  acceptance criterion (within 10% of Jolt) is measured with this benchmark.
- **Follow-ups:**
  - Jolt's jobs on `engine_core::jobs`: a detached-spawn primitive, then JoltC's callback
    job system;
  - a per-world temporary allocator (a JoltC API addition), after which world steps need
    not be serialized;
  - save and restore for rollback; shape casts; character controller; debug drawing;
  - Tracy zones inside Jolt (`JPH_PROFILE` through an external profiler hook);
  - more scenes from Jolt's `PerformanceTest` (ragdolls, convex versus mesh, CCD).
- Build cost: Jolt compiles in about 90 s on 4 cores, once per profile. Cached by CI's
  Rust cache when unchanged.
