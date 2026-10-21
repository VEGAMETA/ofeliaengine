//! Blockout brushes in the player (ADR 0110): a scene file with a group of brushes (a room,
//! its inside cut, a door cut through a wall) and a wedge, stairs and a cylinder beside
//! it, saved as the editor saves it and loaded through `PlayerSettings::scene`, rendered
//! offscreen. The room's inside shows dark through the door, the lit wall beside it shows
//! the grid's lines; moving the door moves the hole, and a cut taken out takes its walls.
//! `ENGINE_BLOCKOUT_PNG=<file>` writes the frame. Rooms: a hall and a corridor drawn into
//! it open into each other with no doorway cut (`ENGINE_ROOMS_PNG=<file>`). Colliders: a
//! ball rolls on over the seam between two floor brushes, and an edit makes the group's
//! body again.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{Brush, BrushGroup, BrushOp, BrushShape};
use engine_scene::{SaveOptions, SceneRegistry, save_with};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn named(world: &mut World, name: &str) -> Entity {
    let mut names = QueryState::<(Entity, &Name)>::new(world);
    names
        .iter_readonly(world)
        .find(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
        .unwrap()
}

#[test]
fn brushes_in_a_scene_file_draw_their_group() {
    let dir = std::env::temp_dir().join(format!("ofelia-blockout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scenes")).unwrap();

    let mut world = World::new();
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    let brush = |world: &mut World, name: &str, at: Vec3, b: Brush| {
        let e = world.spawn((
            SceneId::new(),
            Name::new(name),
            Transform::from_translation(at),
            b,
        ));
        world.set_parent(e, Some(group)).unwrap();
        e
    };
    // A room 6 × 3 × 6 m, walls 0.2 m, its door in the +z wall.
    brush(
        &mut world,
        "Room",
        Vec3::ZERO,
        Brush::new(BrushShape::Box, Vec3::new(6.0, 3.0, 6.0)),
    );
    brush(
        &mut world,
        "Inside",
        Vec3::new(0.0, 0.2, 0.0),
        Brush::new(BrushShape::Box, Vec3::new(5.6, 2.6, 5.6)).with_op(BrushOp::Cut),
    );
    brush(
        &mut world,
        "Door",
        Vec3::new(0.0, 0.2, 3.0),
        Brush::new(BrushShape::Box, Vec3::new(1.2, 2.0, 0.6)).with_op(BrushOp::Cut),
    );
    brush(
        &mut world,
        "Ramp",
        Vec3::new(-6.0, 0.0, 2.0),
        Brush::new(BrushShape::Wedge, Vec3::new(2.0, 1.0, 3.0)),
    );
    brush(
        &mut world,
        "Stairs",
        Vec3::new(6.0, 0.0, 2.0),
        Brush::new(BrushShape::Stairs, Vec3::new(2.0, 1.5, 3.0)),
    );
    brush(
        &mut world,
        "Pillar",
        Vec3::new(5.0, 0.0, 6.0),
        Brush::new(BrushShape::Cylinder, Vec3::new(1.0, 3.0, 1.0)),
    );
    world.spawn((
        SceneId::new(),
        Name::new("Ground"),
        Transform::from_xyz(0.0, -0.05, 0.0).with_scale(Vec3::new(40.0, 0.1, 40.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-0.9)),
        Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.03),
        },
    ));
    // A lamp turned off in the editor: saved off, loaded off.
    world.spawn((
        SceneId::new(),
        Name::new("Spare lamp"),
        Transform::from_xyz(0.0, 2.0, 5.0),
        PointLight::new(Vec3::splat(5.0), 4.0),
        Disabled {},
    ));
    let eye = Vec3::new(0.0, 1.2, 9.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(
            eye,
            Vec3::new(0.0, 1.2, 0.0),
            Vec3::Y,
        )),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    for want in [
        "BrushGroup",
        "Brush.op = Cut",
        "Brush.shape = Stairs",
        "Brush.size = (6, 3, 6)",
        "Disabled = {}",
    ] {
        assert!(text.contains(want), "{want} in {text}");
    }
    assert!(!text.contains("MeshRenderer"), "{text}");
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (640u32, 360u32);
    let settings = PlayerSettings {
        title: String::from("blockout test"),
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
    // Every brush shows something: the room's walls, the cuts' insides, the shapes.
    assert_eq!(player.blockout().meshes(), 6);
    let lamp = named(player.world_mut(), "Spare lamp");
    assert!(
        player.world_mut().get::<Disabled>(lamp).is_some(),
        "loaded off"
    );
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_BLOCKOUT_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let lum = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        (f32::from(rgba[i]) + f32::from(rgba[i + 1]) + f32::from(rgba[i + 2])) / 3.0
    };
    // The middle looks through the door into the room, lit only by the sky: darker than the
    // lit wall 1.5 m to its right.
    let (door, wall) = (lum(width / 2, height / 2), lum(width / 2 + 82, height / 2));
    assert!(door < 0.8 * wall, "door {door}, wall {wall}");
    // The wall shows the grid: a metre's line (x = 1 m, about 55 px right of the middle)
    // darker across a row of it.
    let row: Vec<f32> = (0..60)
        .map(|x| lum(width / 2 + 40 + x, height / 2))
        .collect();
    let (lo, hi) = row
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    assert!(hi - lo > 12.0, "the wall's row {row:?}");

    // The door moved 1.5 m along x: its mesh is made again, the hole follows.
    let world = player.world_mut();
    let door_e = named(world, "Door");
    let before = world.get::<MeshRenderer>(door_e).copied().unwrap();
    world.get_mut::<Transform>(door_e).unwrap().translation.x = 1.5;
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let world = player.world_mut();
    let after = world.get::<MeshRenderer>(door_e).copied().unwrap();
    assert_ne!(before.mesh, after.mesh);
    let rgba = player.capture(&mut game).unwrap().unwrap();
    let lum = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        (f32::from(rgba[i]) + f32::from(rgba[i + 1]) + f32::from(rgba[i + 2])) / 3.0
    };
    assert!(
        lum(width / 2, height / 2) > 0.8 * wall,
        "the old hole is wall again"
    );

    // A cut taken out takes its walls with it.
    let world = player.world_mut();
    let door_e = named(world, "Door");
    world.remove::<Brush>(door_e);
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    assert!(player.world_mut().get::<MeshRenderer>(door_e).is_none());
    assert_eq!(player.blockout().meshes(), 5);

    // The group disabled: its brushes act on nothing and show nothing; enabled, back.
    let world = player.world_mut();
    let group = named(world, "Blockout");
    world.insert(group, Disabled {});
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(player.blockout().meshes(), 0);
    player.world_mut().remove::<Disabled>(group);
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(player.blockout().meshes(), 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rooms_open_into_each_other_with_no_doorway_cut() {
    let dir = std::env::temp_dir().join(format!("ofelia-rooms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scenes")).unwrap();
    let mut world = World::new();
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    let brush = |world: &mut World, name: &str, at: Vec3, b: Brush| {
        let e = world.spawn((
            SceneId::new(),
            Name::new(name),
            Transform::from_translation(at),
            b,
        ));
        world.set_parent(e, Some(group)).unwrap();
    };
    // A hall 6 × 3 × 6 m inside, and a corridor 2 m wide whose inside runs from the hall's
    // inside out through its +z wall, walls 0.2 m; a door at the corridor's far end.
    let room = |size| Brush::new(BrushShape::Box, size).with_hollow(0.2);
    brush(
        &mut world,
        "Hall",
        Vec3::ZERO,
        room(Vec3::new(6.0, 3.0, 6.0)),
    );
    brush(
        &mut world,
        "Corridor",
        Vec3::new(0.0, 0.0, 5.0),
        room(Vec3::new(2.0, 2.4, 6.0)),
    );
    brush(
        &mut world,
        "Door",
        Vec3::new(0.0, 0.0, 8.1),
        Brush::new(BrushShape::Box, Vec3::new(1.2, 2.0, 0.6)).with_op(BrushOp::Cut),
    );
    world.spawn((
        SceneId::new(),
        Name::new("Ground"),
        Transform::from_xyz(0.0, -0.25, 0.0).with_scale(Vec3::new(40.0, 0.1, 40.0)),
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-0.7)),
        Sun {
            color: Vec3::splat(1.5),
            ambient: Vec3::splat(0.03),
        },
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Lamp"),
        Transform::from_xyz(0.0, 2.2, -1.0),
        PointLight::new(Vec3::splat(12.0), 9.0),
    ));
    let eye = Vec3::new(0.0, 1.1, 13.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(
            eye,
            Vec3::new(0.0, 1.1, 0.0),
            Vec3::Y,
        )),
        Camera::perspective(0.8, 0.05, 200.0),
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    assert!(text.contains("Brush.hollow = 0.2"), "{text}");
    std::fs::write(dir.join("scenes/main.scene"), &text).unwrap();

    let (width, height) = (640u32, 360u32);
    let settings = PlayerSettings {
        title: String::from("rooms test"),
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
    // Both rooms show, and the door's hole walls.
    assert_eq!(player.blockout().meshes(), 3);
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_ROOMS_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let lum = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        (f32::from(rgba[i]) + f32::from(rgba[i + 1]) + f32::from(rgba[i + 2])) / 3.0
    };
    // Down the corridor and through the opening, the hall's far wall under its lamp.
    let far = lum(width / 2, height / 2);
    println!("the hall's far wall: {far}");
    assert!(far > 20.0, "the hall lit through the corridor: {far}");
    // From inside the hall, its +z wall is open where the corridor meets it: through the
    // opening, down the corridor and out of its door, the daylit ground
    // (`ENGINE_ROOMS_PNG2=<file>`).
    let world = player.world_mut();
    let camera = named(world, "Camera");
    let eye = Vec3::new(0.0, 1.5, -2.6);
    *world.get_mut::<Transform>(camera).unwrap() = Transform::from_translation(eye)
        .with_rotation(look_rotation(eye, Vec3::new(0.0, 1.2, 8.0), Vec3::Y));
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    if let Ok(path) = std::env::var("ENGINE_ROOMS_PNG2") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_ball_rolls_on_over_the_seam_between_floor_brushes_and_an_edit_makes_the_body_again() {
    use engine_app::ScenePhysics;
    use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, QueryFilter, Ray, ShapeDesc};
    let dir = std::env::temp_dir().join(format!("ofelia-colliders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scenes")).unwrap();
    let mut world = World::new();
    let group = world.spawn((
        SceneId::new(),
        Name::new("Blockout"),
        Transform::IDENTITY,
        BrushGroup::default(),
    ));
    // Two floors 6 × 1 × 6 m side by side, their seam at x = 0, and a crate that does not
    // collide.
    for (name, x) in [("West", -3.0), ("East", 3.0)] {
        let e = world.spawn((
            SceneId::new(),
            Name::new(name),
            Transform::from_xyz(x, -1.0, 0.0),
            Brush::new(BrushShape::Box, Vec3::new(6.0, 1.0, 6.0)),
        ));
        world.set_parent(e, Some(group)).unwrap();
    }
    let mut ghost = Brush::new(BrushShape::Box, Vec3::splat(1.0));
    ghost.collides = false;
    let e = world.spawn((
        SceneId::new(),
        Name::new("Ghost"),
        Transform::from_xyz(2.0, 0.0, 2.0),
        ghost,
    ));
    world.set_parent(e, Some(group)).unwrap();
    let eye = Vec3::new(0.0, 3.0, 9.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: None,
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
    let settings = PlayerSettings {
        title: String::from("colliders test"),
        window: Window::Offscreen {
            width: 320,
            height: 180,
        },
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
    for _ in 0..4 {
        player.frame(&mut game).unwrap();
    }
    let mut physics = ScenePhysics::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap();
    let sync = |physics: &mut ScenePhysics, player: &Player| {
        physics
            .sync_blockout(&player.runtime().world, player.blockout())
            .unwrap()
    };
    assert_eq!(sync(&mut physics, &player), 1, "the group's body");
    assert_eq!(sync(&mut physics, &player), 0, "nothing changed");
    let group = named(player.world_mut(), "Blockout");
    let first = physics.blockout_body(group).unwrap();
    // A ball rolled across the seam: it stays on the floor, no knock.
    let dt = 1.0 / 60.0;
    let w = physics.physics();
    let shape = w.create_shape(&ShapeDesc::Sphere { radius: 0.25 }).unwrap();
    let ball = w
        .create_body(&BodyDesc {
            linear_velocity: Vec3::new(4.0, 0.0, 0.0),
            angular_velocity: Vec3::new(0.0, 0.0, -16.0),
            internal_edges: true,
            ..BodyDesc::dynamic(shape, Vec3::new(-4.0, 0.25, 0.0))
        })
        .unwrap();
    let (mut lowest, mut highest) = (f32::MAX, f32::MIN);
    for _ in 0..120 {
        physics.step(player.world_mut(), dt).unwrap();
        let (p, _) = physics.physics().transform(ball).unwrap();
        if p.x > -1.0 && p.x < 1.0 {
            lowest = lowest.min(p.y);
            highest = highest.max(p.y);
        }
    }
    println!("the ball across the seam: {lowest} to {highest}");
    assert!(highest - lowest < 1e-3, "{lowest} to {highest}");
    assert!((lowest - 0.25).abs() < 0.03, "on the floor: {lowest}");
    // The crate that does not collide: a ray down through it reaches the floor.
    let down = Ray {
        origin: Vec3::new(2.0, 5.0, 2.0),
        direction: Vec3::new(0.0, -10.0, 0.0),
    };
    let hit = physics
        .physics()
        .raycast(&down, QueryFilter::default())
        .unwrap();
    assert!(
        hit.point.y.abs() < 1e-3,
        "through the ghost to the floor: {}",
        hit.point
    );
    // An edit: the east floor 1 cm lower; a frame, and the group's body is made again.
    let world = player.world_mut();
    let east = named(world, "East");
    world.get_mut::<Transform>(east).unwrap().translation.y = -1.01;
    player.frame(&mut game).unwrap();
    assert_eq!(sync(&mut physics, &player), 1);
    assert_ne!(physics.blockout_body(group), Some(first));
    // The group gone: its body too.
    player.world_mut().despawn_recursive(group);
    for _ in 0..2 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(sync(&mut physics, &player), 0);
    assert!(physics.blockout_body(group).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
