//! Replication measurements for ADR 0074 (CPU only; run in release):
//!
//! 1. encoding variants on one workload: bits per entity per tick for raw floats,
//!    quantized whole states, deltas against an immediate or an acknowledged (older)
//!    baseline, deltas against an extrapolated baseline, and Tribes-style "changed
//!    components whole";
//! 2. the server: µs per tick (receive + snapshots for every client) and bytes per client
//!    per tick, for 100 / 1,000 / 10,000 replicated entities and 1 / 8 / 32 clients, all
//!    moving or 10 % moving;
//! 3. the client: µs per frame with 1,000 interpolated entities, and with a rollback of
//!    k ticks every snapshot.
//!
//! ```text
//! cargo run -p engine_net --release --example replication_bench -- [quick]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, missing_docs)]

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use engine_core::math::{Quat, Vec3};
use engine_ecs::{bytemuck, reflect_component};
use engine_net::bits::BitWriter;
use engine_net::{NetSchema, ServerSettings, analysis};
use engine_reflect::Reflect;
use std::time::Instant;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// A moving body: rotation, position and velocity (a character or a crate).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
struct Body {
    rot: Quat,
    #[reflect(net_quantum = 0.001)]
    pos: Vec3,
    #[reflect(net_quantum = 0.001)]
    vel: Vec3,
    health: u32,
    _pad: u32,
}
reflect_component!(pod Body);

fn variants() {
    println!("## 1. Encoding variants (1,000 bodies, 64 Hz, random walk: speed ≤ 6 m/s, turning)");
    let schema = NetSchema::new(1).replicate::<Body>();
    let words = analysis::words(&schema, 0);
    let n = 1000;
    let ticks = 400;
    let mut rng = 0x2545_f491u32;
    let mut rnd = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng as f32 / u32::MAX as f32 * 2.0 - 1.0
    };
    let mut bodies: Vec<Body> = (0..n)
        .map(|_| Body {
            rot: Quat::from_rotation_y(rnd() * 3.0),
            pos: Vec3::new(rnd() * 100.0, 0.0, rnd() * 100.0),
            vel: Vec3::new(rnd() * 4.0, 0.0, rnd() * 4.0),
            health: 100,
            _pad: 0,
        })
        .collect();
    // history[t][i] = quantized words.
    let mut history: Vec<Vec<u32>> = Vec::new();
    let dt = 1.0 / 64.0;
    for t in 0..ticks {
        for (i, b) in bodies.iter_mut().enumerate() {
            // Characters: accelerate and turn now and then; a fifth stand still.
            if i % 5 == 0 {
                continue;
            }
            if (t + i) % 32 == 0 {
                b.vel = Vec3::new(rnd() * 6.0, 0.0, rnd() * 6.0);
            }
            b.pos += b.vel * dt;
            b.rot = Quat::from_rotation_y(b.vel.x.atan2(b.vel.z));
            if (t * 7 + i) % 97 == 0 {
                b.health = b.health.saturating_sub(10);
            }
        }
        let mut frame = vec![0u32; n * words];
        for (i, b) in bodies.iter().enumerate() {
            analysis::quantize(
                &schema,
                0,
                bytemuck::bytes_of(b),
                &mut frame[i * words..(i + 1) * words],
            );
        }
        history.push(frame);
    }
    let mut w = BitWriter::with_limit(1 << 16);
    let floats = 4 + 3 + 3;
    let raw_bits = (floats * 32 + 32) as f64; // every field as 32 bits
    let (mut full, mut d1, mut d8, mut pred8, mut pred1, mut shipped, mut tribes8) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut samples = 0usize;
    let age = 8; // ticks between the baseline and now: ~125 ms round trip at 64 Hz
    for t in (2 * age)..ticks {
        for i in 0..n {
            let s = |tt: usize| &history[tt][i * words..(i + 1) * words];
            let cur = s(t);
            full += analysis::bits_full(&schema, 1, cur, &mut w);
            d1 += analysis::bits_delta(&schema, 1, cur, s(t - 1), &mut w);
            d8 += analysis::bits_delta(&schema, 1, cur, s(t - age), &mut w);
            // Linear prediction from the two newest acknowledged states, `age` apart.
            let (b, bp) = (s(t - age), s(t - 2 * age));
            let predicted: Vec<u32> = b
                .iter()
                .zip(bp)
                .enumerate()
                .map(|(k, (&x, &y))| {
                    // Only positions extrapolate (words 4..7: after the quaternion).
                    if (4..7).contains(&k) {
                        x.wrapping_add(x.wrapping_sub(y))
                    } else {
                        x
                    }
                })
                .collect();
            pred8 += analysis::bits_delta(&schema, 1, cur, &predicted, &mut w);
            // The same from two consecutive acknowledged states (a velocity from one tick,
            // extrapolated `age` ticks: quantization noise grows with the ratio).
            let b1 = s(t - age - 1);
            let predicted: Vec<u32> = b
                .iter()
                .zip(b1)
                .enumerate()
                .map(|(k, (&x, &y))| {
                    if (4..7).contains(&k) {
                        x.wrapping_add(x.wrapping_sub(y).wrapping_mul(age as u32))
                    } else {
                        x
                    }
                })
                .collect();
            pred1 += analysis::bits_delta(&schema, 1, cur, &predicted, &mut w);
            // What ships: every quantized float extrapolated from the two newest
            // acknowledged states, chosen per component when cheaper (+1 bit each).
            shipped +=
                analysis::bits_delta_predicted(&schema, 1, cur, b, b1, age as i64, 1, &mut w);
            // Tribes: unchanged → 1 bit; changed → the whole (quantized) state.
            tribes8 += if cur == b {
                1
            } else {
                1 + analysis::bits_full(&schema, 1, cur, &mut w)
            };
            samples += 1;
        }
    }
    let per = |b: usize| b as f64 / samples as f64;
    println!("| variant | bits per entity per tick | vs raw |");
    println!("|---|---:|---:|");
    for (name, bits) in [
        ("raw 32-bit fields", raw_bits),
        ("quantized, whole state (exp-Golomb)", per(full)),
        ("delta vs previous tick (ideal acks)", per(d1)),
        ("delta vs acknowledged state 8 ticks old", per(d8)),
        (
            "delta vs extrapolated baseline (two acks 8 ticks apart), 8 ticks",
            per(pred8),
        ),
        (
            "delta vs extrapolated baseline (two consecutive acks), 8 ticks",
            per(pred1),
        ),
        (
            "shipped: predicted baseline per component when cheaper, 8 ticks",
            per(shipped),
        ),
        ("Tribes: changed state whole, 8 ticks", per(tribes8)),
    ] {
        println!("| {name} | {bits:.1} | {:.0} % |", bits / raw_bits * 100.0);
    }
}

