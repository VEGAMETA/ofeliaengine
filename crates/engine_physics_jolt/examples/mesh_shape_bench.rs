//! What collision shapes cost in Jolt (ADR 0101's measurements): a level's triangle mesh
//! as a `MeshShape` built from OBJ files (vertices and triangles as the cook stores them),
//! the same shape restored from Jolt's binary form, a static body of it and rays against
//! it; with `--hulls`, convex hulls built and restored. Prints Markdown.
//!
//! `cargo run -p engine_physics_jolt --release --example mesh_shape_bench -- <files.obj>`
//! `cargo run -p engine_physics_jolt --release --example mesh_shape_bench -- --hulls`

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::time::Instant;

use engine_core::math::Vec3;
use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, QueryFilter, Ray, ShapeDesc};
use engine_physics_jolt::JoltBackend;

fn read_obj(path: &str) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let text = std::fs::read_to_string(path).expect("an OBJ file");
    let (mut vertices, mut triangles) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                let mut p = [0.0f32; 3];
                for c in &mut p {
                    *c = parts.next().unwrap().parse().unwrap();
                }
                vertices.push(Vec3::from_array(p));
            }
            Some("f") => {
                let index: Vec<u32> = parts
                    .map(|s| s.split('/').next().unwrap().parse::<u32>().unwrap() - 1)
                    .collect();
                for k in 1..index.len() - 1 {
                    triangles.push([index[0], index[k], index[k + 1]]);
                }
            }
            _ => {}
        }
    }
    (vertices, triangles)
}

/// Convex hulls of `points` points each (on a squashed sphere, jittered): their builds
/// and restores, as a prop's decomposed collider would load.
fn hulls(points: usize) {
    let count = 1000;
    let mut world = JoltBackend::new(&PhysicsSettings::default()).unwrap();
    let mut seed = 0x9e37_79b9_u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    let sets: Vec<Vec<Vec3>> = (0..count)
        .map(|_| {
            (0..points)
                .map(|k| {
                    let a = k as f32 * 2.399_963;
                    let z = 1.0 - 2.0 * (k as f32 + 0.5) / points as f32;
                    let r = (1.0 - z * z).sqrt();
                    Vec3::new(r * a.cos(), 0.6 * z, 0.8 * r * a.sin()) * (0.9 + 0.2 * random())
                })
                .collect()
        })
        .collect();
    let t = Instant::now();
    let shapes: Vec<_> = sets
        .iter()
        .map(|p| {
            world
                .create_shape(&ShapeDesc::ConvexHull { points: p })
                .unwrap()
        })
        .collect();
    let build = t.elapsed().as_secs_f64() * 1e6 / count as f64;
    let blobs: Vec<Vec<u8>> = shapes
        .iter()
        .map(|&s| world.bake_shape(s).unwrap())
        .collect();
    let t = Instant::now();
    for b in &blobs {
        world.create_baked_shape(b, true).unwrap();
    }
    let restore = t.elapsed().as_secs_f64() * 1e6 / count as f64;
    let bytes = blobs.iter().map(Vec::len).sum::<usize>() / count;
    println!("| hull of {points} points | {build:.1} | {bytes} | {restore:.2} |");
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--hulls") {
        println!("| shape | build µs | Jolt bytes | restore µs |");
        println!("|---|---:|---:|---:|");
        for points in [8, 32, 64, 256] {
            hulls(points);
        }
        return;
    }
    println!(
        "| mesh | vertices | triangles | build ms | Jolt bytes | restore ms | rays/s (1 thread) | hits |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for path in std::env::args().skip(1) {
        let (vertices, triangles) = read_obj(&path);
        let mut world = JoltBackend::new(&PhysicsSettings::default()).unwrap();
        let t = Instant::now();
        let shape = world
            .create_shape(&ShapeDesc::TriangleMesh {
                vertices: &vertices,
                triangles: &triangles,
            })
            .unwrap();
        let build = t.elapsed().as_secs_f64() * 1e3;
        // The same shape from Jolt's binary form, as a pack could store it.
        let bytes = world.bake_shape(shape).unwrap();
        let t = Instant::now();
        let restored = world.create_baked_shape(&bytes, false).unwrap();
        let restore = t.elapsed().as_secs_f64() * 1e3;
        let shape = restored;
        world
            .create_body(&BodyDesc::fixed(shape, Vec3::ZERO))
            .unwrap();
        // Rays from the bounds' centre outward, as a game's line-of-sight checks.
        let (lo, hi) = vertices.iter().fold(
            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            |(lo, hi), v| (lo.min(*v), hi.max(*v)),
        );
        let center = (lo + hi) * 0.5;
        let reach = (hi - lo).length();
        let rays = 20_000;
        let mut hits = 0;
        let t = Instant::now();
        for k in 0..rays {
            let a = k as f32 * 2.399_963;
            let z = 1.0 - 2.0 * (k as f32 + 0.5) / rays as f32;
            let r = (1.0 - z * z).sqrt();
            let direction = Vec3::new(r * a.cos(), z, r * a.sin());
            let ray = Ray {
                origin: center,
                direction: direction * reach,
            };
            if world.raycast(&ray, QueryFilter::default()).is_some() {
                hits += 1;
            }
        }
        let rate = rays as f64 / t.elapsed().as_secs_f64();
        let name = std::path::Path::new(&path)
            .file_stem()
            .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        println!(
            "| {name} | {} | {} | {build:.1} | {} | {restore:.2} | {rate:.0} | {hits} |",
            vertices.len(),
            triangles.len(),
            bytes.len()
        );
    }
}
