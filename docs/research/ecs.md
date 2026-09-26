# ECS / scene model

Checked on 2026-09-26. CLAUDE.md §13 asks to compare our own archetype ECS, `bevy_ecs` as
a library, `hecs`, and flecs (C). The criteria are query iteration speed, structural
changes, editor friendliness (reflection, undo), and scripting friendliness. The decision
is one of the "irreversible" ones that the team makes (CLAUDE.md §3). This note and
[ADR 0017](../adr/0017-ecs.md) prepare it.

The harness `bench/ecs-compare` is a standalone project, outside the workspace, so its
dependencies never reach the engine. It compares:
- an archetype ECS prototype written for this comparison (`src/proto.rs`, about 500 lines);
- the three candidates;
- the scenarios of rust-gamedev's `ecs_bench_suite` (archived, MIT/Apache), re-implemented,
  plus a large iteration and random access.

No candidate code was copied into the prototype.

## Sources

| Source | Version | License | Read |
|---|---|---|---|
| `bevy_ecs` | 0.18.1 (crates.io; 0.19 needs Rust 1.95), main `0f38358f573a` | MIT / Apache-2.0 | `storage/table`, `archetype.rs`, component registration, change ticks |
| `hecs` | 0.11.1, master `a0095c6deb9e` | MIT / Apache-2.0 | `archetype.rs`, `world.rs` |
| flecs + `flecs_ecs` (Rust bindings) | flecs master `9e874bca2c05`; `flecs_ecs` 0.2.2 | MIT | docs (relationships, queries), binding API |
| `ecs_bench_suite` | `9b4c98d00c5c` (archived) | MIT / Apache-2.0 | scenario definitions |

All harness dependencies pass our `cargo-deny` license allow-list, checked with the
workspace's `deny.toml`.

## How they work

### `bevy_ecs`

- **Storage:** tables (dense columns per set of "table" components) plus sparse sets for
  components marked as such. An archetype is a table plus sparse-set components.
- **Components** are registered with a `ComponentId`. Custom layouts are supported
  (`ComponentDescriptor`), so components can be defined at run time.
- **Change detection:** "added" and "changed" ticks per component per entity. Every
  mutable access through `Mut<T>` writes a tick.
- **Extras:**
  - hooks and observers;
  - relationships (`ChildOf` / `Children`), required components;
  - cached query states with filters (`With`, `Without`, `Changed`);
  - deferred `Commands`;
  - schedules with a parallel executor (`bevy_tasks`);
  - reflection through `bevy_reflect`.
- **Maintenance:**
  - releases every few months with breaking changes;
  - 0.19 raised the minimum Rust to 1.95 (we pin 1.94.1).

### `hecs`

- **Model:** a minimal archetype ECS. Type-erased columns per archetype, cached add and
  remove edges, generational entities, dynamic borrow checking of columns.
- **Missing:** no change detection, observers, relationships, scheduling or reflection.
  Components are Rust types; there is no run-time component definition for scripts.
- **Small dependency set:** `hashbrown`, `foldhash`, `spin`.

### flecs (C) and `flecs_ecs`

- **Model:** archetype tables keyed by ids that include pairs (relationships), with
  table-graph edges.
- **Features:**
  - hierarchies (`ChildOf` pairs; fragmenting by parent, with a non-fragmenting option in
    recent versions);
  - prefabs and inheritance, observers;
  - a query language and cached queries;
  - reflection (`meta`), run-time components;
  - a REST API and a web explorer.
- **The most feature-complete of the four.** The Rust bindings wrap the C API. Every
  structural operation crosses FFI, and `set` of four components is four archetype moves
  unless the bulk API is used.

## Measurements on ref-1

Ryzen 7 7700, Windows 11, release, one thread, median, µs, lower is better (bench workflow
report on the PR that added this note):

