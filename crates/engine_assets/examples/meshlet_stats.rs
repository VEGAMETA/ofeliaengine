//! Meshlets of glTF scenes as the cook makes them (ADR 0085): per scene, the meshlets and
//! how full they are, the time to make them, and the post-transform vertex cache of each
//! level's triangles in their order before (the file's for the full level, the vertex cache
//! optimizer's for the detail levels), in the vertex cache optimizer's order, and meshlet by
//! meshlet. The cache figures are meshoptimizer's models (vertices transformed per
//! triangle: a 16-entry FIFO, an NVIDIA-like 32/32/32 and an AMD-like 14/64/128), not
//! measurements. Prints Markdown.
//!
//! `cargo run -p engine_assets --release --example meshlet_stats [-- <SCENE.gltf>...]`
//! (the test scenes from `ENGINE_TEST_ASSETS` by default).

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::meshlet::{MAX_TRIANGLES, MAX_VERTICES, build_meshlets};
use engine_assets::{LodSettings, generate_lods, import_gltf};
use std::path::PathBuf;
use std::time::Instant;

/// Cache models: (cache size, warp size, primitive group size).
const MODELS: [(u32, u32, u32); 3] = [(16, 0, 0), (32, 32, 32), (14, 64, 128)];

#[derive(Default)]
struct Totals {
    triangles: usize,
    meshlets: usize,
    meshlet_vertices: usize,
    /// Vertices transformed per model: in the order given, the cache-optimized order and
    /// the meshlets' order.
    given: [f64; 3],
    optimized: [f64; 3],
    meshlets_order: [f64; 3],
}

fn transformed(indices: &[u32], vertices: usize, into: &mut [f64; 3]) {
    for (k, &(cache, warp, group)) in MODELS.iter().enumerate() {
        into[k] += f64::from(
            meshopt::analyze_vertex_cache(indices, vertices, cache, warp, group)
                .vertices_transformed,
        );
    }
}

impl Totals {
    fn add(&mut self, positions: &[[f32; 3]], indices: &[u32]) -> f64 {
        let n = positions.len();
        transformed(indices, n, &mut self.given);
        let optimized = meshopt::optimize_vertex_cache(indices, n);
        transformed(&optimized, n, &mut self.optimized);
        let mut reordered = indices.to_vec();
        let start = Instant::now();
        let meshlets = build_meshlets(positions, &mut reordered);
        let seconds = start.elapsed().as_secs_f64();
        transformed(&reordered, n, &mut self.meshlets_order);
        self.triangles += indices.len() / 3;
        self.meshlets += meshlets.len();
        for m in &meshlets {
            let range = m.first_index as usize..(m.first_index + m.triangles * 3) as usize;
            let mut vertices = reordered[range].to_vec();
            vertices.sort_unstable();
            vertices.dedup();
            self.meshlet_vertices += vertices.len();
        }
        seconds
    }

    fn row(&self, name: &str, level: &str, seconds: f64) {
        let t = self.triangles.max(1) as f64;
        let m = self.meshlets.max(1) as f64;
        let cache: Vec<String> = (0..3)
            .map(|k| {
                format!(
                    "{:.3} / {:.3} / {:.3}",
                    self.given[k] / t,
                    self.optimized[k] / t,
                    self.meshlets_order[k] / t
                )
            })
            .collect();
        println!(
            "| {name} | {level} | {} | {} | {:.0} % | {:.0} % | {} | {} | {} | {:.1} |",
            self.triangles,
            self.meshlets,
            100.0 * t / (m * MAX_TRIANGLES as f64),
            100.0 * self.meshlet_vertices as f64 / (m * MAX_VERTICES as f64),
            cache[0],
            cache[1],
            cache[2],
            seconds * 1e3,
        );
    }
}

fn main() {
    let assets = std::env::var_os("ENGINE_TEST_ASSETS")
        .map_or_else(|| PathBuf::from("test-assets"), PathBuf::from);
    let mut scenes: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if scenes.is_empty() {
        scenes = [
            "Sponza/Sponza.gltf",
            "FlightHelmet/FlightHelmet.gltf",
            "ClearcoatWicker/ClearcoatWicker.gltf",
            "MetalRoughSpheres/MetalRoughSpheres.gltf",
        ]
        .iter()
        .map(|s| assets.join(s))
        .collect();
    }
    println!(
        "| scene | levels | triangles | meshlets | triangles full | vertices full | FIFO 16: given / optimized / meshlets | NV-like 32/32/32 | AMD-like 14/64/128 | build ms |"
    );
    println!("|---|---|---:|---:|---:|---:|---|---|---|---:|");
    for path in scenes {
        let Ok(bytes) = std::fs::read(&path) else {
            println!("{}: missing", path.display());
            continue;
        };
        let scene = import_gltf(&bytes, path.parent()).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let (mut full, mut lods) = (Totals::default(), Totals::default());
        let (mut full_seconds, mut lod_seconds) = (0.0, 0.0);
        for p in scene.meshes.iter().flat_map(|m| &m.primitives) {
            // Skinned meshes get no meshlets.
            if !p.joints.is_empty() {
                continue;
            }
            full_seconds += full.add(&p.positions, &p.indices);
            for l in generate_lods(&p.positions, &p.indices, &LodSettings::default()) {
                lod_seconds += lods.add(&p.positions, &l.indices);
            }
        }
        full.row(&name, "full", full_seconds);
        lods.row(&name, "detail", lod_seconds);
    }
}
