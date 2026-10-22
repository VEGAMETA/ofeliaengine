//! Models' colliders (ADR 0117): a built-in mesh's own shape at once; a glTF mesh's as the
//! cook makes it by the rules beside its file, cooked in the background the first time it
//! is asked for: level geometry by default, a box fitted to it for a prop, none when the
//! rules turn it off, and cooked again each time the file is read again.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::panic)]

use engine_app::prelude::*;
use engine_app::{AssetMeta, BakedBrush, ColliderState, ModelCollider};
use engine_assets::colliders::Motion;
use engine_collision::Shape;
use engine_scene::AssetPaths;
use std::sync::Arc;

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

/// A cube of side 1 around the origin, a vertex of its own at each face's corners.
fn cube() -> BakedBrush {
    let mut b = BakedBrush {
        tint: [0.8, 0.8, 0.8, 1.0],
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
    };
    for axis in 0..3 {
        for sign in [-1.0f32, 1.0] {
            let mut n = [0.0; 3];
            n[axis] = sign;
            // (u, v, axis) right-handed: the corners go counter-clockwise seen from +axis.
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let first = b.positions.len() as u32;
            for (a, c) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
                let mut p = [0.0f32; 3];
                p[axis] = sign * 0.5;
                p[u] = a;
                p[v] = c;
                b.positions.push(p);
                b.normals.push(n);
                b.uvs.push([a, c]);
            }
            let quad: [u32; 6] = if sign > 0.0 {
                [0, 1, 2, 0, 2, 3]
            } else {
                [0, 2, 1, 0, 3, 2]
            };
            b.indices.extend(quad.iter().map(|i| first + i));
        }
    }
    b
}

/// Frames until `asset`'s collider is no longer coming: what it is then.
fn settled(player: &mut Player, asset: AssetRef) -> ColliderState {
    let mut game = Still;
    for _ in 0..2000 {
        player.frame(&mut game).unwrap();
        let state = player.assets_mut().collider(asset);
        if state != ColliderState::Coming {
            return state;
        }
    }
    panic!("the collider never came");
}

fn made(state: ColliderState) -> Arc<ModelCollider> {
    match state {
        ColliderState::Made(c) => c,
        other => panic!("no collider: {other:?}"),
    }
}

#[test]
fn a_models_collider_is_its_meshs_by_its_files_rules() {
    let dir = std::env::temp_dir().join(format!("ofelia-model-colliders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let baked = engine_app::bake::glb("Crate", &[cube()], &engine_app::grid_png());
    let (mesh, _) = baked.parts[0].clone();
    std::fs::write(dir.join("crate.glb"), &baked.bytes).unwrap();
    let id = AssetMeta::ensure(&dir.join("crate.glb")).unwrap();
    let settings = PlayerSettings {
        title: String::from("model colliders test"),
        window: Window::Offscreen {
            width: 160,
            height: 90,
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
    paths.insert(AssetRef::file(id), "crate.glb");
    player.assets_mut().add_paths(&paths);
    player.set_frame_time(Some(1.0 / 60.0));

    // A built-in mesh's own shape, at once.
    let builtin = made(player.assets_mut().collider(builtin::CUBE));
    assert_eq!(builtin.motion, Motion::Static);
    assert!(
        matches!(
            builtin.collider.shapes[0].shape,
            Shape::Box { half_extents, .. } if half_extents == Vec3::splat(0.5)
        ),
        "{builtin:?}"
    );

    // A glTF mesh's, cooked in the background: its triangles, as level geometry.
    let asset = AssetRef::part(id, &mesh);
    assert_eq!(player.assets_mut().collider(asset), ColliderState::Coming);
    let level = made(settled(&mut player, asset));
    assert_eq!(level.motion, Motion::Static);
    match &level.collider.shapes[..] {
        [one] => match &one.shape {
            Shape::Mesh { triangles, .. } => assert_eq!(triangles.len(), 12),
            other => panic!("not its triangles: {other:?}"),
        },
        other => panic!("{} shapes", other.len()),
    }
    let mut edges = Vec::new();
    level.collider.edges(&mut edges);
    assert_eq!(
        edges.len(),
        12,
        "the cube's edges, not its faces' diagonals"
    );
    let epoch = player.assets().collider_epoch();

    // A prop by the rules beside the file: a box fitted to it, moved by the simulation.
    std::fs::write(dir.join("crate.colliders.ron"), "(bodies: Dynamic)").unwrap();
    player.reload_asset(id);
    let prop = made(settled(&mut player, asset));
    assert!(player.assets().collider_epoch() > epoch);
    assert_eq!(prop.motion, Motion::Dynamic);
    match &prop.collider.shapes[..] {
        [one] => match one.shape {
            Shape::Box { half_extents, .. } => {
                assert!((half_extents - Vec3::splat(0.5)).abs().max_element() < 1e-3)
            }
            ref other => panic!("not a box: {other:?}"),
        },
        other => panic!("{} shapes", other.len()),
    }

    // Turned off by its rules: none.
    std::fs::write(dir.join("crate.colliders.ron"), "(collides: false)").unwrap();
    player.reload_asset(id);
    assert_eq!(settled(&mut player, asset), ColliderState::None);
    let _ = std::fs::remove_dir_all(&dir);
}
