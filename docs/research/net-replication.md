# Replication, prediction and lag compensation

Checked on 2026-10-02. Decision: [ADR 0074](../adr/0074-replication-prediction-lag-compensation.md).
The model is fixed by [ADR 0019](../adr/0019-networking-model.md) (and its addendum) and
surveyed in [networking.md](networking.md); the transport is
[ADR 0070](../adr/0070-network-transport.md).

The question: on top of `engine_net_transport`, how the authoritative server's ECS state
reaches clients (what is sent, how it is encoded, which entities first), how a client
predicts its own entities with the server's systems and corrects them, how remote
entities are shown, and how hits are checked against what the shooter saw. Constraints:
CPU per tick on the server, bytes per client, no allocation per tick, and a game
developer who should write a handful of lines.

No code from any of these sources was copied. Quake III is GPL-2.0 and Source SDK 2013 is
under Valve's proprietary SDK license: ideas only. The Unity documentation and the talks
were read for ideas. Fiedler's articles are explanations, not code we use.

## Sources

| Source | Version / date | License | How it was studied |
|---|---|---|---|
| Quake III Arena networking (J. Carmack); F. Sanglard, "Quake 3 Source Code Review: Network Model" | 1999 / code release 2005; review 2012 | GPL-2.0 (ideas only) | known design: one unreliable snapshot stream, each snapshot a delta against the last one the client acknowledged, 32-snapshot history (`PACKET_BACKUP`), per-field netField tables with bit widths, full update when the baseline is gone |
| M. Frohnmayer, T. Gift, "The TRIBES Engine Networking Model" | GDC 1999 / Torque docs | paper | known content: ghost manager (per-client ghost ids, scoping), notify protocol (the sender learns per packet whether it arrived), per-object dirty masks re-marked on loss, priorities by relevance under a fixed packet budget |
| Y. Bernier, "Latency Compensating Methods in Client/Server In-game Protocol Design and Optimization"; Valve wiki "Source Multiplayer Networking", "Lag compensation" | GDC 2001; wiki (HTTP 403 to the fetcher on 2026-10-02) | article / docs | known content: user commands with redundancy, `cl_interp` 100 ms (`cl_interp_ratio 2`), extrapolation up to 0.25 s, prediction re-running commands, error smoothing over 0.1 s, `sv_maxunlag` 1 s, command time = server time − latency − interpolation |
| T. Ford, "Overwatch Gameplay Architecture and Netcode" | GDC 2017 | talk | known content: deterministic ECS systems in fixed command frames shared by client and server, client runs ahead by RTT/2 + buffer and adjusts its rate to keep the server's input buffer small, inputs resent until acknowledged, rollback of predicted components only |
| D. Aldridge, "I Shot You First: Networking the Gameplay of Halo: Reach" | GDC 2011 | talk | known content: per-client priority from relevance (distance, view, recency) under a bandwidth budget; replicated state vs events vs control data; events only for one-shot things |
| J. Cone, "It IS Rocket Science! The Physics of Rocket League Detailed" | GDC 2018 | talk | known content: predict all physics objects from the latest server state with input replay; needs cheap restore and determinism |
| G. Fiedler, "Snapshot Compression" | gafferongames.com 2015, read 2026-10-02 | article | smallest three (2-bit index + 3×9 bits), positions at 512 values/m, delta against the acknowledged baseline, unchanged object 1 bit, relative-index prefix codes (5.5 bits average), small/large delta buckets; 17.38 Mbps raw toward a 256 kbps target |
| G. Fiedler, "State Synchronization" | gafferongames.com 2015, read 2026-10-02 | article | priority accumulator (add priority each frame, reset when sent, skip what does not fit and try the next), jitter buffer of 4–5 frames, quantize both sides identically before each step, adaptive error smoothing (0.95 / 0.85) |
| G. Fiedler, "Networked Physics", "Deterministic Lockstep" | 2014–2015 | articles | known content |
| Unity Netcode for Entities manual (ghosts, `GhostField(Quantization, Smoothing)`, predicted ghosts, snapshot history) | 1.x | docs | known content: per-archetype-chunk serialization, delta against up to three baselines with linear prediction, static entropy model, interpolation tick and prediction tick, `Simulate` tag |
| lightyear (Rust) | `320f825`, 2026-09-21 (also in networking.md) | MIT OR Apache-2.0 | the book: input buffers, `Check` before rollback, time sync, interpolation, priority |
| bevy_replicon | 0.3x docs | MIT OR Apache-2.0 | known design: `app.replicate::<C>()` per component, server-authoritative |
| Counter-Strike 2 sub-tick (Valve public descriptions) | 2023 | — | actions carry their time inside the tick; lag compensation interpolates to it |
| C. Ericson, "Real-Time Collision Detection" §5.3 | 2005 | book | ray against sphere and capsule |
| Google Patents US8678929B1 (EA, "Client-side prediction of a local game object to reduce apparent network lag of multiplayer simulations") | priority 2008-08-01, read 2026-10-02 | patent | status "Expired – Fee Related"; claims a local and a network simulation run together, shared objects course-corrected by blending |
| Web and patent searches: lag compensation / server-side rewind, prediction + reconciliation + snapshot delta | 2026-10-02 | — | see "Patents" |

