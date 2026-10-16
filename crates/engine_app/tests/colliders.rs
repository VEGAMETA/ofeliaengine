//! Colliders from a real scene through the whole chain (ADR 0101), without a GPU: Sponza's
//! glTF made into colliders and bodies, baked into Jolt's binary form, written into a pack,
//! loaded by `ScenePhysics`, and rays dropped onto its floor. Skipped when the test assets
//! are missing (`tools/fetch-test-assets.ps1`).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::ScenePhysics;
use engine_assets::colliders::{BakedCollider, CookedBody, CookedCollider, cook_colliders};
use engine_assets::{CookedScene, import_gltf, write_pack};
use engine_collision::{Collider, ColliderShape, Shape};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3};
use engine_ecs::World;
use engine_physics::{PhysicsBackend, PhysicsSettings, QueryFilter, Ray};
use engine_physics_jolt::JoltBackend;
use std::path::Path;

fn settings() -> PhysicsSettings {
    PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    }
}

#[test]
fn sponza_collides_where_it_is_drawn() {
    let gltf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Sponza/Sponza.gltf");
    let Ok(bytes) = std::fs::read(&gltf) else {
        println!("skipped: {} missing", gltf.display());
        return;
    };
    let scene = import_gltf(&bytes, gltf.parent()).unwrap();
    let instances: Vec<(u32, u32)> = (0..scene.nodes.len() as u32).map(|i| (i, 1)).collect();
    let (mut colliders, bodies) = cook_colliders(&scene, &instances, &scene.collider_rules);
    assert!(!colliders.is_empty() && bodies.len() == colliders.len());
    let mut baker = JoltBackend::new(&settings()).unwrap();
    for c in &mut colliders {
        c.baked = engine_physics::colliders::bake(&mut baker, &c.collider)
            .unwrap()
            .map(|b| BakedCollider {
                format: b.format,
                bytes: b.bytes,
                castable: b.castable,
            });
    }
    // The source triangles in the world, placed by their bodies (Sponza's node scales it):
    // the reference the baked, restored and scaled shape is checked against.
    let mut world_triangles = Vec::new();
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for body in &bodies {
        let to_world = Mat4::from_cols_array(&body.world_from_object);
        for s in &colliders[body.collider as usize].collider.shapes {
            if let Shape::Mesh {
                vertices,
                triangles,
                ..
            } = &s.shape
            {
                for t in triangles {
                    let w = t.map(|i| to_world.transform_point3(vertices[i as usize]));
                    for p in w {
                        lo = lo.min(p);
                        hi = hi.max(p);
                    }
                    world_triangles.push(w);
                }
            }
        }
    }
    let (centre, size) = ((lo + hi) * 0.5, hi - lo);
    let path =
        std::env::temp_dir().join(format!("engine_app_colliders_{}.pack", std::process::id()));
    let jobs = ThreadPool::builder().workers(2).build().unwrap();
    write_pack(
        &path,
        &CookedScene {
            colliders,
            bodies,
            ..CookedScene::default()
        },
        &jobs,
        3,
    )
    .unwrap();

    let mut world = World::new();
    let mut physics = ScenePhysics::new(&settings()).unwrap();
    let report = physics.load_pack(&path, &jobs, &mut world, &[]).unwrap();
    let _ = std::fs::remove_file(&path);
    println!("{report:?}");
    assert_eq!(
        report.restored, report.statics,
        "every collider restored from its bytes"
    );
    assert_eq!(report.built, 0);
    // Rays down through the courtyard and across it: Jolt's first hit is the nearest
    // source triangle's.
    let rays = [
        (Vec3::new(0.0, 0.4, 0.0), Vec3::new(0.0, -1.0, 0.0)),
        (Vec3::new(0.1, 0.3, 0.02), Vec3::new(0.0, -1.0, 0.0)),
        (Vec3::new(-0.2, 0.2, -0.03), Vec3::new(0.3, -1.0, 0.1)),
        (Vec3::new(0.0, 0.1, 0.0), Vec3::new(1.0, 0.0, 0.05)),
        (Vec3::new(0.0, 0.15, 0.0), Vec3::new(-1.0, 0.2, -0.3)),
    ];
    for (from, direction) in rays {
        let ray = Ray {
            origin: centre + from * size,
            direction: direction.normalize() * size.length(),
        };
        let expected = nearest_hit(&world_triangles, ray.origin, ray.direction);
        let hit = physics.physics().raycast(&ray, QueryFilter::default());
        match (expected, hit) {
            (Some(t), Some(hit)) => assert!(
                (hit.fraction - t).abs() * size.length() < 2e-3 * size.length() + 1e-3,
                "Jolt {} against the triangles' {t}",
                hit.fraction
            ),
            (None, None) => {}
            (e, h) => panic!("{e:?} against {h:?}"),
        }
    }
}

/// The smallest fraction of `direction` at which the ray hits one of `triangles` (both
/// sides; Möller and Trumbore).
fn nearest_hit(triangles: &[[Vec3; 3]], origin: Vec3, direction: Vec3) -> Option<f32> {
    let mut best: Option<f32> = None;
    for [a, b, c] in triangles {
        let (e1, e2) = (*b - *a, *c - *a);
        let p = direction.cross(e2);
        let det = e1.dot(p);
        if det.abs() < 1e-12 {
            continue;
        }
        let inv = 1.0 / det;
        let s = origin - *a;
        let u = s.dot(p) * inv;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let q = s.cross(e1);
        let v = direction.dot(q) * inv;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let t = e2.dot(q) * inv;
        if (0.0..=1.0).contains(&t) && best.is_none_or(|b| t < b) {
            best = Some(t);
        }
    }
    best
}

#[test]
fn a_mesh_baked_by_another_physics_build_asks_to_cook_again() {
    let mut baker = JoltBackend::new(&settings()).unwrap();
    let collider = Collider {
        shapes: vec![ColliderShape {
            id: 0,
            shape: Shape::Mesh {
                vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Z],
                triangles: vec![[0, 2, 1]],
                surfaces: Vec::new(),
            },
            surface: 0,
        }],
        ..Collider::default()
    };
    let baked = engine_physics::colliders::bake(&mut baker, &collider)
        .unwrap()
        .unwrap();
    let format = baker.baked_shape_format().unwrap();
    // The pack keeps a baked mesh in the backend's form only.
    let mut packed = collider.clone();
    packed.shapes[0].shape = Shape::Mesh {
        vertices: Vec::new(),
        triangles: Vec::new(),
        surfaces: Vec::new(),
    };
    let colliders = [CookedCollider {
        collider: packed,
        baked: Some(BakedCollider {
            format: format ^ 0x10000,
            bytes: baked.bytes,
            castable: false,
        }),
    }];
    let bodies = [CookedBody::default()];
    let mut world = World::new();
    let mut physics = ScenePhysics::new(&settings()).unwrap();
    let error = physics
        .add(&colliders, &bodies, &mut world, &[])
        .unwrap_err();
    assert!(error.to_string().contains("cook the pack again"), "{error}");
}
