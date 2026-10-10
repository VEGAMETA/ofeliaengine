//! Replication end to end over the in-memory network (ADR 0074): convergence under loss,
//! latency and jitter; late join; prediction without drift and with corrections;
//! interpolation; lag compensation; events; the byte budget.

#![allow(clippy::unwrap_used, clippy::print_stdout, missing_docs)]

use crate::common;

use common::*;
use engine_core::math::Vec3;
use engine_ecs::World;
use engine_net::{ClientEvent, Predicted, ServerSettings, SubTick, Target};

/// Every replicated entity of the server exists on the client with the same values
/// (both are quantized, so equal means equal).
fn assert_converged(server: &World, client: &World) {
    let (pairs, ns, nc) = matched(server, client);
    assert_eq!(ns, nc, "entity counts differ");
    assert_eq!(pairs.len(), ns, "entities missing on the client");
    for (se, ce) in pairs {
        assert_eq!(server.get::<Mover>(se), client.get::<Mover>(ce));
        if let (Some(a), Some(b)) = (server.get::<Pawn>(se), client.get::<Pawn>(ce)) {
            assert_eq!(a, b);
        }
    }
}

fn converges_under(loss: f32, seed: u64) {
    let mut sim = Sim::new(seed, conditions(40, 10, loss), ServerSettings::default());
    sim.spawn_movers(300, 7);
    sim.add_client();
    sim.add_client();
    sim.run(4.0);
    // Stop everything; the clients must end up with the server's exact state.
    let mut q = engine_ecs::QueryState::<&mut Mover>::new(&mut sim.server.world);
    q.for_each(&mut sim.server.world, |m| m.vel = Vec3::ZERO);
    sim.run(3.0);
    for c in &sim.clients {
        assert_converged(&sim.server.world, &c.world);
        let stats = c.client.stats();
        println!("loss {loss}: {stats:?}");
        assert_eq!(stats.decode_errors, 0);
    }
    let st = sim.server.server.stats(engine_net::ClientId(0)).unwrap();
    println!("server: {st:?}");
}

#[test]
fn converges_without_loss() {
    converges_under(0.0, 1);
}

#[test]
fn converges_at_5_percent_loss() {
    converges_under(0.05, 2);
}

#[test]
fn converges_at_15_percent_loss() {
    converges_under(0.15, 3);
}

#[test]
fn converges_at_30_percent_loss() {
    converges_under(0.30, 4);
}

#[test]
fn late_join_gets_the_whole_world() {
    let mut sim = Sim::new(5, conditions(30, 5, 0.05), ServerSettings::default());
    sim.spawn_movers(1000, 11);
    sim.add_client();
    sim.run(2.0);
    // Some despawned before the second client joins, some spawned after.
    let ids = iter_replicated(&sim.server.world);
    for (e, _) in ids.iter().step_by(10) {
        sim.server.world.despawn(*e);
    }
    sim.run(0.5);
    sim.add_client();
    sim.spawn_movers(50, 99);
    let mut q = engine_ecs::QueryState::<&mut Mover>::new(&mut sim.server.world);
    sim.run(1.0);
    q.for_each(&mut sim.server.world, |m| m.vel = Vec3::ZERO);
    // 1,000 entities at ~1.1 KB per snapshot take a while to fill in.
    sim.run(3.0);
    for c in &sim.clients {
        assert_converged(&sim.server.world, &c.world);
    }
    let spawned = sim.clients[1]
        .events
        .iter()
        .filter(|e| matches!(e, ClientEvent::Spawned { .. }))
        .count();
    // 900 movers left (and the first client's pawn, the last of every tenth entity
    // despawned), 50 new ones, its own pawn.
    assert_eq!(spawned, 900 + 50 + 1, "each entity spawned once");
}

/// Input that changes over time, deterministic.
fn wander(t: u64) -> engine_net::NetInput<Input> {
    let phase = (t / 40) % 4;
    let (x, z) = [(1.0, 0.0), (0.0, 1.0), (-1.0, 0.3), (0.2, -1.0)][phase as usize];
    engine_net::NetInput {
        input: Input {
            move_x: x,
            move_z: z,
            yaw: (t % 360) as f32 * 0.01,
            fire: u8::from(t.is_multiple_of(50)),
            _pad: [0; 3],
        },
        header: Default::default(),
    }
}

