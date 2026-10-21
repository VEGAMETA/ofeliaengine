//! Queries, characters and rollback on the Jolt backend (ADR 0066): ray and shape casts
//! per microsecond, the cost of a character move, and the cost of saving and restoring
//! the world state for 1 000 and 10 000 bodies. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_physics_jolt --bench queries
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3};
use engine_physics::{
    CharacterDesc, CharacterId, CharacterMotor, PhysicsBackend, PhysicsSettings, PhysicsSnapshot,
    QueryFilter, ShapeCast, ShapeDesc, scenes,
};
use engine_physics_jolt::JoltBackend;
use std::hint::black_box;
use std::time::Instant;

const DT: f32 = 1.0 / 60.0;

/// Median of `runs` timings of `f`, in µs.
fn median_us(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn settled_pile(bodies: u32, steps: u32) -> JoltBackend {
    // Settle on every core, then measure on the calling thread (queries, saving and
    // restoring run there).
    let mut w = JoltBackend::new(&PhysicsSettings::default()).unwrap();
    scenes::pile(&mut w, bodies).unwrap();
    for _ in 0..steps {
        w.step(DT, 1).unwrap();
    }
    w
}

fn queries() {
    let mut w = settled_pile(1_000, 240);
    let sphere = w.create_shape(&ShapeDesc::Sphere { radius: 0.3 }).unwrap();
    let capsule = w
        .create_shape(&ShapeDesc::Capsule {
            half_height: 0.5,
            radius: 0.3,
        })
        .unwrap();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.3),
        })
        .unwrap();
    let hull = w
        .create_shape(&ShapeDesc::ConvexHull {
            points: &[
                Vec3::new(-0.3, -0.3, -0.3),
                Vec3::new(0.3, -0.3, -0.3),
                Vec3::new(0.0, -0.3, 0.3),
                Vec3::new(0.0, 0.4, 0.0),
                Vec3::new(0.2, 0.2, 0.2),
            ],
            convex_radius: engine_physics::HULL_RADIUS,
        })
        .unwrap();
    let rays = scenes::ray_grid(64);
    let casts = |shape| -> Vec<ShapeCast> {
        rays.iter()
            .map(|r| ShapeCast {
                shape,
                position: r.origin,
                rotation: Quat::from_rotation_x(0.4),
                direction: r.direction,
            })
            .collect()
    };
    let n = rays.len() as f64;
    let filter = QueryFilter::default();
    println!("### Queries over a settled pile of 1 000 bodies (one thread)\n");
    println!(
        "{} vertical casts over a 100 m square from y = 50, 100 m long.\n",
        rays.len()
    );
    println!("| query | µs per query | queries per µs | hits |");
    println!("|---|---:|---:|---:|");
    let row = |name: &str, total_us: f64, hits: usize| {
        println!(
            "| {name} | {:.3} | {:.2} | {hits} |",
            total_us / n,
            n / total_us
        );
    };

    let mut hits = vec![None; rays.len()];
    let t = median_us(30, || w.raycast_batch(&rays, filter, &mut hits));
    row("ray, closest", t, hits.iter().flatten().count());
    let mut all = Vec::with_capacity(64);
    let mut count = 0;
    let t = median_us(30, || {
        count = 0;
        for ray in &rays {
            all.clear();
            w.raycast_all(ray, filter, &mut all);
            count += all.len();
        }
    });
    row("ray, every hit", t, count);
    for (name, shape) in [
        ("sphere cast, closest", sphere),
        ("capsule cast, closest", capsule),
        ("box cast, closest", cube),
        ("convex hull cast, closest", hull),
    ] {
        let casts = casts(shape);
        let mut out = vec![None; casts.len()];
        let t = median_us(30, || w.cast_shape_batch(&casts, filter, &mut out));
        row(name, t, out.iter().flatten().count());
    }
    let capsule_casts = casts(capsule);
    let mut all_hits = Vec::with_capacity(64);
    let t = median_us(30, || {
        count = 0;
        for cast in &capsule_casts {
            all_hits.clear();
            w.cast_shape_all(cast, filter, &mut all_hits);
            count += all_hits.len();
        }
    });
    row("capsule cast, every hit", t, count);
    let mut bodies = Vec::with_capacity(256);
    let t = median_us(30, || {
        count = 0;
        for cast in &capsule_casts {
            bodies.clear();
            w.overlap_shape(
                capsule,
                Vec3::new(cast.position.x, 0.5, cast.position.z),
                Quat::IDENTITY,
                filter,
                &mut bodies,
            );
            count += bodies.len();
        }
    });
    row("capsule overlap at y = 0.5", t, count);
    black_box(&hits);
}

