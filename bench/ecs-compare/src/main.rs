//! ECS comparison for ADR 0017. Scenarios after rust-gamedev's ecs_bench_suite (archived;
//! MIT/Apache), re-implemented here, plus a large iteration and random access. Single
//! thread, release build, median of repeated runs. Prints Markdown.
//!
//! `cargo run --release --manifest-path bench/ecs-compare/Cargo.toml`

#![allow(dead_code)]

mod proto;

use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Transform([f32; 16]);
#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Position([f32; 3]);
#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Rotation([f32; 3]);
#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Velocity([f32; 3]);
#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Tag(f32);
#[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
struct Data(f32);

const IDENTITY: Transform = Transform([
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
]);

fn bundle() -> (Transform, Position, Rotation, Velocity) {
    (
        IDENTITY,
        Position([1.0, 0.0, 0.0]),
        Rotation([1.0, 0.0, 0.0]),
        Velocity([1.0, 0.0, 0.0]),
    )
}

/// Median of `runs` timings of `f`, in microseconds; `setup` runs untimed before each.
fn measure<S>(runs: usize, mut setup: impl FnMut() -> S, mut f: impl FnMut(&mut S)) -> f64 {
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let mut state = setup();
            let start = Instant::now();
            f(&mut state);
            let t = start.elapsed().as_secs_f64() * 1e6;
            black_box(&state);
            drop(state);
            t
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn add(p: &mut Position, v: &Velocity) {
    p.0[0] += v.0[0];
    p.0[1] += v.0[1];
    p.0[2] += v.0[2];
}

/// Pseudo-random permutation of `0..n` (for random access).
fn shuffled(n: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for i in (1..n).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.swap(i, (s % (i as u64 + 1)) as usize);
    }
    v
}

struct Row {
    name: &'static str,
    values: [f64; 4],
}

// ---------------------------------------------------------------------------------------
// Prototype (own archetype ECS)
// ---------------------------------------------------------------------------------------

fn proto_world(n: usize) -> (proto::World, Vec<proto::Entity>) {
    let mut w = proto::World::new();
    let es = (0..n).map(|_| w.spawn(bundle())).collect();
    (w, es)
}

// ---------------------------------------------------------------------------------------
// hecs
// ---------------------------------------------------------------------------------------

fn hecs_world(n: usize) -> (hecs::World, Vec<hecs::Entity>) {
    let mut w = hecs::World::new();
    let es = (0..n).map(|_| w.spawn(bundle())).collect();
    (w, es)
}

// ---------------------------------------------------------------------------------------
// bevy_ecs
// ---------------------------------------------------------------------------------------

fn bevy_world(n: usize) -> (bevy_ecs::world::World, Vec<bevy_ecs::entity::Entity>) {
    let mut w = bevy_ecs::world::World::new();
    let es = (0..n).map(|_| w.spawn(bundle()).id()).collect();
    (w, es)
}

// ---------------------------------------------------------------------------------------
// flecs
// ---------------------------------------------------------------------------------------

fn flecs_world(n: usize) -> (flecs_ecs::prelude::World, Vec<flecs_ecs::prelude::Entity>) {
    use flecs_ecs::prelude::*;
    let w = World::new();
    let (t, p, r, v) = bundle();
    let es = (0..n)
        .map(|_| w.entity().set(t).set(p).set(r).set(v).id())
        .collect();
    (w, es)
}