#[test]
fn prediction_matches_the_server_without_drift() {
    let mut sim = Sim::new(6, conditions(50, 0, 0.0), ServerSettings::default());
    sim.add_client();
    sim.run(1.0);
    let mut frame = 0u64;
    sim.run_with(6.0, |sim| {
        frame += 1;
        sim.clients[0].input = wander(frame).input;
    });
    let c = &sim.clients[0];
    let stats = c.client.stats();
    println!("{stats:?}");
    // The first state of the pawn arrives before any prediction of it: one correction at
    // most; afterwards every prediction is confirmed bit for bit.
    assert!(stats.mispredictions <= 1, "{stats:?}");
    assert!(stats.confirmed > 300, "{stats:?}");
    // Stop and compare the final state.
    sim.clients[0].input = Input::default();
    sim.run(1.0);
    assert_converged(&sim.server.world, &sim.clients[0].world);
}

#[test]
fn prediction_survives_loss_and_jitter() {
    let mut sim = Sim::new(7, conditions(60, 20, 0.10), ServerSettings::default());
    sim.add_client();
    sim.run(1.0);
    let mut frame = 0u64;
    sim.run_with(8.0, |sim| {
        frame += 1;
        sim.clients[0].input = wander(frame).input;
        if frame.is_multiple_of(120) {
            let st = sim.server.server.stats(engine_net::ClientId(0)).unwrap();
            println!(
                "t{frame}: missing {} late {} lead {} client lead {}",
                st.inputs_missing,
                st.inputs_late,
                st.input_lead,
                sim.clients[0].client.stats().input_lead
            );
        }
    });
    let stats = sim.clients[0].client.stats();
    let server = sim.server.server.stats(engine_net::ClientId(0)).unwrap();
    println!("{stats:?}\n{server:?}");
    // Inputs are repeated in every packet: a 10 % loss with 20 ms jitter loses almost
    // none at the server, so mispredictions stay rare.
    assert!(server.inputs_missing < 20, "{server:?}");
    assert!(stats.mispredictions * 50 < stats.confirmed, "{stats:?}");
    sim.clients[0].input = Input::default();
    sim.run(1.5);
    assert_converged(&sim.server.world, &sim.clients[0].world);
}

#[test]
fn mispredictions_are_corrected_by_rollback() {
    let mut sim = Sim::new(8, conditions(50, 0, 0.0), ServerSettings::default());
    sim.add_client();
    // The server pushes the pawn sideways once a second: the client cannot predict it.
    sim.rules = Some(Box::new(|s: &mut ServerSide| {
        if s.server.tick().is_multiple_of(64) {
            for &(_, pawn) in &s.pawns {
                if let Some(p) = s.world.get_mut::<Pawn>(pawn) {
                    p.pos.x += 1.0;
                }
            }
        }
    }));
    sim.run(1.0);
    let mut corrections = 0;
    sim.run_with(4.0, |sim| {
        sim.clients[0].input = Input {
            move_z: 1.0,
            ..Input::default()
        };
        let c = &sim.clients[0];
        if c.last_frame.resimulated > 0 {
            let pawn = common::iter_replicated(&c.world)
                .into_iter()
                .find(|(e, _)| c.world.has::<Predicted>(*e))
                .unwrap()
                .0;
            if let Some((before, after)) = c.client.correction::<Pawn>(pawn) {
                assert!(
                    (after.pos.x - before.pos.x - 1.0).abs() < 1e-3,
                    "{before:?} {after:?}"
                );
                corrections += 1;
            }
        }
    });
    let stats = sim.clients[0].client.stats();
    println!("{stats:?}, {corrections} corrections seen");
    assert!((3..=6).contains(&stats.mispredictions), "{stats:?}");
    assert!(corrections >= 3);
    assert!(stats.resimulated_ticks > 0);
    sim.clients[0].input = Input::default();
    sim.rules = None;
    sim.run(1.0);
    assert_converged(&sim.server.world, &sim.clients[0].world);
}

