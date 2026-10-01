//! Transport measurements for ADR 0070 (`cargo run --release -p engine_net_transport
//! --example net_bench`):
//! 1. CPU per packet: building + sealing + sending, and receiving + opening + parsing,
//!    on an in-process socket (no kernel), for snapshot-sized and small packets;
//! 2. the same through loopback UDP (kernel included);
//! 3. throughput on loopback UDP: reliable bulk transfer and unreliable packet rate;
//! 4. latency the transport adds on localhost: ping-pong through endpoints against raw
//!    UDP, with busy polling and with blocking waits (the I/O-thread question).

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use engine_net_transport::*;

const PROTOCOL: u64 = 0x0f1e_be7c;

/// Two sockets joined by in-process queues of recycled buffers: the transport's own CPU
/// cost without the kernel.
#[derive(Default)]
struct Pipe {
    queues: [VecDeque<(Vec<u8>, usize)>; 2],
    pool: Vec<Vec<u8>>,
}

struct PipeSocket {
    pipe: Rc<RefCell<Pipe>>,
    side: usize,
    addr: SocketAddr,
    peer: SocketAddr,
}

impl DatagramSocket for PipeSocket {
    fn send_to(&mut self, _now: Instant, _to: SocketAddr, data: &[u8]) -> std::io::Result<()> {
        let mut p = self.pipe.borrow_mut();
        let mut buf = p.pool.pop().unwrap_or_else(|| vec![0u8; 2048]);
        buf[..data.len()].copy_from_slice(data);
        p.queues[1 - self.side].push_back((buf, data.len()));
        Ok(())
    }

    fn recv_from(&mut self, _now: Instant, out: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        let mut p = self.pipe.borrow_mut();
        let Some((buf, n)) = p.queues[self.side].pop_front() else { return Ok(None) };
        out[..n].copy_from_slice(&buf[..n]);
        p.pool.push(buf);
        Ok(Some((n, self.peer)))
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

fn config() -> EndpointConfig {
    let mut c = EndpointConfig::new(PROTOCOL);
    c.channels = vec![
        ChannelConfig { buffer_bytes: 1 << 20, max_message_size: 512 * 1024, ..ChannelConfig::reliable() },
        ChannelConfig::unreliable(),
        ChannelConfig::unreliable_sequenced(),
    ];
    c.max_packets_per_flush = 1024;
    c.keepalive_interval = Duration::from_secs(1000);
    c
}

fn token(key: &Key, server: SocketAddr) -> ConnectToken {
    ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id: 1,
            expire_seconds: 60,
            timeout_seconds: 60,
            server_addresses: &[server],
            user_data: [0; USER_DATA_BYTES],
        },
        key,
        unix_now(),
    )
    .unwrap()
}

fn connect<S: DatagramSocket>(server: &mut Endpoint<S>, client: &mut Endpoint<S>) -> (ConnectionId, ConnectionId) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut c = None;
    while c.is_none() {
        assert!(Instant::now() < deadline, "no connection");
        let now = Instant::now();
        client.update(now);
        server.update(now);
        while let Some(e) = client.poll_event() {
            if let Event::Connected(id) = e {
                c = Some(id);
            }
        }
        while server.poll_event().is_some() {}
        client.flush(now);
        server.flush(now);
    }
    let s = server.connections().next().unwrap();
    (c.unwrap(), s)
}

fn pipe_pair() -> (Endpoint<PipeSocket>, Endpoint<PipeSocket>) {
    let pipe = Rc::new(RefCell::new(Pipe::default()));
    let (sa, ca): (SocketAddr, SocketAddr) = ("10.0.0.1:1".parse().unwrap(), "10.0.0.2:1".parse().unwrap());
    let key = Key::random();
    let now = Instant::now();
    let server = Endpoint::server(
        PipeSocket { pipe: pipe.clone(), side: 0, addr: sa, peer: ca },
        config(),
        ServerConfig { private_key: key.clone(), max_clients: 4, public_addresses: vec![sa] },
        now,
    )
    .unwrap();
    let client = Endpoint::client(PipeSocket { pipe, side: 1, addr: ca, peer: sa }, config(), token(&key, sa), now).unwrap();
    (server, client)
}

