# 0018. Physics interface (P0) and the Jolt reference backend (P1)

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/physics-api.md](../research/physics-api.md)

> **Update 2026-09-26 (ADR 0028):** our own 3D solver (P2) is deferred; Jolt stays the
> physics engine. The consequences below that mention P2 apply only if it is reopened.
>
> **Update 2026-10-01 ([ADR 0066](0066-physics-queries-characters-rollback.md)):** shape
> casts, the character controller and save/restore (follow-ups below) are done.

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

1. **Jolt's thread pool** (chosen for P1, `threads > 0`).
2. **JoltC's callback job system over `engine_core::jobs`.** Needs a detached-spawn
   primitive our fork-join pool does not have yet. Follow-up.
3. **`threads = 0` runs on the calling thread** through JoltC's callback job system with a
   callback that executes each job as soon as it is queued. This is how Jolt's own
   `JobSystemSingleThreaded` works. JoltC's thread-pool constructor cannot express it: it
   turns 0 threads into "all cores".

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

`benches/physics.rs` steps each scene on the calling thread alone and with 1, 3, 7 and
15 Jolt workers (2, 4, 8 and 16 threads in total). The sweep shows where extra threads
stop helping, for example on the second hardware thread (SMT) of each core.

**A correction.** The first run of this benchmark (ref-1, and the container numbers first
recorded here) measured `threads = 0` on all cores, not on one thread. JoltC turns a
thread pool of 0 threads into "all cores", so the "1 thread" and "16 threads" rows were
the same configuration (for example 4.08 and 4.06 ms for the 5 000-body pile on ref-1).
The determinism test's one-thread world was affected the same way. Since the fix
(option 3 above), the calling-thread configuration is really single-threaded, and the
test compares one thread against four for real.

Cloud container (4 vCPUs, **not reference**, measured while another job used one core),
median step time in ms at 60 Hz:

| scene | bodies | 1 thread | 2 threads | 4 threads |
|---|---:|---:|---:|---:|
| pile of mixed shapes | 1 000 | 5.09 | 4.26 | 4.62 |
| pile of mixed shapes | 5 000 | 43.3 | 28.4 | 24.2 |
| chain, point joints | 200 | 0.34 | 0.41 | 0.47 |
| orbits in a point field | 200 | 0.08 | 0.12 | 0.16 |

- **Small scenes run slower with more threads:** the job overhead exceeds the work.
- **The 5 000-body pile scales** to 1.8× on 4 threads.
- **4 096 batched raycasts** over the settled 1 000-body pile: 0.28 µs per ray on one
  thread.
### Thread sweep on ref-1

Ryzen 7 7700 (8 cores, 16 threads), bench profile, median / p99 step time in ms:

| scene | bodies | 1 | 2 | 4 | 8 | 16 threads |
|---|---:|---:|---:|---:|---:|---:|
| pyramid, 20 levels (asleep) | 210 | 0.009 | 0.013 | 0.021 | 0.031 | 0.045 |
| pile of mixed shapes | 1 000 | 2.48 / 3.28 | 1.63 / 2.14 | 1.10 / 1.31 | **0.84** / 1.09 | 0.85 / 1.06 |
| pile of mixed shapes | 5 000 | 19.1 / 25.6 | 12.5 / 14.6 | 7.58 / 9.00 | 5.08 / 6.11 | **3.99** / 4.62 |
| chain, point joints | 200 | 0.23 | 0.18 | **0.16** | 0.16 | 0.18 |
| orbits in a point field | 200 | **0.050** | 0.054 | 0.063 | 0.065 | 0.076 |

- 4 096 batched raycasts: 0.14 µs per ray on one thread.
- **Whether SMT helps depends on the load.**
  - The 5 000-body pile gains another 27% from the second hardware thread of each core
    (8 → 16 threads).
  - The 1 000-body pile gains nothing past the 8 physical cores.
  - Small or sleeping scenes are fastest on one or two threads: job hand-out costs more
    than the work.
- **Default for games** (team decision, 2026-09-26): the thread count follows the load.
  - Before each step the backend counts awake bodies.
  - Below `PhysicsSettings::single_thread_below` (default 300) the step runs on the
    calling thread. Otherwise it runs on every logical core (`threads` default:
    `PhysicsSettings::all_cores()`).
  - Jolt's pool starts on the first large step, so small worlds never start threads.
  - Jolt's results do not depend on the thread count. The test
    `awake_body_count_picks_the_thread_count` checks that switching keeps the state
    hash.
  - ref-1 (Bench run for commit bf17f2e), median ms, best fixed setting → auto:
    - pyramid (asleep) 0.010 (1 thread) → 0.013 (1);
    - pile 1 000: 0.860 (16) → 0.853 (16);
    - pile 5 000: 4.21 (16) → 4.16 (16);
    - orbits 200: 0.055 (1) → 0.056 (1);
    - chain 200 (joints): 0.170 (8) → 0.211 (1).
    The chain is the one scene where 200 bodies still gain from threads (joint solving,
    24% here). A cost estimate that counts constraints, not only bodies, would catch it.
  - The benchmark's "auto" row shows the choice per scene. In the container it
    matched the fastest fixed setting in every scene (for example the chain: 0.28 ms
    auto against 0.51 ms on 4 threads).

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
