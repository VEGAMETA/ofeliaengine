//! The 2D scene of [`Source::Sprites`] (ADR 0125), drawn by an offscreen player: a field of
//! tiles, characters of three parts in sort groups on a layer sorted by y (shadows half
//! transparent under them, some facing left), a turned and tinted orb drawn with linear
//! filtering, and a sprite stretched to a size of its own; a pixel-art camera rendering
//! 320 × 180 pixels shown at the output's size by whole numbers, its colours as painted.
//! Its images are painted here, so the shot needs no test assets.

use crate::scene::{RenderOptions, Rendered, Shot, Skip, median};
use engine_app::{
    FrameParts, Game, Player, PlayerSettings, SortGroup, SortingFile, SortingLayer, Sprite, Window,
};
use engine_core::math::{Quat, Vec2, Vec3};
use engine_ecs::{GlobalTransform, Transform, World};
use engine_render::png::{PngFormat, encode_png};
use engine_runtime::Camera;
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef, SpriteFilter, SpriteImport};
use std::collections::BTreeMap;
use std::path::Path;

/// The pixel-art resolution.
const REFERENCE: [u32; 2] = [320, 180];

/// Nothing moves.
struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
    fn animate(&mut self, _parts: &mut FrameParts<'_>, _dt: f32) {}
}

/// A small hash of a pixel, for painted noise that is the same on every run.
fn noise(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b) ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h
}

/// The images: each one's file, pixels and sprite settings.
fn images() -> Vec<(&'static str, u32, u32, Vec<u8>, SpriteImport)> {
    let paint = |w: u32, h: u32, f: &dyn Fn(u32, u32) -> [u8; 4]| -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .flat_map(|(x, y)| f(x, y))
            .collect()
    };
    let feet = SpriteImport {
        pivot: [0.5, 0.0],
        ..SpriteImport::default()
    };
    // Grass: two greens and a few flowers.
    let grass = paint(16, 16, &|x, y| {
        let n = noise(x, y, 7);
        match n % 23 {
            0 => [236, 214, 92, 255],
            1..=8 => [72, 140, 64, 255],
            _ => [92, 164, 76, 255],
        }
    });
    // A character from the front: hair, face, shirt, trousers, boots; a pixel of outline.
    let hero = paint(16, 24, &|x, y| {
        let inside = (3..13).contains(&x);
        let color: [u8; 4] = match y {
            0..=3 if (4..12).contains(&x) => [92, 58, 38, 255],
            4..=9 if (4..12).contains(&x) => {
                if y == 6 && (x == 6 || x == 9) {
                    [20, 20, 30, 255]
                } else {
                    [236, 188, 150, 255]
                }
            }
            10..=16 if inside => [60, 96, 200, 255],
            17..=21 if (4..7).contains(&x) || (9..12).contains(&x) => [70, 60, 52, 255],
            22..=23 if (3..7).contains(&x) || (9..13).contains(&x) => [36, 30, 28, 255],
            _ => [0, 0, 0, 0],
        };
        // Its left side lit: facing matters when flipped.
        if color[3] > 0 && x < 7 {
            [
                color[0].saturating_add(30),
                color[1].saturating_add(30),
                color[2].saturating_add(30),
                255,
            ]
        } else {
            color
        }
    });
    let sword = paint(8, 16, &|x, y| match (x, y) {
        (3..=4, 0..=10) => [210, 220, 232, 255],
        (1..=6, 11) => [140, 96, 40, 255],
        (3..=4, 12..=15) => [96, 60, 30, 255],
        _ => [0, 0, 0, 0],
    });
    // A half transparent ellipse.
    let shadow = paint(16, 6, &|x, y| {
        let (dx, dy) = ((x as f32 + 0.5 - 8.0) / 8.0, (y as f32 + 0.5 - 3.0) / 3.0);
        if dx * dx + dy * dy <= 1.0 {
            [10, 10, 20, 128]
        } else {
            [0, 0, 0, 0]
        }
    });
    // A glowing orb: a radial ramp, its edge fading out.
    let orb = paint(32, 32, &|x, y| {
        let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
        let r = (dx * dx + dy * dy).sqrt() / 16.0;
        let a = (1.0 - r).clamp(0.0, 1.0);
        let v = (255.0 * (1.0 - 0.6 * r)).clamp(0.0, 255.0) as u8;
        [v, v, 255, (255.0 * a.sqrt()) as u8]
    });
    // A sign: a framed board with stripes, stretched.
    let sign = paint(8, 8, &|x, y| {
        if x == 0 || y == 0 || x == 7 || y == 7 {
            [80, 50, 30, 255]
        } else if (x + y) % 3 == 0 {
            [230, 200, 140, 255]
        } else {
            [200, 160, 100, 255]
        }
    });
    vec![
        ("grass.png", 16, 16, grass, SpriteImport::default()),
        ("hero.png", 16, 24, hero, feet.clone()),
        ("sword.png", 8, 16, sword, feet.clone()),
        ("shadow.png", 16, 6, shadow, SpriteImport::default()),
        (
            "orb.png",
            32,
            32,
            orb,
            SpriteImport {
                pixels_per_unit: 32.0,
                filter: SpriteFilter::Linear,
                ..SpriteImport::default()
            },
        ),
        ("sign.png", 8, 8, sign, SpriteImport::default()),
    ]
}