fn cpu_per_packet<S: DatagramSocket>(label: &str, server: &mut Endpoint<S>, client: &mut Endpoint<S>, message: usize) {
    let (c, s) = connect(server, client);
    let payload = vec![0x5au8; message];
    let rounds = 20_000;
    let mut send = Duration::ZERO;
    let mut recv = Duration::ZERO;
    let mut received = 0usize;
    let mut packets_before = server.stats(s).unwrap().packets_sent;
    for _ in 0..1000 {
        server.send(s, ChannelId(1), &payload).unwrap();
        server.flush(Instant::now());
        client.update(Instant::now());
        while client.receive(c, ChannelId(1)).is_some() {}
        client.flush(Instant::now());
        server.update(Instant::now());
    }
    packets_before = server.stats(s).unwrap().packets_sent - packets_before + packets_before;
    let _ = packets_before;
    let sent_before = server.stats(s).unwrap().packets_sent;
    let recv_before = client.stats(c).unwrap().packets_received;
    for i in 0..rounds {
        server.send(s, ChannelId(1), &payload).unwrap();
        let t = Instant::now();
        server.flush(t);
        let t2 = Instant::now();
        send += t2 - t;
        client.update(t2);
        let t3 = Instant::now();
        recv += t3 - t2;
        while client.receive(c, ChannelId(1)).is_some() {
            received += 1;
        }
        // Acks back now and then, as a client at 64 Hz against a server's snapshots would.
        if i % 4 == 0 {
            client.flush(t3);
            server.update(t3);
        }
    }
    let sent = server.stats(s).unwrap().packets_sent - sent_before;
    let got = client.stats(c).unwrap().packets_received - recv_before;
    println!(
        "{label:<34} {message:>5} B message: send {:>6.2} µs/packet, receive {:>6.2} µs/packet ({sent} sent, {got} received, {received} messages)",
        send.as_secs_f64() * 1e6 / sent as f64,
        recv.as_secs_f64() * 1e6 / got.max(1) as f64
    );
}

fn udp_pair() -> (Endpoint<UdpSocket>, Endpoint<UdpSocket>) {
    let server_socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let sa = server_socket.local_addr().unwrap();
    let key = Key::random();
    let now = Instant::now();
    let server = Endpoint::server(
        server_socket,
        config(),
        ServerConfig { private_key: key.clone(), max_clients: 4, public_addresses: vec![sa] },
        now,
    )
    .unwrap();
    let client = Endpoint::client(UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(), config(), token(&key, sa), now).unwrap();
    (server, client)
}

