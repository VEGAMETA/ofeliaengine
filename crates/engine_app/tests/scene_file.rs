//! A scene file (ADR 0108) drawn by the player: built in code and saved as the editor will
//! save it, with a material file beside it, then loaded through `PlayerSettings::scene` and
//! rendered offscreen: the red cube stands on the grey floor in the middle of the picture,
//! as in the quick start. `ENGINE_SCENE_FILE_PNG=<file>` writes the frame.
//!
//! The same with models from a glTF file a blockout bake wrote (ADR 0110 E2b-6): the red
//! crate on the grey floor, both over the grid the file carries.
//! `ENGINE_SCENE_FILE_GLB_PNG=<file>` writes that frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{AssetMeta, Assets, BakedBrush};
use engine_scene::{SaveOptions, SceneRegistry, save_with};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

#[test]
fn a_scene_file_draws_its_models() {
    let dir = std::env::temp_dir().join(format!("ofelia-scene-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join("scenes")).unwrap();

    // The red material, a file of its own.
    let red_id = AssetId::new();
    let red = MaterialFile {
        base_color: [0.8, 0.15, 0.1, 1.0],
        roughness: 0.4,
        ..MaterialFile::default()
    };
    std::fs::write(dir.join("materials/red.material"), red.to_text(red_id)).unwrap();
    let red_ref = AssetRef::file(red_id);

    // The scene, built in code and saved as the editor will save it.
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, red_ref),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(red_ref, "materials/red.material");
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    assert!(
        text.contains("Model.mesh = asset(\"builtin#cube\""),
        "{text}"
    );
    assert!(
        text.contains("Model.material = asset(\"materials/red.material\""),
        "{text}"
    );
    assert!(text.contains("Name = \"Floor\""), "{text}");
    assert!(!text.contains("MeshRenderer"), "{text}");
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (640u32, 360u32);
    let settings = PlayerSettings {
        title: String::from("scene file test"),
        window: Window::Offscreen { width, height },
        scene: Some("scenes/main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_SCENE_FILE_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    assert_eq!(player.assets().problem(red_ref), None);
    let pixel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // The cube in the middle is red; the floor below it grey.
    let [r, g, b] = pixel(width / 2, height / 2);
    assert!(r > 1.5 * g && r > 1.5 * b, "middle {r} {g} {b}");
    let [r, g, b] = pixel(width / 2, height - 8);
    assert!(
        (r - g).abs() < 30.0 && (g - b).abs() < 30.0 && r > 20.0,
        "floor {r} {g} {b}"
    );

    // A model that goes takes its renderer with it.
    let world = player.world_mut();
    let mut names = QueryState::<(Entity, &Name)>::new(world);
    let cube = names
        .iter_readonly(world)
        .find(|(_, name)| name.as_str() == "Cube")
        .map(|(e, _)| e)
        .unwrap();
    assert!(world.get::<MeshRenderer>(cube).is_some());
    world.remove::<Model>(cube);
    player.frame(&mut game).unwrap();
    assert!(player.world_mut().get::<MeshRenderer>(cube).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A box from `lo` to `hi` of tint `tint` as a bake gives it: its faces' texture
/// coordinates in metres.
fn cuboid(tint: [f32; 4], lo: Vec3, hi: Vec3) -> BakedBrush {
    let mut b = BakedBrush {
        tint,
        ..BakedBrush::default()
    };
    let (centre, half) = ((lo + hi) * 0.5, (hi - lo) * 0.5);
    // Each face's normal and its axes, `u × v` the normal: counter-clockwise from in front.
    let faces = [
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    for (n, u, v) in faces {
        let first = b.positions.len() as u32;
        let c = centre + n * half;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = c + u * half * su + v * half * sv;
            b.positions.push(p.to_array());
            b.normals.push(n.to_array());
            b.uvs.push([p.dot(u), p.dot(v)]);
        }
        b.indices
            .extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    }
    b
}

#[test]
fn a_scene_file_draws_models_from_a_baked_glb() {
    let dir = std::env::temp_dir().join(format!("ofelia-scene-glb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();

    // The file a bake writes: a mesh and a material per tint, over the grid; its `.meta`.
    let grey = [0.72, 0.72, 0.72, 1.0];
    let red = [0.8, 0.15, 0.1, 1.0];
    let floor = cuboid(
        grey,
        Vec3::new(-10.0, -0.6, -10.0),
        Vec3::new(10.0, -0.5, 10.0),
    );
    let crate_ = cuboid(red, Vec3::splat(-0.5), Vec3::splat(0.5));
    let baked = engine_app::bake::glb("Yard", &[floor, crate_], &engine_app::grid_png());
    let glb = dir.join("models/yard.glb");
    std::fs::write(&glb, &baked.bytes).unwrap();
    let id = AssetMeta::ensure(&glb).unwrap();
    let mut paths = Assets::new(dir.clone()).paths().clone();
    let mut parts = Vec::new();
    for (mesh, material) in &baked.parts {
        let refs = (AssetRef::part(id, mesh), AssetRef::part(id, material));
        paths.insert(refs.0, format!("models/yard.glb#{mesh}"));
        paths.insert(refs.1, format!("models/yard.glb#{material}"));
        parts.push(refs);
    }

    let mut world = World::new();
    for (k, &(mesh, material)) in parts.iter().enumerate() {
        world.spawn((
            SceneId::new(),
            Name::new(format!("Yard {}", k + 1)),
            Transform::IDENTITY,
            Model::new(mesh, material),
        ));
    }
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    assert!(
        text.contains("Model.mesh = asset(\"models/yard.glb#Yard 2\""),
        "{text}"
    );
    std::fs::write(dir.join("main.scene"), &text).unwrap();

    let (width, height) = (640u32, 360u32);
    let settings = PlayerSettings {
        title: String::from("scene glb test"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_SCENE_FILE_GLB_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    for &(mesh, material) in &parts {
        assert_eq!(player.assets().problem(mesh), None);
        assert_eq!(player.assets().problem(material), None);
    }
    let pixel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // The crate in the middle is red.
    let [r, g, b] = pixel(width / 2, height / 2);
    assert!(r > 1.5 * g && r > 1.5 * b, "middle {r} {g} {b}");
    // The floor near the camera is grey, crossed by the grid's lines: its brightness
    // along a row goes up and down at each line.
    let y = height - 8;
    let light: Vec<f32> = (0..width)
        .map(|x| {
            let [r, g, b] = pixel(x, y);
            assert!(
                (r - g).abs() < 30.0 && (g - b).abs() < 30.0,
                "floor {r} {g} {b}"
            );
            (r + g + b) / 3.0
        })
        .collect();
    let (lo, hi) = light
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &l| (lo.min(l), hi.max(l)));
    assert!(hi - lo > 20.0, "the floor's row from {lo} to {hi}: no grid");
    let middle = (lo + hi) * 0.5;
    let crossings = light
        .windows(2)
        .filter(|w| (w[0] < middle) != (w[1] < middle))
        .count();
    assert!(
        crossings >= 4,
        "{crossings} lines crossed along the floor's row"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
