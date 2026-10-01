//! End-to-end tests of the endpoint on the in-memory network and on loopback UDP.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_net_transport::memory::{LinkConditions, MemoryNetwork, MemorySocket};
use engine_net_transport::*;

const PROTOCOL: u64 = 0x0f1e_7e57;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn token(key: &Key, client_id: u64, servers: &[SocketAddr], expire: u64, now: u64) -> ConnectToken {
    ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id,
            expire_seconds: expire,
            timeout_seconds: 5,
            server_addresses: servers,
            user_data: [client_id as u8; USER_DATA_BYTES],
        },
        key,
        now,
    )
    .unwrap()
}

struct Sim {
    net: MemoryNetwork,
    key: Key,
    now: Instant,
    server: Endpoint<MemorySocket>,
    server_events: Vec<Event>,
    clients: Vec<(Endpoint<MemorySocket>, Vec<Event>)>,
}

const SERVER: &str = "10.0.0.1:27015";

impl Sim {
    fn new(seed: u64, config: EndpointConfig, max_clients: usize) -> Self {
        let net = MemoryNetwork::new(seed);
        let key = Key::random();
        let t0 = Instant::now();
        let server = Endpoint::server(
            net.bind(addr(SERVER)),
            config,
            ServerConfig { private_key: key.clone(), max_clients, public_addresses: vec![addr(SERVER)] },
            t0,
        )
        .unwrap();
        Self { net, key, now: t0, server, server_events: Vec::new(), clients: Vec::new() }
    }

    fn add_client(&mut self, config: EndpointConfig) -> usize {
        let i = self.clients.len();
        let t = token(&self.key, 100 + i as u64, &[addr(SERVER)], 30, unix_now());
        let socket = self.net.bind(addr(&format!("10.0.1.{}:5000", i + 1)));
        self.clients.push((Endpoint::client(socket, config, t, self.now).unwrap(), Vec::new()));
        i
    }

    /// One network step of `dt` on every endpoint.
    fn step(&mut self, dt: Duration) {
        self.now += dt;
        let now = self.now;
        for (c, events) in &mut self.clients {
            c.update(now);
            while let Some(e) = c.poll_event() {
                events.push(e);
            }
        }
        self.server.update(now);
        while let Some(e) = self.server.poll_event() {
            self.server_events.push(e);
        }
        for (c, _) in &mut self.clients {
            c.flush(now);
        }
        self.server.flush(now);
    }

    fn run(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step(Duration::from_millis(10));
        }
    }

    fn client_conn(&self, i: usize) -> Option<ConnectionId> {
        self.clients[i].1.iter().find_map(|e| match e {
            Event::Connected(id) => Some(*id),
            _ => None,
        })
    }

    fn server_conn(&self, client_id: u64) -> Option<ConnectionId> {
        self.server.connections().find(|&c| self.server.client_id(c) == Some(client_id))
    }
}

fn payload(i: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.max(4)];
    v[..4].copy_from_slice(&i.to_le_bytes());
    for (k, b) in v.iter_mut().enumerate().skip(4) {
        *b = (i as usize * 31 + k) as u8;
    }
    v
}

#[test]
fn connects_and_exchanges_messages_both_ways() {
    let mut sim = Sim::new(1, EndpointConfig::new(PROTOCOL), 4);
    sim.add_client(EndpointConfig::new(PROTOCOL));
    sim.run(10);
    let c = sim.client_conn(0).expect("client connected");
    let s = sim.server_conn(100).expect("server sees the client");
    assert_eq!(sim.server.user_data(s).unwrap()[0], 100);
    assert!(matches!(sim.server_events[0], Event::Connected(_)));
    sim.clients[0].0.send(c, ChannelId(0), b"to server").unwrap();
    sim.server.send(s, ChannelId(1), b"to client").unwrap();
    sim.run(3);
    assert_eq!(sim.server.receive(s, ChannelId(0)), Some(&b"to server"[..]));
    assert_eq!(sim.clients[0].0.receive(c, ChannelId(1)), Some(&b"to client"[..]));
    assert_eq!(sim.server.receive(s, ChannelId(0)), None);
}

