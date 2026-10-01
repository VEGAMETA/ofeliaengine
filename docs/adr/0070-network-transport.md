# 0070. Network transport: encrypted UDP with connect tokens, channels, NAT traversal

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0019](0019-networking-model.md) and its addendum (authoritative ticks,
  snapshots, prediction; "the wire format, transport and encryption get their own ADR
  with a prototype")
- Research: [`docs/research/net-transport.md`](../research/net-transport.md)
- Code: `crates/engine_net_transport`; measurements `bench/net-crypto`,
  `crates/engine_net_transport/examples/net_bench.rs`, `examples/nat_probe.rs`

## Context

ADR 0019 fixed the model (authoritative server at a fixed tick, snapshots, prediction,
lag compensation) and left open: our own UDP layer against GameNetworkingSockets or
netcode, and encryption through Windows CNG against a library. The team asked for NAT
traversal so co-op hosts (listen servers) are reachable: UPnP → NAT-PMP/PCP → STUN hole
punching → relay, one UDP port per session, removed on exit. Replication (stage 2) is
built on this API next, so it must provide what replication needs: unreliable snapshots
with delivery notification, reliable events, per-connection estimates and a byte budget.

Constraints: no async runtime in the core, no allocation in steady state (CLAUDE.md §5),
latency first (§7), few and permissive dependencies (§15), no telemetry, Windows 11 only.

## Options

### Transport
1. **GameNetworkingSockets (BSD-3).** The best-engineered option: ack vectors, lanes,
   AES-GCM, ICE, SDR. C++ with protobuf + Abseil and OpenSSL/libsodium, a service
   thread, its own threading and allocation. Vendoring it would be the biggest C++
   dependency in the engine for a few thousand lines of logic we need.
2. **netcode + reliable.io / yojimbo (BSD-3, C/C++), or ENet (MIT).** Small, but C/C++
   behind FFI; netcode needs libsodium; ENet has no security; reliable.io's 32-bit ack
   mask fails on bursts (measured below).
3. **QUIC (quinn, MIT/Apache).** Runtime-free core, but TLS and certificates (a PKI for
   listen servers), rustls + ring/aws-lc, congestion control built for fair bulk transfer;
   quinn itself needs tokio. A good fit for browsers (WebTransport), not for the core.
4. **Our own, in Rust, sans-I/O (chosen):** netcode's tokens and handshake,
   GNS/QUIC-style acknowledgement and loss detection, channels over it, sockets behind a
   trait. ~3,000 lines plus tests, no new crates.

### Encryption
1. **RustCrypto `chacha20poly1305`** (MIT/Apache, ~10 crates): 1.1–1.8 µs per packet here.
2. **`ring`** (Apache/ISC, C and assembly via `cc`): 0.06–0.16 µs.
3. **Windows CNG AES-256-GCM (chosen):** 0.11–0.25 µs, no new dependency.

### NAT traversal
1. **`igd-next` + `natpmp` crates:** HTTP client, XML tree, URL, `rand` (or tokio and
   hyper); no PCP.
2. **Our own (chosen):** the protocols are small and fixed (three SOAP requests, two UDP
   formats, a 20-byte STUN request); ~1,500 lines with mock-router tests.

## Measurements

ref-1 (Ryzen 7 7700, Windows 11), release, commit `3d8c29b` of `feat/net-transport`,
three runs (ranges). The GPUs are not involved: the transport has no rendering code.

**Encryption** (`bench/net-crypto`, seal / open per packet): CNG AES-256-GCM 113/108 ns at
32 B, 217/255 ns at 1,200 B; ring AES-256-GCM 59/75 and 154/162; ring ChaCha20 113/122
and 426/469; CNG ChaCha20 239/241 and 2,104/2,110; RustCrypto ChaCha20 1,103/1,120 and
1,767/1,769. AES-NI makes AES-GCM the fast choice in every library; CNG's ~50–90 ns over
ring is 0.3 ms per second at 4,096 packets/s (64 clients at 64 Hz).

**Transport CPU per packet** (`net_bench`, an in-process socket so the kernel is out):

| Message per packet | Send (build + seal + hand over) | Receive (open + parse + deliver) |
|---|---|---|
| 1,100 B | 0.36–0.45 µs | 0.47–0.56 µs |
| 300 B | 0.27–0.31 µs | 0.33–0.38 µs |
| 32 B | 0.21–0.24 µs | 0.26–0.29 µs |

**With the kernel, loopback UDP:** send 4.4–4.8 µs per packet (`sendto` is ~4.4 µs of
it), receive 1.1–1.5 µs; an empty `update` (one `recvfrom` → `WSAEWOULDBLOCK`) 0.35 µs.

**Throughput, loopback UDP:** reliable bulk (256 KiB messages, 1 MiB window) 187–196 MB/s
with 0 packets lost; unreliable 1,100-byte packets 144–177 k/s (158–195 MB/s), 0 % kernel
drops.

**Latency the transport adds on localhost** (64-byte message, echo on another thread,
20,000 round trips, median): busy polling raw UDP 8.2–10.1 µs, through endpoints
10.3–10.4 µs: **0.2–1.1 µs per one-way trip**; blocking waits (`WSAPoll`) raw 18.5 µs,
endpoints 21.0 µs (1.2 µs per trip). Each blocking wake costs ~5 µs.

**Allocations:** 0 per network step in steady state (update, send, receive, flush,
events, losses, resends, notifications), on loopback UDP and on the in-memory network
with loss, jitter, duplication and reordering (`tests/zero_alloc.rs`).

**Acknowledgement mask, before → after:** with reliable.io's 32-bit mask, a 64 MiB
loopback transfer declared 1,152,515 packets lost and ran at 10 MB/s (packets arriving in
bursts of more than 33 between the receiver's flushes never got acknowledged). With a
mask of 64–256 bits sized to what arrived since the last acknowledgement and at most 192
data packets in flight: 0 lost, 187–196 MB/s.

**Real router** (`nat_probe`, this LAN, 2026-10-01): SSDP answer in 26 ms,
`AddPortMapping` 2.2 ms, `GetSpecificPortMappingEntry` confirmed the mapping,
`DeletePortMapping` 2.3 ms, the router's table afterwards held none of ours. The router's
external address was 10.62.54.160 (the ISP's carrier-grade NAT), so the ladder correctly
treats the mapping as useless and goes on to hole punching. PCP and NAT-PMP: no answer
(1.76 s, 3 tries).

## Decision

### Crate and threading
- `engine_net_transport`: one crate, sans-I/O endpoint, sockets behind
  `DatagramSocket`, NAT traversal in `nat`. Dependencies: `thiserror` and `windows-sys`
  features (both already in the tree).
- **No I/O thread.** The game's network step calls `update` (receive everything, timeouts,
  losses) and `flush` (send). Measured: a blocking wake costs ~5 µs, an empty poll
  0.35 µs; the data is consumed at the tick anyway. A dedicated server idles in
  `UdpSocket::wait_readable` (`WSAPoll`) between ticks. The `Endpoint` is `Send`, so a
  game can still drive it from its own network thread.
- UDP sockets: non-blocking, `SIO_UDP_CONNRESET` off (otherwise one departed peer's ICMP
  error makes the next `recvfrom` fail for every peer), 1 MiB buffers, dual-stack on
  `[::]`.

### Security (netcode's design, our sizes)
- **Connect tokens** (2,048 bytes): whoever admits players (a backend, or the co-op host
  itself) seals a private part with a key shared with the server: client id, timeout,
  up to 8 server addresses, two random 256-bit packet keys, 256 bytes of user data.
  AES-256-GCM, random 96-bit nonce (at most 2^32 tokens per key, NIST SP 800-38D). The
  token travels over an authenticated, encrypted channel (HTTPS, the platform's invite).
- **Handshake:** request (1,061 bytes, plaintext, only full size accepted) → challenge
  (333 bytes, sealed) → response → first payload. No answer at all without a valid,
  unexpired token naming one of the server's addresses; no answer larger than the
  request. A token seen from one address is ignored from any other; one client id holds
  one connection.
- **Packets:** prefix (kind + sequence byte count) + sequence + AES-256-GCM (nonce =
  sequence, AAD = version, protocol id, prefix). A 256-entry replay window drops repeated
  and old sequences before decryption.
- **Nonce safety beyond netcode:** connection sequences start at a random point below
  2^62 and server handshake packets use the upper half, so a token presented again after
  a crash never repeats a (key, nonce) pair.
- **Protocol id** (in every token and AAD): the game changes it whenever its channel
  layout or messages change; mismatched builds cannot talk.

### Reliability and channels
- Payload packets carry the newest sequence received, a 64–256-bit mask of earlier ones
  (sized to everything received since the last acknowledgement), and the
  acknowledgement delay. At most 192 packets with data in flight, so one acknowledgement
  covers them. Loss: QUIC's rules (3 packets, 9/8 RTT, probe timeout); RTT and its
  variance: RFC 6298 with the delay removed.
- Up to 8 channels per connection, configured identically on both sides:
  - **unreliable** and **unreliable-sequenced**: messages up to a configured size,
    fragmented in 1,024-byte pieces, delivered whole or not at all; sequenced drops
    anything older than the newest delivered;
  - **reliable-ordered**: a byte stream of length-prefixed messages; packets carry stream
    chunks; lost chunks are resent; the receiver window (a power of two, default 64 KiB)
    bounds unacknowledged and unread bytes; a packet whose chunk does not fit is dropped
    whole (not acknowledged) so nothing is acknowledged that was not stored.
- **Delivery notification** (per unreliable channel, opt-in): `Delivered`/`Lost` events
  per message id once every fragment is acknowledged or any is lost (Tribes-style
  replication, ADR 0019). Messages dropped over budget are reported lost.
- **Budget:** optional per-connection token bucket (`send_rate`, 100 ms burst);
  `send_budget(conn)` tells replication how many bytes to fill; unreliable data over
  budget is dropped at flush, reliable data waits. No congestion control beyond that:
  games send at a fixed tick rate under a per-client budget (ADR 0019).
- **Estimates** per connection: RTT, RTT variance (jitter), minimum RTT, loss (EWMA),
  send/receive rates, bytes in flight, unacknowledged reliable bytes, drops.
- **Keepalive** after 100 ms without a packet; timeout from the token (default 10 s);
  disconnects sent three times.

### The interface replication builds on
`Transport` (implemented by `Endpoint<S>` for any socket): `update`, `flush`,
`poll_event` (`Connected`, `Disconnected`, `Delivered`, `Lost`), `send` (returns a
`MessageId`), `receive` (borrowed bytes, valid until the next call), `stats`,
`send_budget`, `disconnect`, `for_each_connection`. A listen server is a server
endpoint in the host's process; the host's own player has no connection (ADR 0019
addendum: no loopback).

### Sockets
- `UdpSocket` (Windows), `memory::MemoryNetwork` (latency, jitter, loss, duplication,
  reordering, full-cone / port-restricted / symmetric NATs; virtual clock; seeded),
  and any `DatagramSocket` the game supplies: relays, and Steam Datagram Relay through
  `ISteamNetworkingFakeUDPPort` (interface only; the engine does not link Steamworks).

### NAT traversal ladder (for hosts)
1. **UPnP IGD**, then **PCP**, then **NAT-PMP**, on the `PortMapper` thread: one UDP
   port (the game socket's), the game's port first, then random ones on conflict (718);
   lease 1 hour, permanent only when the router refuses leases (725); renewed at half
   the lease; removed on stop or drop.
2. **STUN** from the game socket (sans-I/O through the endpoint's foreign-datagram
   pass-through): the reflexive address; two servers detect symmetric NATs.
3. **Hole punching** with a key both sides derive from the connect token
   (`ConnectToken::punch_key`) and candidates exchanged through the game's `Rendezvous`.
4. **Relay** as a `DatagramSocket`: our reference TURN-like relay (tickets from the
   game's backend, return-routability cookies, forwarding only within a session), SDR,
   or TURN.
- `nat::decide` reports the rung that worked (`Public`, `Upnp`, `Pcp`, `NatPmp`,
  `HolePunch`, `Relay`, `LanOnly`) and the addresses for tokens. A mapping whose
  external address is not global (double NAT, 100.64.0.0/10) or disagrees with STUN is
  skipped.
- No default servers of any kind: STUN servers, relays and rendezvous are configured by
  the game (no telemetry).

## Consequences

- Replication (stage 2) builds on `Transport`, prototypes against
  `MemoryNetwork` with loss and latency, and uses `Delivered`/`Lost` to compare
  Tribes-style loss notification with baseline deltas (ADR 0019's open question).
- The wire format is ours: no interoperability with netcode or GNS clients (not needed).
- **Revisit when:**
  - a server needs hundreds of clients: Registered I/O and UDP segmentation offload
    (`sendto` is ~90 % of per-packet cost);
  - games ship to browsers: WebTransport gateway or a QUIC `Transport`;
  - games need fairness on shared links (large downloads in-session): a congestion
    controller (LEDBAT-like) under the budget;
  - forward secrecy is required: an X25519 exchange (CNG has ECDH on Curve25519)
    inside the handshake; today keys come from the token, whose channel must be secure.
- Hole punching needs a rendezvous; the engine provides the trait and a local one, not a
  service. Symmetric NATs on both sides need a relay the game hosts or SDR.

## Questions for the team

1. **Rendezvous and relay hosting:** should the engine ship a small reference
   rendezvous + relay server binary (tools/), or leave both to each game (Steam/Epic
   lobbies and SDR cover most shipped games)? The reference relay exists as a library
   (`nat::relay`).
2. **Default STUN servers:** none are built in (no third-party contact without the
   game's consent). Should the editor's template project suggest public ones (Google,
   Cloudflare) in its config, visibly?
3. **Forward secrecy:** is it a requirement for the first games (it costs an X25519
   handshake round)?
