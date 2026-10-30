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
use engine_runtime::SkinnedMesh;
use engine_scene::{AssetPaths, SaveOptions, SceneRegistry, save_with};
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
        pipeline_statistics: true,
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
        same_code: false,
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

    // Its preview again, only its values changed (ADR 0123's E10-3f-2): drawn once with
    // the type's pipelines made, answered as soon as that frame is done (a request that
    // first asks for them takes two drawings, five frames).
    player.request_preview(PreviewRequest {
        key: 7,
        parts: vec![(builtin::CUBE, stripes_ref, Affine3A::IDENTITY)],
        same_code: true,
    });
    previews.clear();
    let mut frames = 0;
    while previews.is_empty() {
        player.frame(&mut game).unwrap();
        player.take_previews(&mut previews);
        frames += 1;
        assert!(frames < 600, "no preview");
    }
    let [r, g, b] = [0, 1, 2].map(|c| f32::from(previews[0].pixels[at + c]));
    assert!(
        r > 1.5 * b && g > 1.5 * b,
        "the held values' preview: {r} {g} {b}"
    );
    assert!(frames <= 3, "drawn once: answered after {frames} frames");

    // What its type costs this GPU (E10-4c): its fragment registers in the variant drawn,
    // beside the engine's own shading's.
    let ty = player.material_type(&dir.join("materials/stripes.slang"), "StripesMaterial");
    let started = Instant::now();
    let cost = loop {
        if let Some(cost) = player.material_cost(ty) {
            break cost;
        }
        player.frame(&mut game).unwrap();
        assert!(started.elapsed() < Duration::from_secs(60), "no cost");
    };
    let plain = cost.variants[0].expect("its plain shading made");
    assert!(plain.registers > 0, "{cost:?}");
    assert!(plain.engine.is_some_and(|e| e > 0), "{cost:?}");
    println!(
        "the stripes' fragment registers {} (the engine's {:?}), {} B spilled, read after {:.2} s",
        plain.registers,
        plain.engine,
        plain.spill,
        started.elapsed().as_secs_f64()
    );
    // Its code (E10-4f): the instructions and image samples its entry point reaches,
    // beside the engine's own shading's, and its last compile.
    let code = plain.code.expect("its code read");
    let engine_code = plain.engine_code.expect("the engine's code read");
    assert!(
        code.instructions > 0 && engine_code.instructions > 0,
        "{cost:?}"
    );
    let (took, entries) = cost.compiled.expect("its last compile");
    assert!(took > Duration::ZERO && entries > 0, "{cost:?}");
    println!(
        "the stripes' code {code:?} (the engine's {engine_code:?}), compiled in {:.2} s ({entries})",
        took.as_secs_f64()
    );
    // Drawn as its fallback (E10-4f): its look (green) until that ends, then itself
    // (the held yellow) again.
    player.draw_as_fallback(stripes_ref, true);
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(
        g > 1.5 * r && g > 1.5 * b,
        "drawn as its fallback: {r} {g} {b}"
    );
    player.draw_as_fallback(stripes_ref, false);
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    let [r, g, b] = middle(&player.capture(&mut game).unwrap().unwrap());
    assert!(r > 1.5 * b && g > 1.5 * b, "itself again: {r} {g} {b}");
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