fn throughput() {
    let (mut server, mut client) = udp_pair();
    let (c, s) = connect(&mut server, &mut client);
    // Reliable: 64 MiB in 256 KiB messages, as fast as acks allow.
    let message = vec![0xa5u8; 256 * 1024];
    let total = 256;
    let (mut queued, mut got) = (0, 0);
    let start = Instant::now();
    while got < total && start.elapsed() < Duration::from_secs(60) {
        let now = Instant::now();
        while queued < total && server.send(s, ChannelId(0), &message).is_ok() {
            queued += 1;
        }
        server.update(now);
        server.flush(now);
        client.update(now);
        while client.receive(c, ChannelId(0)).is_some() {
            got += 1;
        }
        client.flush(now);
    }
    let secs = start.elapsed().as_secs_f64();
    let stats = server.stats(s).unwrap();
    println!(
        "loopback UDP reliable bulk: {:.0} MB/s ({got} x 256 KiB in {secs:.2} s; {} packets lost, rtt {:?})",
        (got * message.len()) as f64 / secs / 1e6,
        stats.packets_lost,
        stats.rtt
    );
    // Unreliable packet rate: 1100-byte messages, one per packet.
    let payload = vec![1u8; 1100];
    let start = Instant::now();
    let mut received = 0u64;
    let mut sent = 0u64;
    while start.elapsed() < Duration::from_secs(2) {
        let now = Instant::now();
        for _ in 0..32 {
            server.send(s, ChannelId(1), &payload).unwrap();
            sent += 1;
        }
        server.flush(now);
        client.update(now);
        while client.receive(c, ChannelId(1)).is_some() {
            received += 1;
        }
        client.flush(now);
        server.update(now);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "loopback UDP unreliable 1100 B: {:.0} kpackets/s sent, {:.0} received ({:.0} MB/s), {:.2} % dropped by the kernel",
        sent as f64 / secs / 1e3,
        received as f64 / secs / 1e3,
        received as f64 * 1100.0 / secs / 1e6,
        100.0 * (sent - received) as f64 / sent as f64
    );
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

/// Round trips of a 64-byte message, echoed by a server on another thread.
fn latency(blocking: bool) {
    let rounds = 20_000;
    // Raw UDP baseline.
    let a = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let b = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let ba = b.local_addr().unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let echo = std::thread::spawn(move || {
        let mut b = b;
        let mut buf = [0u8; 2048];
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            if blocking {
                let _ = b.wait_readable(Duration::from_millis(5));
            }
            if let Some((n, from)) = b.recv_from(Instant::now(), &mut buf).unwrap() {
                b.send_to(Instant::now(), from, &buf[..n]).unwrap();
            }
        }
    });
    let mut a = a;
    let mut raw = Vec::with_capacity(rounds);
    let mut buf = [0u8; 2048];
    while raw.len() < rounds {
        let t = Instant::now();
        a.send_to(t, ba, &[7u8; 64]).unwrap();
        // A datagram lost by the kernel: send again after 50 ms, no sample.
        while t.elapsed() < Duration::from_millis(50) {
            if blocking {
                let _ = a.wait_readable(Duration::from_millis(5));
            }
            if a.recv_from(Instant::now(), &mut buf).unwrap().is_some() {
                raw.push(t.elapsed().as_secs_f64() * 1e6);
                break;
            }
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    echo.join().unwrap();

    // Through endpoints: the server thread echoes messages on the unreliable channel.
    let server_socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let sa = server_socket.local_addr().unwrap();
    let key = Key::random();
    let server_key = key.clone();
    let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready2 = ready.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop3 = stop.clone();
    let echo = std::thread::spawn(move || {
        let mut server = Endpoint::server(
            server_socket,
            config(),
            ServerConfig { private_key: server_key, max_clients: 4, public_addresses: vec![sa] },
            Instant::now(),
        )
        .unwrap();
        let mut msg = [0u8; 64];
        while !stop3.load(std::sync::atomic::Ordering::Relaxed) {
            if blocking {
                let _ = server.socket().wait_readable(Duration::from_millis(5));
            }
            let now = Instant::now();
            server.update(now);
            while server.poll_event().is_some() {
                ready2.store(true, std::sync::atomic::Ordering::Release);
            }
            if let Some(s) = server.connections().next() {
                while let Some(m) = server.receive(s, ChannelId(1)) {
                    msg.copy_from_slice(m);
                    server.send(s, ChannelId(1), &msg).unwrap();
                }
            }
            server.flush(now);
        }
    });
    let mut client = Endpoint::client(UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(), config(), token(&key, sa), Instant::now()).unwrap();
    let mut c = None;
    while c.is_none() || !ready.load(std::sync::atomic::Ordering::Acquire) {
        let now = Instant::now();
        client.update(now);
        while let Some(e) = client.poll_event() {
            if let Event::Connected(id) = e {
                c = Some(id);
            }
        }
        client.flush(now);
        std::thread::sleep(Duration::from_millis(1));
    }
    let c = c.unwrap();
    let mut endpoint = Vec::with_capacity(rounds);
    while endpoint.len() < rounds {
        let t = Instant::now();
        client.send(c, ChannelId(1), &[7u8; 64]).unwrap();
        client.flush(t);
        while t.elapsed() < Duration::from_millis(50) {
            if blocking {
                let _ = client.socket().wait_readable(Duration::from_millis(5));
            }
            client.update(Instant::now());
            if client.receive(c, ChannelId(1)).is_some() {
                endpoint.push(t.elapsed().as_secs_f64() * 1e6);
                break;
            }
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    echo.join().unwrap();
    let mode = if blocking { "blocking (WSAPoll)" } else { "busy polling" };
    let (r50, r99) = (percentile(&mut raw, 0.5), percentile(&mut raw, 0.99));
    let (e50, e99) = (percentile(&mut endpoint, 0.5), percentile(&mut endpoint, 0.99));
    println!(
        "localhost round trip, {mode:<18}: raw UDP median {r50:.1} µs (p99 {r99:.1}), endpoints {e50:.1} µs (p99 {e99:.1}); added per one-way trip {:.1} µs",
        (e50 - r50) / 2.0
    );
}

fn empty_update_cost() {
    let (mut server, _client) = udp_pair();
    let n = 200_000;
    let t = Instant::now();
    for _ in 0..n {
        server.update(Instant::now());
    }
    println!("empty update (one recvfrom returning WSAEWOULDBLOCK): {:.2} µs", t.elapsed().as_secs_f64() * 1e6 / n as f64);
}

fn main() {
    for size in [1100, 300, 32] {
        let (mut s, mut c) = pipe_pair();
        cpu_per_packet("in-process socket (transport only)", &mut s, &mut c, size);
    }
    for size in [1100, 32] {
        let (mut s, mut c) = udp_pair();
        cpu_per_packet("loopback UDP (with the kernel)", &mut s, &mut c, size);
    }
    empty_update_cost();
    throughput();
    latency(false);
    latency(true);
}