"Known content" means the talk or page was not re-read on the check date; claims are
limited to the widely documented core of each, and every number that matters is our own
measurement (ADR 0074).

## How shipped systems do it

### Quake III: one stream, deltas against the acknowledged snapshot
- The server keeps the last 32 snapshots per client. Each snapshot contains every entity
  in the client's PVS, as a delta against the newest snapshot the client acknowledged
  (the client echoes the last snapshot number it received).
- Fields are listed per entity type with bit widths; unchanged fields cost one bit.
- When the acknowledged snapshot has left the window, the server sends against nothing.
- **Strength:** simple, robust to any loss pattern: nothing is "resent", the next snapshot
  is a delta from what surely arrived. **Weakness:** every snapshot carries everything in
  view that differs from one old baseline; under loss the baseline ages and deltas grow;
  all entities share the snapshot's baseline.

### Tribes: ghosts, notification, priorities
- Every object relevant to a client has a ghost (a per-client id). Each packet takes the
  highest-priority ghosts with dirty state that fit the budget.
- The transport tells the sender which packets arrived; lost packets re-mark what they
  carried as dirty. Nothing else is resent.
- Scoping: objects enter and leave a client's scope; leaving sends a ghost delete.
- **Strength:** graceful under a fixed budget, only lost data is resent. **Weakness:**
  values are sent whole (no delta against a known baseline); per-client, per-object
  dirty masks.

### Source: prediction, interpolation, lag compensation
- Clients send user commands every tick with the last few repeated.
- Remote entities are drawn `cl_interp` behind (100 ms by default), interpolating stored
  samples; up to 0.25 s of extrapolation.
- The local player is predicted by re-running the shared movement code from the last
  acknowledged state; `cl_pred_optimize 2` skips re-simulation when the server state
  matches the stored prediction. Errors are smoothed over 0.1 s.
- Lag compensation moves other players back to "command time − latency − interpolation"
  (one second of history), traces, and moves them back.

### Overwatch: deterministic ECS prediction, clients ahead with dilation
- Movement and abilities are deterministic systems in fixed command frames, run by both
  sides on the same components.
- The client runs ahead so its input for frame N arrives just before the server
  simulates N; it speeds up or slows down slightly to keep the server's buffer small.
- On a misprediction only the predicted components roll back and the systems re-run.

### Halo: Reach: priority and relevance
- Each client's view of the world is prioritized per object (distance, direction,
  recency, gameplay importance) under a bandwidth budget; state, events and control are
  separate, with different guarantees.

### Rocket League: predict physics
- Every physics object is predicted from the newest server state by replaying inputs: a
  re-simulation of the physics world per snapshot. Fits small worlds.

### Unity Netcode for Entities: the ECS form
- Ghost components declare per-field quantization; the server serializes per archetype
  chunk into a snapshot ring; deltas against acknowledged baselines with a linear
  prediction from up to three of them; unchanged ghosts stop costing anything; predicted
  ghosts roll back and re-run the prediction systems; interpolated ghosts are shown at an
  interpolation tick.

