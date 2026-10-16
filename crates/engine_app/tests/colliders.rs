//! Colliders from a real scene through the whole chain (ADR 0101), without a GPU: Sponza's
//! glTF made into colliders and bodies, baked into Jolt's binary form, written into a pack,
//! loaded by `ScenePhysics`, and rays dropped onto its floor (where they land and what they
//! land on); then concave props, scaled every way, dropped onto it; and bodies taking their
//! collision layers by name. Skipped when the test assets are missing
//! (`tools/fetch-test-assets.ps1`).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::ScenePhysics;
use engine_assets::colliders::{
    BakedCollider, ColliderRules, CookedBody, CookedCollider, DefaultBodies, Motion, cook_colliders,
};
use engine_assets::{
    CookedScene, ImportedScene, MeshData, NodeData, Primitive, import_gltf, write_pack,
};
use engine_collision::{Collider, ColliderShape, Shape};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Quat, Vec3};
use engine_ecs::World;
use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, QueryFilter, Ray, ShapeDesc};
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
    let (mut colliders, bodies, _) =
        cook_colliders(&scene, &instances, &scene.collider_rules, None, None);
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
                surfaces,
            } = &s.shape
            {
                for (k, t) in triangles.iter().enumerate() {
                    let w = t.map(|i| to_world.transform_point3(vertices[i as usize]));
                    for p in w {
                        lo = lo.min(p);
                        hi = hi.max(p);
                    }
                    world_triangles.push((w, surfaces.get(k).copied().unwrap_or(s.surface)));
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
            (Some((t, surface)), Some(hit)) => {
                assert!(
                    (hit.fraction - t).abs() * size.length() < 2e-3 * size.length() + 1e-3,
                    "Jolt {} against the triangles' {t}",
                    hit.fraction
                );
                // What it landed on: the triangle's glTF material.
                assert_eq!(
                    physics.physics().surface_of(hit.body, hit.part),
                    (surface != u16::MAX).then_some(surface),
                    "{hit:?}"
                );
            }
            (None, None) => {}
            (e, h) => panic!("{e:?} against {h:?}"),
        }
    }
}

/// The smallest fraction of `direction` at which the ray hits one of `triangles` (both
/// sides; Möller and Trumbore), and that triangle's surface.
fn nearest_hit(
    triangles: &[([Vec3; 3], u16)],
    origin: Vec3,
    direction: Vec3,
) -> Option<(f32, u16)> {
    let mut best: Option<(f32, u16)> = None;
    for ([a, b, c], surface) in triangles {
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
        if (0.0..=1.0).contains(&t) && best.is_none_or(|(b, _)| t < b) {
            best = Some((t, *surface));
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

/// A prism: the counter-clockwise outline (x, z) and its triangulation, from y = 0 to
/// `height`, every coordinate times `unit`; one closed mesh wound outward.
fn prism(outline: &[(f32, f32)], caps: &[[u32; 3]], height: f32, unit: f32) -> Primitive {
    let n = outline.len() as u32;
    let mut positions: Vec<[f32; 3]> = outline
        .iter()
        .map(|&(x, z)| [x * unit, 0.0, z * unit])
        .collect();
    positions.extend(
        outline
            .iter()
            .map(|&(x, z)| [x * unit, height * unit, z * unit]),
    );
    let mut indices = Vec::new();
    for &[a, b, c] in caps {
        indices.extend([a, b, c, a + n, c + n, b + n]);
    }
    for k in 0..n {
        let k1 = (k + 1) % n;
        indices.extend([k, k + n, k1, k1, k + n, k1 + n]);
    }
    let (lo, hi) = positions
        .iter()
        .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
            (
                [lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])],
                [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])],
            )
        });
    let count = positions.len();
    Primitive {
        positions,
        normals: vec![[0.0, 1.0, 0.0]; count],
        uvs: Vec::new(),
        tangents: Vec::new(),
        indices,
        material: None,
        min: lo,
        max: hi,
        joints: Vec::new(),
        weights: Vec::new(),
    }
}

fn node(
    name: &str,
    mesh: Option<usize>,
    at: Vec3,
    rotation: Quat,
    scale: Vec3,
    children: Vec<usize>,
) -> NodeData {
    NodeData {
        name: name.to_owned(),
        translation: at.to_array(),
        rotation: rotation.to_array(),
        scale: scale.to_array(),
        mesh,
        children,
        skin: None,
        physics: None,
    }
}