/// Reliable messages (small and fragmented up to 30 KB) arrive in order exactly once under
/// 0-30 % loss with jitter, duplication and reordering, in both directions.
#[test]
fn reliable_delivery_under_loss_and_reordering() {
    for (seed, loss) in [(1u64, 0.0f32), (2, 0.05), (3, 0.15), (4, 0.30)] {
        let mut sim = Sim::new(seed, EndpointConfig::new(PROTOCOL), 2);
        sim.add_client(EndpointConfig::new(PROTOCOL));
        sim.run(10);
        sim.net.set_default_conditions(LinkConditions {
            latency: Duration::from_millis(30),
            jitter: Duration::from_millis(20),
            loss,
            duplicate: 0.05,
            reorder: 0.1,
        });
        let c = sim.client_conn(0).unwrap();
        let s = sim.server_conn(100).unwrap();
        let count = 300u32;
        let size = |i: u32| match i % 10 {
            0 => 30_000,
            1..=3 => 2_000,
            _ => (i as usize * 37) % 500,
        };
        let (mut next_c, mut next_s, mut got_c, mut got_s) = (0u32, 0u32, 0u32, 0u32);
        for _ in 0..3000 {
            while next_c < count && sim.clients[0].0.send(c, ChannelId(0), &payload(next_c, size(next_c))).is_ok() {
                next_c += 1;
            }
            while next_s < count && sim.server.send(s, ChannelId(0), &payload(next_s, size(next_s))).is_ok() {
                next_s += 1;
            }
            sim.step(Duration::from_millis(10));
            while let Some(m) = sim.server.receive(s, ChannelId(0)) {
                assert_eq!(m, &payload(got_s, size(got_s))[..], "loss {loss}");
                got_s += 1;
            }
            while let Some(m) = sim.clients[0].0.receive(c, ChannelId(0)) {
                assert_eq!(m, &payload(got_c, size(got_c))[..], "loss {loss}");
                got_c += 1;
            }
            if got_c == count && got_s == count {
                break;
            }
        }
        assert_eq!((got_s, got_c), (count, count), "loss {loss}");
        let stats = sim.server.stats(s).unwrap();
        assert!(sim.clients[0].1.iter().all(|e| matches!(e, Event::Connected(_))), "loss {loss}");
        if loss >= 0.15 {
            assert!(stats.packet_loss > 0.05, "loss estimate {} at {loss}", stats.packet_loss);
        }
        // RTT about 60 ms + jitter (plus up to a step of polling on each side).
        let rtt = stats.rtt.as_secs_f64();
        assert!((0.05..0.15).contains(&rtt), "rtt {rtt} at loss {loss}");
    }
}

/// Unreliable messages: never corrupted or duplicated; fragmented ones whole or not at
/// all; sequenced ones only newer; delivery notification resolves every message once.
#[test]
fn unreliable_fragmentation_sequencing_and_notification() {
    let mut config = EndpointConfig::new(PROTOCOL);
    config.channels = vec![
        ChannelConfig::reliable(),
        ChannelConfig::unreliable().with_notify(),
        ChannelConfig::unreliable_sequenced(),
    ];
    let mut sim = Sim::new(9, config.clone(), 2);
    sim.add_client(config);
    sim.run(10);
    sim.net.set_default_conditions(LinkConditions {
        latency: Duration::from_millis(20),
        jitter: Duration::from_millis(15),
        loss: 0.1,
        duplicate: 0.1,
        reorder: 0.1,
    });
    let c = sim.client_conn(0).unwrap();
    let s = sim.server_conn(100).unwrap();
    let mut sent_ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut last_seq: Option<u32> = None;
    let mut whole = 0;
    for i in 0..500u32 {
        let len = if i % 4 == 0 { 9_000 } else { 200 };
        sent_ids.push(sim.server.send(s, ChannelId(1), &payload(i, len)).unwrap());
        sim.server.send(s, ChannelId(2), &payload(i, 100)).unwrap();
        sim.step(Duration::from_millis(16));
        while let Some(m) = sim.clients[0].0.receive(c, ChannelId(1)) {
            let i = u32::from_le_bytes(m[..4].try_into().unwrap());
            let len = if i % 4 == 0 { 9_000 } else { 200 };
            assert_eq!(m, &payload(i, len)[..]);
            assert!(seen.insert(i), "duplicate {i}");
            if len == 9_000 {
                whole += 1;
            }
        }
        while let Some(m) = sim.clients[0].0.receive(c, ChannelId(2)) {
            let i = u32::from_le_bytes(m[..4].try_into().unwrap());
            assert!(last_seq.is_none_or(|l| i > l), "sequenced went back");
            last_seq = Some(i);
        }
    }
    sim.net.set_default_conditions(LinkConditions::PERFECT);
    sim.run(100);
    assert!(seen.len() > 350 && seen.len() < 500, "{}", seen.len());
    // 9 fragments at 10 % loss: 0.9^9 = 39 % of 125 arrive whole.
    assert!((30..70).contains(&whole), "fragmented delivered {whole}");
    let mut delivered = std::collections::HashSet::new();
    let mut lost = std::collections::HashSet::new();
    for e in &sim.server_events {
        match e {
            Event::Delivered(_, ChannelId(1), id) => assert!(delivered.insert(*id)),
            Event::Lost(_, ChannelId(1), id) => assert!(lost.insert(*id)),
            _ => {}
        }
    }
    assert!(delivered.is_disjoint(&lost));
    assert_eq!(delivered.len() + lost.len(), 500);
    // Every message the client got was acknowledged, unless its ack was lost.
    assert!(delivered.len() <= seen.len() + 5);
    assert!(delivered.len() + 60 >= seen.len(), "{} {}", delivered.len(), seen.len());
}