/// Writes the project (the images, their `.meta`s, the sorting layers) into `dir`, and
/// returns the images' references by file name.
fn write_project(dir: &Path) -> (AssetPaths, BTreeMap<&'static str, AssetRef>) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("art")).expect("project folder");
    let mut paths = AssetPaths::new();
    let mut refs = BTreeMap::new();
    for (k, (file, w, h, rgba, sprite)) in images().into_iter().enumerate() {
        let path = format!("art/{file}");
        let png = encode_png(w, h, PngFormat::Rgba, &rgba).expect("png");
        std::fs::write(dir.join(&path), png).expect("image");
        // Fixed ids: the same scene every run.
        let id = AssetId::from_u128(0x2d00_0000_0000_0000_0000_0000_0000_0001 + k as u128);
        AssetMeta {
            sprite,
            ..AssetMeta::default()
        }
        .write(&dir.join(&path), id)
        .expect("meta");
        paths.insert(AssetRef::file(id), path);
        refs.insert(file, AssetRef::file(id));
    }
    let sorting = SortingFile {
        layers: BTreeMap::from([
            (
                -1,
                SortingLayer {
                    name: String::from("ground"),
                    y_sort: false,
                },
            ),
            (
                0,
                SortingLayer {
                    name: String::from("characters"),
                    y_sort: true,
                },
            ),
        ]),
    };
    std::fs::write(
        dir.join("main.sorting"),
        sorting.to_text(AssetId::from_u128(0x2d00_5047)),
    )
    .expect("sorting layers");
    (paths, refs)
}

/// Spawns the scene; returns how many sprites it draws.
fn spawn(world: &mut World, refs: &BTreeMap<&'static str, AssetRef>) -> u32 {
    let mut count = 0;
    let mut sprite = |world: &mut World, at: Vec3, sprite: Sprite| {
        count += 1;
        world.spawn((
            Transform::from_translation(at),
            GlobalTransform::IDENTITY,
            sprite,
        ))
    };
    // The field: 21 × 12 tiles of a metre, on the ground layer.
    for y in 0..12 {
        for x in 0..21 {
            let at = Vec3::new(x as f32 - 10.0, y as f32 - 5.5, 0.0);
            sprite(world, at, Sprite::new(refs["grass.png"]).on_layer(-1, 0));
        }
    }
    // Characters: each a group of its shadow (order −1), body (0) and sword (1), sorted
    // by its feet's y with the others; every other one faces left.
    // On the art's pixel grid (a sixteenth of a metre).
    let places = [
        (-6.0, 1.0),
        (-5.375, 0.375),
        (-2.0, -1.5),
        (1.5, 2.0),
        (2.125, 1.625),
        (6.0, -3.0),
    ];
    for (k, &(x, y)) in places.iter().enumerate() {
        let group = world.spawn((
            Transform::from_xyz(x, y, 0.0),
            GlobalTransform::IDENTITY,
            SortGroup { layer: 0, order: 0 },
        ));
        let left = k % 2 == 1;
        let side = if left { -1.0 } else { 1.0 };
        let parts = [
            (Vec3::ZERO, Sprite::new(refs["shadow.png"]).on_layer(0, -1)),
            (
                Vec3::ZERO,
                Sprite {
                    flip_x: u32::from(left),
                    ..Sprite::new(refs["hero.png"]).on_layer(0, 0)
                },
            ),
            (
                Vec3::new(0.4375 * side, 0.3125, 0.0),
                Sprite {
                    flip_x: u32::from(left),
                    ..Sprite::new(refs["sword.png"]).on_layer(0, 1)
                },
            ),
        ];
        for (offset, part) in parts {
            let e = sprite(world, offset, part);
            world
                .set_parent(e, Some(group))
                .expect("a part of a character");
        }
    }
    // The orb, turned and tinted, over the characters; a sign stretched to 3 × 1.5 m.
    let orb = Sprite::new(refs["orb.png"])
        .on_layer(1, 0)
        .with_color([1.0, 0.85, 0.6, 0.9]);
    let e = sprite(world, Vec3::new(4.5, 3.0, 0.0), orb);
    if let Some(t) = world.get_mut::<Transform>(e) {
        t.rotation = Quat::from_rotation_z(0.5);
    }
    let sign = Sprite::new(refs["sign.png"])
        .on_layer(-1, 1)
        .with_size(Vec2::new(3.0, 1.5));
    sprite(world, Vec3::new(-6.5, 3.75, 0.0), sign);
    count
}