/// A U seen from above (3 × 2 with a 1 × 1 notch open to +z), an L and a square.
const U: [(f32, f32); 8] = [
    (0.0, 0.0),
    (3.0, 0.0),
    (3.0, 2.0),
    (2.0, 2.0),
    (2.0, 1.0),
    (1.0, 1.0),
    (1.0, 2.0),
    (0.0, 2.0),
];
const U_CAPS: [[u32; 3]; 6] = [
    [0, 1, 4],
    [0, 4, 5],
    [1, 2, 3],
    [1, 3, 4],
    [0, 5, 6],
    [0, 6, 7],
];
const L: [(f32, f32); 6] = [
    (0.0, 0.0),
    (2.0, 0.0),
    (2.0, 0.5),
    (0.5, 0.5),
    (0.5, 2.0),
    (0.0, 2.0),
];
const L_CAPS: [[u32; 3]; 4] = [[0, 1, 2], [0, 2, 3], [0, 3, 4], [0, 4, 5]];
const SQUARE: [(f32, f32); 4] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
const SQUARE_CAPS: [[u32; 3]; 2] = [[0, 1, 2], [0, 2, 3]];

/// The owner's requirement (2026-10-16): colliders made once take any scale, concave ones
/// included. Concave props, each scaled another way (uniform, non-uniform, mirrored, sheared
/// by a parent), are split into convex parts by the cook, dropped onto clear spots of
/// Sponza's floor (its courtyard has planters and vases), and come to rest on it; a ball
/// dropped into the U's notch falls into it, which a filled hull would not let it do.
#[test]
fn concave_props_scaled_every_way_land_on_sponza() {
    let gltf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/Sponza/Sponza.gltf");
    let Ok(bytes) = std::fs::read(&gltf) else {
        println!("skipped: {} missing", gltf.display());
        return;
    };
    let level = import_gltf(&bytes, gltf.parent()).unwrap();
    let ids: Vec<(u32, u32)> = (0..level.nodes.len() as u32).map(|i| (i, 1)).collect();
    let (level_colliders, level_bodies, _) =
        cook_colliders(&level, &ids, &level.collider_rules, None, None);
    let mut world = World::new();
    let mut physics = ScenePhysics::new(&settings()).unwrap();
    physics
        .add(&level_colliders, &level_bodies, &mut world, &[])
        .unwrap();
    let statics = physics.bodies().len();
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for body in &level_bodies {
        let to_world = Mat4::from_cols_array(&body.world_from_object);
        for s in &level_colliders[body.collider as usize].collider.shapes {
            if let Shape::Mesh { vertices, .. } = &s.shape {
                for v in vertices {
                    let p = to_world.transform_point3(*v);
                    lo = lo.min(p);
                    hi = hi.max(p);
                }
            }
        }
    }
    let (centre, size) = ((lo + hi) * 0.5, hi - lo);
    // A unit a 120th of the level's length (about 25 cm in Sponza).
    let unit = size.x / 120.0;
    // The first surface under (x, z), from above the courtyard.
    let mut floor_at = |x: f32, z: f32| {
        let ray = Ray {
            origin: Vec3::new(x, lo.y + 0.6 * size.y, z),
            direction: Vec3::new(0.0, -size.y, 0.0),
        };
        physics
            .physics()
            .raycast(&ray, QueryFilter::default())
            .map(|h| h.point.y)
    };
    let floor = floor_at(centre.x, centre.z).unwrap();
    // Clear spots in the courtyard, the open strip down the middle of the level (its sides
    // are arcades under roofs, and planters stand along them): two rows 6 units apart, 9
    // units between columns, each spot 8 × 4 units of bare floor (the doubled U is 6 × 4),
    // the nearest to the middle first.
    let mut candidates: Vec<(f32, f32)> = Vec::new();
    for i in -4..=4 {
        for row in [-3.0, 3.0] {
            candidates.push((centre.x + i as f32 * 9.0 * unit, centre.z + row * unit));
        }
    }
    candidates.sort_by(|a, b| (a.0 - centre.x).abs().total_cmp(&(b.0 - centre.x).abs()));
    let mut spots: Vec<Vec3> = Vec::new();
    for (x, z) in candidates {
        let bare = (-4..=4).all(|i| {
            (-2..=2).all(|j| {
                floor_at(x + i as f32 * unit, z + j as f32 * unit)
                    .is_some_and(|y| (y - floor).abs() < 0.01 * unit)
            })
        });
        if bare && spots.len() < 9 {
            spots.push(Vec3::new(x, floor, z));
        }
    }
    assert_eq!(spots.len(), 9, "not enough bare floor in the courtyard");
    // Each prop's origin set so that its footprint (scaled) is centred on its spot, two
    // units above the floor.
    let place = |k: usize, footprint: (f32, f32), scale: Vec3| {
        spots[k]
            + Vec3::new(
                -0.5 * footprint.0 * scale.x * unit,
                2.0 * unit,
                -0.5 * footprint.1 * scale.z * unit,
            )
    };
    let turned = Quat::from_rotation_y(30f32.to_radians());
    let u = (3.0, 2.0);
    let props = ImportedScene {
        meshes: vec![
            MeshData {
                name: "u".into(),
                primitives: vec![prism(&U, &U_CAPS, 1.0, unit)],
            },
            MeshData {
                name: "l".into(),
                primitives: vec![prism(&L, &L_CAPS, 1.0, unit)],
            },
            MeshData {
                name: "crate".into(),
                primitives: vec![prism(&SQUARE, &SQUARE_CAPS, 1.0, unit)],
            },
        ],
        nodes: vec![
            node(
                "u",
                Some(0),
                place(0, u, Vec3::ONE),
                Quat::IDENTITY,
                Vec3::ONE,
                Vec::new(),
            ),
            node(
                "u half",
                Some(0),
                place(1, u, Vec3::splat(0.5)),
                Quat::IDENTITY,
                Vec3::splat(0.5),
                Vec::new(),
            ),
            node(
                "u double",
                Some(0),
                place(2, u, Vec3::splat(2.0)),
                Quat::IDENTITY,
                Vec3::splat(2.0),
                Vec::new(),
            ),
            node(
                "u stretched",
                Some(0),
                place(3, u, Vec3::new(2.0, 0.5, 1.0)),
                Quat::IDENTITY,
                Vec3::new(2.0, 0.5, 1.0),
                Vec::new(),
            ),
            node(
                "u mirrored",
                Some(0),
                place(4, u, Vec3::new(-1.0, 1.0, 1.0)),
                Quat::IDENTITY,
                Vec3::new(-1.0, 1.0, 1.0),
                Vec::new(),
            ),
            // A turned U under a parent stretched along x: sheared, baked by the cook.
            node(
                "shear",
                None,
                place(5, u, Vec3::new(1.5, 1.0, 1.0)),
                Quat::IDENTITY,
                Vec3::new(1.5, 1.0, 1.0),
                vec![6],
            ),
            node(
                "u sheared",
                Some(0),
                Vec3::ZERO,
                turned,
                Vec3::ONE,
                Vec::new(),
            ),
            node(
                "l tall",
                Some(1),
                place(6, (2.0, 2.0), Vec3::ONE),
                Quat::IDENTITY,
                Vec3::new(1.0, 1.5, 1.0),
                Vec::new(),
            ),
            node(
                "crate stretched",
                Some(2),
                place(7, (1.0, 1.0), Vec3::ONE),
                Quat::IDENTITY,
                Vec3::new(1.0, 2.0, 1.0),
                Vec::new(),
            ),
            // A turned crate under a parent stretched along x: a box no longer, the exact
            // hull of its corners.
            node(
                "stretch",
                None,
                place(8, (2.0, 1.0), Vec3::ONE),
                Quat::IDENTITY,
                Vec3::new(2.0, 1.0, 1.0),
                vec![10],
            ),
            node(
                "crate turned",
                Some(2),
                Vec3::ZERO,
                turned,
                Vec3::ONE,
                Vec::new(),
            ),
        ],
        roots: vec![0, 1, 2, 3, 4, 5, 7, 8, 9],
        collider_rules: ColliderRules {
            bodies: DefaultBodies::Dynamic,
            ..ColliderRules::default()
        },
        ..ImportedScene::default()
    };
    let ids: Vec<(u32, u32)> = (0..props.nodes.len() as u32).map(|i| (i, 1)).collect();
    let jobs = ThreadPool::builder().workers(4).build().unwrap();
    let (prop_colliders, prop_bodies, stats) =
        cook_colliders(&props, &ids, &props.collider_rules, None, Some(&jobs));
    assert_eq!(stats.decomposed, 3);
    assert_eq!(prop_bodies.len(), 9);
    assert!(prop_bodies.iter().all(|b| b.motion == Motion::Dynamic));
    // The U in three parts, the L in two, the crate one box.
    let parts = |b: &CookedBody| prop_colliders[b.collider as usize].collider.shapes.len();
    assert_eq!(parts(&prop_bodies[0]), 3);
    assert_eq!(parts(&prop_bodies[6]), 2);
    assert!(matches!(
        prop_colliders[prop_bodies[7].collider as usize]
            .collider
            .shapes[..],
        [ColliderShape {
            shape: Shape::Box { .. },
            ..
        }]
    ));
    let report = physics
        .add(&prop_colliders, &prop_bodies, &mut world, &[])
        .unwrap();
    println!("props: {report:?}");
    let ids: Vec<_> = physics.bodies()[statics..].to_vec();
    let names = [
        "u",
        "u half",
        "u double",
        "u stretched",
        "u mirrored",
        "u sheared",
        "l tall",
        "crate stretched",
        "crate turned",
    ];
    for _ in 0..360 {
        physics.step(&mut world, 1.0 / 60.0).unwrap();
    }
    for (k, (name, &id)) in names.iter().zip(&ids).enumerate() {
        let (p, r) = physics.physics().transform(id).unwrap();
        let (v, _) = physics.physics().velocity(id).unwrap();
        let up = r * Vec3::Y;
        println!(
            "{name}: at {p}, up {up}, speed {:.4}, awake {}",
            v.length(),
            physics.physics().is_active(id)
        );
        // On the floor (its origin at its base, within Jolt's 2 cm of allowed penetration),
        // standing as it fell, at rest, and still on its spot.
        assert!(
            (p.y - floor).abs() < 0.15 * unit,
            "{name} at {p}: not on the floor at {floor}"
        );
        assert!(up.y > 0.99, "{name} tipped over: up {up}");
        assert!(v.length() < 0.01 * unit, "{name} still moves at {v}");
        assert!(
            (p.x - place(k, (0.0, 0.0), Vec3::ONE).x).abs() < 6.0 * unit,
            "{name} slid off its spot"
        );
    }
    // A ball into the U's notch (x 1-2, z 1-2 of its outline, open to +z): it falls to the
    // floor inside it, not onto a filled hull a unit up.
    let (u_at, u_turn) = physics.physics().transform(ids[0]).unwrap();
    let radius = 0.15 * unit;
    let ball = physics
        .physics()
        .create_shape(&ShapeDesc::Sphere { radius })
        .unwrap();
    let above_notch = u_at + u_turn * Vec3::new(1.5 * unit, 2.0 * unit, 1.5 * unit);
    let ball = physics
        .physics()
        .create_body(&BodyDesc::dynamic(ball, above_notch))
        .unwrap();
    for _ in 0..180 {
        physics.step(&mut world, 1.0 / 60.0).unwrap();
    }
    let (b, _) = physics.physics().transform(ball).unwrap();
    let top = u_at.y + unit;
    println!("ball at {b}, the U's top at {top}, floor {floor}");
    assert!(
        b.y < floor + 2.0 * radius,
        "the ball is not down in the notch ({} against the floor {floor}, the top {top})",
        b.y
    );
    assert!(b.y > floor - radius, "the ball fell through the floor");
}

