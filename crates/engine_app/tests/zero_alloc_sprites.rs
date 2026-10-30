//! CLAUDE.md §5: zero heap allocations per frame in steady state with a 2D view (ADR
//! 0125): an orthographic camera drawing a thousand sprites that move every frame, on a
//! layer sorted by y, some in sort groups, some culled, through the player's 2D pass. One
//! test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

use engine_app::{SortGroup, SortingFile, SortingLayer, Sprite};
use engine_ecs::{GlobalTransform, QueryState, Transform};
use engine_runtime::Camera;
use engine_scene::{AssetId, AssetMeta, AssetPaths, AssetRef};
use std::collections::BTreeMap;

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

#[test]
fn frames_with_moving_sprites_do_not_allocate() {
    let dir = std::env::temp_dir().join(format!("ofelia-sprites-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("art")).unwrap();
    let mut paths = AssetPaths::new();
    let mut ids = Vec::new();
    for (k, rgb) in [[220u8, 60, 40], [40, 180, 90], [60, 90, 220]]
        .into_iter()
        .enumerate()
    {
        let rgba: Vec<u8> = (0..16 * 16)
            .flat_map(|i| [rgb[0], rgb[1], rgb[2], if i % 7 == 0 { 0 } else { 255 }])
            .collect();
        let png =
            engine_render::png::encode_png(16, 16, engine_render::png::PngFormat::Rgba, &rgba)
                .unwrap();
        let path = format!("art/sprite{k}.png");
        std::fs::write(dir.join(&path), png).unwrap();
        let id = AssetId::new();
        AssetMeta::default().write(&dir.join(&path), id).unwrap();
        paths.insert(AssetRef::file(id), path);
        ids.push(id);
    }
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
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.project = Some(dir.clone());
    }) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    player.assets_mut().add_paths(&paths);
    let world = player.world_mut();
    // The main camera: orthographic, over the test's perspective one.
    world.spawn((
        Transform::IDENTITY,
        Camera {
            priority: 1,
            ..Camera::pixel_art([320, 240], 16.0)
        },
    ));
    // A thousand sprites, a quarter of them outside the view, every one moving each frame;
    // a hundred characters of three parts in sort groups.
    for k in 0..1000 {
        let x = (k % 50) as f32 * 0.6 - 15.0;
        let y = (k / 50) as f32 * 0.4 - 4.0;
        let sprite = Sprite::new(AssetRef::file(ids[k % 3])).on_layer((k % 2) as i16, 0);
        world.spawn((
            Transform::from_xyz(x, y, 0.0),
            GlobalTransform::IDENTITY,
            sprite,
        ));
    }
    for k in 0..100 {
        let group = world.spawn((
            Transform::from_xyz((k % 10) as f32 - 5.0, (k / 10) as f32 * 0.5 - 2.5, 0.0),
            GlobalTransform::IDENTITY,
            SortGroup { layer: 1, order: 0 },
        ));
        for (part, &id) in ids.iter().enumerate() {
            let sprite = Sprite::new(AssetRef::file(id)).on_layer(1, part as i16);
            let e = world.spawn((
                Transform::from_xyz(part as f32 * 0.25, 0.0, 0.0),
                GlobalTransform::IDENTITY,
                sprite,
            ));
            world.set_parent(e, Some(group)).unwrap();
        }
    }
    let mut moving = QueryState::<(&mut Transform, &Sprite)>::new(player.world_mut());
    let mut t = 0.0f32;
    counting::assert_steady_state_each(
        &mut player,
        &mut game,
        |player| player.sprites_recorded().0 > 900,
        |player| {
            t += 0.01;
            let world = player.world_mut();
            moving.for_each(world, |(transform, _)| {
                transform.translation.x += 0.002 * t.sin();
                transform.rotation = engine_core::math::Quat::from_rotation_z(t);
            });
        },
    );
    let (drawn, culled) = player.sprites_recorded();
    println!("{drawn} sprites drawn, {culled} culled");
    assert!(
        drawn > 900 && culled > 100,
        "{drawn} drawn, {culled} culled"
    );
    assert_eq!(player.assets().sprite_images(), (3, 3));
    let _ = std::fs::remove_dir_all(&dir);
}