| scenario | own prototype | hecs 0.11.1 | bevy_ecs 0.18.1 | flecs_ecs 0.2.2 |
|---|---:|---:|---:|---:|
| spawn 10k × 4 components | 534 | 836 | 958 | 5 582 |
| iterate 10k: position += velocity | 4.3 | 8.9 | 7.6 | 5.8 |
| iterate 1M: position += velocity | 462 | 855 | 829 | 482 |
| fragmented: 26 archetypes × 20, data *= 2 | 0.1 | 0.3 | 0.2 | 0.4 |
| add + remove a component, 10k entities | 398 | 435 | 791 | 1 125 |
| random get of 10k (of 100k) | 93 | 201 | 88 | 237 |

The container shows the same pattern:

## Measurements (container, not reference)

Cloud container (Intel Xeon 2.8 GHz, 4 vCPUs, shared, noisy), release, one thread, median.
Times in µs, lower is better. ref-1 numbers come from the bench workflow.

| scenario | own prototype | hecs 0.11.1 | bevy_ecs 0.18.1 | flecs_ecs 0.2.2 |
|---|---:|---:|---:|---:|
| spawn 10k × 4 components | 1 356 | 1 243 | 1 296 | 13 264 |
| iterate 10k: position += velocity | 7.7 | 21.1 | 13.7 | 10.3 |
| iterate 1M: position += velocity | 1 924 | 3 044 | 2 733 | 1 965 |
| fragmented: 26 archetypes × 20, data *= 2 | 0.2 | 0.7 | 0.4 | 1.0 |
| add + remove a component, 10k entities | 627 | 670 | 1 189 | 2 969 |
| random get of 10k (of 100k) | 166 | 345 | 167 | 757 |

Reading the numbers:
- **The prototype** is the fastest or tied in every scenario. It iterates plain typed
  slices (`zip` over two columns), which the compiler vectorizes.
- **`bevy_ecs`** pays for change detection (a tick write per mutable access) and its query
  machinery: 1.8× the prototype at 10k.
- **`hecs`** pays for dynamic borrow checks and its generic fetch path: 2.7× at 10k.
- **flecs through the bindings** iterates fast (it hands out whole table columns). Its
  structural operations cross FFI one component at a time:
  - spawn with four `set`s is 10× the others;
  - add + remove is 4.7×;
  - random `get` goes through a closure and FFI.
- **The prototype has none of** change detection, observers, relationships, parallel
  iteration or query caching. Those cost something when added. The numbers show the
  storage model's headroom, not a finished ECS.
- **The first prototype version** used the standard SipHash maps and allocated when
  resolving each spawned bundle: spawn 2.0 ms, add + remove 1.06 ms. A multiply-rotate
  hash and a per-bundle-type layout cache brought both to the table's values. The engine
  version would start there.

## What matters beyond speed

`ecs_bench_suite`'s maintainers archived it with the observation that "speed is only one
aspect of an ECS, and a rather small one once a baseline is established". For this engine:

| Criterion | own | bevy_ecs | hecs | flecs |
|---|---|---|---|---|
| Zero allocations per frame, our arenas and job system (§5) | by construction | would need auditing; `bevy_tasks` beside our job system | yes for iteration; own scheduling | own allocator hooks; C internals |
| Determinism for rollback / lockstep (§13) | designed in: stable iteration and entity order | not a goal upstream | archetype order stable, not guaranteed | not a goal upstream |
| Editor: reflection, undo | derive shared with the scripting bindings (§10); every change through a command log | `bevy_reflect`; undo is ours to build | ours to build | `meta` reflection; undo ours |
| Scripting: run-time components for Luau (§10) | designed in (component descriptors) | `ComponentDescriptor` | no | yes |
| Toolchain and API stability | ours | minimum Rust 1.95 for 0.19; breaking releases | stable, small | C ABI stable; bindings 0.x |
| Cost to us | the core, change tracking, observers, relationships, queries, parallel iteration | integration and following upstream | building most features on top | FFI, debugging across C |