#[test]
fn scene_bodies_take_their_collision_layers_by_name() {
    // A third layer, "Props", that collides with everything.
    let settings = PhysicsSettings {
        layers: 3,
        collides: vec![0b110, 0b111, 0b111],
        layer_names: ["Static", "Moving", "Props"].map(String::from).to_vec(),
        ..settings()
    };
    let collider = CookedCollider {
        collider: Collider {
            shapes: vec![ColliderShape {
                id: 0,
                shape: Shape::Box {
                    center: Vec3::ZERO,
                    rotation: Quat::IDENTITY,
                    half_extents: Vec3::ONE,
                },
                surface: 4,
            }],
            ..Collider::default()
        },
        baked: None,
    };
    let body = |layer: Option<&str>| CookedBody {
        layer: layer.map(str::to_owned),
        ..CookedBody::default()
    };
    let ray = Ray {
        origin: Vec3::new(0.0, 10.0, 0.0),
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    let mut world = World::new();
    let mut physics = ScenePhysics::new(&settings).unwrap();
    physics
        .add(
            std::slice::from_ref(&collider),
            &[body(Some("Props"))],
            &mut world,
            &[],
        )
        .unwrap();
    let only = |layer: u8| QueryFilter::layers(1 << layer);
    let hit = physics.physics().raycast(&ray, only(2)).unwrap();
    assert_eq!(physics.physics().surface_of(hit.body, hit.part), Some(4));
    assert!(physics.physics().raycast(&ray, only(0)).is_none());
    // No name: the layer of its motion (static).
    let mut physics = ScenePhysics::new(&settings).unwrap();
    physics
        .add(
            std::slice::from_ref(&collider),
            &[body(None)],
            &mut world,
            &[],
        )
        .unwrap();
    assert!(physics.physics().raycast(&ray, only(0)).is_some());
    // A name the world does not have.
    let mut physics = ScenePhysics::new(&settings).unwrap();
    let error = physics
        .add(
            std::slice::from_ref(&collider),
            &[body(Some("Debris"))],
            &mut world,
            &[],
        )
        .unwrap_err();
    assert!(error.to_string().contains("Debris"), "{error}");
}