fn server(entities: usize, clients: usize, moving: f32, ticks: usize) -> (f64, f64, f64, f64) {
    let mut sim = Sim::new(3, conditions(20, 0, 0.0), ServerSettings::default());
    sim.frame_us = sim.tick_us; // one client frame per tick
    // Moving ones, and static scenery that no system writes (its own archetype).
    let moving_count = (entities as f32 * moving) as usize;
    sim.spawn_movers(moving_count, 17);
    for i in 0..entities - moving_count {
        let pos = Vec3::new((i % 100) as f32 - 50.0, 0.0, (i / 100) as f32 - 50.0);
        sim.server.world.spawn((
            engine_net::Replicated::new(),
            Mover {
                pos,
                vel: Vec3::ZERO,
            },
            Frozen,
        ));
    }
    for _ in 0..clients {
        sim.add_client();
    }
    sim.run(2.0);
    let (mut update, mut send, mut total) = (Vec::new(), Vec::new(), Vec::new());
    let before: u64 = (0..clients)
        .map(|c| {
            sim.server
                .server
                .stats(engine_net::ClientId(c as u16))
                .unwrap()
                .snapshot_bytes
        })
        .sum();
    for _ in 0..ticks {
        sim.now_us = sim.next_tick_us;
        let now = sim.now();
        let s = &mut sim.server;
        let t0 = Instant::now();
        s.server.update(&mut s.world, now);
        let t1 = Instant::now();
        let dt = s.server.dt();
        simulate(&mut s.world, &mut s.pawn_q, dt);
        move_movers(&mut s.world, &mut s.mover_q, dt);
        sync_transforms(&mut s.world, &mut s.sync_q);
        let t2 = Instant::now();
        s.server.send(&mut s.world, now);
        let t3 = Instant::now();
        update.push((t1 - t0).as_secs_f64() * 1e6);
        send.push((t3 - t2).as_secs_f64() * 1e6);
        total.push(((t1 - t0) + (t3 - t2)).as_secs_f64() * 1e6);
        sim.next_tick_us += sim.tick_us;
        sim.now_us = sim.next_frame_us;
        for i in 0..clients {
            sim.client_frame(i);
        }
        sim.next_frame_us += sim.frame_us;
    }
    let after: u64 = (0..clients)
        .map(|c| {
            sim.server
                .server
                .stats(engine_net::ClientId(c as u16))
                .unwrap()
                .snapshot_bytes
        })
        .sum();
    let bytes = (after - before) as f64 / (clients * ticks) as f64;
    (
        median(&mut update),
        median(&mut send),
        median(&mut total),
        bytes,
    )
}

