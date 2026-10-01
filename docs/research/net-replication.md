# Replication, prediction and lag compensation

Checked on 2026-10-02. Decision: ADR 0074 (in progress). Model: [ADR 0019](../adr/0019-networking-model.md)
and [networking.md](networking.md); transport: [ADR 0070](../adr/0070-network-transport.md).

(The full research note — sources with licenses, how Quake III, Tribes, Source, Overwatch,
Halo Reach, Rocket League and Fiedler's articles do it, patents — is being written; see
"Work status".)

## Work status (2026-10-02, paused at the owner's request)

**Done (branch `feat/net-replication`, pushed):**
- `crates/engine_net`: schema from reflection (`NetSchema::replicate::<T>()`, quanta from
  `#[reflect(net_quantum)]`, smallest-three quaternions), bit packing (exp-Golomb),
  server (slots, per-tick quantized frames shared by clients, priority accumulator under
  a byte budget, delta against the acknowledged state, predicted baselines, delivery
  notification, removals, ownership, input stream with redundancy, events reliable and
  unreliable, lag-compensation ring of hit capsules), client (decoding against baseline
  ring, spawn/despawn, prediction ring, misprediction check by comparison, rollback and
  re-simulation, time dilation from the server's input-lead feedback, adaptive
  interpolation delay with short extrapolation, sub-tick view times).
- `engine_ecs`: raw chunk access (`Archetype::chunk_entities`, `pod_column`,
  `World::pod_column_mut`, `get_raw_mut`) with a test.
- Tests (all green): convergence at 0/5/15/30 % loss with latency and jitter, late join,
  prediction without drift (0 mispredictions after spawn), rollback corrections,
  interpolation smoothness (worst frame step error < 20 % of a step), lag compensation
  (30/30 hits vs 0/30 without), reliable/unreliable events, byte budget, zero allocations
  per tick (server and client), the co-op game (Jolt characters, button + door, hitscan).
- `examples/coop` (listen server + guest over UDP loopback) and
  `examples/replication_bench.rs`.
- Parallel snapshot packing on the job system (`Server::send_with(.., Some(&pool))`):
  last change, tests green, bench numbers being collected.

**Findings so far (ref-1, release, machine shared with another agent):**
- Bits per moving entity per tick (1,000 bodies, 8-tick-old baselines): raw 352,
  quantized whole 163, delta 53.9, delta vs predicted baseline 39.9, Tribes-style
  changed-whole 131.5.
- Server: 1,000 moving entities, 32 clients ≈ 0.78 ms per tick serial, 0.47 ms parallel;
  10,000 entities ≈ 0.33 ms + ~35–45 µs per client (per-client scan of every slot).
- Client: 1,000 interpolated entities ≈ 50–60 µs per frame; rollback framework cost
  ≈ 0.12 µs per re-simulated tick (game systems extra).
- Co-op over UDP: host 130 µs per tick (Jolt included), guest 71 µs per frame; 8
  corrections in 14 s (1 at spawn, 7 passing under the opening door, whose interpolated
  position trails the server's).

**Next:** finish this note (sources, patents: core techniques are 1999–2001 prior art;
EA's US8678929B1 on blended dual simulation is expired and not used), ADR 0074, dev guide
section, perf history rows, CLAUDE.md layout/§13/§17 lines, final bench run,
`tools/check.ps1 -Gpu -Perf`, PR.