### Fiedler: compression and state sync
- Smallest-three quaternions, bounded quantized positions, deltas against the
  acknowledged baseline with changed bits and bucketed prefix codes; a priority
  accumulator fills each packet; quantize on both sides so the simulation continues
  from the transmitted values.

## What we take, and why

| Question | Options | Taken | Why (numbers in ADR 0074) |
|---|---|---|---|
| Baselines | per snapshot (Q3) / per entity / none (Tribes, whole values) | **per entity**: the newest state of it the client acknowledged, plus one default baseline per snapshot (1 bit when it matches) | budgets mean not every entity is in every snapshot; whole values (Tribes) cost 131.5 bits per moving entity vs 39.9 |
| What is sent | everything differing from the baseline (Q3) / dirty + lost (Tribes) | **changed since last sent, or the last send reported lost** (Tribes' notification on Q3's deltas) | a record in flight is not repeated; deltas still reference what surely arrived |
| Delta code | fixed widths (Q3) / buckets (Fiedler) / exp-Golomb | **zigzag exp-Golomb (k = 2)**, changed bit per component and per word | one code for every quantized field, 3–5 bits for small deltas |
| Predicted baselines | none / linear from acknowledged states (Unity) | **linear through the two newest acknowledged states, per component, used when cheaper (1 bit)** | 53.9 → 39.9 bits per moving entity |
| Quantization | send tables / reflection | **reflection** (`net_quantum`, `net_skip`; quaternions smallest-three 15 bits) | ADR 0019: no send tables |
| Rounding | client only / both sides (Fiedler) | **both**: the server rounds its own state every tick, the client its predictions | prediction is bit-exact: 0 mispredictions after the first state |
| Selection | sort / accumulator + top-K | **accumulator, `select_nth_unstable` of an estimated K, fit in priority order, written in slot order** | O(n) per client; slot gaps stay short |
| Interest | none / radius from a view | **`NetRelevance` + `Server::set_view`**; leaving the radius removes the entity on that client (Tribes scoping) | simplest useful form |
| Inputs | one per packet / redundant window | **every unacknowledged input (≤ 32) every tick** | 10 % loss: 12 missed of ~500 |
| Client clock | fixed offset / dilation (Overwatch) | **±5 % dilation toward a lead of 1 + 2·jitter (+1 under loss) ticks** | no late inputs in steady state |
| Interpolation | 100 ms fixed (Source) / adaptive | **1 interval + 2·jitter (+1 interval under loss) + 0.25 tick**, extrapolation ≤ 2 ticks to bridge holes | ~30–50 ms at 64 Hz |
| Rollback | whole world / predicted entities | **predicted entities' replicated components**, the game's systems re-run | Overwatch; cheap |
| Lag compensation | move the live world (Source) / query history | **a ring of per-tick hit capsules**, queried at the interpolated instant (ADR 0019) | physics never moved; read-only |
| Sub-tick | none / per action (CS2) | **`SubTick` in the game's input + view delay in every input header** | shot time = tick − delay − (1 − fraction) |

What we do not take: Rocket League's predicted physics for every object (deferred: the
game can mark more entities as predicted later, ADR 0074), Unity's three-baseline
prediction and static entropy model (two baselines measured within 1 % of the best
variant), PVS or portal-based relevance (needs the level's data).

## Patents

- The core techniques (snapshots delta-encoded against acknowledged state, client-side
  prediction with re-simulation, entity interpolation, lag compensation by rewinding hit
  volumes, priority accumulators, quantization) were published and shipped in 1996–2001
  (QuakeWorld, Quake III, Tribes, Half-Life with Bernier's 2001 paper): patents from then
  have expired, and later ones would face that prior art.
- **US8678929B1** (EA, priority 2008): runs a local and a network simulation and blends
  shared objects between them. Expired ("Fee Related"). We run one simulation;
  corrections are rollback and re-simulation.
- Searches for lag compensation / server-side rewind and prediction + reconciliation +
  snapshot delta compression found articles and engine documentation, that patent, and
  ZeniMax's motion-vector patents (US10341678, US11695951: cloud game streaming,
  unrelated).
- This is not a legal opinion: no patent was found that covers what ADR 0074 builds; the
  team may want counsel to confirm (a question in the ADR).