/// A material file naming a graph that calls a function (ADR 0123's E10-3g-2): the cube
/// draws the function's colour (red) once the graph's module is made with the function's
/// nodes; the function written anew (blue) makes the module again, as the graph's own edit
/// does.
#[test]
fn a_graph_calling_a_function_follows_the_functions_edits() {
    let dir = std::env::temp_dir().join(format!("ofelia-function-material-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["materials", "scenes"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let (function_id, graph_id, material_id) = (AssetId::new(), AssetId::new(), AssetId::new());
    let tint = |color: &str| {
        format!(
            "[mfunc version=1 id={function_id}]\n\n\
             nodes[\"c\"].kind = \"vector3\"\nnodes[\"c\"].value = [{color}]\n\
             nodes[\"o\"].kind = \"output_float3\"\nnodes[\"o\"].param = \"color\"\n\
             links[\"o.value\"] = \"c.value\"\n"
        )
    };
    std::fs::write(dir.join("materials/tint.mfunc"), tint("0.9, 0.05, 0.05")).unwrap();
    std::fs::write(
        dir.join("materials/tinted.mgraph"),
        format!(
            "[mgraph version=3 id={graph_id}]\n\nmodel = \"unlit\"\n\
             nodes[\"f\"].kind = \"function\"\nnodes[\"f\"].function = \"tint\"\n\
             nodes[\"f\"].module = asset(\"materials/tint.mfunc\", {function_id})\n\
             nodes[\"f\"].signature = \"void(out float3 color)\"\n\
             nodes[\"out\"].kind = \"output\"\nlinks[\"out.base\"] = \"f.color\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("materials/tinted.material"),
        format!(
            "[material version=2 id={material_id}]\n\nshader = asset(\"materials/tinted.mgraph\", \
             {graph_id})\ntype = \"GraphMaterial\"\nlook = Own\n\
             base_color = [0.05, 0.8, 0.05, 1]\n"
        ),
    )
    .unwrap();
    let tinted = AssetRef::file(material_id);
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Tinted"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, tinted),
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(tinted, "materials/tinted.material");
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
        title: String::from("function material test"),
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
    player.set_material_hot_reload(true);
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    let middle = |rgba: &[u8]| {
        let i = (((height / 2) * width + width / 2) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
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
                "never channel {channel}: {pending} pipelines pending, {:?}",
                player.assets().problem(tinted)
            );
        }
    };
    let took = until(&mut player, &mut game, 0);
    assert_eq!(player.assets().problem(tinted), None);
    println!(
        "the function's colour drawn after {:.2} s",
        took.as_secs_f64()
    );
    // The function written anew: the graph's module made again with its new nodes.
    std::fs::write(dir.join("materials/tint.mfunc"), tint("0.05, 0.05, 0.9")).unwrap();
    player.asset_changed(function_id, "materials/tint.mfunc");
    let took = until(&mut player, &mut game, 2);
    println!(
        "the function's edit drawn after {:.2} s",
        took.as_secs_f64()
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

/// E10-4a: a material variant (ADR 0120's addendum) draws its base's colour, which it leaves
/// to its base; the base's file written anew, the variant follows; the base held by the
/// editor, the variant follows that too, its own roughness kept throughout.
#[test]
fn a_variant_draws_its_base_and_follows_it() {
    let dir = std::env::temp_dir().join(format!("ofelia-variant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join("scenes")).unwrap();
    let (red_id, rough_id) = (AssetId::new(), AssetId::new());
    let base = |base_color: [f32; 4]| MaterialFile {
        base_color,
        roughness: 0.4,
        ..MaterialFile::default()
    };
    let (red, green, blue) = (
        [0.8, 0.15, 0.1, 1.0],
        [0.1, 0.8, 0.15, 1.0],
        [0.1, 0.15, 0.8, 1.0],
    );
    std::fs::write(
        dir.join("materials/red.material"),
        base(red).to_text(red_id),
    )
    .unwrap();
    let (red_ref, rough_ref) = (AssetRef::file(red_id), AssetRef::file(rough_id));
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(red_ref, "materials/red.material");
    paths.insert(rough_ref, "materials/rough.material");
    let mut variant = base(red);
    variant.base = red_ref;
    variant.roughness = 0.9;
    let text = variant.to_variant_text(rough_id, &paths, &base(red));
    assert!(!text.contains("base_color"), "{text}");
    std::fs::write(dir.join("materials/rough.material"), &text).unwrap();

    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Cube"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, rough_ref),
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
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut scene = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut scene,
    )
    .unwrap();
    std::fs::write(dir.join("scenes/main.scene"), &scene).unwrap();
    let (width, height) = (320u32, 180u32);
    let settings = PlayerSettings {
        title: String::from("variant test"),
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
    let middle = |player: &mut Player, game: &mut Still| {
        let rgba = player.capture(game).unwrap().unwrap();
        let i = (((height / 2) * width + width / 2) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
    };
    // Waits until the cube's middle is mostly channel `c`: the seconds it took.
    let until = |player: &mut Player, game: &mut Still, c: usize| {
        let started = Instant::now();
        loop {
            player.frame(game).unwrap();
            let px = middle(player, game);
            if (0..3).all(|k| k == c || px[c] > 1.5 * px[k]) {
                return started.elapsed().as_secs_f64();
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "not drawn: {px:?} (channel {c})"
            );
        }
    };
    let first = until(&mut player, &mut game, 0);
    assert_eq!(player.assets().problem(rough_ref), None);
    // Its base written anew: the variant follows.
    std::fs::write(
        dir.join("materials/red.material"),
        base(green).to_text(red_id),
    )
    .unwrap();
    player.asset_changed(red_id, "materials/red.material");
    let followed = until(&mut player, &mut game, 1);
    // Its base held by the editor: followed too.
    player.set_material_file(red_id, Some((base(blue), AssetPaths::new())));
    let held = until(&mut player, &mut game, 2);
    println!(
        "the variant drawn in its base's red after {first:.2} s; its base's file written \
         anew, followed in {followed:.3} s; held by the editor, in {held:.3} s"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A material graph's node previews (ADR 0123's E10-4g): from its editing build, an atlas
/// of its nodes' values, a tile each in the order `node_preview` numbers them: a red and a
/// green colour over flat squares, their sum yellow, and the surface's normal on a sphere,
/// blue where it faces the camera and transparent around it.
#[test]
fn a_graphs_node_previews_draw_each_nodes_value() {
    use engine_app::{NODE_TILE, NodePreviewRequest};
    use engine_material_graph::graph::OUTPUT;
    use engine_material_graph::{Build, Graph, Model, Node, ParamType, PortRef, generate};
    let dir = std::env::temp_dir().join(format!("ofelia-node-previews-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    let mut graph = Graph::new(Model::Unlit);
    graph
        .add(
            "a",
            Node::new("color", [0.0; 2]).with_value([0.9, 0.05, 0.05, 1.0]),
        )
        .add(
            "b",
            Node::new("color", [0.0; 2]).with_value([0.05, 0.9, 0.05, 1.0]),
        )
        .add("c", Node::new("add", [0.0; 2]))
        .add("n", Node::new("normal", [0.0; 2]))
        .link(PortRef::new("a", "rgba"), PortRef::new("c", "a"))
        .link(PortRef::new("b", "rgba"), PortRef::new("c", "b"))
        .link(PortRef::new("c", "out"), PortRef::new(OUTPUT, "base"));
    let generated = generate(&graph, "Previewed", Build::Editing).unwrap();
    let order: Vec<&str> = generated.previews.iter().map(|p| p.node.as_str()).collect();
    assert_eq!(order, ["a", "b", "c", "n"]);
    // Its module and a material of it, each parameter its node's value: what the editor's
    // graph preview holds.
    std::fs::write(dir.join("materials/previewed.slang"), &generated.text).unwrap();
    let (material_id, module_id) = (AssetId::new(), AssetId::new());
    let mut text = format!(
        "[material version=2 id={material_id}]\n\nshader = asset(\"materials/previewed.slang\", \
         {module_id})\ntype = \"Previewed\"\nlook = Own\n"
    );
    for p in &generated.params {
        match p.ty {
            ParamType::Float(1) => {
                text.push_str(&format!("numbers[\"{}\"] = {}\n", p.name, p.value[0]))
            }
            ParamType::Float(w) => {
                let parts: Vec<String> = p.value[..usize::from(w)]
                    .iter()
                    .map(f32::to_string)
                    .collect();
                text.push_str(&format!(
                    "vectors[\"{}\"] = [{}]\n",
                    p.name,
                    parts.join(", ")
                ));
            }
            ParamType::Texture => {}
        }
    }
    std::fs::write(dir.join("materials/previewed.material"), text).unwrap();
    let material = AssetRef::file(material_id);
    let mut paths = AssetPaths::new();
    paths.insert(material, "materials/previewed.material");

    let settings = PlayerSettings {
        title: String::from("node previews test"),
        window: Window::Offscreen {
            width: 64,
            height: 64,
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
    player.assets_mut().add_paths(&paths);
    let mut game = Still;
    player.request_node_previews(NodePreviewRequest {
        key: 9,
        tag: 4,
        material,
        spheres: generated.previews.iter().map(|p| p.sphere).collect(),
    });
    let started = Instant::now();
    let mut atlases = Vec::new();
    while atlases.is_empty() {
        player.frame(&mut game).unwrap();
        player.take_node_previews(&mut atlases);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no node previews"
        );
    }
    println!(
        "node previews drawn after {:.2} s",
        started.elapsed().as_secs_f64()
    );
    let atlas = &atlases[0];
    assert_eq!(
        (atlas.key, atlas.tag, atlas.columns, atlas.count),
        (9, 4, 4, 4)
    );
    let width = (atlas.columns * NODE_TILE) as usize;
    assert_eq!(atlas.pixels.len(), width * NODE_TILE as usize * 4);
    let at = |tile: u32, x: u32, y: u32| {
        let px = ((tile % atlas.columns) * NODE_TILE + x) as usize;
        let py = ((tile / atlas.columns) * NODE_TILE + y) as usize;
        let i = (py * width + px) * 4;
        [0, 1, 2, 3].map(|c| f32::from(atlas.pixels[i + c]))
    };
    let middle = NODE_TILE / 2;
    let [r, g, b, a] = at(0, middle, middle);
    assert!(
        r > 1.5 * g && r > 1.5 * b && a > 250.0,
        "red: {r} {g} {b} {a}"
    );
    let [r, g, b, _] = at(1, middle, middle);
    assert!(g > 1.5 * r && g > 1.5 * b, "green: {r} {g} {b}");
    let [r, g, b, _] = at(2, middle, middle);
    assert!(r > 1.5 * b && g > 1.5 * b, "their sum, yellow: {r} {g} {b}");
    let [r, g, b, a] = at(3, middle, middle);
    assert!(
        b > 1.5 * r && b > 1.5 * g && a > 250.0,
        "the normal, facing: {r} {g} {b} {a}"
    );
    let [.., a] = at(3, 1, 1);
    assert!(a < 1.0, "around the sphere: {a}");
    let [.., a] = at(0, 1, 1);
    assert!(a > 250.0, "a flat square to its corners: {a}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1a: a scene's model of a skinned glTF mesh (Khronos' CesiumMan, from
/// `tools/fetch-test-assets.ps1`; skipped without it) is drawn skinned: its rig comes with
/// the file's clip, a clip's moment poses it elsewhere than its rest pose, and a model that
/// goes lets its skin's instance go. `ENGINE_SKINNED_PNG=<dir>` writes both frames.
#[test]
fn a_skinned_model_is_drawn_skinned_and_posed_at_a_clips_moment() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-skinned-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::create_dir_all(dir.join("scenes")).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(mesh, "models/CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "models/CesiumMan.gltf#Cesium_Man-effect");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Man"),
        Transform::from_rotation(Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2)),
        Model::new(mesh, material),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Floor"),
        Transform::from_xyz(0.0, -0.05, 0.0).with_scale(Vec3::new(6.0, 0.1, 6.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.2),
        },
    ));
    let eye = Vec3::new(0.0, 0.8, 2.6);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(
            eye,
            Vec3::new(0.0, 0.7, 0.0),
            Vec3::Y,
        )),
        Camera::perspective(1.0, 0.05, 100.0),
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
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (480u32, 480u32);
    let settings = PlayerSettings {
        title: String::from("skinned model test"),
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
    let world = player.world_mut();
    let mut names = QueryState::<(Entity, &Name)>::new(world);
    let man = names
        .iter_readonly(world)
        .find(|(_, name)| name.as_str() == "Man")
        .map(|(e, _)| e)
        .unwrap();
    // Its skin made in the background, its rig with the file's clip.
    let started = Instant::now();
    while player.model_rig(man).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no skin: {:?}",
            player.assets().problem(mesh)
        );
        player.frame(&mut game).unwrap();
    }
    let made = started.elapsed();
    let rig = player.model_rig(man).unwrap();
    let clip = (
        rig.clips().len(),
        rig.clips()[0].duration(),
        rig.joint_count(),
    );
    println!(
        "skinned after {:.0} ms: {} joints, {} clip(s), the first {:.2} s",
        made.as_secs_f64() * 1e3,
        clip.2,
        clip.0,
        clip.1
    );
    assert!(clip.0 >= 1 && clip.1 > 0.0);
    assert!(player.world_mut().get::<SkinnedMesh>(man).is_some());
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let rest = player.capture(&mut game).unwrap().unwrap();
    // Halfway through its walk.
    assert!(player.pose_model(man, Some((0, clip.1 * 0.5))));
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let walking = player.capture(&mut game).unwrap().unwrap();
    if let Ok(out) = std::env::var("ENGINE_SKINNED_PNG") {
        for (name, shot) in [("rest.png", &rest), ("walking.png", &walking)] {
            let png = engine_render::png::encode_png(
                width,
                height,
                engine_render::png::PngFormat::Rgba,
                shot,
            )
            .unwrap();
            std::fs::write(std::path::Path::new(&out).join(name), png).unwrap();
        }
    }
    let changed = |a: &[u8], b: &[u8]| {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(a, b)| (0..3).any(|c| a[c].abs_diff(b[c]) > 40))
            .count()
    };
    // Material slots: a skin a primitive (one here), made anew, its instance replacing the
    // first; drawn in the rest pose again.
    player
        .world_mut()
        .insert(man, engine_app::MaterialSlots(vec![material]));
    player.frame(&mut game).unwrap();
    let started = Instant::now();
    while player.world_mut().get::<SkinnedMesh>(man).is_none() || player.model_rig(man).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin again");
        player.frame(&mut game).unwrap();
    }
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let slotted = player.capture(&mut game).unwrap().unwrap();
    let again = changed(&rest, &slotted);
    assert!(again < 200, "the rest pose again: {again} pixels differ");
    // A model that goes takes its skin's instance with it.
    player.world_mut().remove::<Model>(man);
    player.frame(&mut game).unwrap();
    assert!(player.world_mut().get::<SkinnedMesh>(man).is_none());
    assert!(player.world_mut().get::<MeshRenderer>(man).is_none());
    assert!(player.model_rig(man).is_none());
    assert!(!player.pose_model(man, None));
    let gone = player.capture(&mut game).unwrap().unwrap();
    if let Ok(out) = std::env::var("ENGINE_SKINNED_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &gone,
        )
        .unwrap();
        std::fs::write(std::path::Path::new(&out).join("gone.png"), png).unwrap();
    }
    let (drawn, moved) = (changed(&rest, &gone), changed(&rest, &walking));
    println!("the man covers {drawn} pixels; the pose moved {moved}");
    assert!(drawn > 2_000, "the man drawn: {drawn}");
    assert!(moved > 500, "the pose moved him: {moved}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-2a: a character's `Animator` plays its clip on the game's clock: its time
/// moves on each tick and wraps at the clip's end, each frame poses the model between the
/// last two ticks, a pose the editor holds wins until it lets go, and while the world stands
/// still (as the editor edits) the character stands at its clip's `time`.
#[test]
fn an_animator_plays_its_clip_on_the_ticks_and_poses_each_frame() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-animator-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(mesh, "models/CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "models/CesiumMan.gltf#Cesium_Man-effect");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Man"),
        Transform::from_rotation(Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2)),
        Model::new(mesh, material),
        // A name his file has not: his first clip.
        Animator::new("Stroll"),
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
    assert!(text.contains("Animator"), "saved with the scene: {text}");
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("animator test"),
        window: Window::Offscreen {
            width: 128,
            height: 128,
        },
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
    let man = find_named(player.world_mut(), "Man").unwrap();
    let started = Instant::now();
    while player.model_rig(man).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let length = player.model_rig(man).unwrap().clips()[0].duration();
    let animator = |player: &mut Player| player.world_mut().get::<Animator>(man).unwrap().clone();
    // On the ticks: a sixtieth of a second each, wrapped at the clip's end.
    player.frame(&mut game).unwrap();
    let before = animator(&mut player).time;
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
    }
    let after = animator(&mut player);
    let moved = (after.time - before).rem_euclid(length);
    println!(
        "the clip ({length:.2} s) moved {moved:.4} s in 30 frames, now at {:.4}",
        after.time
    );
    let dt = player.runtime().clock.dt();
    assert!(
        (moved - 0.5).abs() <= dt + 1e-4,
        "half a second, to a tick: {moved}"
    );
    assert_eq!(after.length, length, "its length known to its ticks");
    assert!((0.0..length).contains(&after.time));
    // Each frame: the pose between the last two ticks, as far between them as the frame.
    let mut drawn = Vec::new();
    assert!(player.model_joints(man, &mut drawn));
    let expected = after.at(player.runtime().clock.alpha(), length);
    assert!(player.pose_model(man, Some((0, expected))));
    let mut held = Vec::new();
    assert!(player.model_joints(man, &mut held));
    let off = drawn
        .iter()
        .zip(&held)
        .map(|(a, b)| (Vec3::from(a.translation) - Vec3::from(b.translation)).length())
        .fold(0.0f32, f32::max);
    assert!(
        off < 1e-5,
        "the frame's pose is the clip at {expected}: {off} m off"
    );
    // Held by the editor: the pose stays while the clip goes on; let go, the animator's.
    assert!(player.pose_model(man, Some((0, 0.0))));
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let mut still = Vec::new();
    player.model_joints(man, &mut still);
    let mut first = Vec::new();
    assert!(player.pose_model(man, Some((0, 0.0))));
    player.model_joints(man, &mut first);
    assert_eq!(still, first, "held at the clip's start");
    assert!(player.pose_model(man, None));
    player.frame(&mut game).unwrap();
    let mut back = Vec::new();
    player.model_joints(man, &mut back);
    assert_ne!(back, first, "let go: the animator's pose again");
    // The world standing still, as while the editor edits: so does he.
    player.set_simulating(false);
    player.frame(&mut game).unwrap();
    let time = animator(&mut player).time;
    let mut a = Vec::new();
    player.model_joints(man, &mut a);
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let mut b = Vec::new();
    player.model_joints(man, &mut b);
    assert_eq!(animator(&mut player).time, time, "the clock stopped");
    assert_eq!(a, b, "and the pose with it");
    // Paused while the world goes on: held too.
    player.set_simulating(true);
    player.world_mut().get_mut::<Animator>(man).unwrap().paused = true;
    player.frame(&mut game).unwrap();
    let time = animator(&mut player).time;
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(animator(&mut player).time, time, "paused");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-2b: an animator fading from one clip into another. The frames of the
/// fade show neither clip, nearer each than the two are to each other; a save keeps the
/// fade; once it is over, the new clip alone.
#[test]
fn an_animator_fades_from_one_clip_into_another() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Fox");
    if !source.join("Fox.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-fade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for file in ["Fox.gltf", "Fox.bin", "Texture.png"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/Fox.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "fox1"),
        AssetRef::part(id, "fox_material"),
    );
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(mesh, "models/Fox.gltf#fox1");
    paths.insert(material, "models/Fox.gltf#fox_material");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Fox"),
        Transform::from_scale(Vec3::splat(0.01)),
        Model::new(mesh, material),
        Animator::new("Survey"),
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let registry = SceneRegistry::from_program();
    let mut text = String::new();
    save_with(&mut world, &registry, &options, &mut text).unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("fade test"),
        window: Window::Offscreen {
            width: 128,
            height: 128,
        },
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
    let fox = find_named(player.world_mut(), "Fox").unwrap();
    let started = Instant::now();
    while player.model_rig(fox).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let clips: Vec<(String, f32)> = player
        .model_rig(fox)
        .unwrap()
        .clips()
        .iter()
        .map(|c| (c.name().to_owned(), c.duration()))
        .collect();
    let clip = |name: &str| clips.iter().position(|(n, _)| n == name).unwrap();
    let (survey, run) = (clip("Survey"), clip("Run"));
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let animator = |player: &mut Player| player.world_mut().get::<Animator>(fox).unwrap().clone();
    let joints = |player: &mut Player| {
        let mut joints = Vec::new();
        assert!(player.model_joints(fox, &mut joints));
        joints
    };
    // A clip's pose at a moment, as the editor holds one, then let go.
    let held = |player: &mut Player, clip: usize, time: f32| {
        assert!(player.pose_model(fox, Some((clip, time))));
        let pose = joints(player);
        assert!(player.pose_model(fox, None));
        pose
    };
    let off = |a: &[Affine3A], b: &[Affine3A]| {
        a.iter()
            .zip(b)
            .map(|(a, b)| (Vec3::from(a.translation) - Vec3::from(b.translation)).length())
            .fold(0.0f32, f32::max)
    };
    player
        .world_mut()
        .get_mut::<Animator>(fox)
        .unwrap()
        .play("Run", 0.5);
    let mut midway = false;
    for _ in 0..60 {
        player.frame(&mut game).unwrap();
        let a = animator(&mut player);
        let alpha = player.runtime().clock.alpha();
        let weight = a.weight(alpha);
        if !(0.4..0.6).contains(&weight) {
            continue;
        }
        let drawn = joints(&mut player);
        let from = held(&mut player, survey, a.from_at(alpha, clips[survey].1));
        let to = held(&mut player, run, a.at(alpha, clips[run].1));
        let apart = off(&from, &to);
        let (to_from, to_to) = (off(&drawn, &from), off(&drawn, &to));
        println!(
            "at weight {weight:.3}: {to_from:.2} from Survey, {to_to:.2} from Run, \
             which are {apart:.2} apart (the model's units)"
        );
        assert!(
            to_from > 0.1 * apart && to_to > 0.1 * apart,
            "neither clip at weight {weight}: {to_from}, {to_to} of {apart}"
        );
        assert!(to_from < apart && to_to < apart, "between them");
        // A save keeps the fade, to go on fading after a load.
        let mut saved = String::new();
        save_with(player.world_mut(), &registry, &options, &mut saved).unwrap();
        assert!(
            saved.contains("from_clip") && saved.contains("\"Survey\""),
            "{saved}"
        );
        midway = true;
        break;
    }
    assert!(midway, "the fade's middle shown");
    let mut over = false;
    for _ in 0..60 {
        player.frame(&mut game).unwrap();
        if animator(&mut player).fade == 0.0 {
            over = true;
            break;
        }
    }
    assert!(over, "the fade ends");
    let a = animator(&mut player);
    assert_eq!((a.clip.as_str(), a.from_clip.as_str()), ("Run", ""));
    let alpha = player.runtime().clock.alpha();
    let drawn = joints(&mut player);
    let pure = held(&mut player, run, a.at(alpha, clips[run].1));
    assert!(off(&drawn, &pure) < 1e-5, "Run alone once the fade is over");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A walker made for the root motion tests (ADR 0124's E12-2c), written as `walker.gltf`
/// and `walker.bin` into `dir`: a box skinned to two joints, its hips 1 m up and its head
/// half a metre above. Clip `Walk` crosses 1 m of ground along +z in its second, the hips
/// bobbing; clip `Turn` turns them a quarter about +y on the spot in its second.
fn write_walker(dir: &std::path::Path) {
    use engine_core::math::{Mat4, Quat, Vec3};
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut add = |bin: &mut Vec<u8>, bytes: Vec<u8>, accessor: serde_json::Value| {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        views.push(serde_json::json!({
            "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len()
        }));
        bin.extend_from_slice(&bytes);
        let mut accessor = accessor;
        accessor["bufferView"] = serde_json::json!(views.len() - 1);
        accessors.push(accessor);
        accessors.len() - 1
    };
    let floats = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    // The box: its four lower corners on the hips, its four upper ones on the head.
    let (lo, hi) = (Vec3::new(-0.3, 0.0, -0.2), Vec3::new(0.3, 1.6, 0.2));
    let corners: Vec<Vec3> = (0..8)
        .map(|k| {
            Vec3::new(
                if k & 1 == 0 { lo.x } else { hi.x },
                if k & 2 == 0 { lo.y } else { hi.y },
                if k & 4 == 0 { lo.z } else { hi.z },
            )
        })
        .collect();
    let centre = (lo + hi) * 0.5;
    let positions: Vec<f32> = corners.iter().flat_map(|c| c.to_array()).collect();
    let normals: Vec<f32> = corners
        .iter()
        .flat_map(|c| (*c - centre).normalize().to_array())
        .collect();
    let joints: Vec<u8> = (0..8u16)
        .flat_map(|k| {
            let j: u16 = if k & 2 == 0 { 0 } else { 1 };
            [j, 0, 0, 0].into_iter().flat_map(u16::to_le_bytes)
        })
        .collect();
    let weights: Vec<f32> = (0..8).flat_map(|_| [1.0, 0.0, 0.0, 0.0]).collect();
    let faces: [[u16; 4]; 6] = [
        [0, 2, 3, 1],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 4, 6, 2],
        [1, 3, 7, 5],
    ];
    let indices: Vec<u8> = faces
        .iter()
        .flat_map(|f| [f[0], f[1], f[2], f[0], f[2], f[3]])
        .flat_map(u16::to_le_bytes)
        .collect();
    let position = add(
        &mut bin,
        floats(&positions),
        serde_json::json!({"componentType": 5126, "count": 8, "type": "VEC3",
            "min": lo.to_array(), "max": hi.to_array()}),
    );
    let normal = add(
        &mut bin,
        floats(&normals),
        serde_json::json!({"componentType": 5126, "count": 8, "type": "VEC3"}),
    );
    let joint = add(
        &mut bin,
        joints,
        serde_json::json!({"componentType": 5123, "count": 8, "type": "VEC4"}),
    );
    let weight = add(
        &mut bin,
        floats(&weights),
        serde_json::json!({"componentType": 5126, "count": 8, "type": "VEC4"}),
    );
    let index = add(
        &mut bin,
        indices,
        serde_json::json!({"componentType": 5123, "count": 36, "type": "SCALAR"}),
    );
    let inverse_bind: Vec<f32> = [Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 1.5, 0.0)]
        .iter()
        .flat_map(|at| Mat4::from_translation(-*at).to_cols_array())
        .collect();
    let binds = add(
        &mut bin,
        floats(&inverse_bind),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "MAT4"}),
    );
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
    let time = add(
        &mut bin,
        floats(&times),
        serde_json::json!({"componentType": 5126, "count": times.len(), "type": "SCALAR",
            "min": [0.0], "max": [1.0]}),
    );
    let walk: Vec<f32> = times
        .iter()
        .flat_map(|&t| {
            let bob = 0.02 * (t * core::f32::consts::TAU * 2.0).sin();
            [0.0, 1.0 + bob, t]
        })
        .collect();
    let walk = add(
        &mut bin,
        floats(&walk),
        serde_json::json!({"componentType": 5126, "count": times.len(), "type": "VEC3"}),
    );
    let turn: Vec<f32> = times
        .iter()
        .flat_map(|&t| Quat::from_rotation_y(t * core::f32::consts::FRAC_PI_2).to_array())
        .collect();
    let turn = add(
        &mut bin,
        floats(&turn),
        serde_json::json!({"componentType": 5126, "count": times.len(), "type": "VEC4"}),
    );
    let gltf = serde_json::json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 1]}],
        "nodes": [
            {"name": "Walker", "mesh": 0, "skin": 0},
            {"name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [2]},
            {"name": "Head", "translation": [0.0, 0.5, 0.0]}
        ],
        "meshes": [{"name": "Walker", "primitives": [{
            "attributes": {"POSITION": position, "NORMAL": normal,
                "JOINTS_0": joint, "WEIGHTS_0": weight},
            "indices": index
        }]}],
        "skins": [{"joints": [1, 2], "inverseBindMatrices": binds}],
        "animations": [
            {"name": "Walk",
                "samplers": [{"input": time, "output": walk, "interpolation": "LINEAR"}],
                "channels": [{"sampler": 0, "target": {"node": 1, "path": "translation"}}]},
            {"name": "Turn",
                "samplers": [{"input": time, "output": turn, "interpolation": "LINEAR"}],
                "channels": [{"sampler": 0, "target": {"node": 1, "path": "rotation"}}]}
        ],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"uri": "walker.bin", "byteLength": bin.len()}]
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("walker.bin"), &bin).unwrap();
    std::fs::write(
        dir.join("walker.gltf"),
        serde_json::to_string_pretty(&gltf).unwrap(),
    )
    .unwrap();
}

