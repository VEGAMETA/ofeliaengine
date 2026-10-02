# 0074. Replication, prediction and lag compensation

- Status: Accepted
- Date: 2026-10-02
- Builds on: [ADR 0019](0019-networking-model.md) and its addendum (authoritative ticks,
  snapshots of reflected components, prediction with the same systems, lag compensation,
  events only for one-shot things, no loopback in singleplayer),
  [ADR 0070](0070-network-transport.md) (the transport), [ADR 0017](0017-ecs.md) (ECS
  chunks and change versions), [ADR 0022](0022-reflection.md) (reflection),
  [ADR 0066](0066-physics-queries-characters-rollback.md) (characters)
- Research: [`docs/research/net-replication.md`](../research/net-replication.md)
- Code: `crates/engine_net`; `examples/coop` (listen server + guest over UDP),
  `examples/replication_bench.rs`; tests `tests/replication.rs`, `tests/coop.rs`,
  `tests/zero_alloc.rs`

## Context

ADR 0019 fixed the contract and left the wire format open: "baseline deltas against
Tribes-style loss notification with dirty masks, measured in bytes per client per tick
and server µs per client". ADR 0070 delivered the transport with delivery notification
and a send budget. The owner wants co-op like Portal 2 or Quake to cost a game developer
a few attributes and setup lines, on a core that is measured, allocation-free per tick
and latency-first (CLAUDE.md §5, §7).

## Options

### Wire format
1. **Quake III:** one baseline per snapshot (the last acknowledged), every relevant entity
   in every snapshot. Simple, but with a byte budget not every entity fits every
   snapshot, and then a per-snapshot baseline forces whole states.
2. **Tribes:** dirty masks, values sent whole, re-marked on loss notification. Measured
   131.5 bits per moving entity per tick (below).
3. **Per-entity acknowledged baselines with loss notification (chosen):** each entity is a
   delta against the newest state of it the client acknowledged; it is sent when it
   changed since its last send or that send was reported lost (not repeated while in
   flight); quantized floats can be coded against a baseline extrapolated through the
   two newest acknowledged states (Unity's predicted baselines). 39.9 bits.

### Rollback scope
1. **Whole-world snapshots** (`World::save`/`restore`, physics `save_state`): what a
   lockstep game needs; for a client predicting its own pawn it copies everything.
2. **The predicted entities' replicated components (chosen):** the game's systems derive
   all their state from them (a Jolt character is re-placed from its component every
   tick), so restoring them is enough; the misprediction check is a comparison of
   quantized words (ADR 0019).

### Lag compensation
1. **Move the live world back** (Source): touches physics and animation, sequential.
2. **A ring of hit capsules per tick, queried at the shooter's instant (chosen,
   ADR 0019).**

### Marking components for the network
1. **A `#[reflect(net)]` attribute discovered at run time:** Rust has no type discovery
   without global constructors (`inventory`/`linkme`), and the order must be the same on
   both sides.
2. **One `NetSchema::replicate::<T>()` line per component (chosen)**, field precision from
   the reflection attributes that already exist (`net_quantum`, `net_skip`), overrides
   for engine types (`replicate_with::<Transform>(FieldHints::new().quantum(..))`). The
   schema's hash is checked when a client joins.

## Measurements

