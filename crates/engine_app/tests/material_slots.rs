//! Material slots (ADR 0115): a glTF mesh of two primitives, a red one on the left and a
//! green one on the right, drawn by one model. Without slots it is red all over; with the
//! green material in its slot, each half has its own; the slots taken away, red again and
//! its derived parts gone; the model despawned alone, its parts with it.
//! `ENGINE_SLOTS_PNG=<file>` writes the frame with slots.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{MaterialSlots, Model};
use engine_runtime::Derived;
use engine_scene::{AssetMeta, AssetPaths, AssetRef};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A `.gltf` of one mesh, "Two": a quad from x = -1 to 0 with material "Red" and one from
/// 0 to 1 with "Green", both facing +z, 1 m tall.
fn two_gltf() -> String {
    let mut bin: Vec<u8> = Vec::new();
    for x0 in [-1.0f32, 0.0] {
        for p in [
            [x0, -0.5, 0.0],
            [x0 + 1.0, -0.5, 0.0],
            [x0 + 1.0, 0.5, 0.0],
            [x0, 0.5, 0.0],
        ] {
            for v in p {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    for _ in 0..8 {
        for v in [0.0f32, 0.0, 1.0] {
            bin.extend_from_slice(&v.to_le_bytes());
        }
    }
    for i in [0u32, 1, 2, 0, 2, 3] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    format!(
        r#"{{
  "asset": {{"version": "2.0"}},
  "scene": 0, "scenes": [{{"nodes": [0]}}], "nodes": [{{"name": "Two", "mesh": 0}}],
  "buffers": [{{"byteLength": {len}, "uri": "data:application/octet-stream;base64,{data}"}}],
  "bufferViews": [
    {{"buffer": 0, "byteOffset": 0, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 48, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 96, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 144, "byteLength": 48}},
    {{"buffer": 0, "byteOffset": 192, "byteLength": 24}}
  ],
  "accessors": [
    {{"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-1, -0.5, 0], "max": [0, 0.5, 0]}},
    {{"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3", "min": [0, -0.5, 0], "max": [1, 0.5, 0]}},
    {{"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC3"}},
    {{"bufferView": 3, "componentType": 5126, "count": 4, "type": "VEC3"}},
    {{"bufferView": 4, "componentType": 5125, "count": 6, "type": "SCALAR"}}
  ],
  "meshes": [{{"name": "Two", "primitives": [
    {{"attributes": {{"POSITION": 0, "NORMAL": 2}}, "indices": 4, "material": 0}},
    {{"attributes": {{"POSITION": 1, "NORMAL": 3}}, "indices": 4, "material": 1}}
  ]}}],
  "materials": [
    {{"name": "Red", "pbrMetallicRoughness": {{"baseColorFactor": [0.8, 0.05, 0.05, 1], "metallicFactor": 0}}}},
    {{"name": "Green", "pbrMetallicRoughness": {{"baseColorFactor": [0.05, 0.8, 0.05, 1], "metallicFactor": 0}}}}
  ]
}}"#,
        len = bin.len(),
        data = base64(&bin),
    )
}

/// Frames until `done`, at most 600 of them.
fn frames_until(player: &mut Player, done: impl Fn(&mut Player) -> bool) -> bool {
    let mut game = Still;
    (0..600).any(|_| {
        player.frame(&mut game).unwrap();
        done(player)
    })
}

fn derived(world: &mut World) -> usize {
    let mut q = QueryState::<Entity, engine_ecs::With<Derived>>::new(world);
    q.iter_readonly(world).count()
}

#[test]
fn a_mesh_of_two_materials_draws_each_with_its_slot() {
    let dir = std::env::temp_dir().join(format!("ofelia-slots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("two.gltf"), two_gltf()).unwrap();
    let id = AssetMeta::ensure(&dir.join("two.gltf")).unwrap();
    let (width, height) = (320u32, 180u32);
    let settings = PlayerSettings {
        title: String::from("material slots test"),
        window: Window::Offscreen { width, height },
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
    paths.insert(AssetRef::file(id), "two.gltf");
    player.assets_mut().add_paths(&paths);
    let world = player.world_mut();
    world.spawn((
        SceneId::new(),
        Transform::from_rotation(Quat::from_rotation_x(-0.6)),
        Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.2),
        },
    ));
    let eye = Vec3::new(0.0, 0.0, 3.0);
    world.spawn((
        SceneId::new(),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 100.0),
    ));
    let (red, green) = (AssetRef::part(id, "Red"), AssetRef::part(id, "Green"));
    let model = world.spawn((
        SceneId::new(),
        Transform::IDENTITY,
        Model::new(AssetRef::part(id, "Two"), red),
    ));
    player.set_frame_time(Some(1.0 / 60.0));
    let mut game = Still;
    let left_right = |player: &mut Player, game: &mut Still| {
        let rgba = player.capture(game).unwrap().unwrap();
        let at = |x: u32, y: u32| {
            let i = ((y * width + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
        };
        (
            at(width / 2 - 30, height / 2),
            at(width / 2 + 30, height / 2),
            rgba,
        )
    };
    let drawn = |player: &mut Player| {
        player.assets().loading() == 0 && player.world_mut().get::<MeshRenderer>(model).is_some()
    };
    assert!(frames_until(&mut player, drawn), "the model drawn");
    // Without slots: one material, red on both halves.
    let (l, r, _) = left_right(&mut player, &mut game);
    assert!(
        l[0] > 2.0 * l[1] && r[0] > 2.0 * r[1],
        "red all over: {l:?} {r:?}"
    );
    assert_eq!(derived(player.world_mut()), 0);
    // The green one in the second primitive's slot: each half its own.
    player.world_mut().insert(model, MaterialSlots(vec![green]));
    let split =
        |player: &mut Player| player.assets().loading() == 0 && derived(player.world_mut()) == 1;
    assert!(
        frames_until(&mut player, split),
        "a derived part for the second primitive"
    );
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let (l, r, rgba) = left_right(&mut player, &mut game);
    if let Ok(path) = std::env::var("ENGINE_SLOTS_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    assert!(l[0] > 2.0 * l[1], "the left half red: {l:?}");
    assert!(r[1] > 2.0 * r[0], "the right half green: {r:?}");
    // The slots taken away: red all over again, the part gone.
    player.world_mut().remove::<MaterialSlots>(model);
    let whole =
        |player: &mut Player| player.assets().loading() == 0 && derived(player.world_mut()) == 0;
    assert!(frames_until(&mut player, whole), "the part gone");
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let (l, r, _) = left_right(&mut player, &mut game);
    assert!(
        l[0] > 2.0 * l[1] && r[0] > 2.0 * r[1],
        "red all over again: {l:?} {r:?}"
    );
    // Slots again, then the model despawned alone: its part goes with it.
    player.world_mut().insert(model, MaterialSlots(vec![green]));
    assert!(frames_until(&mut player, split));
    player.world_mut().despawn(model);
    for _ in 0..2 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(derived(player.world_mut()), 0, "no part left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slots_are_written_in_scenes_as_a_list_of_assets() {
    let file = AssetId::new();
    let (mesh, red, green) = (
        AssetRef::part(file, "Two"),
        AssetRef::part(file, "Red"),
        AssetRef::part(file, "Green"),
    );
    let mut paths = AssetPaths::new();
    for (asset, name) in [(mesh, "Two"), (red, "Red"), (green, "Green")] {
        paths.insert(asset, format!("props/two.gltf#{name}"));
    }
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Two"),
        Transform::IDENTITY,
        Model::new(mesh, red),
        MaterialSlots(vec![green]),
    ));
    let registry = engine_scene::SceneRegistry::from_program();
    let options = engine_scene::SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut text = String::new();
    engine_scene::save_with(&mut world, &registry, &options, &mut text).unwrap();
    let line = format!("MaterialSlots = [asset(\"props/two.gltf#Green\", {file})]");
    assert!(text.contains(&line), "{line} in {text}");
    let mut back = World::new();
    engine_scene::load(&mut back, &registry, &text).unwrap();
    let e = engine_scene::find_named(&mut back, "Two").unwrap();
    assert_eq!(
        back.get::<MaterialSlots>(e),
        Some(&MaterialSlots(vec![green]))
    );
}
