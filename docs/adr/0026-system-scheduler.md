# 0026. ECS system scheduler: insertion order, access-derived levels, job system

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/ecs-scheduler.md](../research/ecs-scheduler.md)
- Follows: [ADR 0017](0017-ecs.md) (next step 4), [ADR 0002](0002-job-system.md), [ADR 0019](0019-networking-model.md)

## Context

ADR 0017 left "a system scheduler on the job system (access sets from
`Fetch::register`)" as a next step. Game code, physics sync, animation and rendering
extraction will run as systems.

Requirements:
- deterministic results for rollback and desync checks (ADR 0019): the same world
  state after a tick on any number of threads;
- parallelism between independent systems without the user writing edges;
- no per-frame allocations (§5), and a small per-run cost;
- structural changes (spawn/despawn/insert) must not race with iteration.

## Options

1. **Bevy-style run-time executor.** Dependency graph with explicit edges; at run time a
   ready queue and access checks against running systems. Most parallel. The order of
   conflicting systems without an edge is unspecified, so results can differ between
   runs unless every ambiguity is resolved by hand.
2. **flecs-style phases, one system at a time.** Deterministic, and each system can be
   internally parallel. No overlap between systems.
3. **Insertion order + access-derived levels** (chosen). The order systems are added is
   the semantic order. Level of a system = 1 + the highest level of earlier systems it
   conflicts with (component or resource, read/write). Each level runs as one
   `ThreadPool::scope`. Levels are computed once, at `initialize`.

## Measurements / arguments

`cargo bench -p engine_ecs --bench ecs`, container (4 threads), release, commit of this PR:

| scenario | µs |
|---|---:|
| 3 independent systems × 1M entities, serial | 6700.6 |
| same, pool (4 threads) | 2808.3 (2.4×) |
| 16 empty systems, serial | 0.2 |
| 16 empty systems, pool (4 threads) | 22.5 |

The empty-schedule cost with the pool is the cost of one `scope` with 16 jobs
(~1.4 µs per system, container). ref-1 numbers come from the bench workflow and go to
`docs/perf/history.md`.

Determinism: the test `parallel_runs_equal_serial_runs` runs a schedule that moves,
decays, ages, spawns and despawns entities for 50 ticks, serial and on 4 workers, five
times, and compares `World::state_hash` after every tick.

## Decision

- `System` trait: `name`, `init(&mut World, &mut Access) -> state`, `run(&mut
  SystemContext)`; `system(name, init, run)` builds one from closures.
- `Access::query` / `read` / `write` return handles (`SystemQuery`, `Res`, `ResMut`)
  tagged with the system's id. `SystemContext` refuses (panics on) a handle of another
  system, so a system can only touch what it declared.
- World resources: `insert_resource` / `resource` / `resource_mut` / `remove_resource`,
  one slot per type.
- Archetype change versions became atomics, so systems can mark changes through `&World`.
  `QueryState::for_each_shared` is the unsafe entry used by the scheduler; it is sound
  because conflicting systems never share a level.
- Each system has its own `Commands`; all are applied after the last level, in add order.
  `add_exclusive(name, FnMut(&mut World))` is a sync point: it conflicts with
  everything and runs alone.
- One change tick per `Schedule::run`.

## Consequences

- Results equal a serial run in add order, independent of thread count.
- A level waits for its slowest system. If profiles show idle workers between levels,
  consider a ready queue that keeps the same edges (the order would stay deterministic,
  only the packing would change).
- Conflicts are per component id, not per archetype, so two systems writing the same
  component on disjoint entity sets are serialized. Split components or merge systems
  where that matters.
- Declaring access is explicit (`init`); a Bevy-like parameter macro can be added on
  top later without changing the scheduler.
- Commands become visible only on the next run. Systems that need a structural change
  earlier in the same tick use `add_exclusive`.
