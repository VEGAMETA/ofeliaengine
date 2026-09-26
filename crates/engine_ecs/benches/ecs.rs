//! `engine_ecs` costs: structural changes, iteration (sequential, per chunk, parallel),
//! random access, commands, and the rollback primitives (save, restore, state hash).
//! Single thread unless noted, median of repeated runs. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_ecs --bench ecs
//! ```
//!
//! The comparison with other ECS libraries is `bench/ecs-compare` (ADR 0017).

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_ecs::{
    Commands, Entity, QueryState, Schedule, Snapshot, World, bytemuck, pod_component, system,
};
use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Transform([f32; 16]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Position([f32; 3]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Rotation([f32; 3]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Velocity([f32; 3]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Tag(f32);
pod_component!(Transform, Position, Rotation, Velocity, Tag);

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

fn world(n: usize) -> (World, Vec<Entity>) {
    let mut w = World::new();
    let es = (0..n).map(|_| w.spawn(bundle())).collect();
    (w, es)
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

fn main() {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let pool = engine_core::jobs::ThreadPool::builder()
        .workers(threads - 1)
        .build()
        .unwrap();
    let mut rows: Vec<(String, f64)> = Vec::new();

    let n = 10_000;
    rows.push((
        "spawn 10k × 4 components, new world".into(),
        measure(50, || (), |_| drop(black_box(world(n)))),
    ));
    rows.push((
        "spawn_batch 10k × 4 components".into(),
        measure(
            50,
            || (),
            |_| {
                let mut w = World::new();
                w.spawn_batch((0..n).map(|_| bundle()));
                black_box(&w);
            },
        ),
    ));
    {
        // Steady state: the world's chunk pool already holds the memory.
        let (mut w, _) = world(n);
        let mut samples: Vec<f64> = (0..50)
            .map(|_| {
                w.clear();
                let start = Instant::now();
                for _ in 0..n {
                    w.spawn(bundle());
                }
                start.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        rows.push((
            "spawn 10k × 4 components, pooled chunks".into(),
            samples[samples.len() / 2],
        ));
    }
    rows.push((
        "despawn 10k (of 10k)".into(),
        measure(
            50,
            || world(n),
            |(w, es)| {
                for &e in es.iter() {
                    w.despawn(e);
                }
            },
        ),
    ));

    for n in [10_000usize, 1_000_000] {
        let runs = if n > 100_000 { 30 } else { 300 };
        let label = if n == 10_000 { "10k" } else { "1M" };
        let (mut w, _) = world(n);
        let mut q = QueryState::<(&mut Position, &Velocity)>::new(&mut w);
        rows.push((
            format!("iterate {label}: position += velocity"),
            measure(runs, || (), |_| q.for_each(&mut w, |(p, v)| add(p, v))),
        ));
        rows.push((
            format!("iterate {label}, per chunk arrays"),
            measure(
                runs,
                || (),
                |_| {
                    q.for_each_chunk(&mut w, |(ps, vs)| {
                        for (p, v) in ps.iter_mut().zip(vs) {
                            add(p, v);
                        }
                    });
                },
            ),
        ));
        rows.push((
            format!("iterate {label}, iterator"),
            measure(
                runs,
                || (),
                |_| {
                    for (p, v) in q.iter(&mut w) {
                        add(p, v);
                    }
                },
            ),
        ));
        rows.push((
            format!("iterate {label}, parallel ({threads} threads)"),
            measure(
                runs,
                || (),
                |_| q.par_for_each(&mut w, &pool, |(p, v)| add(p, v)),
            ),
        ));
    }

    {
        let n = 10_000;
        rows.push((
            "add + remove a component, 10k entities".into(),
            measure(
                30,
                || {
                    let mut w = World::new();
                    let es: Vec<_> = (0..n).map(|_| w.spawn((Tag(0.0),))).collect();
                    (w, es)
                },
                |(w, es)| {
                    for &e in es.iter() {
                        w.insert(e, Position([0.0; 3]));
                    }
                    for &e in es.iter() {
                        w.remove::<Position>(e);
                    }
                },
            ),
        ));
        rows.push((
            "commands: 10k spawns + 10k inserts, apply".into(),
            measure(
                30,
                || (World::new(), Commands::new()),
                |(w, c)| {
                    for _ in 0..n {
                        let e = c.spawn(bundle());
                        c.insert(e, Tag(1.0));
                    }
                    w.apply(c);
                },
            ),
        ));
    }

    {
        let n = 100_000;
        let order = shuffled(n);
        let (w, es) = world(n);
        rows.push((
            "random get of 10k (of 100k)".into(),
            measure(
                100,
                || (),
                |_| {
                    let mut s = 0.0;
                    for &i in &order[..10_000] {
                        s += w.get::<Position>(es[i]).unwrap().0[0];
                    }
                    black_box(s);
                },
            ),
        ));
    }

    {
        let n = 100_000;
        let (mut w, _) = world(n);
        let mut snap = Snapshot::new();
        w.save(&mut snap).unwrap();
        let bytes = snap.byte_len();
        rows.push((
            format!("save 100k entities ({:.1} MB)", bytes as f64 / 1e6),
            measure(50, || (), |_| w.save(&mut snap).unwrap()),
        ));
        rows.push((
            "restore 100k entities".into(),
            measure(50, || (), |_| w.restore(&snap).unwrap()),
        ));
        rows.push((
            "state hash of 100k entities".into(),
            measure(
                50,
                || (),
                |_| {
                    black_box(w.state_hash());
                },
            ),
        ));
    }

    {
        // Three systems over 1M entities with disjoint writes: one level, so the pool
        // runs them side by side (each system itself is sequential).
        let (mut w, _) = world(1_000_000);
        let mut s = Schedule::new();
        s.add(system(
            "position",
            |w, a| a.query::<(&mut Position, &Velocity), ()>(w),
            |q, ctx| ctx.for_each(q, |(p, v)| add(p, v)),
        ));
        s.add(system(
            "rotation",
            |w, a| a.query::<(&mut Rotation, &Velocity), ()>(w),
            |q, ctx| {
                ctx.for_each(q, |(r, v)| {
                    for i in 0..3 {
                        r.0[i] += v.0[i];
                    }
                });
            },
        ));
        s.add(system(
            "transform",
            |w, a| a.query::<(&mut Transform, &Velocity), ()>(w),
            |q, ctx| {
                ctx.for_each(q, |(t, v)| {
                    for i in 0..3 {
                        t.0[12 + i] += v.0[i];
                    }
                });
            },
        ));
        s.initialize(&mut w);
        assert_eq!(s.levels().len(), 1);
        rows.push((
            "schedule: 3 independent systems × 1M, serial".into(),
            measure(30, || (), |_| s.run(&mut w, None)),
        ));
        rows.push((
            format!("schedule: 3 independent systems × 1M, pool ({threads} threads)"),
            measure(30, || (), |_| s.run(&mut w, Some(&pool))),
        ));
    }
    {
        // Scheduling overhead: 16 systems whose queries match nothing.
        let mut w = World::new();
        w.spawn((Tag(0.0),));
        let mut s = Schedule::new();
        for _ in 0..16 {
            s.add(system(
                "empty",
                |w, a| a.query::<&Velocity, ()>(w),
                |q, ctx| {
                    ctx.for_each(q, |v| {
                        black_box(v);
                    })
                },
            ));
        }
        s.initialize(&mut w);
        rows.push((
            "schedule overhead: 16 empty systems, serial".into(),
            measure(300, || (), |_| s.run(&mut w, None)),
        ));
        rows.push((
            format!("schedule overhead: 16 empty systems, pool ({threads} threads)"),
            measure(300, || (), |_| s.run(&mut w, Some(&pool))),
        ));
    }

    println!("### engine_ecs (µs, median; lower is better)\n");
    println!("| scenario | µs |");
    println!("|---|---:|");
    for (name, us) in &rows {
        println!("| {name} | {us:.1} |");
    }
    println!();
}
