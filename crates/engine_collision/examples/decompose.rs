//! The convex decomposition on OBJ files (ADR 0101, 8c-3): each one's hulls written as an OBJ
//! with an object per hull, and a Markdown row of what it took. The research note's
//! comparison (`docs/research/mesh-colliders.md`) judges the output.
//!
//! `cargo run -p engine_collision --release --example decompose -- <in.obj> <out.obj> [threshold]`

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used
)]

use engine_collision::decompose::{DecomposeSettings, decompose};
use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use std::fmt::Write as _;
use std::time::Instant;

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, output) = (&args[0], &args[1]);
    let threshold: f64 = args.get(2).map_or(0.05, |t| t.parse().unwrap());
    let (vertices, triangles) = read_obj(input);
    let jobs = ThreadPool::builder().build().unwrap();
    let settings = DecomposeSettings {
        threshold,
        ..DecomposeSettings::default()
    };
    let start = Instant::now();
    let (hulls, stats) = decompose(&vertices, &triangles, &settings, Some(&jobs));
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let mut obj = String::new();
    let mut base = 1;
    for (k, hull) in hulls.iter().enumerate() {
        let h = engine_collision::Hull::new(hull).unwrap();
        writeln!(obj, "o hull{k}").unwrap();
        for p in &h.points {
            writeln!(obj, "v {} {} {}", p.x, p.y, p.z).unwrap();
        }
        for t in &h.triangles {
            writeln!(obj, "f {} {} {}", t[0] + base, t[1] + base, t[2] + base).unwrap();
        }
        base += h.points.len() as u32;
    }
    std::fs::write(output, obj).unwrap();
    let points: usize = hulls.iter().map(Vec::len).sum();
    let name = std::path::Path::new(input)
        .file_stem()
        .unwrap()
        .to_string_lossy();
    println!(
        "| {name} | {} | Ours {threshold} | {} | {points} | {ms:.1} | remeshed {} | search {} triangles, {} cuts, {} merged; prepare {:.2} s, cut {:.2} s, merge {:.2} s |",
        triangles.len(),
        hulls.len(),
        stats.remeshed,
        stats.search_triangles,
        stats.cuts,
        stats.merged,
        stats.prepare_seconds,
        stats.cut_seconds,
        stats.merge_seconds,
    );
}