/// ADR 0124's E12-2c: root motion. A walker moved by its animator crosses the ground as its
/// clip does while its pose stays on the spot; one that reports it stays put and leaves the
/// motion in its animator; one turned by its clip turns; one that keeps the motion in its
/// pose walks off its spot and back each loop.
#[test]
fn root_motion_moves_the_character_or_is_left_for_the_game() {
    let dir = std::env::temp_dir().join(format!("ofelia-root-motion-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_walker(&dir.join("models"));
    let id = AssetMeta::ensure(&dir.join("models/walker.gltf")).unwrap();
    let mesh = AssetRef::part(id, "Walker");
    let mut paths = AssetPaths::new();
    paths.insert(mesh, "models/walker.gltf#Walker");
    let settings = PlayerSettings {
        title: String::from("root motion test"),
        window: Window::Offscreen {
            width: 128,
            height: 128,
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
    player.assets_mut().add_paths(&paths);
    let spawn = |player: &mut Player, x: f32, clip: &str, root_motion: RootMotion| {
        player.world_mut().spawn((
            Transform::from_xyz(x, 0.0, 0.0),
            Model::new(mesh, engine_app::builtin::MATERIAL),
            Animator {
                root_motion,
                ..Animator::new(clip)
            },
        ))
    };
    let mover = spawn(&mut player, 0.0, "Walk", RootMotion::Move);
    let reporter = spawn(&mut player, 3.0, "Walk", RootMotion::Report);
    let turner = spawn(&mut player, -3.0, "Turn", RootMotion::Move);
    let stander = spawn(&mut player, 6.0, "Walk", RootMotion::InPose);
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 64.0));
    let started = Instant::now();
    while [mover, reporter, turner, stander]
        .iter()
        .any(|&e| player.model_rig(e).is_none())
    {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let rig = player.model_rig(mover).unwrap();
    let hips = rig.skeleton().find("Hips").unwrap();
    let speed = rig.clips()[0].root_speed();
    println!("Walk's root speed {speed} a second");
    assert!((speed - 1.0).abs() < 1e-3, "{speed}");
    let at = |player: &mut Player, e| *player.world_mut().get::<Transform>(e).unwrap();
    let animator = |player: &mut Player, e| player.world_mut().get::<Animator>(e).unwrap().clone();
    let hips_at = |player: &mut Player, e| {
        let mut joints = Vec::new();
        assert!(player.model_joints(e, &mut joints));
        Vec3::from(joints[hips].translation)
    };
    let (mover_start, turner_start) = (at(&mut player, mover), at(&mut player, turner));
    let ticks_start = player.ticks_run();
    let mut stander_hips = Vec::new();
    for _ in 0..80 {
        player.frame(&mut game).unwrap();
        // The pose keeps the hips on the spot: only the bob moves them.
        let h = hips_at(&mut player, mover);
        assert!(
            h.x.abs() < 1e-3 && h.z.abs() < 1e-3,
            "moved off the spot: {h}"
        );
        stander_hips.push(hips_at(&mut player, stander).z);
        let a = animator(&mut player, reporter);
        assert!(
            (a.moved - Vec3::new(0.0, 0.0, 1.0 / 64.0)).length() < 5e-4 && a.turned.abs() < 1e-4,
            "a tick's motion reported: {} {}",
            a.moved,
            a.turned
        );
    }
    let ticks = (player.ticks_run() - ticks_start) as f32;
    let moved = at(&mut player, mover).translation - mover_start.translation;
    println!("moved {moved} in {ticks} ticks");
    assert!(
        (moved - Vec3::new(0.0, 0.0, ticks / 64.0)).length() < 2e-3,
        "crossed as its clip does: {moved} in {ticks} ticks"
    );
    assert_eq!(
        at(&mut player, reporter).translation,
        Vec3::new(3.0, 0.0, 0.0)
    );
    // Turned a quarter a second about its up, on the spot.
    let turned = at(&mut player, turner);
    let expected =
        turner_start.rotation * Quat::from_rotation_y(ticks / 64.0 * core::f32::consts::FRAC_PI_2);
    assert!(
        turned.rotation.angle_between(expected) < 2e-3,
        "{:?} against {expected:?}",
        turned.rotation
    );
    assert!((turned.translation - Vec3::new(-3.0, 0.0, 0.0)).length() < 1e-3);
    // Kept in the pose: the stander's hips walk the clip and go back each loop.
    let (low, high) = stander_hips
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &z| (lo.min(z), hi.max(z)));
    assert!(
        low < 0.1 && high > 0.9,
        "walked its loop in place: {low}..{high}"
    );
    assert_eq!(
        at(&mut player, stander).translation,
        Vec3::new(6.0, 0.0, 0.0)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1b-3: the model preview's floor. Asked with a floor, the preview draws the
/// blockout's grid under the model and takes its clip's root motion out of its pose (it walks
/// on its spot); the floor drawn where its walk took it, it goes by under it. The rig gives
/// the walk in its file's scene. `ENGINE_FLOOR_PREVIEW_PNG=<dir>` writes the pictures.
#[test]
fn a_model_previews_floor_goes_by_under_its_walk() {
    let dir = std::env::temp_dir().join(format!("ofelia-floor-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_walker(&dir.join("models"));
    let id = AssetMeta::ensure(&dir.join("models/walker.gltf")).unwrap();
    let mesh = AssetRef::part(id, "Walker");
    let material = engine_app::builtin::MATERIAL;
    let mut paths = AssetPaths::new();
    paths.insert(mesh, "models/walker.gltf#Walker");
    let settings = PlayerSettings {
        title: String::from("floor preview test"),
        window: Window::Offscreen {
            width: 128,
            height: 128,
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
    player.assets_mut().add_paths(&paths);
    player.set_frame_time(Some(1.0 / 60.0));
    let asked = |pose, floor| engine_app::ModelPreview {
        mesh,
        material,
        turn: [0.6, 0.3],
        pose,
        floor,
    };
    let drawn = |player: &mut Player, preview: engine_app::ModelPreview| {
        player.set_model_preview(Some(preview));
        let mut pixels = Vec::new();
        let started = Instant::now();
        loop {
            player.frame(&mut Still).unwrap();
            if player.take_model_preview(&mut pixels) {
                return pixels;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no model preview: {:?}",
                player.assets().problem(mesh)
            );
        }
    };
    let differ = |a: &[u8], b: &[u8]| {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
            .count()
    };
    let covered = |p: &[u8]| p.chunks_exact(4).filter(|c| c[3] > 128).count();
    let bare = drawn(&mut player, asked(Some((0, 0.0)), None));
    let rig = player.model_preview_rig().expect("the walker's rig");
    let (moved, turned) = rig.scene_motion(0, 0.0, 0.5).unwrap();
    assert!(
        (moved - Vec3::new(0.0, 0.0, 0.5)).length() < 1e-3 && turned.abs() < 1e-4,
        "half a second of Walk in its scene: {moved} {turned}"
    );
    let (_, quarter) = rig.scene_motion(1, 0.0, 0.5).unwrap();
    assert!(
        (quarter - core::f32::consts::FRAC_PI_4).abs() < 1e-3,
        "half a second of Turn: {quarter}"
    );
    let floor = drawn(&mut player, asked(Some((0, 0.0)), Some([0.0; 3])));
    let gone = drawn(&mut player, asked(Some((0, 0.0)), Some([0.0, -0.3, 0.0])));
    // Half a second into Turn: with a floor it stays facing as at its start (its turn taken
    // out of the pose); without one it turned an eighth. (A walk would not show: the preview
    // frames the posed box, so a box carried along looks the same.)
    let start = drawn(&mut player, asked(Some((1, 0.0)), Some([0.0; 3])));
    let in_place = drawn(&mut player, asked(Some((1, 0.5)), Some([0.0; 3])));
    let made_start = drawn(&mut player, asked(Some((1, 0.0)), None));
    let as_made = drawn(&mut player, asked(Some((1, 0.5)), None));
    let side = engine_app::PREVIEW_SIZE as usize;
    println!(
        "covered: bare {}, with its floor {} of {}; the floor gone by 0.3: {} pixels; \
         turned in place {} pixels from its start, as made {}",
        covered(&bare),
        covered(&floor),
        side * side,
        differ(&floor, &gone),
        differ(&start, &in_place),
        differ(&made_start, &as_made)
    );
    if let Ok(out) = std::env::var("ENGINE_FLOOR_PREVIEW_PNG") {
        for (name, pixels) in [("bare", &bare), ("floor", &floor), ("gone", &gone)] {
            let png = engine_render::png::encode_png(
                side as u32,
                side as u32,
                engine_render::png::PngFormat::Rgba,
                pixels,
            )
            .unwrap();
            std::fs::write(
                std::path::Path::new(&out).join(format!("floor-{name}.png")),
                png,
            )
            .unwrap();
        }
    }
    assert!(
        covered(&floor) > covered(&bare) + side * side / 10,
        "the floor drawn under it"
    );
    assert!(
        differ(&floor, &gone) > side * side / 100,
        "the floor went by"
    );
    assert!(
        differ(&start, &in_place) < side * side / 200,
        "with a floor its turn is the floor's, not its pose's"
    );
    assert!(
        differ(&made_start, &as_made) > side * side / 50,
        "without one it turns as made"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1c: a socket under a character (`JointAttachment`) follows its joint:
/// where the joint is in the pose held, through the animator's clip as it plays, and what
/// hangs under the socket with it; standing still, it stands; a joint the skeleton lacks
/// leaves its socket alone.
#[test]
fn a_socket_follows_its_joint_and_carries_what_hangs_under_it() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-sockets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let mut paths = Assets::new(dir.clone()).paths().clone();
    paths.insert(mesh, "CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "CesiumMan.gltf#Cesium_Man-effect");
    let mut world = World::new();
    let man = world.spawn((
        SceneId::new(),
        Name::new("Man"),
        Transform::from_xyz(1.0, 0.0, 0.0)
            .with_rotation(Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2)),
        Model::new(mesh, material),
        Animator::new(""),
    ));
    let hand = world.spawn((
        SceneId::new(),
        Name::new("Hand"),
        Transform::IDENTITY,
        JointAttachment::new("Skeleton_arm_joint_R__3_"),
    ));
    world.set_parent(hand, Some(man)).unwrap();
    let held = world.spawn((
        SceneId::new(),
        Name::new("Held"),
        Transform::from_xyz(0.0, 0.1, 0.0),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.set_parent(held, Some(hand)).unwrap();
    let lost = world.spawn((
        SceneId::new(),
        Name::new("Lost"),
        Transform::from_xyz(0.5, 0.5, 0.5),
        JointAttachment::new("Tail"),
    ));
    world.set_parent(lost, Some(man)).unwrap();
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
    assert!(text.contains("JointAttachment"), "saved with the scene");
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let settings = PlayerSettings {
        title: String::from("sockets test"),
        window: Window::Offscreen {
            width: 128,
            height: 128,
        },
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
    let world = player.world_mut();
    let (man, hand, held, lost) = (
        find_named(world, "Man").unwrap(),
        find_named(world, "Hand").unwrap(),
        find_named(world, "Held").unwrap(),
        find_named(world, "Lost").unwrap(),
    );
    let started = Instant::now();
    while player.model_rig(man).is_none() {
        assert!(started.elapsed() < Duration::from_secs(30), "no skin");
        player.frame(&mut game).unwrap();
    }
    let rig = player.model_rig(man).unwrap();
    let joint = rig.skeleton().find("Skeleton_arm_joint_R__3_").unwrap();
    let length = rig.clips()[0].duration();
    // Held halfway through the walk: the socket where the joint is then.
    assert!(player.pose_model(man, Some((0, length * 0.5))));
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    let mut joints = Vec::new();
    assert!(player.model_joints(man, &mut joints));
    let at = |player: &mut Player, e| *player.world_mut().get::<Transform>(e).unwrap();
    let socket = at(&mut player, hand);
    let off = (socket.translation - Vec3::from(joints[joint].translation)).length();
    println!("the hand's socket {off} m off its joint");
    assert!(off < 1e-5, "{off}");
    // What hangs under it is carried: the cube a tenth of a meter along the hand's y.
    let world_at = |player: &mut Player, e| player.world_mut().get::<GlobalTransform>(e).unwrap().0;
    let expected = world_at(&mut player, man)
        * joints[joint]
        * Affine3A::from_translation(Vec3::new(0.0, 0.1, 0.0));
    let got = world_at(&mut player, held);
    let carried = (Vec3::from(got.translation) - Vec3::from(expected.translation)).length();
    assert!(carried < 1e-4, "the cube in his hand: {carried} m off");
    // A joint he has not: its socket stays where it was put.
    assert_eq!(at(&mut player, lost).translation, Vec3::new(0.5, 0.5, 0.5));
    // Let go: the animator plays, the socket goes with the hand.
    assert!(player.pose_model(man, None));
    let mut path = Vec::new();
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
        path.push(at(&mut player, hand).translation);
    }
    let went = path.windows(2).map(|w| (w[1] - w[0]).length()).sum::<f32>();
    println!("the hand went {went:.3} m in 30 frames");
    assert!(went > 0.05, "the socket follows the walk: {went} m");
    // The world standing still: so does the socket.
    player.set_simulating(false);
    player.frame(&mut game).unwrap();
    let still = at(&mut player, hand);
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(at(&mut player, hand), still, "standing still");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0124's E12-1b: the editor's live model preview draws a skinned model of no scene
/// alone, its skin made in the background; it draws again only when its pose or its turn
/// changed, a clip's moment and a quarter turn each giving another picture, and nothing once
/// it is let go. `ENGINE_MODEL_PREVIEW_PNG=<dir>` writes its three pictures.
#[test]
fn a_model_preview_draws_a_skinned_model_posed_and_turned_alone() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("ofelia-model-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let settings = PlayerSettings {
        title: String::from("model preview test"),
        window: Window::Offscreen {
            width: 320,
            height: 240,
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
    let mut paths = player.assets().paths().clone();
    paths.insert(mesh, "models/CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "models/CesiumMan.gltf#Cesium_Man-effect");
    player.assets_mut().add_paths(&paths);
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    let asked = |pose, turn| engine_app::ModelPreview {
        mesh,
        material,
        turn,
        pose,
        floor: None,
    };
    let mut pixels = Vec::new();
    let drawn = |player: &mut Player,
                 game: &mut Still,
                 preview: engine_app::ModelPreview,
                 pixels: &mut Vec<u8>| {
        player.set_model_preview(Some(preview));
        let started = Instant::now();
        loop {
            player.frame(game).unwrap();
            if player.take_model_preview(pixels) {
                return started.elapsed();
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no model preview: {:?}",
                player.assets().problem(mesh)
            );
        }
    };
    let covered = |p: &[u8]| p.chunks_exact(4).filter(|c| c[3] > 128).count();
    let differ = |a: &[u8], b: &[u8]| {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
            .count()
    };
    let took = drawn(&mut player, &mut game, asked(None, [0.0, 0.0]), &mut pixels);
    let rest = pixels.clone();
    let side = engine_app::PREVIEW_SIZE as usize;
    assert_eq!(rest.len(), side * side * 4);
    let clips = player.model_preview_rig().map(|r| r.clips().len());
    println!(
        "the model preview drawn after {:.0} ms: {} of {} pixels covered, clips {clips:?}",
        took.as_secs_f64() * 1e3,
        covered(&rest),
        side * side
    );
    assert!(
        covered(&rest) > side * side / 20,
        "the man covers the preview"
    );
    assert_eq!(clips, Some(1));
    // Asked again the same: nothing drawn anew.
    for _ in 0..10 {
        player.set_model_preview(Some(asked(None, [0.0, 0.0])));
        player.frame(&mut game).unwrap();
    }
    assert!(
        !player.take_model_preview(&mut pixels),
        "drawn again unchanged"
    );
    // A clip's moment: another picture; a quarter turn: another again.
    drawn(
        &mut player,
        &mut game,
        asked(Some((0, 1.0)), [0.0, 0.0]),
        &mut pixels,
    );
    let walking = differ(&rest, &pixels);
    let walking_shot = pixels.clone();
    drawn(
        &mut player,
        &mut game,
        asked(Some((0, 1.0)), [core::f32::consts::FRAC_PI_2, 0.0]),
        &mut pixels,
    );
    let turned = covered(&pixels);
    if let Ok(out) = std::env::var("ENGINE_MODEL_PREVIEW_PNG") {
        for (name, shot) in [
            ("rest.png", &rest),
            ("walking.png", &walking_shot),
            ("turned.png", &pixels),
        ] {
            let png = engine_render::png::encode_png(
                side as u32,
                side as u32,
                engine_render::png::PngFormat::Rgba,
                shot,
            )
            .unwrap();
            std::fs::write(std::path::Path::new(&out).join(name), png).unwrap();
        }
    }
    println!("posed mid-walk: {walking} pixels changed; turned: {turned} covered");
    assert!(walking > 100, "the pose shows: {walking}");
    assert!(
        turned > side * side / 40,
        "turned, still in the frame: {turned}"
    );
    // What it costs the GPU: frames drawing it anew each time against frames without it.
    let gpu = |player: &mut Player, game: &mut Still, live: bool| {
        let (mut total, mut n) = (0.0f64, 0u32);
        for k in 0..150 {
            let preview = live.then(|| asked(Some((0, k as f32 / 60.0)), [0.0, 0.0]));
            player.set_model_preview(preview);
            player.frame(game).unwrap();
            if let Some(us) = player.graph().gpu_frame_time().filter(|_| k >= 30) {
                total += f64::from(us);
                n += 1;
            }
        }
        total / f64::from(n.max(1))
    };
    let with = gpu(&mut player, &mut game, true);
    let without = gpu(&mut player, &mut game, false);
    println!(
        "the GPU's frame: {with:.0} us drawing the preview each frame, {without:.0} us without"
    );
    // Let go: nothing more.
    player.set_model_preview(None);
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    assert!(!player.take_model_preview(&mut pixels));
    let _ = std::fs::remove_dir_all(&dir);
}
