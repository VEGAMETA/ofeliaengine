# Networking model: Source-style client/server and the alternatives

Checked on 2026-09-26.

The team's request:
- networking with the gameplay patterns of Source games: an authoritative server, client
  prediction, interpolation of other players, lag-compensated hits;
- the spirit of Source, not its form;
- performance first.

This note compares the models used in shipped games and open-source libraries. It fixes
what the simulation and the ECS must provide from day one
([ADR 0019](../adr/0019-networking-model.md), [ADR 0017](../adr/0017-ecs.md)).

The wire protocol, transport and encryption are decided later, with a prototype and
measurements. No code from any of these sources was carried over:
- **Source SDK 2013** is under Valve's proprietary SDK license;
- **Quake III** is GPL;
- the rest were read for ideas.

## Sources

| Source | Version / date | License | How it was studied |
|---|---|---|---|
| Source SDK 2013 (`ValveSoftware/source-sdk-2013`) | master, 2026-09-26 | Source 1 SDK License (proprietary; ideas only) | read `player_lagcompensation.cpp`, `prediction.cpp`, `usercmd.h/.cpp`, `interpolatedvar.h`, `dt_send.h` |
| lightyear | `320f825` (2026-09-21) | MIT / Apache-2.0 | read the book: prediction, inputs, time sync, bandwidth, replication |
| GGPO | master | MIT | read `doc/README.md` (rollback, input queue, sync test) |
| GameNetworkingSockets | master | BSD-3-Clause | read README and build notes (reliability, lanes, encryption, dependencies) |
| netcode (mas-bandwidth) | main | BSD-3-Clause | read README (connect tokens, encrypted UDP) |
| Quake III Arena | id-Software release | GPL-2.0 (ideas only) | known design: delta against the last acknowledged snapshot, field tables |
| Y. Bernier, "Latency Compensating Methods in Client/Server In-game Protocol Design and Optimization" | GDC 2001 | article | known content; the Valve wiki is unreachable from the build container |
| T. Ford, "Overwatch Gameplay Architecture and Netcode" | GDC 2017 | talk | known content, not re-read (GDC Vault unreachable) |
| M. Frohnmayer, T. Gift, "The TRIBES Engine Networking Model" | GDC 1999 | paper | known content, not re-read |
| J. Cone, "It IS Rocket Science! The Physics of Rocket League Detailed" | GDC 2018 | talk | known content, not re-read |
| D. Aldridge, "I Shot You First: Networking the Gameplay of Halo: Reach" | GDC 2011 | talk | known content, not re-read |
| G. Fiedler, "Networked Physics", "Snapshot Compression", "State Synchronization", "Reliable Ordered Messages" | gafferongames.com, 2014–2016 | articles | known content, not re-read |
| Unity Netcode for Entities manual | 1.x | documentation | known design (ghost snapshots, predicted delta baselines), not re-read |
| Valve, Counter-Strike 2 "sub-tick" updates | 2023 | public descriptions | known behavior, no source available |

"Known content, not re-read" means the host was blocked by the container's network policy
on the check date. Claims from those sources are limited to their widely documented core
ideas. Every number that matters for us is to be measured on our own prototype.

## The models

### A. Source (and Quake III before it): snapshots, prediction, interpolation, lag compensation

- **The server simulates at a fixed tick.** The default is 66 in the SDK; CS:GO used
  64/128.
- **Clients send user commands**, one per tick. Each command carries:
  - the command number and the client's tick;
  - view angles, movement axes, buttons, weapon select;
  - a random seed, so weapon spread matches on both sides.

  Packets repeat the last few commands, so one lost packet loses no input.
- **The server sends snapshots.**
  - Each snapshot is a delta against the last snapshot the client acknowledged.
  - Networked fields are declared per class in send tables. Each field has a bit count,
    a range and flags; a proxy converts the value on send.
  - When the history runs out, the server sends a full update.
- **Other entities are interpolated.** The client renders them at "now − interp", with
  `cl_interp` 100 ms by default, or two snapshot intervals with `cl_interp_ratio 2`.
  - Every networked variable keeps a history of time-stamped samples (`CInterpolatedVar`).
  - Rendering interpolates between the two samples around the render time.
  - It extrapolates for up to 0.25 s when packets stop.
- **The local player is predicted.** The movement and weapon code is shared by client and
  server and runs on the client with the same commands.
  - The client keeps the predicted result of every command in 90 slots.
  - When a snapshot arrives:
    - `cl_pred_optimize 2` compares the server state with the stored prediction for the
      acknowledged command;
    - if they match, nothing is re-simulated;
    - if not, the client restores the server state and re-runs every unacknowledged
      command.
  - The visible error is smoothed over 0.1 s.
- **Lag compensation.** The server keeps one second (`sv_maxunlag`) of history per player:
  - the history holds origin, angles, bounds, animation layers and pose parameters;
  - on a firing command, the server computes the target time as the command's tick minus
    the client's interpolation, clamped against the latency estimate (a > 200 ms
    disagreement falls back to latency);
  - it moves nearby players back, interpolating between records;
  - it traces, then restores them;
  - a teleport larger than 64 units breaks the history.
