//! A large glTF model loaded by the player (ADR 0108's E4-4): what its first frames cost
//! while its file is read, its meshes built and its textures decoded, and when it shows.
//! Measured by hand on `test-assets/` (gitignored):
//!
//! ```text
//! cargo nextest run --release -p engine_app --test asset_loading --run-ignored only --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{AssetMeta, Assets, BakedBrush};
use engine_assets::GltfOutline;
use engine_scene::AssetPaths;
use std::path::Path;
use std::time::{Duration, Instant};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

/// The part that names thing `i` of `names`: its name when nothing else has it, else `#i`.
fn part_name(names: &[&str], i: usize) -> String {
    let name = names[i];
    if !name.is_empty()
        && !name.starts_with('#')
        && names.iter().filter(|&&n| n == name).count() == 1
    {
        name.to_owned()
    } else {
        format!("#{i}")
    }
}

/// The frames of a player whose world holds a model of every mesh of `file` in
/// `test-assets/<folder>`, seen from `eye`: each frame's time until every model draws and
/// nothing is loading, then 60 more. With `later` the models come after the first frames
/// (loaded in the background, as an editor's drop), else they are there from the first
/// (which waits for them). Prints what it found; none when the folder is missing or there
/// is no GPU.
fn measure(folder: &str, file: &str, eye: Vec3, target: Vec3, later: bool) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets")
        .join(folder);
    let Ok(outline) = GltfOutline::read(&dir.join(file)) else {
        println!("skipped: {} missing", dir.join(file).display());
        return;
    };
    let settings = PlayerSettings {
        title: format!("{folder} loading"),
        window: Window::Offscreen {
            width: 640,
            height: 360,
        },
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) => {
            println!("skipped: {e}");
            return;
        }
    };
    let id = AssetId::new();
    // A `.meta` naming another id would refuse the file: none is written into
    // test-assets, and one there from elsewhere is read for its id.
    let id = std::fs::read_to_string(AssetMeta::path(&dir.join(file)))
        .ok()
        .and_then(|text| AssetMeta::parse(&text).ok())
        .and_then(|(_, report)| report.id)
        .unwrap_or(id);
    let mut paths = AssetPaths::new();
    paths.insert(AssetRef::file(id), file);
    player.assets_mut().add_paths(&paths);
    let meshes: Vec<&str> = outline.meshes.iter().map(|m| m.name.as_str()).collect();
    let materials: Vec<&str> = outline.materials.iter().map(String::as_str).collect();
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    if later {
        for _ in 0..5 {
            player.frame(&mut game).unwrap();
        }
    }
    let world = player.world_mut();
    let mut models = Vec::new();
    for (i, mesh) in outline.meshes.iter().enumerate() {
        let material = mesh
            .primitives
            .first()
            .and_then(|p| p.material)
            .map_or(builtin::MATERIAL, |k| {
                AssetRef::part(id, &part_name(&materials, k))
            });
        let model = Model::new(AssetRef::part(id, &part_name(&meshes, i)), material);
        models.push(world.spawn((SceneId::new(), Transform::IDENTITY, model)));
    }
    world.spawn((
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    world.spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 5000.0),
    ));
    let started = Instant::now();
    let mut frames: Vec<Duration> = Vec::new();
    let mut shown = None;
    while frames.len() < 2000 {
        let at = Instant::now();
        player.frame(&mut game).unwrap();
        frames.push(at.elapsed());
        let world = player.world_mut();
        let drawn = models
            .iter()
            .filter(|&&e| world.get::<MeshRenderer>(e).is_some())
            .count();
        if shown.is_none() && drawn == models.len() && player.assets().loading() == 0 {
            shown = Some((frames.len(), started.elapsed()));
        }
        if shown.is_some_and(|(n, _)| frames.len() >= n + 60) {
            break;
        }
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let (count, at) = shown.expect("the model shows");
    let loading = &frames[..count];
    let after = &frames[count..];
    let mut sorted: Vec<f64> = after.iter().map(|&d| ms(d)).collect();
    sorted.sort_by(f64::total_cmp);
    let worst = loading.iter().copied().max().map_or(0.0, ms);
    println!(
        "{folder}{}: {} meshes, {} materials, {} textures; shown after {count} frames, {:.0} \
         ms; worst frame while loading {worst:.1} ms; after: median {:.2} ms, worst {:.2} ms",
        if later {
            " added later"
        } else {
            " in the first frame"
        },
        outline.meshes.len(),
        outline.materials.len(),
        player.assets().textures_made(),
        ms(at),
        sorted[sorted.len() / 2],
        sorted.last().copied().unwrap_or(0.0),
    );
    let used: Vec<Model> = models
        .iter()
        .filter_map(|&e| player.world_mut().get::<Model>(e).copied())
        .collect();
    for m in used {
        let assets = player.assets();
        if let Some(problem) = assets.problem(m.mesh).or(assets.problem(m.material)) {
            println!("  problem: {problem}");
        }
    }
    let _ = Assets::new(dir);
}

#[test]
#[ignore = "measured by hand, on test-assets"]
fn measure_loading_flight_helmet() {
    for later in [false, true] {
        measure(
            "FlightHelmet",
            "FlightHelmet.gltf",
            Vec3::new(0.0, 0.35, 1.0),
            Vec3::new(0.0, 0.3, 0.0),
            later,
        );
    }
}

#[test]
#[ignore = "measured by hand, on test-assets"]
fn measure_loading_sponza() {
    for later in [false, true] {
        measure(
            "Sponza",
            "Sponza.gltf",
            Vec3::new(0.0, 300.0, 0.0),
            Vec3::new(1000.0, 300.0, 0.0),
            later,
        );
    }
}

