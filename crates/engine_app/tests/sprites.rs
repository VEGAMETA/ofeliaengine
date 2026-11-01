//! Sprites in scenes (ADR 0125's E13-1a): an orthographic camera's view draws the 2D world
//! through the player's 2D pass, its colours exactly as painted, sorted by layer, order, y
//! and sort groups, every sprite placed between ticks; a perspective view pays nothing for
//! it. Offscreen, under synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{
    FrameParts, Game, Player, PlayerSettings, SortGroup, SortingFile, SortingLayer, Sprite, Window,
};
use engine_core::math::{Vec2, Vec3};
use engine_ecs::{Entity, GlobalTransform, QueryState, Transform, World, bytemuck, pod_component};
use engine_runtime::{Animator, Camera};
use engine_scene::{
    AssetId, AssetMeta, AssetPaths, AssetRef, SliceImport, SpriteClipImport, SpriteImport,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The output: twice the pixel-art resolution, so each pixel of art is 2 × 2 of it.
const OUTPUT: (u32, u32) = (640, 360);
const REFERENCE: [u32; 2] = [320, 180];
/// Output pixels a metre: 16 pixels of art, each two of the output's.
const SCALE: f32 = 32.0;

/// Moves the entities with a `Slide` by their speed each tick.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Slide(f32);
pod_component!(Slide);

struct Slides(Option<QueryState<(&'static mut Transform, &'static Slide)>>);

impl Game for Slides {
    fn tick(&mut self, world: &mut World, _dt: f32) {
        self.0
            .get_or_insert_with(|| QueryState::new(world))
            .for_each(world, |(t, slide)| t.translation.x += slide.0);
    }
    fn animate(&mut self, _parts: &mut FrameParts<'_>, _dt: f32) {}
}

/// An image of `width` × `height` pixels, `pixel(x, y)` each (rows from the top).
fn image(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let rgba: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .flat_map(|(x, y)| pixel(x, y))
        .collect();
    engine_render::png::encode_png(width, height, engine_render::png::PngFormat::Rgba, &rgba)
        .unwrap()
}

/// A project folder holding `images` (each with its `.meta`), and their ids.
fn project(
    name: &str,
    images: &[(&str, Vec<u8>, SpriteImport)],
) -> (PathBuf, AssetPaths, Vec<AssetId>) {
    let dir = std::env::temp_dir().join(format!("ofelia-sprites-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("art")).unwrap();
    let mut paths = AssetPaths::new();
    let mut ids = Vec::new();
    for (file, png, sprite) in images {
        let path = format!("art/{file}");
        std::fs::write(dir.join(&path), png).unwrap();
        let id = AssetId::new();
        AssetMeta {
            sprite: sprite.clone(),
            ..AssetMeta::default()
        }
        .write(&dir.join(&path), id)
        .unwrap();
        paths.insert(AssetRef::file(id), path);
        ids.push(id);
    }
    (dir, paths, ids)
}

/// An offscreen player of the project in `dir` (whose images `paths` names), or `None`
/// without a GPU unless `ENGINE_REQUIRE_GPU=1`.
fn player(dir: &Path, paths: &AssetPaths) -> Option<Player> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let settings = PlayerSettings {
        title: String::from("engine_app sprites test"),
        window: Window::Offscreen {
            width: OUTPUT.0,
            height: OUTPUT.1,
        },
        project: Some(dir.to_path_buf()),
        validation: Some(true),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            return None;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    player.assets_mut().add_paths(paths);
    player.set_frame_time(Some(1.0 / 60.0));
    Some(player)
}

/// A pixel-art camera at the origin: 320 × 180 pixels of art, 16 a metre, shown twice as
/// large.
fn camera(world: &mut World) -> Entity {
    world.spawn((Transform::IDENTITY, Camera::pixel_art(REFERENCE, 16.0)))
}

/// Frames until `drawn` sprites were recorded (their images made), then a capture.
fn capture_with(player: &mut Player, game: &mut impl Game, drawn: u32) -> Vec<u8> {
    for _ in 0..600 {
        player.frame(game).unwrap();
        if player.sprites_recorded().0 == drawn {
            return player.capture(game).unwrap().unwrap();
        }
    }
    panic!(
        "{:?} sprites recorded, {drawn} expected",
        player.sprites_recorded()
    );
}

/// The output pixel at world point `(x, y)` (the camera at the origin).
fn at(rgba: &[u8], x: f32, y: f32) -> [u8; 4] {
    let px = (OUTPUT.0 as f32 * 0.5 + x * SCALE) as u32;
    let py = (OUTPUT.1 as f32 * 0.5 - y * SCALE) as u32;
    let i = ((py * OUTPUT.0 + px) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn assert_valid(player: &Player) {
    let report = player.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}

/// Every 8-bit value of each channel reaches the screen as it was painted (ADR 0125's
/// neutral 2D view: no tone curve, no bloom, no grading, exposure 1), through the scene's
/// half-float target and integer scaling.
#[test]
fn a_2d_view_shows_sprites_exactly_as_painted() {
    // 16 × 16 pixels, every value of red once, green and blue running other ways.
    let paint = |x: u32, y: u32| {
        let i = y * 16 + x;
        [i as u8, (255 - i) as u8, ((i * 37) % 256) as u8, 255]
    };
    let (dir, paths, ids) = project(
        "exact",
        &[("ramp.png", image(16, 16, paint), SpriteImport::default())],
    );
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    // A metre square from (0, 0) to (1, 1): its pixels on the art's grid.
    world.spawn((
        Transform::from_xyz(0.5, 0.5, 0.0),
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
    ));
    let mut game = Slides(None);
    let rgba = capture_with(&mut player, &mut game, 1);
    let (left, top) = (OUTPUT.0 / 2, OUTPUT.1 / 2 - 32);
    let mut checked = 0;
    for y in 0..16 {
        for x in 0..16 {
            let want = paint(x, y);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (px, py) = (left + 2 * x + dx, top + 2 * y + dy);
                let i = ((py * OUTPUT.0 + px) * 4) as usize;
                assert_eq!(
                    &rgba[i..i + 4],
                    &want,
                    "art pixel ({x}, {y}) at ({px}, {py})"
                );
                checked += 1;
            }
        }
    }
    // Around it, the view's clear color (dark blue, not the sprite's).
    let background = at(&rgba, -3.0, 3.0);
    assert!(background[2] > background[0], "{background:?}");
    assert_eq!(at(&rgba, 1.2, 0.5), background);
    println!("{checked} pixels exact; background {background:?}");
    // The 2D pass drew it; no scene pass ran (the GPU's times of a frame come back a few
    // frames later).
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let passes: Vec<&str> = player
        .graph()
        .gpu_pass_times()
        .iter()
        .map(|t| t.name)
        .collect();
    println!("passes: {passes:?}");
    if !passes.is_empty() {
        assert!(passes.contains(&"2d"), "{passes:?}");
        assert!(!passes.contains(&"forward"), "{passes:?}");
    }
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sheet cut into slices (ADR 0125's E13-2): a sprite shows a slice by its frame number
/// or by the slice's name as a part of the image, each at the slice's own size and pivot.
#[test]
fn a_sheets_slices_are_drawn_by_frame_and_by_name() {
    // 32 × 16: red on the left 16 pixels, green on the right.
    let sheet = image(32, 16, |x, _| {
        if x < 16 {
            [255, 0, 0, 255]
        } else {
            [0, 255, 0, 255]
        }
    });
    let slice = |name: &str, x: u32, pivot: [f32; 2]| SliceImport {
        name: String::from(name),
        rect: [x, 0, 16, 16],
        pivot,
        ..SliceImport::default()
    };
    let import = SpriteImport {
        // The second stands on its feet.
        slices: vec![slice("red", 0, [0.5, 0.5]), slice("green", 16, [0.5, 0.0])],
        ..SpriteImport::default()
    };
    let (dir, paths, ids) = project("slices", &[("sheet.png", sheet, import)]);
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    let mut at_x = |x: f32, sprite: Sprite| {
        world.spawn((
            Transform::from_xyz(x, 0.0, 0.0),
            GlobalTransform::IDENTITY,
            sprite,
        ))
    };
    // Frame 0 (red, centred), frame 1 (green, on its feet), a frame past the last (green
    // again), and the red slice by name.
    let whole = Sprite::new(AssetRef::file(ids[0]));
    at_x(-4.0, whole);
    at_x(-2.0, Sprite { frame: 1, ..whole });
    at_x(0.0, Sprite { frame: 9, ..whole });
    let named = at_x(2.0, Sprite::new(AssetRef::part(ids[0], "red")));
    let mut game = Slides(None);
    let rgba = capture_with(&mut player, &mut game, 4);
    let (red, green) = ([255, 0, 0, 255], [0, 255, 0, 255]);
    let background = at(&rgba, -6.0, 3.0);
    // A metre square each: red about its middle; green above its feet.
    assert_eq!(at(&rgba, -4.4, -0.4), red, "frame 0");
    assert_eq!(
        at(&rgba, -4.4, 0.6),
        background,
        "frame 0 is one metre high"
    );
    assert_eq!(at(&rgba, -2.4, 0.9), green, "frame 1, on its feet");
    assert_eq!(
        at(&rgba, -2.4, -0.1),
        background,
        "frame 1, nothing below its feet"
    );
    assert_eq!(at(&rgba, 0.4, 0.5), green, "past the last: the last");
    assert_eq!(at(&rgba, 2.4, 0.4), red, "by name");
    assert_eq!(at(&rgba, 2.6, 0.0), background, "by name: one metre wide");
    // Picking agrees.
    assert_eq!(
        player.sprite_at(Vec2::new(2.3, 0.3), |_, _| true),
        Some(named)
    );
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sheet's clip played by an `Animator` (ADR 0125's E13-2): the sprite's frame follows
/// the clip's time, the animator learns its length and wraps there.
#[test]
fn an_animator_plays_a_sheets_clip() {
    let sheet = image(32, 16, |x, _| {
        if x < 16 {
            [255, 0, 0, 255]
        } else {
            [0, 255, 0, 255]
        }
    });
    let slice = |name: &str, x: u32| SliceImport {
        name: String::from(name),
        rect: [x, 0, 16, 16],
        ..SliceImport::default()
    };
    let import = SpriteImport {
        slices: vec![slice("red", 0), slice("green", 16)],
        clips: vec![SpriteClipImport {
            name: String::from("blink"),
            first: 0,
            last: 1,
            frame_time: 0.25,
            ..SpriteClipImport::default()
        }],
        ..SpriteImport::default()
    };
    let (dir, paths, ids) = project("clip", &[("sheet.png", sheet, import)]);
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    let blinker = world.spawn((
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
        Animator::new("blink"),
    ));
    let mut game = Slides(None);
    // A second at 60 frames a second: each frame's (clip time, sprite frame).
    let mut seen = Vec::new();
    for _ in 0..60 {
        player.frame(&mut game).unwrap();
        let world = player.world_mut();
        let animator = world.get::<Animator>(blinker).unwrap();
        let (time, length) = (animator.time, animator.length);
        let frame = world.get::<Sprite>(blinker).unwrap().frame;
        seen.push((time, length, frame));
    }
    let (_, length, _) = *seen.last().unwrap();
    assert!(
        (length - 0.5).abs() < 1e-6,
        "the clip's length told: {length}"
    );
    // Once the length is known, the frame is the clip's at the animator's time.
    let told = seen.iter().position(|s| s.1 > 0.0).unwrap();
    for &(time, _, frame) in &seen[told + 1..] {
        let want = u32::from(time.rem_euclid(0.5) >= 0.25);
        assert!(time < 0.5 + 1e-4, "wrapped: {time}");
        assert_eq!(frame, want, "at {time}: {seen:?}");
    }
    assert!(seen.iter().any(|s| s.2 == 0) && seen.iter().any(|s| s.2 == 1));
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sliced sprite (ADR 0125's E13-2d) keeps its border's pixels at their size when
/// stretched; a simple one stretches them with the rest.
#[test]
fn a_sliced_sprite_keeps_its_border_when_stretched() {
    // 16 × 16: a 4-pixel red border around a green middle.
    let panel = image(16, 16, |x, y| {
        if x < 4 || y < 4 || x >= 12 || y >= 12 {
            [255, 0, 0, 255]
        } else {
            [0, 255, 0, 255]
        }
    });
    let import = SpriteImport {
        slices: vec![SliceImport {
            name: String::from("panel"),
            rect: [0, 0, 16, 16],
            border: [4, 4, 4, 4],
            ..SliceImport::default()
        }],
        ..SpriteImport::default()
    };
    let (dir, paths, ids) = project("sliced", &[("panel.png", panel, import)]);
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    // Both 3 × 1 metres: the sliced one at y 1, the simple one at y -1.
    let base = Sprite::new(AssetRef::file(ids[0])).with_size(Vec2::new(3.0, 1.0));
    for (y, mode) in [(1.0, Sprite::SLICED), (-1.0, Sprite::SIMPLE)] {
        world.spawn((
            Transform::from_xyz(0.0, y, 0.0),
            GlobalTransform::IDENTITY,
            Sprite { mode, ..base },
        ));
    }
    let mut game = Slides(None);
    let rgba = capture_with(&mut player, &mut game, 2);
    let (red, green) = ([255, 0, 0, 255], [0, 255, 0, 255]);
    // Half a metre in from the left edge (x -1.5): inside the sliced one's middle, still
    // in the simple one's stretched border (0.75 metres wide).
    assert_eq!(
        at(&rgba, -1.0, 1.0),
        green,
        "sliced: past its 0.25 m border"
    );
    assert_eq!(at(&rgba, -1.4, 1.0), red, "sliced: its border");
    assert_eq!(at(&rgba, -1.0, -1.0), red, "simple: the border stretched");
    assert_eq!(at(&rgba, 0.0, 1.0), green);
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sprite's animation graph (ADR 0125's E13-2e): an `.agraph` whose states play the
/// image's clips, cooked against them; its parameter moves it from idle to run, the
/// sprite showing the run's frames and the animator naming its state.
#[test]
fn an_animation_graph_plays_a_sprites_clips() {
    // 48 × 16: three frames, red then green then blue.
    let sheet = image(48, 16, |x, _| match x / 16 {
        0 => [255, 0, 0, 255],
        1 => [0, 255, 0, 255],
        _ => [0, 0, 255, 255],
    });
    let slice = |k: u32| SliceImport {
        name: format!("f{k}"),
        rect: [16 * k, 0, 16, 16],
        ..SliceImport::default()
    };
    let clip = |name: &str, first: u32, last: u32| SpriteClipImport {
        name: String::from(name),
        first,
        last,
        frame_time: 0.25,
        ..SpriteClipImport::default()
    };
    let import = SpriteImport {
        slices: vec![slice(0), slice(1), slice(2)],
        clips: vec![clip("idle", 0, 0), clip("run", 1, 2)],
        ..SpriteImport::default()
    };
    let (dir, mut paths, ids) = project("graph", &[("hero.png", sheet, import)]);
    let graph_id = AssetId::new();
    let graph = format!(
        "[agraph version=1 id={graph_id}]

params[\"speed\"].kind = Float
layers[\"base\"].name = \"Base\"
layers[\"base\"].root = \"m0\"
nodes[\"m0\"].kind = Machine
nodes[\"m0\"].entry = \"idle\"
nodes[\"c1\"].clip = \"idle\"
nodes[\"c2\"].clip = \"run\"
states[\"idle\"].machine = \"m0\"
states[\"idle\"].name = \"Idle\"
states[\"idle\"].pose = \"c1\"
states[\"run\"].machine = \"m0\"
states[\"run\"].name = \"Run\"
states[\"run\"].pose = \"c2\"
transitions[\"t1\"].from = \"idle\"
transitions[\"t1\"].to = \"run\"
transitions[\"t1\"].when = \"speed > 1\"
"
    );
    std::fs::write(dir.join("art/hero.agraph"), graph).unwrap();
    paths.insert(AssetRef::file(graph_id), "art/hero.agraph");
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    let hero = world.spawn((
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
        Animator {
            graph: AssetRef::file(graph_id),
            ..Animator::new("")
        },
    ));
    let mut game = Slides(None);
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
    }
    let world = player.world_mut();
    assert_eq!(world.get::<Animator>(hero).unwrap().state, "Idle");
    assert_eq!(world.get::<Sprite>(hero).unwrap().frame, 0, "idle's frame");
    // Running: the graph goes to Run, the sprite shows its frames 1 and 2 in turn.
    {
        let animator = world.get_mut::<Animator>(hero).unwrap();
        animator.params[0] = 3.0;
        animator.params_set = 1;
    }
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..60 {
        player.frame(&mut game).unwrap();
        seen.insert(player.world_mut().get::<Sprite>(hero).unwrap().frame);
    }
    let world = player.world_mut();
    assert_eq!(world.get::<Animator>(hero).unwrap().state, "Run");
    assert!(
        seen.contains(&1) && seen.contains(&2),
        "run's frames: {seen:?}"
    );
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Layers, then order within a layer; y in a layer that sorts by y; a sort group's parts
/// together at the group's place.
#[test]
fn sprites_sort_by_layer_order_y_and_group() {
    let solid = |rgb: [u8; 3]| image(16, 16, move |_, _| [rgb[0], rgb[1], rgb[2], 255]);
    let colors = [
        ("red.png", [255, 0, 0]),
        ("green.png", [0, 255, 0]),
        ("blue.png", [0, 0, 255]),
        ("yellow.png", [255, 255, 0]),
        ("white.png", [255, 255, 255]),
    ];
    let images: Vec<(&str, Vec<u8>, SpriteImport)> = colors
        .iter()
        .map(|(file, rgb)| (*file, solid(*rgb), SpriteImport::default()))
        .collect();
    let (dir, paths, ids) = project("sorting", &images);
    // Layer 1 sorts by y.
    let sorting = SortingFile {
        layers: BTreeMap::from([(
            1,
            SortingLayer {
                name: String::from("characters"),
                y_sort: true,
            },
        )]),
    };
    std::fs::write(dir.join("main.sorting"), sorting.to_text(AssetId::new())).unwrap();
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let [red, green, blue, yellow, white] =
        core::array::from_fn(|k| Sprite::new(AssetRef::file(ids[k])));
    let world = player.world_mut();
    camera(world);
    let mut sprite_at = |x: f32, y: f32, sprite: Sprite| {
        world.spawn((
            Transform::from_xyz(x, y, 0.0),
            GlobalTransform::IDENTITY,
            sprite,
        ))
    };
    // Layer 0 by order: red (1) over green (0), though green comes later; blue on layer
    // −1 under both.
    let red_e = sprite_at(0.0, 0.0, red.on_layer(0, 1));
    let green_e = sprite_at(0.5, 0.0, green.on_layer(0, 0));
    let blue_e = sprite_at(0.25, 0.5, blue.on_layer(-1, 9));
    // Layer 1 by y: yellow (lower, nearer) over white, though white comes later.
    let yellow_e = sprite_at(4.0, 0.0, yellow.on_layer(1, 0));
    let white_e = sprite_at(4.5, 0.5, white.on_layer(1, 0));
    // Two characters at the same height on layer 1, each a body (order 0) and a sword
    // (order 1) made sword first: within a group by order (the sword over the body), and
    // the groups whole (the second over the first), never interleaved by a tie in y.
    for (k, (body, sword)) in [(red, green), (blue, yellow)].into_iter().enumerate() {
        let x = -5.0 + k as f32 * 0.75;
        let group = world.spawn((
            Transform::from_xyz(x, -2.0, 0.0),
            GlobalTransform::IDENTITY,
            SortGroup { layer: 1, order: 0 },
        ));
        let parts = [
            (Vec3::new(0.5, 0.0, 0.0), sword.on_layer(1, 1)),
            (Vec3::ZERO, body.on_layer(1, 0)),
        ];
        for (offset, sprite) in parts {
            let part = world.spawn((
                Transform::from_translation(offset),
                GlobalTransform::IDENTITY,
                sprite,
            ));
            world.set_parent(part, Some(group)).unwrap();
        }
    }
    let mut game = Slides(None);
    let rgba = capture_with(&mut player, &mut game, 9);
    let (r, g, b, y, w) = (
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 255, 255, 255],
    );
    // Red over green where they overlap; green alone right of it; blue under both, alone
    // above them.
    assert_eq!(at(&rgba, 0.35, 0.1), r, "order within a layer");
    assert_eq!(at(&rgba, 0.8, 0.1), g);
    assert_eq!(at(&rgba, 0.35, 0.4), r, "a lower layer under");
    assert_eq!(at(&rgba, 0.35, 0.6), b);
    // Yellow (y 0) over white (y 0.5).
    assert_eq!(at(&rgba, 4.35, 0.35), y, "the lower y in front");
    assert_eq!(at(&rgba, 4.65, 0.65), w);
    // The first character: its body (red, x −5.5 to −4.5) under its sword (green, −5 to
    // −4). The second: its body (blue, −4.75 to −3.75) over the first's sword, under its
    // own sword (yellow, −4.25 to −3.25).
    assert_eq!(at(&rgba, -5.4, -2.0), r, "the first body alone");
    assert_eq!(at(&rgba, -4.9, -2.0), g, "a group's sword over its body");
    assert_eq!(at(&rgba, -4.4, -2.0), b, "the second group over the first");
    assert_eq!(at(&rgba, -4.0, -2.1), y, "the second sword over its body");
    assert_eq!(at(&rgba, -3.4, -2.0), y, "the second sword alone");
    // The editor's 2D picking (E13-1b) names the sprite drawn on top there.
    let picks = [
        ((0.35, 0.1), Some(red_e)),
        ((0.8, 0.1), Some(green_e)),
        ((0.35, 0.6), Some(blue_e)),
        ((4.35, 0.35), Some(yellow_e)),
        ((4.65, 0.65), Some(white_e)),
        ((9.0, 9.0), None),
    ];
    for ((x, y), expected) in picks {
        assert_eq!(
            player.sprite_at(Vec2::new(x, y), |_, _| true),
            expected,
            "picked at {x}, {y}"
        );
    }
    // Where red's image is clear, the click goes through it to green under it.
    let through = player.sprite_at(Vec2::new(0.35, 0.1), |image, _| image != red.image);
    assert_eq!(
        through,
        Some(green_e),
        "a clear pixel picks what is under it"
    );
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first column of the output's row `y` with `color`.
fn left_edge(rgba: &[u8], y: u32, color: [u8; 4]) -> Option<u32> {
    (0..OUTPUT.0).find(|&x| {
        let i = ((y * OUTPUT.0 + x) * 4) as usize;
        rgba[i..i + 4] == color
    })
}

/// A sprite moving a metre a tick is drawn half a metre further each frame when frames
/// come twice a tick: placed between its last two ticks, as 3D instances are.
#[test]
fn sprites_move_between_ticks() {
    let (dir, paths, ids) = project(
        "motion",
        &[(
            "red.png",
            image(16, 16, |_, _| [255, 0, 0, 255]),
            SpriteImport::default(),
        )],
    );
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    world.spawn((
        Transform::from_xyz(-8.0, 0.0, 0.0),
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
        Slide(1.0),
    ));
    let mut game = Slides(None);
    capture_with(&mut player, &mut game, 1);
    // Two frames a tick (64 a second): a tick every other frame.
    player.set_frame_time(Some(1.0 / 128.0));
    let row = OUTPUT.1 / 2 - 4;
    let mut edges = Vec::new();
    for _ in 0..6 {
        let rgba = player.capture(&mut game).unwrap().unwrap();
        edges.push(left_edge(&rgba, row, [255, 0, 0, 255]).unwrap());
    }
    println!("left edges: {edges:?}");
    // Half a metre (16 output pixels) a frame, never a whole metre at once.
    for pair in edges.windows(2) {
        assert_eq!(pair[1] - pair[0], 16, "{edges:?}");
    }
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A perspective view pays nothing for sprites: no 2D pass, nothing recorded. An
/// orthographic view with nothing to draw declares the clear alone.
#[test]
fn views_without_2d_content_pay_nothing_for_it() {
    let (dir, paths, ids) = project(
        "unused",
        &[(
            "red.png",
            image(16, 16, |_, _| [255, 0, 0, 255]),
            SpriteImport::default(),
        )],
    );
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    let camera3d = world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera::perspective(1.0, 0.1, 100.0),
    ));
    let sprite = world.spawn((
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
    ));
    let mut game = Slides(None);
    let names = |player: &Player| -> Vec<&'static str> {
        player
            .graph()
            .gpu_pass_times()
            .iter()
            .map(|t| t.name)
            .collect()
    };
    for _ in 0..8 {
        let report = player.frame(&mut game).unwrap().unwrap();
        assert_eq!(report.sprites, 0.0);
    }
    assert_eq!(player.sprites_recorded(), (0, 0));
    let passes = names(&player);
    println!("a perspective view: {passes:?}");
    assert!(!passes.contains(&"2d") && !passes.contains(&"2d depth"));
    // An orthographic camera with nothing to draw: the clear alone.
    player.world_mut().despawn(camera3d);
    player.world_mut().despawn(sprite);
    camera(player.world_mut());
    for _ in 0..8 {
        player.frame(&mut game).unwrap();
    }
    let passes = names(&player);
    println!("an empty 2D view: {passes:?}");
    if !passes.is_empty() {
        assert!(passes.contains(&"clear"), "{passes:?}");
        assert!(
            !passes.contains(&"2d") && !passes.contains(&"forward"),
            "{passes:?}"
        );
        // Its depth is cleared only when something reads it.
        assert!(!passes.contains(&"2d depth"), "{passes:?}");
    }
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sprite, a sort group and a pixel-art camera written to a scene file and read back,
/// and taken into a save and applied: as any component (ADR 0125).
#[test]
fn sprites_are_kept_in_scenes_and_saves() {
    use engine_scene::{SaveOptions, SceneId, SceneRegistry, load, save_with};
    let registry = SceneRegistry::from_program();
    let mut world = World::new();
    let image = AssetId::new();
    let mut paths = AssetPaths::new();
    paths.insert(AssetRef::file(image), "art/hero.png");
    let sprite = Sprite {
        flip_x: 1,
        size: Vec2::new(2.0, 0.0),
        ..Sprite::new(AssetRef::file(image))
            .on_layer(3, -2)
            .with_color([1.0, 0.5, 0.25, 0.75])
    };
    let group = SortGroup { layer: 1, order: 4 };
    let camera = Camera::pixel_art([320, 180], 16.0);
    world.spawn((SceneId::new(), Transform::from_xyz(2.0, 1.0, 0.0), sprite));
    world.spawn((SceneId::new(), Transform::IDENTITY, group));
    world.spawn((SceneId::new(), Transform::IDENTITY, camera));
    let mut text = String::new();
    let options = SaveOptions {
        id: None,
        assets: Some(&paths),
        prefabs: None,
    };
    save_with(&mut world, &registry, &options, &mut text).unwrap();
    println!("{text}");
    for line in [
        format!("Sprite.image = asset(\"art/hero.png\", {image})"),
        String::from("Sprite.flip_x = 1"),
        String::from("Sprite.layer = 3"),
        String::from("Sprite.order = -2"),
        String::from("SortGroup.order = 4"),
        String::from("Camera.projection = 1"),
        String::from("Camera.reference = [320, 180]"),
    ] {
        assert!(text.contains(&line), "{line}");
    }
    let mut back = World::new();
    let report = load(&mut back, &registry, &text).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let found = |world: &mut World| {
        let mut sprites = QueryState::<&Sprite>::new(world);
        let mut groups = QueryState::<&SortGroup>::new(world);
        let mut cameras = QueryState::<&Camera>::new(world);
        (
            sprites.iter_readonly(world).copied().collect::<Vec<_>>(),
            groups.iter_readonly(world).copied().collect::<Vec<_>>(),
            cameras.iter_readonly(world).copied().collect::<Vec<_>>(),
        )
    };
    assert_eq!(found(&mut back), (vec![sprite], vec![group], vec![camera]));
    // A save: taken, written as text, read and applied to a fresh world.
    let save = engine_save::capture(&world, &engine_save::CaptureOptions::default());
    let bytes =
        engine_save::encode(&save, engine_save::Encoding::Text, &Default::default()).unwrap();
    let mut loaded = World::new();
    engine_save::apply(&mut loaded, &engine_save::decode(&bytes).unwrap()).unwrap();
    assert_eq!(
        found(&mut loaded),
        (vec![sprite], vec![group], vec![camera])
    );
}

/// An Aseprite file (ADR 0125's E13-2f) drawn as a sprite: its frames are its slices, its
/// tag a clip with the frames' own durations, a cel's `event:` line an event of the clip,
/// and the drawn sprite shows the frame's merged layers.
#[test]
fn an_aseprite_files_tag_plays_its_frames() {
    use engine_assets::aseprite::sample::{Writer, cel, layer, tags, user_text};
    // 16 × 16, three frames: red, green, blue; the green one shown twice as long.
    let mut w = Writer::new(16, 16, 32);
    let solid = |c: [u8; 4]| c.repeat(16 * 16);
    w.frames.push((
        100,
        vec![
            layer(1, 0, 0, 0, 255, "paint"),
            cel(0, 0, 0, 16, 16, &solid([255, 0, 0, 255])),
            tags(&[(0, 2, 0, 0, "walk")]),
        ],
    ));
    w.frames.push((
        200,
        vec![
            cel(0, 0, 0, 16, 16, &solid([0, 255, 0, 255])),
            user_text("event:step"),
        ],
    ));
    w.frames
        .push((100, vec![cel(0, 0, 0, 16, 16, &solid([0, 0, 255, 255]))]));
    let (dir, paths, ids) = project(
        "aseprite",
        &[("walker.aseprite", w.bytes(), SpriteImport::default())],
    );
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    let walker = world.spawn((
        Transform::from_xyz(0.5, 0.5, 0.0),
        GlobalTransform::IDENTITY,
        Sprite::new(AssetRef::file(ids[0])),
        Animator::new("walk"),
    ));
    let mut game = Slides(None);
    // Each frame's colour at the sprite's middle against its frame number, over a loop.
    let mut seen = BTreeMap::new();
    for _ in 0..30 {
        let rgba = capture_with(&mut player, &mut game, 1);
        let frame = player.world_mut().get::<Sprite>(walker).unwrap().frame;
        *seen.entry((frame, at(&rgba, 0.5, 0.5))).or_insert(0) += 1;
    }
    let colours = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
    for &(frame, colour) in seen.keys() {
        assert_eq!(colour, colours[frame as usize], "frame {frame}: {seen:?}");
    }
    let frames: std::collections::BTreeSet<u32> = seen.keys().map(|k| k.0).collect();
    assert_eq!(frames.len(), 3, "every frame shown: {seen:?}");
    // The green frame shows for 0.2 s, the others 0.1 s: about twice as often.
    let count = |f: u32| {
        seen.iter()
            .filter(|(k, _)| k.0 == f)
            .map(|(_, n)| n)
            .sum::<u32>()
    };
    assert!(count(1) > count(0) && count(1) > count(2), "{seen:?}");
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tile map (ADR 0125's E13-3b) drawn from its tile set: each cell its tile's pixels,
/// a flipped cell mirrored, cells in negative chunks where they belong, empty cells clear.
#[test]
fn a_tile_map_draws_its_cells_from_its_tile_set() {
    use engine_app::TileMap;
    use engine_scene::{TileSetFile, TileSource};
    // 32 × 16: tile 0 red; tile 1 white on its left half, black on its right.
    let sheet = image(32, 16, |x, _| match x {
        0..16 => [255, 0, 0, 255],
        16..24 => [255, 255, 255, 255],
        _ => [0, 0, 0, 255],
    });
    let (dir, mut paths, ids) = project(
        "tilemap",
        &[(
            "tiles.png",
            sheet,
            SpriteImport {
                filter: engine_scene::SpriteFilter::Nearest,
                ..SpriteImport::default()
            },
        )],
    );
    let mut set = TileSetFile::default();
    set.sources.insert(
        String::from("k0"),
        TileSource {
            image: AssetRef::file(ids[0]),
            ..TileSource::default()
        },
    );
    let set_id = AssetId::new();
    std::fs::write(dir.join("art/forest.tileset"), set.to_text(set_id)).unwrap();
    paths.insert(AssetRef::file(set_id), "art/forest.tileset");
    let Some(mut player) = player(&dir, &paths) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let world = player.world_mut();
    camera(world);
    let mut map = TileMap::new(AssetRef::file(set_id));
    map.set(0, 0, Some("k0:0,0"));
    map.set(1, 0, Some("k0:1,0"));
    map.set(2, 0, Some("k0:1,0 flip_x"));
    map.set(-1, -1, Some("k0:0,0"));
    world.spawn((Transform::IDENTITY, GlobalTransform::IDENTITY, map));
    let mut game = Slides(None);
    let red = [255, 0, 0, 255];
    let mut rgba = Vec::new();
    for _ in 0..600 {
        player.frame(&mut game).unwrap();
        rgba = player.capture(&mut game).unwrap().unwrap();
        if at(&rgba, 0.5, 0.5) == red {
            break;
        }
    }
    let (white, black) = ([255, 255, 255, 255], [0, 0, 0, 255]);
    assert_eq!(at(&rgba, 0.5, 0.5), red, "cell (0, 0)");
    assert_eq!(at(&rgba, 1.25, 0.5), white, "cell (1, 0)'s left half");
    assert_eq!(at(&rgba, 1.75, 0.5), black, "cell (1, 0)'s right half");
    assert_eq!(at(&rgba, 2.25, 0.5), black, "flipped: its left half black");
    assert_eq!(at(&rgba, 2.75, 0.5), white);
    assert_eq!(
        at(&rgba, -0.5, -0.5),
        red,
        "cell (-1, -1) in chunk (-1, -1)"
    );
    assert_ne!(at(&rgba, 0.5, 1.5), red, "an empty cell");
    assert_ne!(at(&rgba, 0.5, -0.5), red, "an empty cell");
    assert_valid(&player);
    let _ = std::fs::remove_dir_all(&dir);
}