/// Renders the 2D scene of `shot` (its name and output size) through an offscreen player.
///
/// # Panics
///
/// When a frame fails, or the sprites are not all drawn within a few seconds of frames.
pub(crate) fn render(shot: &Shot, options: RenderOptions) -> Result<Rendered, Skip> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let dir = std::env::temp_dir().join(format!(
        "engine_golden_sprites_{}_{}",
        shot.name,
        std::process::id()
    ));
    let (paths, refs) = write_project(&dir);
    let settings = PlayerSettings {
        title: format!("engine_golden {}", shot.name),
        window: Window::Offscreen {
            width: shot.size[0],
            height: shot.size[1],
        },
        project: Some(dir.clone()),
        validation: Some(options.validation),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Skip::NoGpu(e.to_string()));
        }
        Err(e) => panic!("{}: the player: {e}", shot.name),
    };
    player.set_frame_time(Some(1.0 / 60.0));
    player.assets_mut().add_paths(&paths);
    let drawn = {
        let world = player.world_mut();
        world.spawn((Transform::IDENTITY, Camera::pixel_art(REFERENCE, 16.0)));
        spawn(world, &refs)
    };
    let mut game = Still;
    let mut rgba8 = None;
    for _ in 0..600 {
        player.frame(&mut game).expect("frame");
        if player.sprites_recorded().0 == drawn {
            rgba8 = player.capture(&mut game).expect("frame");
            break;
        }
    }
    let rgba8 = rgba8.unwrap_or_else(|| {
        panic!(
            "{}: {:?} of {drawn} sprites drawn",
            shot.name,
            player.sprites_recorded()
        )
    });
    // GPU times of the frames after the capture.
    let mut timings: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    let mut frame_times = Vec::new();
    for _ in 0..options.timed_frames {
        player.frame(&mut game).expect("frame");
        let mut frame: BTreeMap<&'static str, f64> = BTreeMap::new();
        for time in player
            .graph()
            .gpu_pass_times()
            .iter()
            .filter(|t| !matches!(t.name, "offscreen frame" | "offscreen capture"))
        {
            *frame.entry(time.name).or_default() += f64::from(time.gpu_us);
        }
        for (name, us) in frame {
            timings.entry(name).or_default().push(us);
        }
        if let Some(us) = player.graph().gpu_frame_time() {
            frame_times.push(f64::from(us));
        }
    }
    let report = player.validation_report();
    assert!(
        !options.validation || report.errors == 0,
        "{}: validation errors: {:#?}",
        shot.name,
        report.first_errors
    );
    let adapter = player.device().adapter();
    let rendered = Rendered {
        adapter: adapter.summary(),
        vendor_id: adapter.vendor_id,
        width: shot.size[0],
        height: shot.size[1],
        frames: vec![rgba8.clone()],
        rgba8,
        pass_us: timings
            .into_iter()
            .map(|(name, mut v)| (name, median(&mut v)))
            .collect(),
        frame_us: median(&mut frame_times),
        startup_us: BTreeMap::new(),
        async_compute: player.graph().async_compute(),
        antialiasing: player.antialiasing(),
        gi_bytes: 0,
        gi_parts: Vec::new(),
        gi_settle: (0, 0.0),
        gi_means: Vec::new(),
        gi_image: None,
        ui_regions: Vec::new(),
    };
    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(rendered)
}