ref-1 (Ryzen 7 7700, 8C/16T), release, commit `a4fa1dc` (+ docs), CPU only (no GPU
work), the machine shared with another agent's builds. `cargo run -p engine_net --release
--example replication_bench`.

**Encoding variants**, 1,000 bodies (quaternion, position and velocity in mm, health) in
a random walk at 64 Hz, baselines 8 ticks old (125 ms round trip):

| variant | bits per entity per tick |
|---|---:|
| raw 32-bit fields | 352.0 |
| quantized, whole (exp-Golomb) | 163.0 |
| delta against the previous tick (ideal) | 29.7 |
| delta against the acknowledged state | 53.9 |
| delta against a baseline extrapolated through two acknowledged states | 38.8–39.2 |
| **shipped: extrapolated per component when cheaper (+1 bit)** | **39.9** |
| Tribes: changed state sent whole | 131.5 |

**Server per tick** (receive + quantize + choose and pack snapshots + transport; median
µs) and bytes per client per tick, everything moving or 10 % moving (the rest static,
never written):

| entities | moving | clients | serial µs | parallel µs (16 threads) | B/client/tick |
|---:|---:|---:|---:|---:|---:|
| 100 | 100 % | 1 / 8 / 32 | 9 / 51 / 200 | 9 / 36 / 103 | 208 |
| 100 | 10 % | 1 / 8 / 32 | 4 / 15 / 62 | 4 / 14 / 56 | 27 |
| 1,000 | 100 % | 1 / 8 / 32 | 52 / 205 / 930 | 52 / 110 / 341 | 834 |
| 1,000 | 10 % | 1 / 8 / 32 | 24 / 79 / 293 | 24 / 56 / 130 | 208 |
| 10,000 | 100 % | 1 / 8 / 32 | 326 / 616 / 2,162 | 313 / 405 / 847 | 1,026 |
| 10,000 | 10 % | 1 / 8 / 32 | 228 / 480 / 1,523 | 254 / 330 / 626 | 708 |

- 1,100 bytes per snapshot is the budget (one packet): 100 moving entities fit with
  predicted baselines (208 B); 1,000 do not, the accumulator rotates them (every entity
  still updates several times a second).
- Quantizing is done once per tick for all clients: ~30 µs per 1,000 entities; chunks
  whose ECS change version did not move are copied from the previous frame (10,000
  entities, 10 % moving, one client: 376 → 198 µs per tick).
- Per client the cost is a scan of every networked entity (~20–40 µs per 1,000):
  packing runs in parallel on the job system (`Server::send_with(.., Some(&pool))`) from
  8 clients up.

**Client per frame** (one tick per frame; median µs): own pawn only 1.5; with a rollback
every snapshot re-simulating 3 / 6.6 / 14 ticks (10 / 40 / 100 ms one way) 1.9 / 2.4 /
3.1, i.e. ~0.12 µs per re-simulated tick of framework cost plus the game's systems;
1,000 interpolated entities 65–71.

**Correctness** (in-memory network, seeded, virtual clock):
- converges exactly to the server's state at 0 / 5 / 15 / 30 % loss with 40 ms ± 10 ms,
  0 decode errors;
- late join of 1,000 entities (with 100 despawned, 50 spawned) gets everything once;
- prediction without drift: 0 mispredictions after the pawn's first state over 6 s of
  changing input (rounding on both sides); 1 under 10 % loss and 20 ms jitter (12 inputs
  of ~500 missed at the server);
- forced corrections (server pushes the pawn 1 m every second): 4–5 rollbacks, the
  correction exactly 1 m, converged afterwards;
- interpolation of a 3 m/s mover at 120 Hz under 15 ms jitter and 5 % loss: never
  backwards, worst frame step 2.5 mm off the ideal 25 mm (10 %), delay ~3 ticks;
- lag compensation (80 ms ± 10 ms, target crossing at 6 m/s): 30 of 30 shots aimed at the
  interpolated target hit, 0 of 30 without compensation;
- reliable events all once in order at 30 % loss; unreliable ones at most once (216 of
  300 arrived);
- snapshots never exceed the budget (600-byte test: 566 B on average);
- **0 allocations per tick** on the server and per frame on the client in steady state,
  with loss, jitter and a rollback every second.

**Co-op example over UDP loopback** (`examples/coop`, Jolt characters, button, door,
hitscan): host 130 µs per tick (simulation, rules, Jolt, snapshots), guest 71 µs per
frame; 12 bytes per snapshot; 795 predictions confirmed, 8 corrected in 14 s (one at
spawn, the others while passing under the opening door, whose interpolated position on
the guest trails the server's by ~2 ticks).

## Decision

**`engine_net`** (dependencies: `engine_ecs`, `engine_reflect`, `engine_net_transport`,
`engine_core`, `engine_profiler`, `bytemuck`; nothing new in the tree).

- **Schema:** `NetSchema::new(protocol_id).replicate::<T>()...event::<E>()`. Components
  are plain data with reflection; floats with `net_quantum` become integers of quanta,
  quaternions smallest-three (15 bits), integers/enums/bools exact, floats without a
  quantum 32 bits; `net_skip`/`skip` fields stay local. At most 64 components. The hash
  of names, layouts and codecs is checked at join.
- **Entities:** `Replicated` marks a networked entity; the server assigns network slots
  (with generations) on its next send and frees them through the ECS removal log.
  `Server::set_owner(entity, client)` gives a pawn to a client: it gets `NetInput<I>`
  on the server and arrives as `Predicted` + `NetInput<I>` on that client. Ownership
  changes replace the entity on clients (new generation).
- **Server per tick:** `update` (receive; inputs for this tick into the owners' pawns,
  the previous input repeated if missing), the game's systems, `send`/`send_with`:
  quantize once into a frame ring (32 ticks) and round the simulation's own values to
  the network precision; record hit capsules; per client, accumulate priority for
  entities that changed since sent or whose send was lost (owner ×100, `NetRelevance`
  radius and priority against `set_view`), choose the top that fit the budget
  (`min(max_snapshot_bytes, transport budget)`), write them in slot order as removals,
  whole states or deltas; delivery notifications advance acknowledged states.
- **Snapshot encoding:** tick, input lead, default baseline, then per entity: slot gap,
  kind, delta against its acknowledged state (1 bit if the default), an optional older
  acknowledged state for predicted baselines, per component a changed bit (and a
  "predicted" bit for components with quantized floats), per word a changed bit and a
  zigzag exp-Golomb delta.
- **Client per frame** (`Client::frame(world, now, input, simulate)`): receive and decode
  against a ring of 64 decoded snapshots; spawn, despawn (deferred one frame, with
  events), update; compare the server's state of predicted entities with the stored
  prediction of that tick and on a difference restore them and re-run `simulate` for
  the stored inputs; predict the due ticks with time dilation (±5 %) toward an input
  lead of 1 + 2·jitter (+1 under loss) ticks reported by the server; send every
  unacknowledged input (≤ 32) every tick; interpolate remote entities at server time −
  (1 + 2·jitter (+1 under loss) + 0.25) ticks, extrapolating at most 2 ticks across
  holes. `Client::correction::<C>(entity)` gives the before/after of a correction for
  visual smoothing.
- **Inputs and sub-ticks:** every input header carries the view delay (how far behind
  remote entities were shown); `InputHeader::view_time(SubTick)` is the instant the
  player saw when acting.
- **Lag compensation:** `LagCompensation` keeps `max_rewind` (1 s) of per-tick capsules
  (`HitCapsule` + `Transform`); `raycast(origin, dir, max, time, ignore)` interpolates
  between ticks; the live world is never moved.
- **Events:** `send_event(Target, reliable, &e)` on the reliable channel or the unreliable
  one, stamped with the tick; read once per update/frame. Never for state.
- **Channels** (`NetSchema::endpoint_config`): 0 reliable (welcome, reliable events),
  1 unreliable with notification (snapshots), 2 unreliable (inputs, unreliable events).
- **Listen server:** the host plays in the server's world (`set_local_input`); no loopback.
  Singleplayer creates none of this (ADR 0019 addendum).
- **`engine_ecs` additions:** `Archetype::{column_index, chunk_len, chunk_entities,
  pod_column}`, `World::{pod_column_mut, get_raw_mut}`: layout-level access to plain-data
  columns for replication (and later serialization).

## Consequences

- A game writes: the schema (one line per component and event), an input type, its tick
  systems reading `NetInput` and replicated components, and two loops (dev guide
  `networking.md` §6). The co-op example's networking is about 30 lines.
- Prediction is exact when the game's systems derive their state only from replicated
  components (the dev guide says so); anything else shows up as corrections, counted in
  `ClientStats`.
- The server's per-client cost scans every networked entity: fine to ~10,000 entities and
  dozens of clients (2.2 ms serial, 0.85 ms parallel at 32 clients); beyond that, keep
  per-client candidate lists updated from the frame's changed slots instead of scanning.
- **Revisit / deferred:**
  - predicted spawns (projectiles a client fires before the server confirms them) and
    predicting more than owned entities (nearby physics, Rocket League style);
  - per-component priorities and update rates, PVS/portal relevance;
  - delta-compressed inputs beyond "same as previous", input batching for slow clients;
  - visual error smoothing built in (today the game gets the correction and smooths);
  - snapshot intervals other than every tick (`snapshot_interval` exists; the client's
    interpolation assumes one tick and adapts through jitter);
  - a windowed demo through `engine_runtime` (the example is console-only: the runtime's
    frame loop has no hook for a dilated client clock yet).

## Questions for the team

1. **Patents:** no patent covering this design was found (research note); is a legal
   check wanted before shipping a game on it?
2. **Default tick and budget:** 64 Hz and 1,100 bytes per client per tick (~70 KB/s)
   are the defaults; should co-op templates default lower (30 Hz, ~35 KB/s) for
   upload-limited home hosts?
3. **Owner priority:** clients receive their own pawn first (×100). Should hosts be able
   to set per-component update rates (e.g. cosmetic state at 10 Hz) in the first version?
