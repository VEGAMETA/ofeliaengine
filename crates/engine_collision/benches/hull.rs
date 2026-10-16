//! Convex hulls and fitting (ADR 0101): what the cook pays per mesh and the runtime per
//! rebuilt shape. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_collision --bench hull
//! ```

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_collision::fit;
use engine_collision::hull::{Hull, HullSettings};
use engine_collision::shape::Shape;
use engine_core::math::{Affine3A, Vec3};
use std::hint::black_box;
use std::time::Instant;

struct Random(u64);

impl Random {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }

    fn in_ball(&mut self) -> Vec3 {
        loop {
            let p = Vec3::new(self.next(), self.next(), self.next()) * 2.0 - 1.0;
            if p.length_squared() <= 1.0 {
                return p;
            }
        }
    }
}

/// Median of `runs` timings, in microseconds.
fn median_us<R>(runs: usize, mut f: impl FnMut() -> R) -> f64 {
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            black_box(f());
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let mut random = Random(0x5eed);
    println!("| work | points | hull vertices | median |");
    println!("|---|---:|---:|---:|");
    for (name, count) in [
        ("ball", 1_000),
        ("ball", 10_000),
        ("ball", 100_000),
        ("sphere", 1_000),
        ("sphere", 10_000),
    ] {
        let points: Vec<Vec3> = (0..count)
            .map(|_| {
                let p = random.in_ball();
                if name == "sphere" { p.normalize() } else { p }
            })
            .collect();
        let hull = Hull::new(&points).unwrap();
        let t = median_us(9, || Hull::new(&points).unwrap());
        println!(
            "| hull of a {name} | {count} | {} | {t:.0} µs |",
            hull.points.len()
        );
        let settings = HullSettings {
            max_vertices: 64,
            ..HullSettings::default()
        };
        let limited = Hull::with_settings(&points, &settings).unwrap();
        let t = median_us(9, || Hull::with_settings(&points, &settings).unwrap());
        println!(
            "| hull of a {name}, at most 64 vertices (error {:.4}) | {count} | {} | {t:.0} µs |",
            limited.error,
            limited.points.len()
        );
        let t = median_us(9, || fit::smallest(&limited, fit::Kinds::ALL));
        println!("| primitives around the 64-vertex hull | 64 | | {t:.0} µs |");
    }
    let sphere = Shape::Sphere {
        center: Vec3::ZERO,
        radius: 0.5,
    };
    let squash = Affine3A::from_scale(Vec3::new(2.0, 1.0, 0.5));
    let t = median_us(99, || sphere.transformed(squash, 0.005));
    println!("| a sphere squashed into a hull (0.5 % of its size) | | | {t:.1} µs |");
}
