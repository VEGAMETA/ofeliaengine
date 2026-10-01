//! CLAUDE.md §5: no allocation per network step in steady state (update, send, receive,
//! flush, events), on loopback UDP and on the in-memory network. One test per file.

#![allow(clippy::unwrap_used)]
// Real sockets and mock routers on other threads: plain sleeps are fine here.
#![allow(clippy::disallowed_methods)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use engine_net_transport::memory::{LinkConditions, MemoryNetwork};
use engine_net_transport::*;

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const PROTOCOL: u64 = 0x0f1e_a110;

fn config() -> EndpointConfig {
    let mut c = EndpointConfig::new(PROTOCOL);
    c.channels = vec![
        ChannelConfig::reliable(),
        ChannelConfig::unreliable().with_notify(),
        ChannelConfig::unreliable_sequenced(),
    ];
    c
}

fn token(key: &Key, server: SocketAddr) -> ConnectToken {
    ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id: 1,
            expire_seconds: 30,
            timeout_seconds: 10,
            server_addresses: &[server],
            user_data: [0; USER_DATA_BYTES],
        },
        key,
        unix_now(),
    )
    .unwrap()
}

/// Runs `steps` network steps with traffic in both directions; returns allocations
/// counted after the first `warmup` steps.
fn run<S: DatagramSocket>(
    server: &mut Endpoint<S>,
    client: &mut Endpoint<S>,
    mut now: impl FnMut(usize) -> Instant,
    warmup: usize,
    steps: usize,
) -> (usize, usize) {
    let snapshot = vec![7u8; 3000];
    let event = [1u8; 40];
    let mut received = 0;
    let mut client_id = None;
    for step in 0..steps {
        if step == warmup {
            ALLOCATIONS.store(0, Ordering::Relaxed);
            COUNTING.with(|c| c.set(true));
        }
        let t = now(step);
        client.update(t);
        server.update(t);
        while let Some(e) = client.poll_event() {
            if let Event::Connected(id) = e {
                client_id = Some(id);
            }
        }
        while server.poll_event().is_some() {}
        let mut server_ids = [None; 1];
        if let Some(id) = server.connections().next() {
            server_ids[0] = Some(id);
        }
        if let Some(s) = server_ids[0] {
            let _ = server.send(s, ChannelId(1), &snapshot);
            let _ = server.send(s, ChannelId(2), &event);
            if step % 8 == 0 {
                let _ = server.send(s, ChannelId(0), &event);
            }
            while server.receive(s, ChannelId(0)).is_some() {
                received += 1;
            }
            while server.receive(s, ChannelId(2)).is_some() {
                received += 1;
            }
        }
        if let Some(c) = client_id {
            let _ = client.send(c, ChannelId(2), &event);
            if step % 4 == 0 {
                let _ = client.send(c, ChannelId(0), &snapshot[..300]);
            }
            for ch in 0..3 {
                while client.receive(c, ChannelId(ch)).is_some() {
                    received += 1;
                }
            }
        }
        client.flush(t);
        server.flush(t);
    }
    COUNTING.with(|c| c.set(false));
    (ALLOCATIONS.load(Ordering::Relaxed), received)
}

#[test]
fn network_steps_do_not_allocate() {
    // In-memory network with loss, jitter and reordering (resends, losses, notifications).
    let net = MemoryNetwork::new(3);
    let key = Key::random();
    let server_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
    let t0 = Instant::now();
    let mut server = Endpoint::server(
        net.bind(server_addr),
        config(),
        ServerConfig {
            private_key: key.clone(),
            max_clients: 4,
            public_addresses: vec![server_addr],
        },
        t0,
    )
    .unwrap();
    let mut client = Endpoint::client(
        net.bind("10.0.0.2:1".parse().unwrap()),
        config(),
        token(&key, server_addr),
        t0,
    )
    .unwrap();
    let mut conditions =
        LinkConditions::new(Duration::from_millis(20), Duration::from_millis(10), 0.1);
    conditions.reorder = 0.05;
    conditions.duplicate = 0.02;
    net.set_default_conditions(conditions);
    let (allocations, received) = run(
        &mut server,
        &mut client,
        |s| t0 + Duration::from_millis(16 * s as u64),
        300,
        2300,
    );
    assert!(received > 3000, "{received}");
    assert_eq!(allocations, 0, "in-memory network steps allocated");

    // Loopback UDP in real time.
    let server_socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = server_socket.local_addr().unwrap();
    let t0 = Instant::now();
    let mut server = Endpoint::server(
        server_socket,
        config(),
        ServerConfig {
            private_key: key.clone(),
            max_clients: 4,
            public_addresses: vec![server_addr],
        },
        t0,
    )
    .unwrap();
    let mut client = Endpoint::client(
        UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(),
        config(),
        token(&key, server_addr),
        t0,
    )
    .unwrap();
    let (allocations, received) = run(
        &mut server,
        &mut client,
        |_| {
            std::thread::sleep(Duration::from_millis(1));
            Instant::now()
        },
        200,
        1200,
    );
    assert!(received > 1000, "{received}");
    assert_eq!(allocations, 0, "UDP network steps allocated");
}
