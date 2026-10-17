# Networking: hosting, joining, NAT, making a game co-op

The engine's multiplayer model is an authoritative server at a fixed tick with snapshot
replication and client prediction ([ADR 0019](../adr/0019-networking-model.md)). §1–6
cover the transport, `engine_net_transport` ([ADR 0070](../adr/0070-network-transport.md)):
connections, channels, security, and how a player's PC becomes reachable when it hosts a
co-op game. §7 is what most games use directly: `engine_net`, replication, prediction and
lag compensation ([ADR 0074](../adr/0074-replication-prediction-lag-compensation.md)).
Singleplayer uses none of it (no loopback server).

## 1. The pieces

| Piece | What it is |
|---|---|
| `Endpoint<S>` | a server or a client on one socket. Implements `Transport`, the interface replication uses. |
| `ConnectToken` | a player's permission to join one server, made by whoever admits players |
| `UdpSocket` | the real socket; `memory::MemoryNetwork` simulates one for tests |
| `nat::PortMapper`, `nat::NatSession`, `nat::decide` | NAT traversal for hosts |

Everything happens in your network step, on the thread you call it from; there is no
networking thread and no async runtime:

```rust
// Once per tick (or once per frame on a client):
endpoint.update(now);                        // receive, timeouts, losses
while let Some(event) = endpoint.poll_event() { /* Connected, Disconnected, Delivered, Lost */ }
for conn in endpoint.connections() {
    while let Some(bytes) = endpoint.receive(conn, ChannelId(0)) { /* events, chat */ }
}
// ... simulate, then queue what goes out:
endpoint.send(conn, ChannelId(1), &snapshot)?;
endpoint.flush(now);                         // build, encrypt, send
```

A dedicated server that has nothing to do until its next tick can wait on its socket
instead of sleeping: `endpoint.socket().wait_readable(time_to_next_tick)`.

## 2. Channels

Configure them in `EndpointConfig::channels`, the same list on server and client (up to
8). The defaults are 0 reliable, 1 unreliable, 2 unreliable-sequenced.

| Kind | Guarantees | Use for |
|---|---|---|
| `ReliableOrdered` | every message, once, in order | one-shot events, chat, RPCs, the join-time world state |
| `Unreliable` | may be lost; never duplicated or corrupted; large messages arrive whole or not at all | snapshots (each carries its tick) |
| `UnreliableSequenced` | as unreliable, and nothing older than the newest delivered | latest-value streams (voice levels, cursor positions) |

- **Sizes:** each channel has `max_message_size` and `buffer_bytes`. Reliable messages
  wait in a window of `buffer_bytes` (a power of two) until acknowledged and read;
  `send` returns `QueueFull` when it is full. Unreliable messages queue for one flush.
- **Player input is not a reliable message.** ADR 0019: inputs go every tick on an
  unreliable channel with redundancy (the last few ticks in every packet).
- **Delivery notification:** `ChannelConfig::unreliable().with_notify()` reports
  `Event::Delivered` or `Event::Lost` for every message id `send` returned. Replication
  uses it to re-send only what was lost.
- **Budget:** `EndpointConfig::send_rate` (bytes per second per connection) and
  `send_budget(conn)` let you fill each client's packet to what it can take; unreliable
  messages over budget are dropped (and reported lost), reliable ones wait.
- **Statistics:** `stats(conn)` gives round trip, jitter (RTT variance), loss, rates,
  bytes in flight. Show them in a debug overlay.

## 3. Security: connect tokens

Nothing joins without a token. Whoever decides who may join generates one per player:

```rust
let token = ConnectToken::generate(&TokenParams {
    protocol_id: MY_GAME_PROTOCOL,          // change it when messages or channels change
    client_id: account_id,                  // unique on the server
    expire_seconds: 30,                     // how long it may be used to start joining
    timeout_seconds: 10,                    // silence before a connection is dropped
    server_addresses: &report.addresses,    // where the server can be reached
    user_data,                              // 256 bytes only the server can read
}, &private_key, unix_now())?;
```

- **Dedicated servers:** your backend (after it authenticated the player) generates the
  token; the servers know `private_key`.
- **Co-op (listen server):** the host is the one admitting players: it keeps its own
  random `private_key` and makes tokens for the friends it invites.
- **The token is a secret** (it holds the packet keys): deliver it over HTTPS or the
  platform's invite/lobby system, never in plain text over the internet.
- Every packet afterwards is encrypted and authenticated (AES-256-GCM); replayed,
  forged or modified packets are dropped. A server never answers a request without a
  valid token, and never with more bytes than it received, so it cannot be used to
  amplify attacks.

