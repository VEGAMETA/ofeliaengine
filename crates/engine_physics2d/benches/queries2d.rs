//! 2D queries and the character mover on Box2D (ADR 0066): casts per microsecond over a
//! settled pile, and the cost of a mover move. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_physics2d --bench queries2d
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec2;
use engine_physics2d::{BodyKind, Material, Mover2d, QueryFilter2d, Shape, World2d};
use std::hint::black_box;
use std::time::Instant;

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

fn main() {
    // 1 000 boxes and circles settled on a 100 m ground.
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let ground = w.create_body(BodyKind::Static, Vec2::new(0.0, -1.0), 0.0);
    w.add_shape(
        ground,
        Shape::Box {
            half_extents: Vec2::new(50.0, 1.0),
        },
        Material::default(),
    )
    .unwrap();
    for i in 0..1_000 {
        let b = w.create_body(
            BodyKind::Dynamic,
            Vec2::new((i % 50) as f32 * 1.5 - 37.5, 1.0 + (i / 50) as f32 * 1.2),
            i as f32 * 0.3,
        );
        let shape = if i % 2 == 0 {
            Shape::Box {
                half_extents: Vec2::splat(0.4),
            }
        } else {
            Shape::Circle { radius: 0.4 }
        };
        w.add_shape(b, shape, Material::default()).unwrap();
    }
    for _ in 0..600 {
        w.step(1.0 / 60.0, 4);
    }
    let n = 4096;
    let origins: Vec<Vec2> = (0..n)
        .map(|i| Vec2::new((i as f32 + 0.5) / n as f32 * 100.0 - 50.0, 50.0))
        .collect();
    let down = Vec2::new(0.0, -100.0);
    let all = QueryFilter2d::default();
    println!("### 2D queries over a settled pile of 1 000 bodies (Box2D 3.1.1, one thread)\n");
    println!("| query | µs per query | queries per µs |");
    println!("|---|---:|---:|");
    let row = |name: &str, us: f64| {
        println!("| {name} | {:.3} | {:.2} |", us / n as f64, n as f64 / us);
    };
    let t = median_us(30, || {
        for &o in &origins {
            black_box(w.raycast(o, down, all));
        }
    });
    row("ray, closest", t);
    let mut hits = Vec::with_capacity(64);
    let t = median_us(30, || {
        for &o in &origins {
            hits.clear();
            w.raycast_all(o, down, all, &mut hits);
        }
    });
    row("ray, every hit", t);
    for (name, shape) in [
        ("circle cast, closest", Shape::Circle { radius: 0.3 }),
        (
            "capsule cast, closest",
            Shape::Capsule {
                half_length: 0.5,
                radius: 0.3,
            },
        ),
        (
            "box cast, closest",
            Shape::Box {
                half_extents: Vec2::splat(0.3),
            },
        ),
    ] {
        let t = median_us(30, || {
            for &o in &origins {
                black_box(w.cast_shape(shape, o, 0.3, down, all));
            }
        });
        row(name, t);
    }
    let mut bodies = Vec::with_capacity(64);
    let t = median_us(30, || {
        for &o in &origins {
            bodies.clear();
            w.overlap_shape(
                Shape::Circle { radius: 1.0 },
                Vec2::new(o.x, 1.0),
                0.0,
                all,
                &mut bodies,
            );
        }
    });
    row("circle overlap at y = 1", t);

    // A mover walking back and forth over the ground and into the pile.
    let mut mover = Mover2d::new(Vec2::new(-49.0, 0.0), 1.8, 0.3);
    let mut times = Vec::new();
    for i in 0..1_200 {
        let walk = if (i / 300) % 2 == 0 { 4.0 } else { -4.0 };
        let vy = if mover.ground.is_some() {
            0.0
        } else {
            mover.velocity.y
        } - 10.0 / 60.0;
        let start = Instant::now();
        w.move_mover(&mut mover, Vec2::new(walk, vy), 1.0 / 60.0);
        times.push(start.elapsed().as_secs_f64() * 1e6);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "\nMover move: median {:.2} µs, p99 {:.2} µs\n",
        times[times.len() / 2],
        times[times.len() * 99 / 100]
    );
}