/// Where the frame's time goes when FlightHelmet loads on one thread: reading and
/// importing the file, then each texture read, decoded and given its mip levels.
#[test]
#[ignore = "measured by hand, on test-assets"]
fn measure_the_parts_of_loading_flight_helmet() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/FlightHelmet");
    let Ok(bytes) = std::fs::read(dir.join("FlightHelmet.gltf")) else {
        println!("skipped: FlightHelmet missing");
        return;
    };
    let at = Instant::now();
    let scene = engine_assets::import_gltf(&bytes, Some(&dir)).unwrap();
    let import = at.elapsed();
    let (mut read, mut decode, mut mips) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    for texture in &scene.textures {
        let at = Instant::now();
        let encoded = engine_assets::read_texture(&texture.source, Some(&dir)).unwrap();
        read += at.elapsed();
        let at = Instant::now();
        let image = engine_assets::decode_image(&encoded).unwrap();
        decode += at.elapsed();
        let at = Instant::now();
        let _ = engine_assets::build_mips(&image, engine_assets::TextureUsage::Color);
        mips += at.elapsed();
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "FlightHelmet on one thread: import {:.0} ms; {} textures: read {:.0} ms, decode {:.0} ms, mip levels {:.0} ms",
        ms(import),
        scene.textures.len(),
        ms(read),
        ms(decode),
        ms(mips)
    );
}

/// A quad of tint `tint`, facing up: a bake's model of one mesh and one textured material.
fn quad(tint: [f32; 4]) -> BakedBrush {
    BakedBrush {
        tint,
        positions: vec![
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// Frames until `done`, at most 1,000 of them: how many it took (none if it never was).
fn frames_until(player: &mut Player, done: impl Fn(&mut Player) -> bool) -> Option<usize> {
    let mut game = Still;
    (1..=1000).find(|_| {
        player.frame(&mut game).unwrap();
        done(player)
    })
}

#[test]
fn a_model_added_later_loads_in_the_background() {
    let dir = std::env::temp_dir().join(format!("ofelia-loading-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let baked = engine_app::bake::glb(
        "Tile",
        &[quad([0.8, 0.2, 0.1, 1.0])],
        &engine_app::grid_png(),
    );
    let (mesh, material) = baked.parts[0].clone();
    std::fs::write(dir.join("tile.glb"), &baked.bytes).unwrap();
    let id = AssetMeta::ensure(&dir.join("tile.glb")).unwrap();
    let settings = PlayerSettings {
        title: String::from("loading test"),
        window: Window::Offscreen {
            width: 320,
            height: 180,
        },
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
    let mut paths = AssetPaths::new();
    paths.insert(AssetRef::file(id), "tile.glb");
    paths.insert(AssetRef::file(AssetId::from_u128(9)), "missing.glb");
    player.assets_mut().add_paths(&paths);
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    // Added after the first frame: the frame does not wait for it.
    let model = Model::new(AssetRef::part(id, &mesh), AssetRef::part(id, &material));
    let tile = player
        .world_mut()
        .spawn((SceneId::new(), Transform::IDENTITY, model));
    player.frame(&mut game).unwrap();
    assert!(
        player.world_mut().get::<MeshRenderer>(tile).is_none(),
        "not drawn in the frame that asked for it"
    );
    assert!(player.assets().loading() > 0);
    // Then drawn, its texture made, nothing left loading.
    let drawn = |player: &mut Player| {
        player.world_mut().get::<MeshRenderer>(tile).is_some() && player.assets().loading() == 0
    };
    let took = frames_until(&mut player, drawn).expect("drawn once loaded");
    println!("drawn {took} frames on");
    assert_eq!(player.assets().textures_made(), 1);
    assert_eq!(player.assets().problem(model.mesh), None);
    let first = *player.world_mut().get::<MeshRenderer>(tile).unwrap();
    // Written anew and read again: its new mesh and material in place.
    let again = engine_app::bake::glb(
        "Tile",
        &[quad([0.1, 0.2, 0.8, 1.0])],
        &engine_app::grid_png(),
    );
    std::fs::write(dir.join("tile.glb"), &again.bytes).unwrap();
    player.reload_asset(id);
    let renewed = |player: &mut Player| {
        player
            .world_mut()
            .get::<MeshRenderer>(tile)
            .is_some_and(|r| r.mesh != first.mesh && r.material != first.material)
            && player.assets().loading() == 0
    };
    frames_until(&mut player, renewed).expect("drawn again from the new file");
    // A file that is not there: said as its problem, nothing waiting for it.
    let lost = Model::new(
        AssetRef::part(AssetId::from_u128(9), "Gone"),
        builtin::MATERIAL,
    );
    let gone = player
        .world_mut()
        .spawn((SceneId::new(), Transform::IDENTITY, lost));
    let failed = |player: &mut Player| {
        player.assets().problem(lost.mesh).is_some() && player.assets().loading() == 0
    };
    frames_until(&mut player, failed).expect("the missing file said");
    assert!(player.world_mut().get::<MeshRenderer>(gone).is_none());
    // Asked to wait: the next frame has a model added now drawn.
    let second =
        player
            .world_mut()
            .spawn((SceneId::new(), Transform::from_xyz(2.0, 0.0, 0.0), model));
    player.wait_for_assets();
    player.frame(&mut game).unwrap();
    assert!(player.world_mut().get::<MeshRenderer>(second).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
