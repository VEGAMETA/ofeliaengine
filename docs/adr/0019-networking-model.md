# 0019. Networking model: authoritative ticks, prediction, lag compensation

- Status: Accepted (model and simulation requirements); the wire format, transport and
  encryption get their own ADR with a prototype
- Date: 2026-09-26
- Research: [docs/research/networking.md](../research/networking.md)

## Context

The team asked for networking with the patterns of Source games: an authoritative
server, client prediction, interpolation, lag-compensated hits. They asked for the spirit
of Source, not its form, and for speed above all.

CLAUDE.md §13 asks that physics determinism and the tick architecture allow future
rollback and lockstep. The networking code itself comes later. The simulation model and
the ECS are built now, so what they must provide is decided now.

## Options

1. **Source's model as is.**
   - Per-class send tables, per-client delta packing.
   - Prediction by re-running entity code.
   - Lag compensation that moves players back in the live world.
   - Proven, but manual and sequential.
2. **Deterministic lockstep with rollback only** (GGPO, Photon Quantum).
   - Tiny bandwidth.
   - No authoritative server and no hidden information.
   - Poor fit for 32-player shooters.
3. **Source's contract on an ECS built for it** (chosen). The ideas of Overwatch, Unity
   Netcode for Entities, Tribes, Rocket League and Counter-Strike 2 fill in the form.

## Decision

### The contract (what players and game developers see)

- **Server and tick:**
  - dedicated or listen server, authoritative;
  - fixed simulation tick, default 64 Hz, configurable per game.
- **Rendering interpolates between ticks,** locally and for remote entities. Remote
  entities are shown at "now − interpolation delay"; the delay adapts to the snapshot rate
  and jitter.
- **Inputs:** the client sends its input every tick with redundancy.
  - It runs ahead of the server by the estimated one-way latency plus a jitter margin.
  - It keeps that lead with small speed changes (time dilation) rather than jumps.
- **Prediction:**
  - the client predicts its own entities by running the same systems as the server;
  - the game can mark more entities as predicted (projectiles, nearby physics);
  - mispredictions are corrected by rollback and re-simulation, with visual error
    smoothing.
- **Hits:**
  - lag compensation up to 1 s;
  - discrete input events (firing) carry sub-tick timestamps;
  - hit tests use the targets' shapes interpolated to that instant, as the shooter saw
    them.
- **Deterministic lockstep/rollback** is a second mode on the same simulation, for genres
  that fit it.

### The form (how it is fast)

- **Networked components are declared by reflection metadata** with per-field
  quantization. There are no hand-written send tables.
- **ECS storage is fixed-size chunks** with per-chunk, per-component change versions
  ([ADR 0017](0017-ecs.md)). This gives:
  - incremental snapshots: copy the chunks written since the last save;
  - rollback: copy back;
  - replication change detection: compare quantized values only in chunks whose version
    moved.

  Gameplay systems pay nothing per write.
- **A misprediction check is a comparison** of the quantized server state with the stored
  prediction. Re-simulation runs only on a mismatch.
- **Encoding and priorities:**
  - the server quantizes each tick once;
  - clients are packed in parallel from read-only history;
  - deltas against acknowledged baselines, with predicted baselines for moving values;
  - a priority accumulator and interest management under a per-client byte budget.
- **Lag compensation keeps a per-tick ring of hit shapes** and queries it directly. The
  live physics and animation state are never moved back, and queries from different
  shooters run in parallel.

### Required of the simulation from day one

1. A fixed tick counter (`Tick`) in the world.
2. Deterministic entity allocation, archetype and query order, command application order,
   and ordered combination of parallel work.
3. Snapshot and restore of chosen components, cheap enough for every tick.
4. Per-chunk, per-component change versions.
5. A world state hash in deterministic order.
6. Network ids separate from local entity ids (a mapping component).
7. Plain-data (`Copy`) components on the fast rollback path.

## Open, with measurements before deciding

- **Wire format:**
  - baseline deltas (Source, Quake III, Unity) against Tribes-style loss notification
    with dirty masks;
  - the entropy coding of deltas;
  - measured in bytes per client per tick and server µs per client, on a prototype with
    a loss, latency and jitter simulator.
- **Transport:**
  - our own UDP layer with ack vectors (Windows Registered I/O on the server);
  - against GameNetworkingSockets (BSD-3, but protobuf and OpenSSL) or netcode (BSD-3,
    libsodium).
- **Encryption:** Windows CNG (AES-GCM, X25519) against a library.

### Addendum (team, 2026-10-01): no loopback in singleplayer, the camera per frame

- **Singleplayer has no client/server split.** One ECS world, one simulation, no network
  layer: no loopback server, no snapshot serialization, no interpolation delay for remote
  entities (there are none). Source runs a listen server even offline; that costs two
  copies of entity state, delta encoding inside one process, prediction on top of the
  simulation, and the interpolation delay (`cl_interp`, 100 ms in early Source), and buys
  nothing a single process needs. Multiplayer reuses the same systems: the server runs
  them authoritatively, the client predicts with them.
- **What the client/server shape is really good for is kept:** the fixed simulation tick
  separate from presentation (rendering interpolates between ticks at the display's
  rate), simulation and rendering on different cores (frame N rendered while N+1
  simulates), snapshots for saves, replays and rollback.
- **The player's camera is updated every rendered frame from the latest input,** not once
  per tick: view rotation and aim (and the locally controlled character's presentation)
  are applied after input sampling, right before extraction. The simulation still
  consumes the input on its tick, with a sub-tick timestamp (as above, like CS2's
  sub-tick input). Otherwise interpolating between ticks would add up to one tick of
  latency: 15.6 ms at 64 Hz.

## Consequences

- `engine_ecs` is built against the requirements above from its first version.
  Determinism and snapshot tests are part of its test suite.
- Physics already hashes state and is deterministic on the same build (ADR 0018).
  Save and restore of the physics world moves up in priority: prediction of physics
  objects and lockstep need it.
- The networking crates come after the editor skeleton (§17).
  - They start with a loss and latency simulator and the wire-format measurements.
  - Then transport, replication, prediction and lag compensation, each with numbers in
    `docs/perf/history.md`.