fn client(rollback_latency_ms: Option<u64>, movers: usize, ticks: usize) -> (f64, f64, f64) {
    let latency = rollback_latency_ms.unwrap_or(20);
    let mut sim = Sim::new(4, conditions(latency, 0, 0.0), ServerSettings::default());
    sim.frame_us = sim.tick_us;
    sim.spawn_movers(movers, 23);
    sim.add_client();
    if rollback_latency_ms.is_some() {
        // The server nudges the pawn every tick: every snapshot is a misprediction.
        sim.rules = Some(Box::new(|s: &mut ServerSide| {
            for &(_, pawn) in &s.pawns {
                if let Some(p) = s.world.get_mut::<Pawn>(pawn) {
                    p.pos.x += 0.001;
                }
            }
        }));
    }
    sim.clients[0].input.move_z = 1.0;
    sim.run(2.0);
    let mut frame = Vec::new();
    let resim_before = sim.clients[0].client.stats().resimulated_ticks;
    let mispredictions_before = sim.clients[0].client.stats().mispredictions;
    for _ in 0..ticks {
        sim.now_us = sim.next_tick_us;
        sim.server_tick();
        sim.next_tick_us += sim.tick_us;
        sim.now_us = sim.next_frame_us;
        let t0 = Instant::now();
        sim.client_frame(0);
        frame.push(t0.elapsed().as_secs_f64() * 1e6);
        sim.next_frame_us += sim.frame_us;
    }
    let st = sim.clients[0].client.stats();
    let resim = (st.resimulated_ticks - resim_before) as f64
        / (st.mispredictions - mispredictions_before).max(1) as f64;
    let rollbacks = (st.mispredictions - mispredictions_before) as f64 / ticks as f64;
    (median(&mut frame), resim, rollbacks)
}

fn main() {
    let quick = std::env::args().any(|a| a == "quick");
    let ticks = if quick { 64 } else { 256 };
    variants();

    println!(
        "\n## 2. Server per tick (median µs; receive + quantize + snapshots + transport), bytes per client per tick"
    );
    println!("| entities | moving | clients | update µs | send µs | total µs | B/client/tick |");
    println!("|---:|---:|---:|---:|---:|---:|---:|");
    for &entities in &[100usize, 1000, 10_000] {
        for &moving in &[1.0f32, 0.1] {
            for &clients in &[1usize, 8, 32] {
                if quick && clients == 32 && entities == 10_000 {
                    continue;
                }
                let (u, s, t, b) = server(entities, clients, moving, ticks);
                println!(
                    "| {entities} | {:.0} % | {clients} | {u:.1} | {s:.1} | {t:.1} | {b:.0} |",
                    moving * 100.0
                );
            }
        }
    }

    println!(
        "\n## 3. Client per frame (one tick per frame; own pawn predicted; the game's tick is trivial)"
    );
    println!(
        "| interpolated entities | case | median µs per frame | ticks re-simulated per rollback | rollbacks per frame |"
    );
    println!("|---:|---|---:|---:|---:|");
    for movers in [0usize, 1000] {
        let (f, _, _) = client(None, movers, ticks);
        println!("| {movers} | no misprediction | {f:.1} | 0 | 0 |");
        for latency in [10u64, 40, 100] {
            let (f, k, r) = client(Some(latency), movers, ticks);
            println!(
                "| {movers} | rollback every snapshot, {latency} ms one way | {f:.1} | {k:.1} | {r:.2} |"
            );
        }
    }
}
