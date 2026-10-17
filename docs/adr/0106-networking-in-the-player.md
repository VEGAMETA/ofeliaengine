# 0106. Networking in the player: the player runs the session, the game talks through a resource

- Status: Accepted (2026-10-17)
- Date: 2026-10-17
- Related:
  - [ADR 0019](0019-networking-model.md): the model and its addendum (no loopback, the
    camera per frame);
  - [ADR 0070](0070-network-transport.md): the transport, NAT traversal, connect tokens;
  - [ADR 0074](0074-replication-prediction-lag-compensation.md): replication, prediction,
    lag compensation;
  - [ADR 0099](0099-player-crate.md): the player;
  - research note `docs/research/networking-in-player.md`;
  - core audit 3 §4, queue item 4.

## Context

- **`engine_net` replicates, predicts and compensates lag,** measured, but a game wires
  it itself: a console loop of about 30 lines in the co-op example.
- **The player runs fixed ticks on its own clock.** A network client must run its ticks
  inside `Client::frame`, which decides how many and re-runs past ones after a
  correction. The runtime has no way to let it.
- **The game's hooks get the world alone.** A network endpoint holds Windows CNG key
  handles, which are not `Sync`, so the session cannot be a resource of the world.

## Options

1. **The game owns the session** (the co-op example). Every game writes the loops, and
   the runtime still cannot run a client's ticks.
2. **The player owns it** (Unreal's net driver, Unity's network manager), configured with
   the game's schema and input type; the game reaches it through a resource of the world.
   Chosen.

## Decision

### What the game writes

- **`PlayerSettings::network = Some(NetConfig::new::<Input>(schema()))`**, the same schema
  on every machine.
- **`Game::net_input`:** writes the input for the next tick (`net.set_input(&input)`). The
  player calls it before every host tick and every client frame.
- **`Game::tick`:** the shared systems. They run on the host for everyone and on a client
  for its predictions, reading `NetInput<Input>` and replicated components (ADR 0074).
- **The world's `Network` resource:**
  - `host(HostOptions)`, `join(&invite)`, `leave()`;
  - `invite()`: a connect token as text, one per player;
  - `set_local_pawn(entity)`: the host's own pawn, fed its input;
  - `give(pawn, player)`: a joined player's pawn, which that player predicts;
  - `events()`: players joining and leaving, connected and disconnected, replicated
    entities spawned and despawned on a client, another build;
  - `send_event(&e, to, reliable)` and `read_events::<E>(|tick, from, e| ..)`: the
    schema's one-shot events;
  - `state()`, `addresses()`, `stats()`, and `alpha()`: a client's fraction of its next
    tick, to present its pawn between ticks.

### What the player does

- **Offline:** nothing (ADR 0019's addendum: no loopback).
- **Hosting:**
  - it binds a UDP port and makes a server endpoint with a fresh key;
  - it maps the port on the router in the background (UPnP, PCP, NAT-PMP; the mapping is
    removed when the session ends) and lists the LAN and mapped addresses for invites;
  - every tick: `Game::net_input` → `Server::update` (guests' inputs into their pawns, joins
    and leaves as events) → the host's input into its pawn → `Game::tick` → the pawns
    given → `Server::send`.
- **Joined:**
  - each frame the client runs in place of the player's ticks
    (`Runtime::frame_driven`), `Game::tick` as its simulation;
  - every tick it runs propagates transforms, so the next one sees them;
  - extraction runs once after the frame, with remote entities interpolated, and shows it
    as it is.
- **Events** travel between the resource and the session as bytes tagged with their
  type: the session knows only the input type. `engine_net` gains byte-level event calls
  for it.

### Not now

- Hole punching and relays: they need a rendezvous and servers a game picks.
- Dedicated servers: a player without a window or a device.
- Predicted spawns and more than owned entities predicted (ADR 0074's list).

## Consequences

- A co-op game writes its schema, its input, its shared tick, a pawn for a player who
  joins, and the menu's calls; the player does the rest.
- **On a client** the player shows every frame as it is: remote entities interpolated by
  the client, its own pawn at its newest predicted tick. The game presents the pawn
  between ticks with `alpha()`, as it presents the camera every frame.
- The host's input is sampled before its tick, as in singleplayer.
