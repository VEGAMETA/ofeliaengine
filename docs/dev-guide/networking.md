# Networking: hosting, joining, NAT

The engine's multiplayer model is an authoritative server at a fixed tick with snapshot
replication and client prediction ([ADR 0019](../adr/0019-networking-model.md)). This
page covers the layer under it, `engine_net_transport` ([ADR 0070](../adr/0070-network-transport.md)):
connections, channels, security, and how a player's PC becomes reachable when it hosts a
co-op game. Singleplayer uses none of it (no loopback server).

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
