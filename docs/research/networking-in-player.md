# Networking in the player: who runs the loops, how a game talks to them

Research for queue item 4 (core audit 3 §4: "networking in the player: `engine_net`
exists, measured; a game wires it itself"). Checked 2026-10-17, from each engine's
documentation; no code was read.

## What the engine has

- **`engine_net_transport`** (ADR 0070): encrypted UDP endpoints, channels, connect
  tokens, NAT traversal (UPnP, PCP, NAT-PMP, STUN, hole punching, relay).
- **`engine_net`** (ADR 0074):
  - a `Server` the host runs per tick: `update` → the game's systems → `send`;
  - a `Client` per frame: `frame(world, now, input, simulate)` receives, corrects
    mispredictions by re-running `simulate`, predicts the due ticks on its own clock
    (dilated toward an input lead), interpolates remote entities and sends inputs.
- **The co-op example** wires both by hand: about 30 lines of networking in a console
  loop. ADR 0074 notes the runtime's frame loop has no hook for a client's dilated
  clock yet.
- **The player** (ADR 0099) runs the fixed ticks on its own clock and calls the game's
  hooks with the world alone.

## How engines put networking in their loop

| Engine | Session | Who ticks it | Prediction | How game code reaches it |
|---|---|---|---|---|
| Unreal 5 | `UNetDriver` per world | the engine: receive at the frame's start, replicate and send at its end | `CharacterMovementComponent`; the Network Prediction plugin with fixed or independent ticks | actors' replicated properties and RPCs; the game mode spawns a player's pawn on login |
| Unity Netcode for GameObjects | `NetworkManager`: `StartHost`, `StartClient`, `StartServer` | its tick system at a fixed rate | none built in | `NetworkObject`, `NetworkVariable`, RPCs; connection callbacks spawn player objects |
| Unity Netcode for Entities | separate client and server worlds, in one process for a host | the world's system groups; a prediction group re-run for rollback | built in: ghosts predicted by re-running the prediction group | components marked as ghosts; input buffers per tick |
| Godot 4 | `MultiplayerAPI` with a peer (ENet, WebRTC) | the scene tree polls it every frame | none built in | `MultiplayerSynchronizer`, `MultiplayerSpawner`, RPCs |
| Bevy (lightyear) | plugins in the app | the schedule's fixed update, re-run for rollback | built in | components registered for replication; input buffers |

**What they agree on:**
- the engine owns the session's loop: receive before the simulation, send after it;
- a host plays in the server's world;
- a joining player gets a pawn from the game's code (a login or connection callback);
- prediction, where built in, re-runs the same simulation systems.

**Where we differ:** Netcode for Entities runs a host as two worlds in one process.
ADR 0019's addendum rules that out: a listen server is the server's world with the host's
input fed in.

## Options

**Who owns the session:**
1. **The game,** as in the co-op example. Every game writes the loops, and a client must
   run its ticks inside `Client::frame` while the player runs them on its own clock.
2. **The player,** configured with the game's schema and input type. It runs the server
   around the game's ticks and the client in place of them. Chosen.

**How the game's hooks reach it:** they get the world only, so through a resource of the
world.
- The session itself cannot live there: an endpoint holds Windows CNG key handles, which
  are not `Sync`.
- The player keeps the session (erased to its input type behind a trait) and the
  world's **`Network`** resource carries the game's requests and the session's news:
  - requests: host, join, leave, the host's pawn, pawns given to players;
  - this tick's input;
  - outgoing events, events received, the state, invites.

**A client's ticks:** the client's clock decides how many ticks a frame runs, and
re-simulation runs past ones again. The runtime gets a driven frame:
- the client calls a step per tick it runs, which runs the game's tick and propagates
  transforms;
- extraction runs once after the frame, when the remote entities are interpolated too;
- the frame is shown as it is, at alpha 1: remote entities are already where the client
  interpolated them;
- the client's own fraction of its next tick is the game's, to present its pawn between
  ticks (ADR 0019's addendum: the camera follows input every frame).

**The host's input:** sampled for each tick. A hook before every host tick and client
frame (`Game::net_input`) writes it, so the host's input is a tick's age at most, as in
singleplayer.

## What we take

- `PlayerSettings::network: Option<NetConfig>`, made by `NetConfig::new::<Input>(schema)`.
- The world's `Network` resource, the game's whole interface.
- **The host:**
  - binds a UDP port and maps it on the router in the background (UPnP, PCP, NAT-PMP);
  - lists its LAN and mapped addresses;
  - makes invites: connect tokens as text, one per player.
- **A joining player** connects with an invite.
- Hole punching and relays need a rendezvous and servers the engine does not pick (ADR
  0070: no default third-party servers): later, through a game's lobby service.
- Events between the game and the session are moved as bytes tagged with their type, so
  the session needs to know only the input type.
