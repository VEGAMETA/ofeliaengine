//! Co-op over real UDP on this machine: a listen server (the host plays in it) and a
//! guest, each with its own world and Jolt physics, in one process. The host stands on a
//! button so the guest can pass a door; then the guest shoots the strafing host with a
//! lag-compensated hitscan gun. Inputs are scripted; the log shows what each side saw.
//!
//! ```text
//! cargo run -p engine_net --release --example coop -- [seconds]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, missing_docs)]

mod game;

use engine_net_transport::{
    ConnectToken, Endpoint, Key, ServerConfig, TokenParams, USER_DATA_BYTES, UdpSocket, unix_now,
};
use engine_platform::time::PreciseSleeper;
use game::*;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: f64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(14.0);

    // net: the host binds a UDP port and admits players with tokens it makes itself.
    let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.as_std().local_addr().unwrap();
    let key = Key::random();
    let start = Instant::now();
    let endpoint = Endpoint::server(
        socket,
        schema().endpoint_config(),
        ServerConfig {
            private_key: key.clone(),
            max_clients: 4,
            public_addresses: vec![addr],
        },
        start,
    )
    .unwrap();
    let mut host = Host::new(endpoint);
    println!("host listening on {addr}");

    // net: an invitation (in a real game it travels over the platform's invite or HTTPS).
    let token = ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id: 1,
            expire_seconds: 30,
            timeout_seconds: 10,
            server_addresses: &[addr],
            user_data: [0; USER_DATA_BYTES],
        },
        &key,
        unix_now(),
    )
    .unwrap();
    let client_socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let client = Endpoint::client(client_socket, schema().endpoint_config(), token, start).unwrap();
    let mut guest = Guest::new(client);

    let tick = Duration::from_micros(15_625);
    let frame = Duration::from_micros(8_333);
    let (mut next_tick, mut next_frame) = (start, start);
    let mut sleeper = PreciseSleeper::new().unwrap();
    let mut phase = 0;
    let mut printed = (0, 0);
    let (mut host_us, mut guest_us, mut ticks, mut frames) = (0.0, 0.0, 0u32, 0u32);
    while start.elapsed().as_secs_f64() < seconds {
        let next = next_tick.min(next_frame);
        sleeper.sleep_until(next).unwrap();
        let now = Instant::now();
        let t = (now - start).as_secs_f64();
        if next_tick <= next_frame {
            let input = host_script(t, host.player(host.me));
            let s = Instant::now();
            host.tick(now, input);
            host_us += s.elapsed().as_secs_f64() * 1e6;
            ticks += 1;
            next_tick += tick;
        } else {
            let me = guest.player(guest.me).unwrap_or_default();
            let them = guest.player(guest.host);
            let input = guest_script(t, &mut phase, me, them);
            let s = Instant::now();
            guest.frame(now, input);
            guest_us += s.elapsed().as_secs_f64() * 1e6;
            frames += 1;
            next_frame += frame;
        }
        for line in &host.log[printed.0..] {
            println!("[{t:6.2}] host:  {line}");
        }
        for line in &guest.log[printed.1..] {
            println!("[{t:6.2}] guest: {line}");
        }
        printed = (host.log.len(), guest.log.len());
    }
    let stats = guest.client.stats();
    println!(
        "\nhost: {ticks} ticks, {:.1} µs per tick (simulation, rules, physics, snapshots)",
        host_us / f64::from(ticks.max(1))
    );
    println!(
        "guest: {frames} frames, {:.1} µs per frame (receive, reconcile, predict, interpolate)",
        guest_us / f64::from(frames.max(1))
    );
    if let Some(&(id, _)) = host.guests.first()
        && let Some(s) = host.server.stats(id)
    {
        println!(
            "snapshots to the guest: {}, {:.0} bytes each on average",
            s.snapshots,
            s.snapshot_bytes as f64 / s.snapshots.max(1) as f64
        );
    }
    println!(
        "guest predictions: {} confirmed, {} corrected ({} ticks re-simulated); input lead {:.1} ticks, interpolation delay {:.1} ticks",
        stats.confirmed,
        stats.mispredictions,
        stats.resimulated_ticks,
        stats.input_lead,
        stats.interp_delay
    );
    println!(
        "host health {}, door opened {} time(s) on the guest's screen, {} hits seen",
        host.player(host.me).health,
        guest.doors_opened,
        guest.hits_seen
    );
}
