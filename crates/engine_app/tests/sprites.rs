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
use engine_runtime::Camera;
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef, SliceImport, SpriteImport};
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