- **Predicted effects.** The client plays its own predicted effects (sounds, tracers). The
  server suppresses sending them back to that client.

**Weaknesses we can fix:**
- **Lag compensation mutates the live world** (move players back, trace, restore). It is
  sequential, and touches physics and animation state.
- **Send tables and data tables are hand-written per class.** Delta encoding compares
  fields per client.
- **Prediction re-runs whole entity think functions,** so every predicted entity must be
  written with prediction in mind.
- **Shots are quantized to the tick.** A 64-tick server resolves a click to 15.6 ms.
  CS2's sub-tick timestamps address exactly this.

### B. Tribes / Torque: ghosting with dirty masks, priorities and delivery notification

- **Each replicated object** ("ghost") has a per-client dirty mask of state groups.
- **Every packet takes the highest-priority dirty ghosts that fit the budget.** Priority
  comes from relevance, distance and the time since the last update.
- **The transport tells the sender which packets arrived and which were lost.** The
  sender remembers what each packet carried:
  - on loss, it re-marks only those state groups dirty;
  - nothing is retransmitted as such: the newest value is sent next time.
- **Strengths:**
  - bandwidth use degrades gracefully;
  - no per-client baseline history is needed;
  - a loss costs only what was lost, not everything since an old baseline.
- **Weaknesses:**
  - per-client, per-object state;
  - a change fans out to every client's dirty mask.
- **The same idea shows up elsewhere:**
  - Halo Reach: priority by relevance under a bandwidth budget;
  - Fiedler's networked physics: a priority accumulator;
  - GameNetworkingSockets' ack vectors deliver exactly this notification.

### C. Deterministic lockstep and rollback (GGPO, Photon Quantum)

- **Only inputs travel.** Every peer runs the same deterministic simulation.
  - GGPO predicts missing remote inputs by repeating the last one.
  - When the real input arrives and differs, it rolls the whole game state back and
    re-simulates to the present, without rendering the intermediate frames.
  - A "sync test" mode rolls back every frame to catch non-determinism.
- **Photon Quantum:**
  - applies the same model to a whole ECS;
  - keeps the entire game state in one contiguous frame, so a rollback is a memory copy;
  - uses fixed-point math for cross-platform determinism.
- **Strengths:**
  - tiny bandwidth, independent of entity count;
  - no replication code;
  - replays for free.
- **Weaknesses:**
  - needs bit-exact determinism;
  - hidden information is visible to every peer (wallhacks cannot be prevented);
  - CPU cost grows with rollback depth × simulation cost;
  - late joiners need the full state.
- **Best for** fighting games, RTS, small sessions. Not for 32-player shooters with an
  authoritative server.

### D. Overwatch: ECS, deterministic prediction, clients running ahead

- **Gameplay is an ECS.** The simulation runs in fixed command frames. Movement,
  abilities and weapons are written as deterministic systems.
- **The client runs ahead of the server** by half the round trip plus a small buffer, so
  its input for tick T reaches the server just before the server simulates T.
  - It speeds up or slows down its simulation slightly ("time dilation") to keep the
    server's input buffer small, instead of jumping.
  - Under packet loss the buffer grows. Inputs are sent with redundancy.
- **On a misprediction,** the client rolls back only the predicted state of its own
  entities and re-runs the deterministic systems. The ECS makes "which state" explicit:
  the predicted components.
- **Hit detection favors the shooter, with limits:**
  - rewound hitboxes;
  - movement abilities that should save the target favor the defender beyond a latency
    threshold.

### E. Rocket League: predict the physics of everything

- **The server is authoritative.**
- **The client predicts every physics object** (ball, other cars) from the latest server
  state, re-simulating from it up to the present with the other players' last known
  inputs.
- **Result:** collisions with fast objects happen in the client's present, not in the
  interpolated past.
- **Costs:**
  - re-simulating the physics world every snapshot;
  - visible corrections when a remote player's input changes;
  - needs a deterministic, cheap-to-restore physics state.

### F. Counter-Strike 2: sub-tick input timestamps

- **Every input event carries its exact time within the tick,** so shots and movement
  changes are resolved at their real instant.
- **Lag compensation interpolates targets to that instant,** instead of the tick
  boundary.
- **Accuracy stops depending on the tick rate;** the tick rate keeps setting simulation
  cost and snapshot frequency.
- **Pitfalls in the first public versions:**
  - animation and effect timing that did not match the sub-tick resolution;
  - server/client disagreement where the sub-tick data was applied inconsistently.

### G. Unity Netcode for Entities: ghost snapshots in an archetype ECS

- **Replicated entities ("ghosts")** are serialized per archetype chunk into a snapshot
  ring. The per-field quantization comes from component attributes.
- **Deltas are against an acknowledged baseline.** For moving values they are against a
  *prediction* from up to three baselines (linear extrapolation), so a constant-velocity
  object costs almost nothing.
