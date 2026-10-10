//! The co-op example's game (`examples/coop`) over the in-memory network: a listen server
//! and a guest with Jolt characters, a button and a door, a lag-compensated hitscan gun.

#![allow(clippy::unwrap_used, clippy::print_stdout, missing_docs)]

#[path = "../../examples/coop/game.rs"]
mod game;

use engine_net_transport::memory::{LinkConditions, MemoryNetwork};
use engine_net_transport::{
    ConnectToken, Endpoint, Key, ServerConfig, TokenParams, USER_DATA_BYTES, unix_now,
};
use game::*;
use std::time::{Duration, Instant};

#[test]
fn two_players_open_the_door_and_shoot() {
    let net = MemoryNetwork::new(42);
    net.set_default_conditions(LinkConditions::new(
        Duration::from_millis(40),
        Duration::from_millis(8),
        0.02,
    ));
    let server_addr = "10.0.0.1:27015".parse().unwrap();
    let key = Key::random();
    let t0 = Instant::now();
    let endpoint = Endpoint::server(
        net.bind(server_addr),
        schema().endpoint_config(),
        ServerConfig {
            private_key: key.clone(),
            max_clients: 4,
            public_addresses: vec![server_addr],
        },
        t0,
    )
    .unwrap();
    let mut host = Host::new(endpoint);
    let token = ConnectToken::generate(
        &TokenParams {
            protocol_id: PROTOCOL,
            client_id: 7,
            expire_seconds: 30,
            timeout_seconds: 10,
            server_addresses: &[server_addr],
            user_data: [0; USER_DATA_BYTES],
        },
        &key,
        unix_now(),
    )
    .unwrap();
    let client = Endpoint::client(
        net.bind("10.0.1.1:5000".parse().unwrap()),
        schema().endpoint_config(),
        token,
        t0,
    )
    .unwrap();
    let mut guest = Guest::new(client);

    let (tick_us, frame_us) = (15_625u64, 8_333u64);
    let (mut next_tick, mut next_frame, mut now_us) = (0u64, 0u64, 0u64);
    let mut phase = 0;
    let mut deepest_while_closed = f32::MIN;
    let mut passed = false;
    let mut mispredictions_before_shooting = 0;
    while now_us < 14_000_000 {
        let t = now_us as f64 / 1e6;
        if next_tick <= next_frame {
            now_us = next_tick;
            let input = host_script(t, host.player(host.me));
            host.tick(t0 + Duration::from_micros(now_us), input);
            next_tick += tick_us;
        } else {
            now_us = next_frame;
            let me = guest.player(guest.me).unwrap_or_default();
            let them = guest.player(guest.host);
            let input = guest_script(t, &mut phase, me, them);
            if phase == 0 {
                mispredictions_before_shooting = guest.client.stats().mispredictions;
            }
            guest.frame(t0 + Duration::from_micros(now_us), input);
            // On the server: how far the guest got while the door was shut.
            let door = host.world.get::<Door>(host.door).copied().unwrap();
            if let Some(&(_, pawn)) = host.guests.first() {
                let z = host.player(pawn).pos.z;
                if door.open == 0.0 {
                    deepest_while_closed = deepest_while_closed.max(z);
                }
                passed |= z > 3.5;
            }
            next_frame += frame_us;
        }
    }
    for line in host.log.iter().chain(&guest.log).take(12) {
        println!("{line}");
    }
    let stats = guest.client.stats();
    let server_stats = host.server.stats(host.guests[0].0).unwrap();
    let host_health = host.player(host.me).health;
    println!(
        "guest: {stats:?}\nserver: {server_stats:?}\ndeepest while closed z = {deepest_while_closed:.3}, \
         host health {host_health}, hits seen {}, mispredictions before shooting {}",
        guest.hits_seen, mispredictions_before_shooting
    );
    assert!(
        deepest_while_closed < -0.3,
        "the closed door blocks the guest"
    );
    assert!(passed, "the guest went through once the door opened");
    assert_eq!(guest.doors_opened, 1, "one reliable DoorOpened event");
    assert!(
        host_health < 100 - 5 * DAMAGE,
        "lag-compensated shots hit the strafing host"
    );
    assert!(guest.hits_seen >= 5);
    assert_eq!(stats.decode_errors, 0);
    // Walking and pushing against the closed door is predicted exactly by the same Jolt
    // character code on both sides. Measured: one correction when the pawn first
    // arrives, then about seven while the guest passes under the opening door (its
    // interpolated position on the client trails the server's by ~2 ticks).
    assert!(
        mispredictions_before_shooting <= 10,
        "{mispredictions_before_shooting} mispredictions"
    );
    // The guest's own player on its screen is the server's, exactly.
    let mine = guest.player(guest.me).unwrap();
    let server_view = host.player(host.guests[0].1);
    assert!(
        (mine.pos - server_view.pos).length() < 0.01,
        "{mine:?} {server_view:?}"
    );
}