#[test]
fn interpolation_is_smooth_under_jitter_and_loss() {
    let mut sim = Sim::new(9, conditions(40, 15, 0.05), ServerSettings::default());
    // One mover at 3 m/s along x, far from the walls.
    sim.server.world.spawn((
        engine_net::Replicated::new(),
        Mover {
            pos: Vec3::new(-45.0, 1.0, 0.0),
            vel: Vec3::new(3.0, 0.0, 0.0),
        },
    ));
    sim.add_client();
    sim.run(2.0);
    let mut positions = Vec::new();
    sim.run_with(5.0, |sim| {
        let c = &sim.clients[0];
        if let Some((e, _)) = iter_replicated(&c.world)
            .into_iter()
            .find(|(e, _)| c.world.has::<Mover>(*e))
        {
            positions.push(c.world.get::<Mover>(e).unwrap().pos.x);
        }
    });
    // 120 Hz frames: 3 m/s is 2.5 cm per frame. Never backwards, never a jump.
    let steps: Vec<f32> = positions.windows(2).map(|w| w[1] - w[0]).collect();
    let expected = 3.0 / 120.0;
    let worst = steps
        .iter()
        .map(|s| (s - expected).abs())
        .fold(0.0f32, f32::max);
    let mean = steps.iter().sum::<f32>() / steps.len() as f32;
    let stats = sim.clients[0].client.stats();
    println!("worst step error {worst} m, mean step {mean}, {stats:?}");
    assert!(steps.iter().all(|&s| s >= 0.0), "moved backwards");
    // Late or lost snapshots are bridged by short extrapolation and the clock's ±10 %
    // corrections: no frame is off by more than 20 % of a step.
    assert!(worst < expected * 0.2, "worst step error {worst}");
    assert!((mean - expected).abs() < expected * 0.02);
    assert!(stats.interp_delay < 6.0, "{stats:?}");
}

#[test]
fn lag_compensation_hits_what_the_shooter_saw() {
    let mut sim = Sim::new(10, conditions(80, 10, 0.0), ServerSettings::default());
    // A target crossing the shooter's view at 6 m/s, 20 m away.
    let target = sim.server.world.spawn((
        engine_net::Replicated::new(),
        Mover {
            pos: Vec3::new(-40.0, 0.0, 20.0),
            vel: Vec3::new(6.0, 0.0, 0.0),
        },
        engine_ecs::Transform::from_translation(Vec3::new(-40.0, 0.0, 20.0)),
        engine_net::HitCapsule::upright(1.8, 0.3),
    ));
    // Keep the target's Transform on its Mover position (its hit shape follows it).
    let shots = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let shots_in = shots.clone();
    sim.rules = Some(Box::new(move |s: &mut ServerSide| {
        let p = s.world.get::<Mover>(target).unwrap().pos;
        s.world
            .get_mut::<engine_ecs::Transform>(target)
            .unwrap()
            .translation = p;
        for &(_, pawn) in &s.pawns {
            let input = *s.world.get::<engine_net::NetInput<Input>>(pawn).unwrap();
            let Some(sub) = input.input.fire_subtick() else {
                continue;
            };
            if !input.header.fresh {
                continue;
            }
            let origin = s.world.get::<Pawn>(pawn).unwrap().pos + Vec3::Y;
            let dir = Vec3::new(input.input.yaw.sin(), 0.0, input.input.yaw.cos());
            let view = input.header.view_time(sub);
            let compensated = s.server.lag().raycast(origin, dir, 100.0, view, Some(pawn));
            let now = f64::from(s.server.tick());
            let uncompensated = s.server.lag().raycast(origin, dir, 100.0, now, Some(pawn));
            shots_in.borrow_mut().push((
                compensated.is_some_and(|h| h.entity == target),
                uncompensated.is_some_and(|h| h.entity == target),
                view,
                now,
            ));
        }
    }));
    sim.add_client();
    sim.run(2.0);
    // Each frame: aim at the target as this client shows it; fire every 10th frame.
    let mut frame = 0;
    let mut aimed = Vec::new();
    sim.run_with(5.0, |sim| {
        frame += 1;
        let c = &mut sim.clients[0];
        let shown = iter_replicated(&c.world)
            .into_iter()
            .find(|(e, _)| c.world.has::<Mover>(*e))
            .map(|(e, _)| c.world.get::<Mover>(e).unwrap().pos);
        let me = iter_replicated(&c.world)
            .into_iter()
            .find(|(e, _)| c.world.has::<Predicted>(*e))
            .map(|(e, _)| c.world.get::<Pawn>(e).unwrap().pos);
        c.input = Input::default();
        if let (Some(t), Some(me)) = (shown, me)
            && frame % 10 == 0
            && t.x.abs() < 30.0
        {
            let d = (t + Vec3::Y) - (me + Vec3::Y);
            c.input.yaw = d.x.atan2(d.z);
            c.input.fire = SubTick::END.0;
            aimed.push(t);
        }
    });
    let shots = shots.borrow();
    let hits = shots.iter().filter(|s| s.0).count();
    let naive = shots.iter().filter(|s| s.1).count();
    println!(
        "{} shots: {hits} hits with lag compensation, {naive} without; view lag {:.2} ticks",
        shots.len(),
        shots.iter().map(|s| s.3 - s.2).sum::<f64>() / shots.len().max(1) as f64
    );
    assert!(shots.len() >= 15);
    assert_eq!(hits, shots.len(), "every shot at what the shooter saw hits");
    assert!(
        naive * 4 < shots.len(),
        "without compensation most miss ({naive})"
    );
}

