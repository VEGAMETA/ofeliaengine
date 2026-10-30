//! The 2D pass measured (ADR 0125's E13-1): an offscreen player at 1920 × 1080 whose
//! orthographic camera draws `n` sprites of 16 × 16 pixels moving every tick, on one layer
//! sorted by y (the canvas sorts every frame), either rendering at the output's size (a
//! 1080p view) or at a pixel-art 640 × 360 shown three times as large.
//!
//! Reports per configuration the GPU time of the 2D pass and of the composite that scales
//! and tone-maps the view (medians over the timed frames), and the CPU time of recording
//! the sprites (reading their poses, placing them between ticks, culling, keying, sorting:
//! `FrameReport::sprites`) and of the whole frame.
//!
//! ```text
//! cargo run -p engine_app --release --example sprite_bench
//!     [--counts 1000,10000,100000]   sprites per configuration
//!     [--frames <n>]                 timed frames (default 300, after 120 to settle)
//!     [--sizes 1080,360]             render heights: 1080 (the output's) or 360
//! ENGINE_ADAPTER=radeon cargo run ...   the integrated Radeon
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_app::{
    FrameParts, Game, Player, PlayerSettings, SortingFile, SortingLayer, Sprite, Window,
};
use engine_ecs::{GlobalTransform, QueryState, Transform, World, bytemuck, pod_component};
use engine_runtime::Camera;
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef};
use std::collections::BTreeMap;

/// A sprite's walk: its velocity, metres a second.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Walk([f32; 2]);
pod_component!(Walk);

/// Moves every walker each tick, turning back at the view's edges.
struct Walkers {
    query: Option<QueryState<(&'static mut Transform, &'static mut Walk)>>,
    half: [f32; 2],
}

impl Game for Walkers {
    fn tick(&mut self, world: &mut World, dt: f32) {
        let half = self.half;
        self.query
            .get_or_insert_with(|| QueryState::new(world))
            .for_each(world, |(t, walk)| {
                for (k, (speed, edge)) in walk.0.iter_mut().zip(half).enumerate() {
                    t.translation[k] += *speed * dt;
                    if t.translation[k].abs() > edge {
                        *speed = -*speed;
                    }
                }
            });
    }
    fn animate(&mut self, _parts: &mut FrameParts<'_>, _dt: f32) {}
}

fn lcg(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32) / ((1u64 << 24) as f32)
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn arg<T: core::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn list(args: &[String], name: &str, default: &[u32]) -> Vec<u32> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.split(',').filter_map(|s| s.parse().ok()).collect())
        .unwrap_or_else(|| default.to_vec())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let counts = list(&args, "--counts", &[1_000, 10_000, 100_000]);
    let sizes = list(&args, "--sizes", &[1080, 360]);
    let frames: usize = arg(&args, "--frames").unwrap_or(300);
    // The project: four 16 × 16 images, a layer sorted by y.
    let dir = std::env::temp_dir().join(format!("ofelia-sprite-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("art")).unwrap();
    let mut paths = AssetPaths::new();
    let mut images = Vec::new();
    for (k, rgb) in [
        [220u8, 70, 50],
        [60, 180, 80],
        [70, 110, 230],
        [230, 200, 60],
    ]
    .into_iter()
    .enumerate()
    {
        let rgba: Vec<u8> = (0..256u32)
            .flat_map(|i| {
                let (x, y) = (i % 16, i / 16);
                let edge = x == 0 || y == 0 || x == 15 || y == 15;
                let a = if (x + y) % 11 == 0 { 0 } else { 255 };
                if edge {
                    [20, 20, 30, a]
                } else {
                    [rgb[0], rgb[1], rgb[2], a]
                }
            })
            .collect();
        let png =
            engine_render::png::encode_png(16, 16, engine_render::png::PngFormat::Rgba, &rgba)
                .unwrap();
        let path = format!("art/s{k}.png");
        std::fs::write(dir.join(&path), png).unwrap();
        let id = AssetId::new();
        AssetMeta::default().write(&dir.join(&path), id).unwrap();
        paths.insert(AssetRef::file(id), path);
        images.push(AssetRef::file(id));
    }
    let sorting = SortingFile {
        layers: BTreeMap::from([(
            0,
            SortingLayer {
                name: String::from("world"),
                y_sort: true,
            },
        )]),
    };
    std::fs::write(dir.join("main.sorting"), sorting.to_text(AssetId::new())).unwrap();

    let settings = PlayerSettings {
        title: String::from("sprite_bench"),
        window: Window::Offscreen {
            width: 1920,
            height: 1080,
        },
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut adapter = String::new();
    println!(
        "{:>8} {:>10} {:>12} {:>12} {:>14} {:>12}",
        "sprites", "render", "2d GPU µs", "scale µs", "record CPU µs", "frame CPU µs"
    );
    for &height in &sizes {
        for &count in &counts {
            let mut player = Player::new(&settings).unwrap();
            adapter = player.device().adapter().summary();
            player.assets_mut().add_paths(&paths);
            player.set_frame_time(Some(1.0 / 60.0));
            // The camera: 16 pixels a metre at the render size.
            let camera = if height == 1080 {
                Camera::orthographic(1080.0 / 16.0)
            } else {
                Camera::pixel_art([640, 360], 16.0)
            };
            let half = [
                (camera.height * 16.0 / 9.0) * 0.5 - 0.5,
                camera.height * 0.5 - 0.5,
            ];
            let world = player.world_mut();
            world.spawn((Transform::IDENTITY, camera));
            let mut state = 0x5eed_u64 + u64::from(count);
            for k in 0..count {
                let at = [
                    (lcg(&mut state) * 2.0 - 1.0) * half[0],
                    (lcg(&mut state) * 2.0 - 1.0) * half[1],
                ];
                world.spawn((
                    Transform::from_xyz(at[0], at[1], 0.0),
                    GlobalTransform::IDENTITY,
                    Sprite::new(images[k as usize % images.len()]),
                    Walk([(lcg(&mut state) - 0.5) * 4.0, (lcg(&mut state) - 0.5) * 4.0]),
                ));
            }
            let mut game = Walkers { query: None, half };
            // Until every sprite is drawn, then a while to settle.
            let mut settle = 0;
            while settle < 120 {
                player.frame(&mut game).unwrap();
                if player.sprites_recorded().0 + player.sprites_recorded().1 == count {
                    settle += 1;
                }
            }
            let (mut gpu_2d, mut gpu_scale, mut record, mut cpu) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..frames {
                let report = player.frame(&mut game).unwrap().unwrap();
                record.push(report.sprites);
                cpu.push(report.cpu);
                let times = player.graph().gpu_pass_times();
                let pass = |name: &str| -> f64 {
                    times
                        .iter()
                        .filter(|t| t.name == name)
                        .map(|t| f64::from(t.gpu_us))
                        .sum()
                };
                if !times.is_empty() {
                    gpu_2d.push(pass("2d"));
                    gpu_scale.push(pass("composite") + pass("tonemap"));
                }
            }
            println!(
                "{count:>8} {:>10} {:>12.1} {:>12.1} {:>14.1} {:>12.1}",
                if height == 1080 {
                    "1920x1080"
                } else {
                    "640x360"
                },
                median(&mut gpu_2d),
                median(&mut gpu_scale),
                median(&mut record),
                median(&mut cpu),
            );
        }
    }
    println!("{adapter}");
    let _ = std::fs::remove_dir_all(&dir);
}