fn main() {
    use flecs_ecs::prelude::*;

    let mut rows: Vec<Row> = Vec::new();

    // 1. Spawn 10 000 entities with (Transform, Position, Rotation, Velocity).
    let n = 10_000;
    rows.push(Row {
        name: "spawn 10k × 4 components",
        values: [
            measure(50, || (), |_| drop(black_box(proto_world(n)))),
            measure(50, || (), |_| drop(black_box(hecs_world(n)))),
            measure(50, || (), |_| drop(black_box(bevy_world(n)))),
            measure(20, || (), |_| drop(black_box(flecs_world(n)))),
        ],
    });

    // 2. Iterate (Position += Velocity) over 10k and 1M entities.
    for n in [10_000usize, 1_000_000] {
        let runs = if n > 100_000 { 30 } else { 300 };
        let (mut pw, _) = proto_world(n);
        let (mut hw, _) = hecs_world(n);
        let (mut bw, _) = bevy_world(n);
        let mut bq = bw.query::<(&mut Position, &Velocity)>();
        let (fw, _) = flecs_world(n);
        let fq = fw.new_query::<(&mut Position, &Velocity)>();
        rows.push(Row {
            name: if n == 10_000 {
                "iterate 10k: position += velocity"
            } else {
                "iterate 1M: position += velocity"
            },
            values: [
                measure(runs, || (), |_| pw.each_mut2::<Position, Velocity>(add)),
                measure(
                    runs,
                    || (),
                    |_| {
                        for (p, v) in hw.query_mut::<(&mut Position, &Velocity)>() {
                            add(p, v);
                        }
                    },
                ),
                measure(
                    runs,
                    || (),
                    |_| {
                        for (mut p, v) in bq.iter_mut(&mut bw) {
                            add(&mut p, v);
                        }
                    },
                ),
                measure(runs, || (), |_| fq.each(|(p, v)| add(p, v))),
            ],
        });
    }

    // 3. Fragmented: 26 archetypes × 20 entities, all with Data; double Data.
    {
        macro_rules! frag_types {
            ($($t:ident),*) => {
                $(
                    #[derive(Clone, Copy, bevy_ecs::component::Component, flecs_ecs::prelude::Component)]
                    struct $t(f32);
                )*
                fn frag_proto() -> proto::World {
                    let mut w = proto::World::new();
                    $( for _ in 0..20 { w.spawn(($t(0.0), Data(1.0))); } )*
                    w
                }
                fn frag_hecs() -> hecs::World {
                    let mut w = hecs::World::new();
                    $( for _ in 0..20 { w.spawn(($t(0.0), Data(1.0))); } )*
                    w
                }
                fn frag_bevy() -> bevy_ecs::world::World {
                    let mut w = bevy_ecs::world::World::new();
                    $( for _ in 0..20 { w.spawn(($t(0.0), Data(1.0))); } )*
                    w
                }
                fn frag_flecs() -> flecs_ecs::prelude::World {
                    let w = flecs_ecs::prelude::World::new();
                    $( for _ in 0..20 { w.entity().set($t(0.0)).set(Data(1.0)); } )*
                    w
                }
            };
        }
        frag_types!(
            A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z
        );
        let mut pw = frag_proto();
        let mut hw = frag_hecs();
        let mut bw = frag_bevy();
        let mut bq = bw.query::<&mut Data>();
        let fw = frag_flecs();
        let fq = fw.new_query::<&mut Data>();
        rows.push(Row {
            name: "fragmented: 26 archetypes × 20, data *= 2",
            values: [
                measure(1000, || (), |_| pw.each_mut::<Data>(|d| d.0 *= 2.0)),
                measure(
                    1000,
                    || (),
                    |_| {
                        for d in hw.query_mut::<&mut Data>() {
                            d.0 *= 2.0;
                        }
                    },
                ),
                measure(
                    1000,
                    || (),
                    |_| {
                        for mut d in bq.iter_mut(&mut bw) {
                            d.0 *= 2.0;
                        }
                    },
                ),
                measure(1000, || (), |_| fq.each(|d| d.0 *= 2.0)),
            ],
        });
    }

    // 4. Add then remove a component on 10 000 entities (archetype moves).
    {
        let n = 10_000;
        rows.push(Row {
            name: "add + remove a component, 10k entities",
            values: [
                measure(
                    30,
                    || {
                        let mut w = proto::World::new();
                        let es: Vec<_> = (0..n).map(|_| w.spawn((Data(0.0),))).collect();
                        (w, es)
                    },
                    |(w, es)| {
                        for &e in es.iter() {
                            w.insert(e, Tag(0.0));
                        }
                        for &e in es.iter() {
                            w.remove::<Tag>(e);
                        }
                    },
                ),
                measure(
                    30,
                    || {
                        let mut w = hecs::World::new();
                        let es: Vec<_> = (0..n).map(|_| w.spawn((Data(0.0),))).collect();
                        (w, es)
                    },
                    |(w, es)| {
                        for &e in es.iter() {
                            w.insert_one(e, Tag(0.0)).unwrap();
                        }
                        for &e in es.iter() {
                            w.remove_one::<Tag>(e).unwrap();
                        }
                    },
                ),
                measure(
                    30,
                    || {
                        let mut w = bevy_ecs::world::World::new();
                        let es: Vec<_> = (0..n).map(|_| w.spawn(Data(0.0)).id()).collect();
                        (w, es)
                    },
                    |(w, es)| {
                        for &e in es.iter() {
                            w.entity_mut(e).insert(Tag(0.0));
                        }
                        for &e in es.iter() {
                            w.entity_mut(e).remove::<Tag>();
                        }
                    },
                ),
                measure(
                    30,
                    || {
                        let w = World::new();
                        let es: Vec<_> = (0..n).map(|_| w.entity().set(Data(0.0)).id()).collect();
                        (w, es)
                    },
                    |(w, es)| {
                        for &e in es.iter() {
                            w.entity_from_id(e).set(Tag(0.0));
                        }
                        for &e in es.iter() {
                            w.entity_from_id(e).remove(Tag::id());
                        }
                    },
                ),
            ],
        });
    }

    // 5. Random access: read Position of 10 000 entities in shuffled order (of 100 000).
    {
        let n = 100_000;
        let order = shuffled(n);
        let (pw, pe) = proto_world(n);
        let (hw, he) = hecs_world(n);
        let (bw, be) = bevy_world(n);
        let (fw, fe) = flecs_world(n);
        rows.push(Row {
            name: "random get of 10k (of 100k)",
            values: [
                measure(
                    100,
                    || (),
                    |_| {
                        let mut s = 0.0;
                        for &i in &order[..10_000] {
                            s += pw.get::<Position>(pe[i]).unwrap().0[0];
                        }
                        black_box(s);
                    },
                ),
                measure(
                    100,
                    || (),
                    |_| {
                        let mut s = 0.0;
                        for &i in &order[..10_000] {
                            s += hw.get::<&Position>(he[i]).unwrap().0[0];
                        }
                        black_box(s);
                    },
                ),
                measure(
                    100,
                    || (),
                    |_| {
                        let mut s = 0.0;
                        for &i in &order[..10_000] {
                            s += bw.get::<Position>(be[i]).unwrap().0[0];
                        }
                        black_box(s);
                    },
                ),
                measure(
                    100,
                    || (),
                    |_| {
                        let mut s = 0.0;
                        for &i in &order[..10_000] {
                            fw.entity_from_id(fe[i]).get::<&Position>(|p| s += p.0[0]);
                        }
                        black_box(s);
                    },
                ),
            ],
        });
    }

    println!("### ECS comparison (µs, median; lower is better)\n");
    println!("| scenario | own prototype | hecs 0.11.1 | bevy_ecs 0.18.1 | flecs_ecs 0.2.2 |");
    println!("|---|---:|---:|---:|---:|");
    for row in &rows {
        println!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} |",
            row.name, row.values[0], row.values[1], row.values[2], row.values[3]
        );
    }
    println!();
}