#[test]
fn reliable_events_arrive_once_and_unreliable_at_most_once() {
    let mut sim = Sim::new(11, conditions(30, 10, 0.30), ServerSettings::default());
    sim.add_client();
    sim.run(1.0);
    let mut got_reliable = Vec::new();
    let mut got_unreliable = Vec::new();
    for n in 0..300 {
        sim.server.server.send_event(Target::All, true, &Ping { n });
        sim.server
            .server
            .send_event(Target::All, false, &Chat { n });
        sim.run_with(1.0 / 64.0, |sim| {
            sim.clients[0]
                .client
                .read_events::<Ping>(|_, e| got_reliable.push(e.n));
            sim.clients[0]
                .client
                .read_events::<Chat>(|_, e| got_unreliable.push(e.n));
        });
    }
    sim.run_with(2.0, |sim| {
        sim.clients[0]
            .client
            .read_events::<Ping>(|_, e| got_reliable.push(e.n));
        sim.clients[0]
            .client
            .read_events::<Chat>(|_, e| got_unreliable.push(e.n));
    });
    assert_eq!(
        got_reliable,
        (0..300).collect::<Vec<_>>(),
        "all, once, in order"
    );
    let mut u = got_unreliable.clone();
    u.sort_unstable();
    u.dedup();
    assert_eq!(u.len(), got_unreliable.len(), "never twice");
    println!("unreliable: {} of 300 arrived", u.len());
    assert!(u.len() < 300 && u.len() > 150);
    // Client to server.
    let mut got = Vec::new();
    for k in 0..50 {
        sim.clients[0].client.send_event(true, &Ping { n: k });
        sim.run_with(1.0 / 64.0, |_| {});
        sim.server
            .server
            .read_events::<Ping>(|_, _, e| got.push(e.n));
    }
    for _ in 0..200 {
        sim.run_with(1.0 / 64.0, |_| {});
        sim.server
            .server
            .read_events::<Ping>(|_, _, e| got.push(e.n));
    }
    assert_eq!(got, (0..50).collect::<Vec<_>>());
}

#[test]
fn snapshots_stay_within_the_byte_budget() {
    let settings = ServerSettings {
        max_snapshot_bytes: 600,
        ..ServerSettings::default()
    };
    let mut sim = Sim::new(12, conditions(30, 5, 0.02), settings);
    sim.spawn_movers(2000, 3);
    sim.add_client();
    sim.run(3.0);
    let st = sim.server.server.stats(engine_net::ClientId(0)).unwrap();
    let avg = st.snapshot_bytes as f64 / st.snapshots as f64;
    println!("{st:?}, average {avg:.0} B");
    assert!(st.last_snapshot_bytes <= 600);
    assert!(avg <= 600.0 && avg > 500.0, "fills the budget: {avg}");
    // Everything moves: the client still sees every entity (priorities rotate).
    let (pairs, ns, nc) = matched(&sim.server.world, &sim.clients[0].world);
    if pairs.len() != ns {
        let c: Vec<_> = iter_replicated(&sim.clients[0].world)
            .into_iter()
            .map(|x| x.1.net_id().unwrap().slot)
            .collect();
        let missing: Vec<_> = iter_replicated(&sim.server.world)
            .into_iter()
            .map(|x| x.1.net_id().unwrap().slot)
            .filter(|s| !c.contains(s))
            .collect();
        println!("client has {nc}; missing {missing:?}");
    }
    assert_eq!(pairs.len(), ns);
}