- **Values are encoded with a static entropy model** (bucketed prefix codes).
- **Static ghosts** that stop changing are not sent at all.
- **Predicted ghosts on the client keep a history,** roll back to the server state and
  re-run the prediction systems. Interpolated ghosts use an interpolation tick.

## Comparison for our requirements

| | A Source | B Tribes | C Lockstep | D Overwatch | E predict-all | G Unity NfE |
|---|---|---|---|---|---|---|
| Authoritative server, anti-cheat | yes | yes | no | yes | yes | yes |
| 32+ players, large worlds | yes | yes | poor | yes | CPU-bound | yes |
| Own actions feel instant | yes | yes (with prediction) | yes | yes | yes | yes |
| Hits on what the shooter saw | yes (lag comp.) | game code | exact | yes | n/a | lag comp. |
| Bandwidth under loss | resends since old baseline | only what was lost | inputs only | snapshots | snapshots | resends since baseline |
| Server CPU per client | delta packing | dirty masks | minimal | delta packing | delta packing | chunked, parallel |
| Needs bit-exact determinism | no | no | **yes** | for prediction quality | for prediction quality | no |

## What we take

The base is **model A's contract**:
- an authoritative server;
- a fixed tick;
- user commands with redundancy;
- snapshots of networked state;
- interpolation of remote entities;
- prediction of the local player;
- lag-compensated hit tests.

This is what Source games feel like, and it scales to shooters.

The form is ours, and each point is chosen for speed:

1. **The simulation is the ECS in a fixed tick** (D, G).
   - There are no networked entity classes. A component is networked by metadata
     (reflection with per-field quantization), not by a hand-written send table.
   - Prediction runs the same systems as the server.
2. **Snapshot and restore are storage operations** (C, Quantum).
   - ECS data lives in fixed-size chunks with per-chunk, per-component change versions.
   - Saving the predicted state is a copy of the chunks written since the last save, not
     per-entity code.
   - Restoring is a copy back.
3. **A misprediction check is a comparison, not a re-simulation** (A's `cl_pred_optimize 2`,
   lightyear's `Check`).
   - Compare the quantized server state with the stored prediction for that tick.
   - On a match, nothing is re-run: the common case costs a memory compare.
4. **Change detection for replication compares quantized values** of the chunks whose
   change version moved.
   - Gameplay systems pay nothing per write (no per-access ticks, as in bevy).
   - Writing an equal value produces no traffic.
5. **Delta encoding:**
   - against the acknowledged baseline, with G's predicted baselines for moving values;
   - quantized fields, bit-packed;
   - whether B's loss notification beats A/G's baseline deltas in bytes and CPU is
     measured on the prototype, with a loss/latency simulator, before the wire format
     ADR.
   - Either way the server quantizes each tick once. Clients are packed in parallel on
     the job system from read-only history.
6. **Priority accumulator and interest management** (B, Halo Reach, Fiedler) under a
   per-client byte budget.
7. **Clients run ahead with time dilation** (D) instead of Source's fixed command rate.
   - Inputs are sent every tick with redundancy (A).
   - The server keeps a small per-client input buffer; a missing input repeats the last
     one.
8. **Sub-tick input timestamps** (F): firing and other discrete events carry their time
   within the tick. Lag compensation interpolates to that instant.
9. **Lag compensation never touches the live world** (improving A):
   - the server keeps a ring of per-tick hit shapes (one second, SoA) for the
     lag-compensated entities;
   - a hit query evaluates interpolated historical shapes directly;
   - queries from different shooters run in parallel;
   - physics and animation are never moved back and restored.
10. **Predicting more than the local player is a per-entity choice** (E): physics objects
    near the player, projectiles. The cost is visible in the profiler, and the default
    stays at the local player's entities.
11. **Deterministic lockstep/rollback (C) is a second mode** on the same deterministic
    ECS, for genres that fit it (fighting, RTS, small co-op). Because the ECS and
    physics are deterministic from day one, it needs no second engine.

## What the simulation must provide from day one

These are the requirements [ADR 0017](../adr/0017-ecs.md) accepts for `engine_ecs` and
[ADR 0019](../adr/0019-networking-model.md) fixes for the simulation:

1. **Fixed simulation tick** (`Tick(u32)`), with rendering interpolating between ticks.
2. **Deterministic order** for the same build, inputs and API calls:
   - entity allocation;
   - archetype creation and query iteration;
   - structural changes applied from command buffers in a fixed order;
   - parallel work combined in chunk order.
3. **Snapshot and restore of chosen components,** cheap enough to run every tick.
4. **Per-chunk, per-component change versions,** for replication and incremental
   snapshots.
5. **A world state hash** in deterministic order, for desync detection and the
   determinism tests.
6. **Stable network ids,** separate from local entity ids.
7. **Plain-data components on the rollback path** (`Copy`, no pointers into other
   state). Other components roll back through reflection, more slowly.