fn characters() {
    println!("\n### Character moves (one thread)\n");
    println!(
        "Characters 2.5 m apart walking in circles next to a settled 1 000-body pile, some into stairs. Every character tests every other (Jolt's `CharacterVsCharacterCollisionSimple`, bounding boxes first).\n"
    );
    println!("| characters | µs per move (median of frames) | µs per frame |");
    println!("|---:|---:|---:|");
    for count in [1u32, 16, 128] {
        let mut w = settled_pile(1_000, 240);
        for i in 0..8 {
            let shape = w
                .create_shape(&ShapeDesc::Box {
                    half_extents: Vec3::new(4.0 - 0.25 * i as f32, 0.125 * (i + 1) as f32, 40.0),
                })
                .unwrap();
            w.create_body(&engine_physics::BodyDesc::fixed(
                shape,
                Vec3::new(36.0 + 0.25 * i as f32, 0.125 * (i + 1) as f32, 0.0),
            ))
            .unwrap();
        }
        // A grid 2.5 m apart east of the pile, so neighbours meet only now and then.
        let ids: Vec<CharacterId> = (0..count)
            .map(|i| {
                let feet = Vec3::new(
                    30.0 + (i % 12) as f32 * 2.5,
                    0.0,
                    (i / 12) as f32 * 2.5 - 15.0,
                );
                w.create_character(&CharacterDesc::capsule(feet, 1.8, 0.3))
                    .unwrap()
            })
            .collect();
        let motor = CharacterMotor::default();
        let mut frame = 0u32;
        let mut frame_us = Vec::new();
        for _ in 0..240 {
            frame += 1;
            let start = Instant::now();
            for (i, &c) in ids.iter().enumerate() {
                let state = w.character(c).unwrap();
                let a = frame as f32 * 0.02 + i as f32;
                let walk = Vec3::new(a.cos(), 0.0, a.sin()) * 3.0;
                let v = motor.velocity(&state, walk, frame % 60 == i as u32 % 60, DT);
                w.move_character(c, v, DT).unwrap();
            }
            frame_us.push(start.elapsed().as_secs_f64() * 1e6);
            w.step(DT, 1).unwrap();
        }
        frame_us.sort_by(f64::total_cmp);
        let median = frame_us[frame_us.len() / 2];
        println!(
            "| {count} | {:.2} | {median:.1} |",
            median / f64::from(count)
        );
    }
}

fn rollback() {
    println!("\n### Save and restore (one thread)\n");
    println!(
        "Pile scenes after 30 steps (\"falling\": in the air, few contacts), 120 (\"landed\": awake, in contact) and 600 (\"settled\": mostly asleep, contacts kept).\n"
    );
    println!(
        "| bodies | state | snapshot KiB | save µs | restore µs | save + restore ns per body | Jolt allocations per save / restore |"
    );
    println!("|---:|---|---:|---:|---:|---:|---|");
    for bodies in [1_000u32, 10_000] {
        let mut w = JoltBackend::new(&PhysicsSettings::default()).unwrap();
        scenes::pile(&mut w, bodies).unwrap();
        for (state, steps) in [("falling", 30u32), ("landed", 90), ("settled", 480)] {
            for _ in 0..steps {
                w.step(DT, 1).unwrap();
            }
            let mut snapshot = PhysicsSnapshot::new();
            w.save_state(&mut snapshot).unwrap();
            let save = median_us(50, || w.save_state(&mut snapshot).unwrap());
            let restore = median_us(50, || w.restore_state(&snapshot).unwrap());
            let jolt = JoltBackend::jolt_allocations();
            w.save_state(&mut snapshot).unwrap();
            let after_save = JoltBackend::jolt_allocations();
            w.restore_state(&snapshot).unwrap();
            let after_restore = JoltBackend::jolt_allocations();
            println!(
                "| {bodies} | {state} | {:.0} | {save:.0} | {restore:.0} | {:.0} | {} / {} |",
                snapshot.len() as f64 / 1024.0,
                (save + restore) * 1e3 / f64::from(bodies + 1),
                after_save - jolt,
                after_restore - after_save,
            );
            // Hashing the snapshot (desync checks).
            if state == "settled" {
                let hash = median_us(50, || {
                    black_box(snapshot.hash());
                });
                println!(
                    "|  | (hash of the snapshot: {hash:.0} µs, {:.1} GB/s) | | | | | |",
                    snapshot.len() as f64 / hash / 1e3
                );
            }
        }
    }
}

fn main() {
    queries();
    characters();
    rollback();
}
