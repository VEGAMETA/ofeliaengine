# Network transport and NAT traversal

Checked on 2026-10-01. Decision: [ADR 0070](../adr/0070-network-transport.md). The model
above the transport (authoritative ticks, snapshots, prediction, lag compensation) is
[ADR 0019](../adr/0019-networking-model.md) and [networking.md](networking.md).

The question: what carries the bytes of ADR 0019 between a server (dedicated, or a
listen server where the host also plays) and its clients, how it is secured, and how a
co-op host behind a home router becomes reachable. Requirements from CLAUDE.md: no
async runtime in the core (§5), no allocation in steady state, minimal latency (§7),
permissive licenses only and few dependencies (§15), no telemetry.

No code from any of these projects was copied. Designs taken from them are named below
and in the ADR; the netcode, reliable.io and GameNetworkingSockets ideas we adapted are
BSD-3-Clause, ENet's MIT, the RFCs are freely implementable.

## Sources

| Source | Version / date | License | How it was studied |
|---|---|---|---|
| netcode (mas-bandwidth, G. Fiedler), `STANDARD.md` | 1.02, read 2026-10-01 | BSD-3-Clause | the specification: connect tokens, packet prefix, encryption, replay buffer, token reuse history |
| reliable.io / yojimbo (G. Fiedler) | main, known design | BSD-3-Clause | ack + 32-bit ack bits per packet, packet-level reliability, fragments; yojimbo's message channels and blocks |
| G. Fiedler, "Building a Game Network Protocol" series, "Reliable Ordered Messages", "Packet Fragmentation and Reassembly" | gafferongames.com, 2016 | articles | known content |
| GameNetworkingSockets (Valve) | v1.6.0, release page read 2026-10-01 | BSD-3-Clause | README and BUILDING: ack vectors (DCCP/QUIC style), lanes, AES-GCM-256 per packet with Curve25519 key exchange, ICE for P2P, SDR; dependencies protobuf + OpenSSL / libsodium / bcrypt ("bcrypt mainly for an Xbox requirement") |
| ENet (L. Salzman) | 1.3.18 (2024-04-14) | MIT | known design: channels (reliable, unreliable sequenced, unsequenced), fragments, no encryption |
| quinn / quinn-proto | 0.11.12 (2026-09-14) | MIT OR Apache-2.0 | `quinn-proto/Cargo.toml`: a sans-I/O state machine (no runtime) but rustls + ring/aws-lc, `rand`, `bytes`, `slab`, `tracing`; quinn itself needs tokio or async-std |
| RFC 9000 / 9001 / 9002 (QUIC transport, TLS, loss detection) | 2021 | IETF | loss detection thresholds (3 packets, 9/8 RTT), PTO, ACK delay field, RTT estimator |
| RFC 9221 (QUIC datagrams), RFC 9297, WebTransport drafts | 2022–2026 | IETF / W3C | unreliable datagrams inside QUIC; WebTransport = HTTP/3 + QUIC streams/datagrams for browsers |
| RFC 6298 (RTO computation) | 2011 | IETF | SRTT / RTTVAR |
| Quake III Arena networking (J. Carmack; F. Sanglard's code review) | 2005 release, GPL | GPL-2.0, ideas only | delta snapshots against the last acknowledged one, a single unreliable channel, fragmentation of large snapshots, qport against NAT port changes |
| Source engine networking (Valve developer wiki) | known content | proprietary docs | `net_maxroutable` fragmentation, split packets, rate (`rate`, `cl_updaterate`) budget per client |
| T. Ford, "Overwatch Gameplay Architecture and Netcode", GDC 2017 | known content | talk | unreliable input with redundancy, adaptive command frame rate, packet loss handled above the transport |
| RFC 6886 (NAT-PMP), RFC 6887 (PCP), RFC 7488 (PCP server selection) | 2013 | IETF | message layouts, retry schedule (250 ms doubling), version fallback, epoch, nonce, IPv6 pinholes |
| UPnP Device Architecture 1.1 / 2.0; IGD:1 and IGD:2 WANIPConnection specs | UPnP Forum | specs | SSDP M-SEARCH, device description, SOAP AddPortMapping / DeletePortMapping / GetSpecificPortMappingEntry, error codes 718 and 725 |
| igd-next (rust-igd fork) | 0.17.1 (2026-06-01) | MIT | `Cargo.toml`: attohttpc, xmltree, url, rand, log (+ tokio, hyper, futures for async) |
| natpmp (crate) | 0.5.0 (2024-11-06) | MIT | NAT-PMP only, no PCP |
| RFC 8489 (STUN), RFC 5769 (STUN test vectors), RFC 5389 | 2020 / 2010 | IETF | binding request/response, XOR-MAPPED-ADDRESS, magic cookie, retransmission; the IPv4 sample response is our test vector |
| RFC 4787 (NAT behavioral requirements for UDP), RFC 5128 (P2P across NATs) | 2007 / 2008 | IETF | mapping (endpoint-independent vs dependent) and filtering behaviors, hole punching |
| B. Ford, P. Srisuresh, D. Kegel, "Peer-to-Peer Communication Across Network Address Translators" | USENIX ATC 2005 | paper | simultaneous UDP hole punching through a rendezvous; success rates by NAT type |
| RFC 8656 (TURN), RFC 8445 (ICE) | 2020 / 2018 | IETF | relaying, candidate gathering and connectivity checks (GNS uses WebRTC's ICE) |
| Steamworks networking: `ISteamNetworkingSockets`, `ISteamNetworkingMessages`, `ISteamNetworkingFakeUDPPort` | partner docs, known content | proprietary SDK | Steam Datagram Relay; FakeIP gives existing UDP code relayed fake addresses |
| NIST SP 800-38D (GCM) | 2007 | public | nonce uniqueness; random 96-bit nonces at most 2^32 per key |
| Windows CNG (`BCryptEncrypt` with `BCRYPT_AES_GCM_ALG_HANDLE`, `BCRYPT_CHACHA20_POLY1305_ALG_HANDLE`) | Windows 11, bcrypt.dll | system | in-place AEAD; measured in `bench/net-crypto` |
| RustCrypto `chacha20poly1305` | 0.11.0 | MIT OR Apache-2.0 | measured in `bench/net-crypto` (NCC Group audited the 0.x line in 2020) |
| `ring` | 0.17.14 | Apache-2.0 AND ISC | measured in `bench/net-crypto` |

## How the candidates work

### netcode (Fiedler)
- **Connect tokens.** A web backend that authenticated the player generates a 2048-byte
  token: a public part (server addresses, two random 256-bit packet keys, timeout) and a
  1024-byte private part sealed with a key shared by the backend and the dedicated
  servers (XChaCha20-Poly1305, random 192-bit nonce), carrying the client id, the same
  keys and 256 bytes of user data.
- **Handshake.** The client sends a 1078-byte plaintext request with the sealed part; the
  server opens it, checks expiry and that its own address is in the token, then answers
  with a smaller encrypted challenge (a token sealed with a server-random key); the
  client echoes it; the server assigns a slot and sends keep-alives. A server answers
  nothing without a valid token, and its answers are smaller than the request: no
  amplification.
- **Packets.** Prefix byte = type (low nibble) + count of sequence bytes (high nibble);
  variable-length sequence; ChaCha20-Poly1305 with nonce = sequence; AAD = version,
  protocol id, prefix. A 256-entry replay buffer drops old or repeated sequences.
- **Token reuse.** The server remembers token MACs with the address they came from; the
  same token from a different address is ignored.
- **What it leaves out:** reliability, channels, fragmentation (that is reliable.io and
  yojimbo), NAT traversal.

### reliable.io and yojimbo
- Every packet carries `sequence`, `ack` (newest received) and 32 `ack_bits`, so the
  sender learns per packet whether it arrived, with redundancy against lost acks.
- Packets larger than the MTU are fragmented at packet level; reliable messages are
  resent until a packet carrying them is acknowledged; yojimbo adds message channels
  (reliable-ordered, unreliable-unordered) and "blocks" sent one fragment window at a
  time.
- **Weakness found here:** the 32-bit mask only covers 33 packets. When more than that
  arrive between two of the receiver's sends, acknowledgements are lost for good; every
  such packet is declared lost and resent. Our first version did this: a 64 MiB loopback
  transfer resent 1.15 million packets and ran at 10 MB/s (`net_bench`, ADR 0070).

### GameNetworkingSockets (Valve)
- Connection-oriented, message-oriented: reliable and unreliable messages over UDP,
  fragmentation and retransmission with an ack-vector model taken from DCCP and early
  QUIC, lanes with priorities and weights, AES-GCM-256 per packet after a Curve25519
  key exchange with certificates.
- P2P through WebRTC's ICE (STUN/TURN), and Steam Datagram Relay for Steam partners.
- **Cost for us:** C++ with protobuf (Abseil) and OpenSSL or libsodium (bcrypt only "to
  satisfy an Xbox requirement"), a service thread, its own allocator use. Excellent
  design reference; too much to vendor for a transport whose core is a few thousand lines.

### ENet
- Reliable, unreliable-sequenced and unsequenced packets on numbered channels,
  fragmentation, bandwidth throttling, no encryption, no connect tokens. MIT, small C.
  Its channel model is what most engines expose (Godot's ENet backend). No security
  layer means we would build netcode's anyway.

### QUIC (quinn) and WebTransport
- **Handshake:** TLS 1.3 inside QUIC, 1 RTT (0-RTT on resumption). Needs certificates or
  raw public keys; a listen server would need a self-signed certificate pinned through
  the invite. netcode's connect token gives the same authentication with no certificate
  machinery, at the same 1 RTT (challenge/response).
- **Head-of-line blocking:** QUIC streams block only within a stream; unreliable data
  goes as RFC 9221 datagrams. Equivalent to our channels.
- **CPU and size:** quinn-proto is runtime-free, but brings rustls + ring or aws-lc-rs,
  `rand`, `bytes`, `slab`, `tracing`; quinn adds tokio. Its congestion control (NewReno,
  Cubic, BBR) is built for fair bulk transfer; games send at a fixed tick rate under a
  per-client budget (ADR 0019) and prefer dropping stale snapshots to queuing them.
  Datagram frames still pay QUIC's per-packet overhead (short header + frame types +
  AEAD) similar to ours.
- **WebTransport** is QUIC over HTTP/3 for browsers. It matters only if the engine ever
  ships browser clients; CLAUDE.md targets Windows 11 only. A browser target would put a
  WebTransport gateway in front of the same server.
- **Verdict:** not for the core. If a future case needs QUIC (browser clients, a
  stream-heavy service), it can be another `Transport` implementation.

### Quake III, Source, Overwatch packet design
- **Quake III:** one unreliable channel; snapshots are deltas against the last snapshot
  the client acknowledged (the acknowledgement rides in the client's command packets);
  reliable commands are retransmitted inside every packet until acknowledged; large
  snapshots are fragmented; the `qport` lets a server recognize a client whose NAT port
  changed.
- **Source:** the same shape plus split packets, a per-client byte rate (`rate`) and
  update rate; reliable data in subchannels.
- **Overwatch:** unreliable inputs sent with redundancy every command frame;
  snapshots unreliable; the simulation adapts to loss (time dilation) instead of the
  transport resending.
- **What this means for the transport:** the bulk of traffic is unreliable and
  latency-critical; reliable data is small (events, chat) except at join; the
  replication layer wants per-packet delivery notification (Tribes, ADR 0019) and a byte
  budget per client.

### Packet loss and jitter simulation
- Every serious library ships one (netcode/yojimbo network simulator, GNS's fake loss
  and lag settings, clumsy on Windows at the driver level). The useful knobs: one-way
  latency, jitter (independent per packet, which also reorders), loss, duplication,
  explicit reordering, and per-link settings; deterministic for a seed so failures
  reproduce. We add NAT behaviors (full cone, port-restricted, symmetric) so hole
  punching is testable without routers.

## NAT traversal

### Rungs

1. **UPnP IGD.** SSDP `M-SEARCH` to 239.255.255.250:1900 for
   `InternetGatewayDevice:1`/`:2`; the response's `LOCATION` is the device description
   (XML), which lists `WANIPConnection`/`WANPPPConnection` services and their control
   URLs; SOAP actions `GetExternalIPAddress`, `AddPortMapping` (remote host empty,
   external port, `UDP`, internal port, internal client, enabled, description, lease),
   `DeletePortMapping`, `GetSpecificPortMappingEntry`. Error 718: the external port is
   taken (choose another); error 725: only permanent (lease 0) mappings supported. IGD:1
   routers often reject finite leases; IGD:2 caps them at a week. Renew before the lease
   ends; delete on exit. Widely enabled on home routers, disabled on many ISP boxes for
   security.
2. **PCP (RFC 6887) and NAT-PMP (RFC 6886).** UDP to the default gateway, port 5351.
   NAT-PMP: 2-byte external-address request, 12-byte map request (internal port,
   suggested external port, lifetime), 16-byte response; delete with lifetime 0. PCP: a
   60-byte MAP request with the client's address (IPv4-mapped), a 96-bit nonce that ties
   renewals and deletions to the requester, protocol, ports, suggested external address;
   also opens **IPv6 firewall pinholes**. A NAT-PMP-only server answers a PCP request
   with version 0, result 1 "unsupported version": fall back. Retries start at 250 ms
   and double (RFC: up to 9 times; a game wants 2–3).
3. **STUN (RFC 8489) and hole punching.** A 20-byte binding request; the response's
   XOR-MAPPED-ADDRESS is the address the server saw. Sent **from the game socket**, it is
   that socket's NAT mapping. Two servers seeing different ports means
   endpoint-dependent ("symmetric") mapping (RFC 4787): punching fails, use a relay.
   With endpoint-independent mapping, both peers send to each other's candidates
   (reflexive, LAN) at the same time; each NAT sees outgoing traffic first and accepts
   the reply (Ford et al. measured ~82 % success for UDP in 2005; symmetric NATs are the
   failures). Candidates travel through a rendezvous the game already has (lobby,
   platform invite, matchmaker).
4. **Relay.** TURN (RFC 8656) allocates a relayed address per client; ICE (RFC 8445)
   orders candidates and runs connectivity checks. Steam Datagram Relay does the same on
   Valve's backbone for Steamworks games; `ISteamNetworkingFakeUDPPort` gives existing
   UDP code fake IPv4 addresses that route over SDR. Relays need return-routability
   checks (TURN's nonce challenge) or they become reflectors.

### Double NAT and carrier-grade NAT
- A router can report an external address that is itself private (another router in
  front) or in 100.64.0.0/10 (RFC 6598 shared space, carrier-grade NAT). A mapping on
  such a router reaches nobody on the internet. The external address must be global and
  agree with what STUN saw; otherwise skip to hole punching.
- **Measured on this machine's LAN (2026-10-01):** the router answered SSDP in 26 ms,
  `AddPortMapping`/`DeletePortMapping` in ~2 ms each, and reported its external address
  as 10.62.54.160: the ISP puts it behind carrier-grade NAT, so a UPnP mapping alone would
  not have made a host reachable. PCP and NAT-PMP got no answer (1.76 s with 3 tries).
  The router's table afterwards held no mapping of ours (other programs' entries: 17,
  ZeroTier and Tailscale).

### Crates against our own code
| | igd-next 0.17.1 | natpmp 0.5.0 | own code |
|---|---|---|---|
| Scope | UPnP IGD | NAT-PMP only | UPnP IGD + PCP + NAT-PMP + STUN + punching |
| Dependencies | attohttpc, xmltree, url, rand, log (+ tokio, hyper, futures for async) | (tokio / async-std optional) | none (std + windows-sys already in the tree) |
| Size | thousands of lines incl. HTTP and XML stacks | small | ~1,500 lines with tests |
| Fit | blocking or async; general XML | no PCP | sans-I/O STUN/punching on the game socket; blocking mapper on its own thread |

The protocols are small and fixed; the HTTP is three requests with
`Connection: close`, the XML a handful of known tags. Writing them took less time than
auditing the dependency trees (CLAUDE.md §3: no dependency for something written in an
hour or two), and the mock-router tests cover them.

## Encryption measurement (`bench/net-crypto`, ref-1, release, 2026-10-01)

ns per packet, seal / open, 17-byte AAD:

| Size | CNG AES-128-GCM | CNG AES-256-GCM | CNG ChaCha20-Poly1305 | RustCrypto ChaCha20-Poly1305 | ring AES-256-GCM | ring ChaCha20-Poly1305 |
|---|---|---|---|---|---|---|
| 32 B | 108 / 106 | 113 / 108 | 239 / 241 | 1,103 / 1,120 | 59 / 75 | 113 / 122 |
| 128 B | 114 / 118 | 120 / 124 | 353 / 360 | 1,173 / 1,147 | 61 / 69 | 132 / 142 |
| 512 B | 133 / 143 | 140 / 172 | 970 / 983 | 1,285 / 1,281 | 94 / 102 | 244 / 256 |
| 1200 B | 198 / 219 | 217 / 255 | 2,104 / 2,110 | 1,767 / 1,769 | 154 / 162 | 426 / 469 |

- AES-GCM with AES-NI beats ChaCha20 on this CPU in every library.
- CNG costs ~50–90 ns more than ring per packet (the call into bcrypt.dll and its
  parameter checks): at 4,096 packets/s (64 clients at 64 Hz) that is 0.3 ms per second.
- RustCrypto's ChaCha20-Poly1305 0.11 has a ~1 µs fixed cost per message on this machine.
- CNG adds no dependency (bcrypt.dll; `windows-sys` is already in the tree), is FIPS
  140-validated and maintained with the OS; ring adds C and assembly built by `cc`.

## Socket and thread model (`net_bench`, ref-1, release)

- `sendto` on Windows loopback costs ~4.4 µs, `recvfrom` ~1 µs; an empty poll
  (`WSAEWOULDBLOCK`) 0.35 µs. The kernel dominates: our whole transport is 0.2–0.45 µs
  per packet sent and 0.26–0.56 µs received.
- Localhost round trip of a 64-byte message: raw UDP busy-polling 8.2–10.1 µs, through
  endpoints 10.3–10.4 µs; with blocking waits (`WSAPoll`) 18.5 µs raw: each wake costs
  ~5 µs. A dedicated I/O thread adds such a wake plus a hand-off per packet, and the data
  is consumed at the next tick anyway. Polling in the game's network step is cheaper and
  adds no latency the simulation would see; a dedicated server idles in `WSAPoll` between
  ticks.
- Windows Registered I/O and UDP segmentation offload (`WSASetUdpSendMessageSize`) cut
  the kernel's per-datagram cost for servers with hundreds of clients; not needed at
  64 clients (≈2 % of a core).

## What we take, what we don't

**Take:**
- netcode's connect tokens, handshake and amplification rules, with our sizes; its
  replay buffer and token-reuse history.
- AES-256-GCM through CNG (GNS's cipher, our measurement), nonce = sequence.
- Random starting sequences (below 2^62 for connections; server handshake packets in the
  upper half, as netcode's global sequence starts at 2^63) so a token reused after a crash
  never repeats a nonce under its keys: GCM must never see a nonce twice.
- reliable.io's per-packet acknowledgement, widened to a 64–256-bit mask sized to what
  arrived since the last acknowledgement (the fix for the measured burst problem), and
  QUIC's loss rules, RTT estimator and acknowledgement delay.
- Reliable-ordered channels as a byte stream with chunk acknowledgement (QUIC streams,
  GNS reliable segments): one mechanism for small and large messages.
- Unreliable fragmentation all-or-nothing (Quake III, Source split packets); sequenced
  channels drop old data (ENet).
- Delivery notification of unreliable messages (Tribes; GNS's ack vectors make it
  possible) for the replication layer.
- STUN, hole punching, UPnP, PCP, NAT-PMP from the RFCs and UPnP specs; TURN's
  return-routability idea in our reference relay; SDR through the socket interface.

**Don't take:**
- Certificates and TLS (QUIC): tokens authenticate without a PKI.
- Congestion control for bulk transfer: games send at a fixed tick rate under a
  per-client budget; we cap packets in flight and offer a token-bucket budget.
- ICE in full: its candidate pairing for many interfaces is more than a game needs; the
  ladder tries a fixed order and reports what worked.
- Any default third-party server (STUN, relay, rendezvous): the game configures them.

## Patents

AES-GCM, ChaCha20-Poly1305, STUN, TURN, ICE, UPnP IGD, NAT-PMP and PCP are royalty-free
standards; no patent concern was found for UDP hole punching (published 2005 and in
RFC 5128). Delivery-notification and ack-vector schemes are decades-old techniques (TCP
SACK, DCCP). Nothing here needs the team's attention, but see the ADR's questions.