## 4. Hosting a co-op game: how players reach you

Home routers (NAT) drop unsolicited packets, so a host is usually unreachable. The engine
climbs a ladder and tells you which rung worked:

1. **UPnP** — asks the router to forward one UDP port (most home routers).
2. **PCP, then NAT-PMP** — the same through another router protocol (Apple and newer
   routers; PCP also opens IPv6 firewalls).
3. **Hole punching** — finds the router's public mapping of your socket with **STUN**,
   then host and player send to each other at the same time so both routers let the
   other side in. Needs a **rendezvous** to exchange addresses (your lobby service, Steam
   or Epic lobby data, an invite message).
4. **Relay** — when both sides are behind strict ("symmetric") NATs, packets go through a
   server: the reference relay in `nat::relay`, Steam Datagram Relay (implement
   `DatagramSocket` over Steamworks' `ISteamNetworkingFakeUDPPort`), or TURN.

```rust
let socket = UdpSocket::bind("[::]:27015".parse()?)?;          // or port 0 for any
let port = socket.local_addr()?.port();
let mapper = nat::PortMapper::start(nat::NatConfig::default(), port);   // rungs 1-2, background
let mut server = Endpoint::server(socket, config, ServerConfig {
    private_key: Key::random(), max_clients: 4, public_addresses: vec![],
}, now)?;
let mut nat_session = nat::NatSession::new(&my_stun_servers, now);       // rung 3, sans-I/O

// In the network step, after server.update(now):
nat_session.service(&mut server, now);

// Once STUN is done and the mapper has an answer:
let mapping = match mapper.status() { nat::MappingStatus::Mapped(m) => Some(m), _ => None };
let report = nat::decide(lan_addr, mapping.as_ref(), nat_session.stun_result().as_ref(), have_relay);
server.set_public_addresses(&report.addresses);
// report.method: Public, Upnp, Pcp, NatPmp, HolePunch, Relay or LanOnly. Show it in the
// host's lobby UI ("Open NAT", "Players join through hole punching", ...).
```

For each joining player with `report.method == HolePunch`: publish your candidates
(`report.addresses`) on the rendezvous; when the player's candidates arrive, add
`nat::punch::Puncher::new(&token.punch_key(), session_id, false, their_candidates, now, 5 s)`
to `nat_session.punches`. The player does the same with `initiator = true` and connects
with the token as usual; its requests get through once both sides have punched.

### What happens to the router

- Exactly **one UDP port** is mapped per session: the game socket's.
- The mapping has a **lease** (1 hour by default) renewed at half; it is **removed** when
  the `PortMapper` is stopped or dropped (quitting the session). If the game crashes, the
  router removes it when the lease runs out. Only routers that refuse leases get a
  permanent mapping, and it is removed on exit too.
- A router behind another NAT (double NAT, or carrier-grade NAT where the ISP shares one
  public address among customers, `100.64.x.x` or private addresses on the router's WAN
  side) can map the port, but nobody on the internet can reach it. The ladder detects
  this (the mapped address is not public or disagrees with STUN) and moves on.
- Nothing is contacted unless you configure it: there are no built-in STUN servers,
  relays or rendezvous. Choose them (yours, or public STUN servers) and say so in your
  privacy notice: a STUN server sees the player's IP address.

### Checking a machine

`cargo run -p engine_net_transport --example nat_probe` maps one port with UPnP, checks
it, removes it, checks the router's table holds nothing of ours, then tries PCP/NAT-PMP.
It prints the router's external address: if that address is private or `100.64-127.x.x`,
the ISP's NAT is in front and players can only join by hole punching or a relay.

## 5. Testing with bad networks

`memory::MemoryNetwork` runs endpoints in one process on a virtual clock:

```rust
let net = MemoryNetwork::new(seed);
net.set_default_conditions(LinkConditions {
    latency: Duration::from_millis(50), jitter: Duration::from_millis(10),
    loss: 0.05, duplicate: 0.01, reorder: 0.02,
});
let home = net.add_nat("198.51.100.1".parse()?, NatBehavior::PortRestricted);
let host_socket = net.bind_behind_nat("192.168.1.10:27015".parse()?, home);
```

Results repeat for the same seed. Per-link conditions (`set_link_conditions`) cut one
direction or one player.

## 6. Numbers (ref-1, release)

- 0.2–0.45 µs of CPU per packet sent and 0.26–0.56 µs received (encryption included);
  Windows' `sendto` adds ~4.4 µs, so a 64-player server at 64 Hz spends ~2 % of a core.
- The transport adds ~1 µs to a localhost one-way trip.
- No allocation per network step.

See `docs/perf/history.md` and ADR 0070 for the full measurements.

## 7. Making a game co-op: replication, prediction, lag compensation

`engine_net` ([ADR 0074](../adr/0074-replication-prediction-lag-compensation.md)) turns
an ECS game into a networked one. The server is authoritative and runs at a fixed tick;
clients predict their own player with **the same systems** and see everything else
slightly in the past, smoothly. A complete example is `crates/engine_net/examples/coop`
(two players, Jolt characters, a button that opens a door, a lag-compensated gun);
`cargo run -p engine_net --release --example coop` runs it over UDP on this machine.

### 7.1 Mark what travels

State is plain-data components with reflection; the attributes say how precise:

```rust
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
struct Player {
    #[reflect(net_quantum = 0.001)] pos: Vec3,   // millimetres
    #[reflect(net_quantum = 0.001)] vel: Vec3,
    #[reflect(net_quantum = 0.0001)] yaw: f32,
    grounded: u8, cooldown: u8, health: u8, _pad: u8, // exact
}
reflect_component!(pod Player);

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct DoorOpened { tick: u32 }                  // a one-shot event

fn schema() -> NetSchema {                       // the same on host and clients
    NetSchema::new(MY_PROTOCOL)
        .replicate::<Player>()
        .replicate::<Door>()
        .event::<DoorOpened>()
}
```

- `net_quantum = q`: the float travels as a whole number of `q` (small changes cost a few
  bits). Without it a float is sent exactly (32 bits).
- `Quat` fields go as "smallest three" at 15 bits per component.
- `net_skip` (or `skip`): stays local.
- Engine types you cannot annotate: `replicate_with::<Transform>(FieldHints::new()
  .quantum("translation", 0.001).skip("scale"))`.
- **Events are for things that happen once** (a door opened, a hit for effects), never for
  state: state is replicated, and a late joiner gets it.
- **Player actions are input**, not events: put them in the input type with a `SubTick`
  for when they happened inside the tick.

### 7.2 The input and the shared systems

```rust
#[derive(Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct PlayerInput { move_x: f32, move_z: f32, yaw: f32, jump: u8, fire: u8, _pad: [u8; 2] }

fn simulate(world: &mut World, players: &mut QueryState<(&NetInput<PlayerInput>, &mut Player)>, dt: f32) {
    players.for_each(world, |(input, p)| { /* move p from input.input */ });
}
```

`NetInput<I>` exists on every player's pawn on the server and only on its own pawn on a
client, so this system runs for exactly the entities each side simulates. **Rule for
exact prediction:** the system reads its state only from replicated components (and
resets anything derived from them, e.g. `set_character_transform` from `Player.pos`
before `move_character`). The engine rounds values to their quanta on both sides, so the
client's prediction then matches the server bit for bit and nothing is re-simulated.

### 7.3 The host (listen server)

```rust
let mut server = Server::<PlayerInput, _>::new(endpoint, schema(), ServerSettings::default(), &mut world);
let door = world.spawn((Replicated::new(), Door::default()));
let me = world.spawn((Replicated::new(), Player::default(), Transform::IDENTITY, HitCapsule::upright(1.8, 0.3)));

// every tick:
server.update(&mut world, now);                       // guests' inputs land in their pawns
while let Some(e) = server.poll_event() {
    if let ServerEvent::ClientConnected(id) = e {
        let pawn = world.spawn((Replicated::new(), Player::default(), Transform::IDENTITY, HitCapsule::upright(1.8, 0.3)));
        server.set_owner(&mut world, pawn, Some(id)); // this guest predicts it
    }
}
server.set_local_input(&mut world, me, my_input);     // the host plays directly
simulate(&mut world, &mut players, server.dt());
rules(&mut world, &mut server);                       // server-only: doors, damage
server.send(&mut world, now);                         // snapshots out
```

Despawning a `Replicated` entity removes it on every client. A dedicated server is the
same without `set_local_input`.

### 7.4 A guest

```rust
let mut client = Client::<PlayerInput, _>::new(endpoint, schema(), ClientSettings::default(), &mut world);
// every rendered frame, with the input sampled for it:
client.frame(&mut world, now, input, |w, dt| simulate(w, &mut players, dt));
while let Some(e) = client.poll_event() {
    match e {
        ClientEvent::Spawned { entity, predicted, .. } => { /* add a mesh, a Jolt character */ }
        ClientEvent::Despawned { entity, .. } => { /* free what you attached */ }
        _ => {}
    }
}
client.read_events::<DoorOpened>(|tick, _| play_sound());
```

`frame` receives, corrects mispredictions (re-running your `simulate`), predicts the due
ticks, sends inputs and writes remote entities' components interpolated. After it, draw:
your own pawn is current, the others are ~2–3 ticks in the past and smooth.
`client.correction::<Player>(pawn)` gives the before/after of a correction if you want to
blend it out visually.

### 7.5 Hits on what the shooter saw

The server records every entity with a `HitCapsule` and a `Transform` each tick. A shot
from input is checked at the instant the player saw:

```rust
let time = input.header.view_time(SubTick(fire_subtick));     // ticks, fractional
if let Some(hit) = server.lag().raycast(eye, dir, wall_distance, time, Some(shooter)) { /* damage hit.entity */ }
```

Walls do not move: test them in the live physics world and pass the distance as `max`.
History is one second (`ServerSettings::max_rewind`).

### 7.6 Tuning and diagnostics

- `ServerSettings`: tick rate (64), `max_snapshot_bytes` per client per tick (1,100: one
  packet), owner priority, rewind. The transport's `send_rate` caps bytes per second.
- `NetRelevance { radius, priority }` on an entity plus `server.set_view(client, pos)`:
  far entities are not sent (and are removed on that client).
- `ServerStats` (bytes, records, losses, missing inputs, input lead) and `ClientStats`
  (mispredictions, re-simulated ticks, interpolation delay, extrapolations) belong in a
  debug overlay. Mispredictions in steady walking mean your systems read state that is
  not replicated.
- Test under bad conditions with `MemoryNetwork` (§5); `crates/engine_net/tests` shows a
  whole harness on a virtual clock.

### 7.7 Numbers (ref-1, release)

- Bits per moving entity per tick: ~40 (positions in mm, rotation, velocity), against 352
  raw.
- Server: 100 moving entities and 8 clients ~50 µs per tick; 1,000 and 32 clients
  0.93 ms (0.34 ms with packing in parallel, `send_with`).
- Client: own pawn 1.5 µs per frame, a rollback adds ~0.12 µs per re-simulated tick plus
  your systems; 1,000 interpolated entities ~65 µs.
- No allocation per tick on either side.

## 8. Co-op in the player: hosting and joining without writing the loops

In the player ([ADR 0106](../adr/0106-networking-in-the-player.md)) you do not write the
loops of §7.3 and §7.4: the player runs the session, and your game talks to it through the
world's `Network` resource.

```rust
use engine_app::{HostOptions, NetConfig, NetEvent, Network, To};

// Settings: your input type and the function that builds your schema (§7.1).
settings.network = Some(NetConfig::new::<PlayerInput>(schema));

impl Game for MyGame {
    // The input for the next tick, from what the player pressed (§7.2).
    fn net_input(&mut self, world: &mut World) {
        let input = self.sample(world.resource::<engine_input::Input>().unwrap());
        world.resource_mut::<Network>().unwrap().set_input(&input);
    }

    // The shared systems: on the host for everyone, on a guest for its predictions.
    fn tick(&mut self, world: &mut World, dt: f32) {
        simulate(world, dt); // reads NetInput<PlayerInput> and the replicated components

        let net = world.resource_mut::<Network>().unwrap();
        for event in net.events().collect::<Vec<_>>() {
            if let NetEvent::PlayerJoined(player) = event {
                let pawn = world.spawn((Replicated::new(), Player::default(), Transform::IDENTITY));
                world.resource_mut::<Network>().unwrap().give(pawn, Some(player));
            }
        }
    }
}
```

- **The menu:**
  - `net.host(HostOptions::default())` hosts at the next frame. The router is asked to
    forward the port in the background; `NetEvent::PortMapped` says when players on the
    internet can reach you.
  - `net.invite()` makes an invite: text to paste into a chat, or to send through your
    platform's invite. One per player.
  - `net.join(&invite)` joins; `net.leave()` ends the session.
- **Your own pawn:** spawn it while hosting and say so with `net.set_local_pawn(Some(pawn))`.
  Its input is fed every tick, as a guest's is.
- **What happens on a guest:**
  - `NetEvent::Connected`, then `NetEvent::Spawned { entity, predicted }` for every
    replicated entity: attach its mesh there. `predicted` is the guest's own pawn.
  - Read events in `present`: a guest's ticks run inside the frame.
  - Show your own pawn between its ticks with `net.alpha()`; the remote ones are already
    smooth.
- **One-shot events:** `net.send_event(&e, To::All, true)` and
  `net.read_events::<E>(|tick, from, e| ...)`, for the event types in your schema.
- **Offline,** the player runs your ticks as in singleplayer and sends nothing. Feed your
  pawn's `NetInput` yourself there.
- **Not there yet:**
  - hole punching and relays, which need a rendezvous of yours (§4);
  - dedicated servers.
