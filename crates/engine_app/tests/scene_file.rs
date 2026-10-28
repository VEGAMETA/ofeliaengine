//! A scene file (ADR 0108) drawn by the player: built in code and saved as the editor will
//! save it, with a material file beside it, then loaded through `PlayerSettings::scene` and
//! rendered offscreen: the red cube stands on the grey floor in the middle of the picture,
//! as in the quick start. `ENGINE_SCENE_FILE_PNG=<file>` writes the frame.
//!
//! The same with models from a glTF file a blockout bake wrote (ADR 0110 E2b-6): the red
//! crate on the grey floor, both over the grid the file carries.
//! `ENGINE_SCENE_FILE_GLB_PNG=<file>` writes that frame.
//!
//! And with typed material files (ADR 0120's E10-1b): a cube draws its file's look, then its
//! type once the type is ready, off the frame's thread.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{AssetMeta, Assets, BakedBrush, PREVIEW_SIZE, PreviewRequest};
use engine_render::material::MaterialTypeId;
use engine_scene::{SaveOptions, SceneRegistry, save_with};
use std::time::{Duration, Instant};

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
        prefabs: None,
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

/// A typed material file (ADR 0120's E10-1b), the golden test bed's stripes in one red: the
/// cube draws the file's look (green) at once, then its type once the type's layout is
/// reflected and its pipelines are made, neither on the frame's thread. A second cube's
/// fabric takes the image of its texture parameter, loaded in the background. The file
/// written anew (blue) is read again, the red drawn until the blue is laid out.
#[test]
fn a_typed_material_file_draws_its_look_then_its_type() {
    let dir = std::env::temp_dir().join(format!("ofelia-typed-material-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["materials", "scenes", "textures"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(
        dir.join("materials/stripes.slang"),
        include_str!("../../../tests/golden/materials/stripes.slang"),
    )
    .unwrap();
    std::fs::write(
        dir.join("materials/fabric.slang"),
        include_str!("../../../tests/golden/materials/fabric.slang"),
    )
    .unwrap();
    let white =
        engine_render::png::encode_png(4, 4, engine_render::png::PngFormat::Rgba, &[255u8; 64])
            .unwrap();
    std::fs::write(dir.join("textures/white.png"), white).unwrap();
    let (stripes_id, stripes_module) = (AssetId::new(), AssetId::new());
    let (fabric_id, fabric_module, white_id) = (AssetId::new(), AssetId::new(), AssetId::new());
    let stripes = |color: &str| {
        format!(
            "[material version=2 id={stripes_id}]\n\nshader = asset(\"materials/stripes.slang\", \
             {stripes_module})\ntype = \"StripesMaterial\"\nnumbers[\"frequency\"] = 0.01\n\
             numbers[\"roughness\"] = 0.5\nvectors[\"color_a\"] = [{color}, 1]\n\
             vectors[\"color_b\"] = [{color}, 1]\nlook = Own\nbase_color = [0.05, 0.8, 0.05, 1]\n"
        )
    };
    std::fs::write(
        dir.join("materials/stripes.material"),
        stripes("0.9, 0.05, 0.05"),
    )
    .unwrap();
    std::fs::write(
        dir.join("materials/fabric.material"),
        format!(
            "[material version=2 id={fabric_id}]\n\nshader = asset(\"materials/fabric.slang\", \
             {fabric_module})\ntype = \"FabricMaterial\"\nvectors[\"tint\"] = [1, 1, 1, 1]\n\
             textures[\"albedo\"].image = asset(\"textures/white.png\", {white_id})\n\
             textures[\"albedo\"].encoding = Color\nlook = Own\n"
        ),
    )
    .unwrap();
    let (stripes_ref, fabric_ref) = (AssetRef::file(stripes_id), AssetRef::file(fabric_id));

    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Striped"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, stripes_ref),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Cloth"),
        Transform::from_xyz(0.0, 0.0, -6.0),
        Model::new(builtin::CUBE, fabric_ref),
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
    paths.insert(stripes_ref, "materials/stripes.material");
    paths.insert(fabric_ref, "materials/fabric.material");
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (320u32, 180u32);
    let settings = PlayerSettings {
        title: String::from("typed material test"),
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
    let middle = |rgba: &[u8]| {
        let i = (((height / 2) * width + width / 2) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // At once: the file's look.
    for _ in 0..2 {
        player.frame(&mut game).unwrap();
    }
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(g > 1.5 * r && g > 1.5 * b, "the look first: {r} {g} {b}");
    let named = |player: &mut Player, name: &str| {
        let world = player.world_mut();
        let mut names = QueryState::<(Entity, &Name)>::new(world);
        let e = names
            .iter_readonly(world)
            .find(|(_, n)| n.as_str() == name)
            .map(|(e, _)| e)
            .unwrap();
        world.get::<MeshRenderer>(e).unwrap().material
    };
    let (striped, cloth) = (named(&mut player, "Striped"), named(&mut player, "Cloth"));
    // Then its type: laid out, its pipelines made (some frames after it became typed),
    // and the fabric given its image.
    let wait = |player: &mut Player, game: &mut Still, material: MaterialId| {
        let started = Instant::now();
        let mut typed_for = 0;
        let mut longest = Duration::ZERO;
        loop {
            let frame = Instant::now();
            player.frame(game).unwrap();
            longest = longest.max(frame.elapsed());
            let materials = &*player.setup().materials;
            let typed = materials
                .material_type(material)
                .is_some_and(|t| t != MaterialTypeId::GLTF);
            let images = materials
                .typed_params(cloth)
                .map_or(0, |p| p.textures.len());
            typed_for = if typed { typed_for + 1 } else { 0 };
            let pending = player.renderer().forward().material_types_pending();
            if typed_for >= 3 && images == 1 && pending == 0 {
                return (started.elapsed(), longest);
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "typed for {typed_for} frames, {images} images, {pending} pipelines pending"
            );
        }
    };
    let (took, longest) = wait(&mut player, &mut game, striped);
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(r > 1.5 * g && r > 1.5 * b, "its type: {r} {g} {b}");
    assert_eq!(player.assets().problem(stripes_ref), None);
    assert_eq!(player.assets().problem(fabric_ref), None);
    println!(
        "typed after {:.2} s, the longest frame meanwhile {:.1} ms",
        took.as_secs_f64(),
        longest.as_secs_f64() * 1e3
    );

    // Its preview (the editor's thumbnail) shows the type too: drawn once for its
    // pipelines, answered once they are made.
    player.request_preview(PreviewRequest {
        key: 7,
        parts: vec![(builtin::CUBE, stripes_ref, Affine3A::IDENTITY)],
    });
    let started = Instant::now();
    let mut previews = Vec::new();
    while previews.is_empty() {
        player.frame(&mut game).unwrap();
        player.take_previews(&mut previews);
        assert!(started.elapsed() < Duration::from_secs(60), "no preview");
    }
    let side = PREVIEW_SIZE as usize;
    let at = (side / 2 * side + side / 2) * 4;
    let [r, g, b] = [0, 1, 2].map(|c| f32::from(previews[0].pixels[at + c]));
    assert!(r > 1.5 * g && r > 1.5 * b, "the preview: {r} {g} {b}");

    // Written anew: read again, the red drawn until the blue is laid out.
    std::fs::write(
        dir.join("materials/stripes.material"),
        stripes("0.05, 0.05, 0.9"),
    )
    .unwrap();
    player.asset_changed(stripes_id, "materials/stripes.material");
    let started = Instant::now();
    let blue = loop {
        player.frame(&mut game).unwrap();
        let now = named(&mut player, "Striped");
        if now != striped {
            break now;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never read again"
        );
    };
    wait(&mut player, &mut game, blue);
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(b > 1.5 * r && b > 1.5 * g, "written anew: {r} {g} {b}");
    println!(
        "written anew, drawn in {:.3} s",
        started.elapsed().as_secs_f64()
    );

    // Held by the editor (ADR 0120's E10-1c): a yellow version in place of the file, the
    // same material laid out again by the next frame, never its look in between.
    let (held, report) = MaterialFile::parse(&stripes("0.9, 0.9, 0.05")).unwrap();
    player.set_material_file(stripes_id, Some((held, report.assets)));
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
        assert_eq!(named(&mut player, "Striped"), blue, "the same material");
        let ty = player.setup().materials.material_type(blue);
        assert!(
            ty.is_some_and(|t| t != MaterialTypeId::GLTF),
            "never its look"
        );
    }
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(r > 1.5 * b && g > 1.5 * b, "held: {r} {g} {b}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A material file naming a material graph (ADR 0123's E10-3c): the cube draws the file's
/// look (green) at once, then the graph's surface (red, unlit) once its module is generated
/// and compiled; the graph written anew (blue) is read again and its module compiled again,
/// the red drawn until then.
#[test]
fn a_material_naming_a_graph_draws_the_graph_and_follows_its_edits() {
    let dir = std::env::temp_dir().join(format!("ofelia-graph-material-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["materials", "scenes"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let (graph_id, material_id) = (AssetId::new(), AssetId::new());
    let glow = |color: &str| {
        format!(
            "[mgraph version=1 id={graph_id}]\n\nmodel = \"unlit\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [{color}, 1]\n\
             nodes[\"out\"].kind = \"output\"\nlinks[\"out.base\"] = \"c.rgba\"\n"
        )
    };
    std::fs::write(dir.join("materials/glow.mgraph"), glow("0.9, 0.05, 0.05")).unwrap();
    std::fs::write(
        dir.join("materials/glow.material"),
        format!(
            "[material version=2 id={material_id}]\n\nshader = asset(\"materials/glow.mgraph\", \
             {graph_id})\ntype = \"GraphMaterial\"\nlook = Own\n\
             base_color = [0.05, 0.8, 0.05, 1]\n"
        ),
    )
    .unwrap();
    let glow_ref = AssetRef::file(material_id);
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Glowing"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, glow_ref),
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(glow_ref, "materials/glow.material");
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (320u32, 180u32);
    let settings = PlayerSettings {
        title: String::from("graph material test"),
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
    // As the editor runs it: a module changed on disk compiles again.
    player.set_material_hot_reload(true);
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    let middle = |rgba: &[u8]| {
        let i = (((height / 2) * width + width / 2) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    for _ in 0..2 {
        player.frame(&mut game).unwrap();
    }
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(g > 1.5 * r && g > 1.5 * b, "the look first: {r} {g} {b}");
    // Frames until the middle is `channel`'s (0 red, 2 blue), at most 90 s.
    let until = |player: &mut Player, game: &mut Still, channel: usize| {
        let started = Instant::now();
        loop {
            player.frame(game).unwrap();
            let pending = player.renderer().forward().material_types_pending();
            if pending == 0 {
                let c = middle(&player.capture(game).unwrap().unwrap());
                let others = (0..3).filter(|&k| k != channel).map(|k| c[k]);
                if others.clone().all(|o| c[channel] > 1.5 * o) {
                    return started.elapsed();
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "never channel {channel}: {pending} pipelines pending"
            );
        }
    };
    let took = until(&mut player, &mut game, 0);
    assert_eq!(player.assets().problem(glow_ref), None);
    println!("the graph drawn after {:.2} s", took.as_secs_f64());
    // The graph written anew: read again, its module made anew and compiled again.
    std::fs::write(dir.join("materials/glow.mgraph"), glow("0.05, 0.05, 0.9")).unwrap();
    player.asset_changed(graph_id, "materials/glow.mgraph");
    let took = until(&mut player, &mut game, 2);
    println!("the graph's edit drawn after {:.2} s", took.as_secs_f64());
    let module = dir
        .join(".ofelia/cache/graphs")
        .join(format!("{graph_id}.slang"));
    assert!(
        std::fs::read_to_string(module)
            .unwrap()
            .contains("0.9, 1.0)")
    );
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
        prefabs: None,
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

/// The view in a window of its own (ADR 0107 E3-4), offscreen: the scene drawn into it at
/// its size, then, closed, into the main output again.
#[test]
fn the_view_draws_into_a_window_of_its_own() {
    let dir = std::env::temp_dir().join(format!("ofelia-view-window-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let red_id = AssetId::new();
    let red = MaterialFile {
        base_color: [0.8, 0.15, 0.1, 1.0],
        ..MaterialFile::default()
    };
    std::fs::write(dir.join("red.material"), red.to_text(red_id)).unwrap();
    let red_ref = AssetRef::file(red_id);
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, red_ref),
    ));
    world.spawn((
        SceneId::new(),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    world.spawn((
        SceneId::new(),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(red_ref, "red.material");
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &SaveOptions {
            id: Some(AssetId::new()),
            assets: Some(&paths),
            prefabs: None,
        },
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let (width, height) = (640u32, 360u32);
    let settings = PlayerSettings {
        title: String::from("view window test"),
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
    let (w, h) = (320u32, 180u32);
    player.open_view_window("the view", w, h, None).unwrap();
    assert!(player.has_view_window());
    for _ in 0..20 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player
        .capture_view_window(&mut game, None)
        .unwrap()
        .unwrap();
    assert_eq!(rgba.len(), (w * h * 4) as usize);
    let red_at = |rgba: &[u8], width: u32, x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from);
        r > 1.5 * g && r > 1.5 * b
    };
    assert!(
        red_at(&rgba, w, w / 2, h / 2),
        "the cube in the view's window"
    );
    // Closed: the view draws into the main output again.
    player.close_view_window();
    assert!(!player.has_view_window());
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    assert!(
        red_at(&rgba, width, width / 2, height / 2),
        "the cube in the main output"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scene placing a prefab twice (ADR 0108's E5): the player reads the prefab from the
/// project's folder and draws both lamps where their instances put them.
#[test]
fn a_scene_places_its_prefabs() {
    let dir = std::env::temp_dir().join(format!("ofelia-scene-prefabs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    // The lamp: a cube on a pole, its own scene.
    let (lamp_id, root, bulb) = (AssetId::new(), SceneId::new(), SceneId::new());
    let mut lamp = World::new();
    let pole = lamp.spawn((
        root,
        Name::new("Lamp"),
        Transform::IDENTITY,
        Model::new(builtin::CYLINDER, builtin::MATERIAL),
    ));
    let light = lamp.spawn((
        bulb,
        Name::new("Bulb"),
        Transform::from_xyz(0.0, 1.0, 0.0),
        Model::new(builtin::SPHERE, builtin::MATERIAL),
    ));
    lamp.set_parent(light, Some(pole)).unwrap();
    let paths = Assets::new(dir.clone()).paths().clone();
    let mut text = String::new();
    save_with(
        &mut lamp,
        &SceneRegistry::from_program(),
        &SaveOptions {
            id: Some(lamp_id),
            assets: Some(&paths),
            prefabs: None,
        },
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("props/lamp.scene"), &text).unwrap();
    // Two lamps, the second one's bulb renamed.
    let (a, b) = (SceneId::new(), SceneId::new());
    let scene = format!(
        "[scene version=4 id={main}]\n\n[instance {a}]\nprefab = asset(\"props/lamp.scene\", \
         {lamp_id})\nTransform.translation = (-2, 0, 0)\n\n[instance {b}]\nprefab = \
         asset(\"props/lamp.scene\", {lamp_id})\nTransform.translation = (2, 0, 0)\n\n\
         [override {b}/{bulb}]\nName = \"Red bulb\"\n",
        main = AssetId::new(),
    );
    std::fs::write(dir.join("main.scene"), &scene).unwrap();
    let settings = PlayerSettings {
        title: String::from("prefab scene test"),
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
    let report = player.load_scene("main.scene").unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.entities.len(), 4, "two lamps of two entities each");
    let mut game = Still;
    player.frame(&mut game).unwrap();
    let world = player.world_mut();
    let lamps: Vec<Entity> = report
        .entities
        .iter()
        .copied()
        .filter(|&e| world.get::<Name>(e).is_some_and(|n| n.as_str() == "Lamp"))
        .collect();
    assert_eq!(lamps.len(), 2);
    let xs: Vec<f32> = lamps
        .iter()
        .map(|&e| world.get::<Transform>(e).unwrap().translation.x)
        .collect();
    assert!(xs.contains(&-2.0) && xs.contains(&2.0), "{xs:?}");
    for &e in &report.entities {
        assert!(world.get::<MeshRenderer>(e).is_some(), "every part drawn");
    }
    let red = report
        .entities
        .iter()
        .filter(|&&e| {
            world
                .get::<Name>(e)
                .is_some_and(|n| n.as_str() == "Red bulb")
        })
        .count();
    assert_eq!(red, 1);
    let _ = std::fs::remove_dir_all(&dir);
}
