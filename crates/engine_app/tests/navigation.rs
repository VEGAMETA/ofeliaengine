//! A navigation mesh of a real level (ADR 0103): Sponza's level colliders, as the cook
//! makes them, baked for a human-sized agent, then paths through its courtyard. Skipped
//! when the test assets are missing (`tools/fetch-test-assets.ps1`).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::colliders::cook_colliders;
use engine_assets::import_gltf;
use engine_assets::navigation::level_triangles;
use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use engine_nav::{BakeSettings, NavMesh, PathStatus, bake};
use std::path::Path;
use std::time::Instant;

#[test]
fn sponza_bakes_and_paths_cross_its_courtyard() {
    let gltf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Sponza/Sponza.gltf");
    let Ok(bytes) = std::fs::read(&gltf) else {
        println!("skipped: {} missing", gltf.display());
        return;
    };
    let scene = import_gltf(&bytes, gltf.parent()).unwrap();
    let instances: Vec<(u32, u32)> = (0..scene.nodes.len() as u32).map(|i| (i, 1)).collect();
    let (colliders, bodies, _) =
        cook_colliders(&scene, &instances, &scene.collider_rules, None, None);
    // The level's triangles in the world, as the cook gathers them.
    let (vertices, triangles) = level_triangles(&colliders, &bodies);
    let (lo, hi) = vertices.iter().fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(lo, hi), &v| (lo.min(v), hi.max(v)),
    );
    println!("Sponza: {} triangles, {lo} to {hi}", triangles.len());
    let jobs = ThreadPool::builder().build().unwrap();
    let start = Instant::now();
    let data = bake(&vertices, &triangles, &BakeSettings::default(), Some(&jobs)).unwrap();
    let bake_ms = start.elapsed().as_secs_f64() * 1e3;
    let bytes: usize = data.tiles.iter().map(|t| t.bytes.len()).sum();
    let start = Instant::now();
    let mut nav = NavMesh::new(&data).unwrap();
    let load_ms = start.elapsed().as_secs_f64() * 1e3;
    println!(
        "baked {} tiles ({:.1} KiB) in {bake_ms:.0} ms; loaded in {load_ms:.2} ms",
        data.tiles.len(),
        bytes as f64 / 1024.0
    );
    assert!(!data.tiles.is_empty());
    // Along the courtyard's length (Sponza's floor at y = 0, its long axis X), between the
    // two ends: a complete path, longer than the straight line by little.
    let extent = Vec3::new(1.0, 2.0, 1.0);
    let floor = |x: f32| nav.nearest(Vec3::new(x, 0.5, 0.0), extent);
    let (from, to) = (floor(lo.x * 0.6).unwrap(), floor(hi.x * 0.6).unwrap());
    let mut path = Vec::new();
    let start = Instant::now();
    let status = nav.path(from, to, extent, &mut path);
    let path_us = start.elapsed().as_secs_f64() * 1e6;
    let walked: f32 = path.windows(2).map(|w| w[0].distance(w[1])).sum();
    println!(
        "path {from} to {to}: {status:?}, {} corners, {walked:.2} m (straight {:.2}) in {path_us:.0} µs",
        path.len(),
        from.distance(to)
    );
    assert_eq!(status, PathStatus::Complete);
    assert!(walked >= from.distance(to) - 1e-3 && walked < 1.3 * from.distance(to));
    // Every corner on the floor's level, none through the walls (within the mesh's bounds).
    assert!(
        path.iter()
            .all(|p| p.y.abs() < 0.5 && p.x >= lo.x && p.x <= hi.x)
    );
}