#[test]
fn timeouts_and_disconnects() {
    let mut sim = Sim::new(3, EndpointConfig::new(PROTOCOL), 4);
    sim.add_client(EndpointConfig::new(PROTOCOL));
    sim.add_client(EndpointConfig::new(PROTOCOL));
    sim.run(10);
    assert_eq!(sim.server.connection_count(), 2);
    // Client 0 leaves politely.
    let c0 = sim.client_conn(0).unwrap();
    sim.clients[0].0.disconnect(sim.now, c0);
    sim.run(2);
    assert!(sim.server_events.iter().any(|e| matches!(e, Event::Disconnected(_, DisconnectReason::ByPeer))));
    // Client 1's link dies: both sides time out after the token's 5 s.
    sim.net.set_link_conditions(addr("10.0.1.2:5000"), addr(SERVER), LinkConditions::new(Duration::ZERO, Duration::ZERO, 1.0));
    sim.net.set_link_conditions(addr(SERVER), addr("10.0.1.2:5000"), LinkConditions::new(Duration::ZERO, Duration::ZERO, 1.0));
    sim.run(490);
    assert_eq!(sim.server.connection_count(), 1);
    sim.run(20);
    assert_eq!(sim.server.connection_count(), 0);
    assert!(sim.clients[1].1.iter().any(|e| matches!(e, Event::Disconnected(_, DisconnectReason::TimedOut))));
}

#[test]
fn connect_failures() {
    let config = EndpointConfig::new(PROTOCOL);
    // Nobody at the address.
    let net = MemoryNetwork::new(1);
    let key = Key::random();
    let t0 = Instant::now();
    let t = token(&key, 1, &[addr("10.9.9.9:1"), addr("10.9.9.8:1")], 30, unix_now());
    let mut client = Endpoint::client(net.bind(addr("10.0.0.2:1")), config.clone(), t, t0).unwrap();
    let mut failed = None;
    for step in 0..1200 {
        client.update(t0 + Duration::from_millis(10 * step));
        if let Some(Event::Disconnected(_, r)) = client.poll_event() {
            failed = Some((r, step));
            break;
        }
    }
    // Two addresses, 5 s each.
    let (reason, step) = failed.unwrap();
    assert_eq!(reason, DisconnectReason::ConnectTimedOut);
    assert!((1000..1010).contains(&step), "{step}");

    // Server full: denied.
    let mut sim = Sim::new(4, config.clone(), 1);
    sim.add_client(config.clone());
    sim.run(10);
    sim.add_client(config.clone());
    sim.run(10);
    assert!(sim.clients[1].1.contains(&Event::Disconnected(ConnectionId { index: 0, generation: 1 }, DisconnectReason::Denied)));

    // Expired token, wrong key: no answer at all.
    let mut sim = Sim::new(5, config.clone(), 4);
    let expired = token(&sim.key, 7, &[addr(SERVER)], 30, unix_now() - 100);
    let wrong_key = token(&Key::random(), 8, &[addr(SERVER)], 30, unix_now());
    for (i, t) in [expired, wrong_key].into_iter().enumerate() {
        let socket = sim.net.bind(addr(&format!("10.0.2.{}:1", i + 1)));
        sim.clients.push((Endpoint::client(socket, config.clone(), t, sim.now).unwrap(), Vec::new()));
    }
    sim.run(100);
    assert_eq!(sim.server.connection_count(), 0);
    assert!(sim.clients[0].1.contains(&Event::Disconnected(ConnectionId { index: 0, generation: 1 }, DisconnectReason::TokenExpired)));
    assert!(sim.clients[1].1.is_empty());
    assert!(sim.server.endpoint_stats().dropped >= 9, "{:?}", sim.server.endpoint_stats());
    // A valid token for an address that is not one of the server's: refused.
    sim.server.set_public_addresses(&[addr("203.0.113.1:27015")]);
    let i = sim.add_client(config.clone());
    sim.run(100);
    assert_eq!(sim.server.connection_count(), 0);
    assert!(sim.clients[i].1.is_empty());
}

