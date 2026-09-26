//! Scene save and load cost: 10 000 entities in 100 hierarchies of 100, each with a
//! `Transform`. Median of repeated runs, single thread. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_scene --bench scene
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_ecs::{Transform, World};
use engine_scene::{SceneId, SceneRegistry, load, save};
use std::time::Instant;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let registry = SceneRegistry::with_engine_types();
    let mut world = World::new();
    for r in 0..100 {
        let root = world.spawn((SceneId::new(), Transform::from_xyz(r as f32, 0.0, 0.0)));
        for c in 0..99 {
            let child = world.spawn((
                SceneId::new(),
                Transform::from_xyz(0.0, c as f32 * 0.37, 1.5)
                    .with_scale(engine_ecs::Transform::IDENTITY.scale * 1.25),
            ));
            world.set_parent(child, Some(root)).unwrap();
        }
    }
    let mut text = String::new();
    let save_us = median(
        (0..20)
            .map(|_| {
                text.clear();
                let t = Instant::now();
                save(&mut world, &registry, &mut text).unwrap();
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect(),
    );
    let load_us = median(
        (0..20)
            .map(|_| {
                let mut w = World::new();
                let t = Instant::now();
                load(&mut w, &registry, &text).unwrap();
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect(),
    );
    println!("### engine_scene (10 000 entities, Transform, 100 × 100 hierarchy)\n");
    println!("| operation | µs | per entity |");
    println!("|---|---:|---:|");
    println!("| save | {save_us:.0} | {:.2} µs |", save_us / 1e4);
    println!("| load | {load_us:.0} | {:.2} µs |", load_us / 1e4);
    println!(
        "\nText size: {} KiB, {} lines\n",
        text.len() / 1024,
        text.lines().count()
    );
}
