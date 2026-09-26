//! Physics benchmark suite (CLAUDE.md §8) on the Jolt backend: the shared scenes of
//! `engine_physics::scenes`, stepped at 60 Hz. Reports step times (median, p99, max) on
//! the calling thread and with worker threads, and batched raycasts. Our own solver will
//! run the same scenes. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_physics_jolt --bench physics
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_physics::{PhysicsBackend, PhysicsSettings, QueryFilter, scenes};
use engine_physics_jolt::JoltBackend;
use std::time::Instant;

const DT: f32 = 1.0 / 60.0;

fn world(threads: u32) -> JoltBackend {
    JoltBackend::new(&PhysicsSettings {
        threads,
        ..PhysicsSettings::default()
    })
    .unwrap()
}

/// Step times in ms: median, p99, max.
fn step_times(w: &mut JoltBackend, steps: usize) -> (f64, f64, f64) {
    let mut times: Vec<f64> = (0..steps)
        .map(|_| {
            let start = Instant::now();
            w.step(DT, 1).unwrap();
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    times.sort_by(f64::total_cmp);
    (
        times[times.len() / 2],
        times[(times.len() * 99 / 100).min(times.len() - 1)],
        times[times.len() - 1],
    )
}

type Scene = fn(&mut dyn PhysicsBackend) -> usize;

fn main() {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get() as u32)
        .saturating_sub(1)
        .max(1);
    println!("### Physics (Jolt 5.6.0 via JoltC), 60 Hz, 1 collision step\n");
    println!("| scene | bodies | threads | median ms | p99 ms | max ms |");
    println!("|---|---:|---:|---:|---:|---:|");
    let scenes: [(&str, Scene, usize); 5] = [
        (
            "pyramid, 20 levels",
            |w| scenes::pyramid(w, 20).unwrap().len(),
            300,
        ),
        (
            "pile of 1 000 mixed shapes",
            |w| scenes::pile(w, 1_000).unwrap().len(),
            300,
        ),
        (
            "pile of 5 000 mixed shapes",
            |w| scenes::pile(w, 5_000).unwrap().len(),
            300,
        ),
        (
            "chain of 200 links (joints)",
            |w| scenes::chain(w, 200).unwrap().len(),
            300,
        ),
        (
            "200 orbits in a point gravity field",
            |w| scenes::orbits(w, 200).unwrap().len(),
            300,
        ),
    ];
    for (name, build, steps) in scenes {
        for threads in [0, workers] {
            let mut w = world(threads);
            let bodies = build(&mut w);
            let (median, p99, max) = step_times(&mut w, steps);
            println!(
                "| {name} | {bodies} | {} | {median:.3} | {p99:.3} | {max:.3} |",
                if threads == 0 { 1 } else { threads + 1 }
            );
        }
    }

    // Batched raycasts over a settled pile.
    let mut w = world(0);
    scenes::pile(&mut w, 1_000).unwrap();
    for _ in 0..240 {
        w.step(DT, 1).unwrap();
    }
    let rays = scenes::ray_grid(64);
    let mut hits = vec![None; rays.len()];
    let mut times: Vec<f64> = (0..50)
        .map(|_| {
            let start = Instant::now();
            w.raycast_batch(&rays, QueryFilter::default(), &mut hits);
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let hit_count = hits.iter().filter(|h| h.is_some()).count();
    println!(
        "\nBatched raycasts: {} rays over a settled pile of 1 000, {hit_count} hits: median {:.0} µs ({:.2} µs per ray, one thread)\n",
        rays.len(),
        times[times.len() / 2],
        times[times.len() / 2] / rays.len() as f64
    );
}