/// A token seen from one address is refused from another; forged packets are dropped.
#[test]
fn token_reuse_replay_and_tampering() {
    use engine_net_transport::DatagramSocket;
    let config = EndpointConfig::new(PROTOCOL);
    let mut sim = Sim::new(6, config.clone(), 4);
    let t = token(&sim.key, 1, &[addr(SERVER)], 30, unix_now());
    let mut bytes = [0u8; CONNECT_TOKEN_BYTES];
    t.write(&mut bytes);
    sim.clients.push((
        Endpoint::client(sim.net.bind(addr("10.0.3.1:1")), config.clone(), t, sim.now).unwrap(),
        Vec::new(),
    ));
    sim.clients.push((
        Endpoint::client(sim.net.bind(addr("10.0.3.2:1")), config.clone(), ConnectToken::read(&bytes).unwrap(), sim.now)
            .unwrap(),
        Vec::new(),
    ));
    sim.run(50);
    assert_eq!(sim.server.connection_count(), 1);
    let connected: Vec<bool> = sim.clients.iter().map(|(_, e)| e.contains(&Event::Connected(ConnectionId { index: 0, generation: 1 }))).collect();
    assert_eq!(connected.iter().filter(|&&c| c).count(), 1);

    let dropped_before = sim.server.endpoint_stats().dropped;
    // Garbage and truncated packets with valid prefixes from the client's address.
    let mut forged = [0u8; 64];
    forged[0] = 0x18; // payload kind, one sequence byte
    let mut raw = sim.net.bind(addr("10.0.3.12:1"));
    raw.send_to(sim.now, addr(SERVER), &forged).unwrap();
    raw.send_to(sim.now, addr(SERVER), &forged[..3]).unwrap();
    sim.run(1);
    assert!(sim.server.endpoint_stats().dropped >= dropped_before + 2);
    assert_eq!(sim.server.connection_count(), 1);
}

/// Captures real packets with a pass-through socket and replays and tampers them.
#[test]
fn replayed_and_tampered_packets_are_dropped() {
    use std::sync::{Arc, Mutex};

    struct Tap {
        inner: MemorySocket,
        captured: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    impl DatagramSocket for Tap {
        fn send_to(&mut self, now: Instant, to: SocketAddr, data: &[u8]) -> std::io::Result<()> {
            self.captured.lock().unwrap().push(data.to_vec());
            self.inner.send_to(now, to, data)
        }
        fn recv_from(&mut self, now: Instant, buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
            self.inner.recv_from(now, buf)
        }
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            self.inner.local_addr()
        }
    }

    let config = EndpointConfig::new(PROTOCOL);
    let net = MemoryNetwork::new(11);
    let key = Key::random();
    let t0 = Instant::now();
    let mut server = Endpoint::server(
        net.bind(addr(SERVER)),
        config.clone(),
        ServerConfig { private_key: key.clone(), max_clients: 2, public_addresses: vec![] },
        t0,
    )
    .unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let client_addr = addr("10.0.4.1:1");
    let tap = Tap { inner: net.bind(client_addr), captured: captured.clone() };
    let mut client = Endpoint::client(tap, config, token(&key, 5, &[addr(SERVER)], 30, unix_now()), t0).unwrap();
    let mut now = t0;
    let step = |client: &mut Endpoint<Tap>, server: &mut Endpoint<MemorySocket>, now: &mut Instant| {
        *now += Duration::from_millis(10);
        client.update(*now);
        server.update(*now);
        client.flush(*now);
        server.flush(*now);
    };
    for _ in 0..10 {
        step(&mut client, &mut server, &mut now);
    }
    let c = std::iter::from_fn(|| client.poll_event()).find_map(|e| match e {
        Event::Connected(id) => Some(id),
        _ => None,
    });
    let c = c.unwrap();
    let s = server.connections().next().unwrap();
    captured.lock().unwrap().clear();
    client.send(c, ChannelId(1), b"exactly once").unwrap();
    step(&mut client, &mut server, &mut now);
    step(&mut client, &mut server, &mut now);
    assert_eq!(server.receive(s, ChannelId(1)), Some(&b"exactly once"[..]));
    assert_eq!(server.receive(s, ChannelId(1)), None);
    let packets = captured.lock().unwrap().clone();
    assert!(!packets.is_empty());
    // An attacker spoofing the client's address replays every captured packet...
    let dropped = server.endpoint_stats().dropped;
    std::mem::forget(client); // no disconnect packets; frees nothing the test needs

    let mut spoofed = net.bind(client_addr);
    for p in &packets {
        spoofed.send_to(now, addr(SERVER), p).unwrap();
        // ...and tampered copies.
        let mut t = p.clone();
        let last = t.len() - 1;
        t[last / 2] ^= 0x40;
        spoofed.send_to(now, addr(SERVER), &t).unwrap();
    }
    server.update(now + Duration::from_millis(1));
    assert_eq!(server.receive(s, ChannelId(1)), None, "replayed message delivered again");
    assert_eq!(server.endpoint_stats().dropped, dropped + 2 * packets.len() as u64);
}

