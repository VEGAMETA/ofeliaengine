# ECS system scheduling

Checked: 2026-09-26. Sources were read for design ideas only; no code was carried over.

## Sources

| Project | Version | License | What we looked at |
|---|---|---|---|
| Bevy (`bevy_ecs::schedule`) | 0.18 | MIT/Apache-2.0 | `Schedule`, `SystemSet`, ambiguity detection, `MultiThreadedExecutor`, `apply_deferred` |
| flecs | 4.x | MIT | pipelines, phases, `ecs_run_pipeline`, merge points ("sync points") computed from `[in]/[out]` terms |
| Unity Entities | 1.x (product docs) | proprietary (docs only) | `ComponentSystemGroup`, `UpdateBefore/After`, job dependencies from declared access, `EntityCommandBuffer` playback points |
| Naughty Dog (Gyrling, GDC 2015) | talk | — | fibers, counters instead of a dependency graph per frame |

## How each one works

**Bevy.** Systems declare their access through their parameters. The user adds explicit
ordering (`before`/`after`, sets, chains); the order between conflicting systems that
have no explicit edge is *unspecified*, and an "ambiguity checker" reports such pairs.
The multithreaded executor walks the dependency graph at run time, and for every ready
system checks its access against the systems currently running. Deferred commands are
applied at automatically inserted `apply_deferred` points.
- Pro: very flexible, maximal parallelism.
- Con: the run order of conflicting systems depends on the order the user writes edges
  in, not on the order systems were added. Bevy runs are not deterministic unless every
  ambiguity is resolved by hand. The run-time readiness checks add a per-system cost
  (not measured here against ours).

**flecs.** Systems are placed in phases (`OnLoad` … `OnStore`) and run in declaration
order inside a phase. The pipeline inserts merge points where a system reads something
an earlier system wrote through deferred operations. Multithreading splits *one system*
over workers, not different systems at the same time.
- Pro: deterministic order, simple mental model.
- Con: no parallelism between systems.

**Unity Entities.** Systems run on the main thread in a group order
(`UpdateBefore/After`). Each system schedules jobs; the safety system derives job
dependencies from the declared read/write component types, so jobs of independent
systems overlap. Command buffers play back at fixed points in the group.
- Pro: order is explicit, and parallelism comes from declared access.
- Con: the dependency tracking lives in a large runtime safety system.

**Naughty Dog.** No systems graph at all; the frame is written as ordinary code that
spawns jobs and waits on counters.
- Pro: zero overhead.
- Con: all ordering is manual.

## What we take

- **Insertion order is the order** (flecs, Unity). A schedule is equivalent to running
  its systems one by one in the order they were added. This fits our rollback and
  lockstep plans (ADR 0019) better than Bevy's unspecified ambiguous order.
- **Parallelism only from declared access** (Unity, Bevy). Two systems get an edge only
  if they conflict. Conflicts are on component ids and on resources, read vs write.
- **Commands applied at one point** after the schedule, in add order (Bevy, Unity).
  `add_exclusive` marks an explicit sync point.
- **Levels computed once**, not a run-time ready queue. Each level is a `scope` on our
  job system. This is less parallel than Bevy's executor (a level waits for its slowest
  system). We accept that for determinism and a tiny per-run cost. Revisit if profiles
  show idle workers between levels.

## What we do not take

- Bevy's parameter-type magic (`fn(Query<..>, Res<..>)`). Declaring access in an
  explicit `init` keeps the code free of macro-generated `SystemParam` tuples, and keeps
  compile times low. It can be added later on top of the same `Access`.
- Run conditions, system sets and states. The editor/game loop will decide whether a
  whole schedule runs.
- Archetype-level conflict refinement: Bevy can run two systems that write the same
  component in parallel if their queries match disjoint archetypes. We do not do this;
  the edge is per component id. It is simpler and does not change as archetypes appear.