/// Two endpoints on loopback UDP sockets.
#[test]
fn loopback_udp() {
    let config = EndpointConfig::new(PROTOCOL);
    let server_socket = UdpSocket::bind(addr("127.0.0.1:0")).unwrap();
    let server_addr = server_socket.local_addr().unwrap();
    let key = Key::random();
    let t0 = Instant::now();
    let mut server = Endpoint::server(
        server_socket,
        config.clone(),
        ServerConfig { private_key: key.clone(), max_clients: 2, public_addresses: vec![server_addr] },
        t0,
    )
    .unwrap();
    let mut client = Endpoint::client(
        UdpSocket::bind(addr("127.0.0.1:0")).unwrap(),
        config,
        token(&key, 1, &[server_addr], 30, unix_now()),
        t0,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut c = None;
    let big = payload(7, 20_000);
    let mut got = false;
    while Instant::now() < deadline && !got {
        let now = Instant::now();
        client.update(now);
        server.update(now);
        while let Some(e) = client.poll_event() {
            if let Event::Connected(id) = e {
                c = Some(id);
                client.send(id, ChannelId(0), &big).unwrap();
            }
        }
        while server.poll_event().is_some() {}
        let first = server.connections().next();
        if let Some(s) = first {
            if let Some(m) = server.receive(s, ChannelId(0)) {
                assert_eq!(m, &big[..]);
                got = true;
            }
        }
        client.flush(now);
        server.flush(now);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(c.is_some() && got);
}

/// A large reliable transfer on a clean link causes no spurious losses (one
/// acknowledgement covers every packet the receiver got since its last one) and no
/// retransmission.
#[test]
fn bulk_reliable_transfer_without_spurious_losses() {
    let mut config = EndpointConfig::new(PROTOCOL);
    config.channels[0] = ChannelConfig { buffer_bytes: 1 << 20, max_message_size: 256 * 1024, ..ChannelConfig::reliable() };
    config.max_packets_per_flush = 1024;
    let mut sim = Sim::new(12, config.clone(), 2);
    sim.add_client(config);
    sim.run(10);
    sim.net.set_default_conditions(LinkConditions::new(Duration::from_millis(30), Duration::ZERO, 0.0));
    let s = sim.server_conn(100).unwrap();
    let c = sim.client_conn(0).unwrap();
    let message = payload(1, 200_000);
    for _ in 0..4 {
        sim.server.send(s, ChannelId(0), &message).unwrap();
    }
    let mut got = 0;
    for _ in 0..500 {
        sim.step(Duration::from_millis(16));
        while let Some(m) = sim.clients[0].0.receive(c, ChannelId(0)) {
            assert_eq!(m, &message[..]);
            got += 1;
        }
        if got == 4 {
            break;
        }
    }
    assert_eq!(got, 4);
    let stats = sim.server.stats(s).unwrap();
    assert_eq!(stats.packets_lost, 0, "{stats:?}");
    assert!(stats.bytes_sent < 4 * 200_000 * 21 / 20, "{} bytes sent for 800 000", stats.bytes_sent);
}
