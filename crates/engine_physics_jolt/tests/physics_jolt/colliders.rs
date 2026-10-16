//! Colliders placed at any transform (ADR 0101): scales Jolt takes, scales it does not,
//! shears, mirrors and zero, against geometry with known answers; and colliders baked into
//! Jolt's binary form.

#![allow(clippy::unwrap_used)]

use engine_collision::{Collider, ColliderShape, Shape};
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_physics::colliders::{bake, build, placed};
use engine_physics::{BodyDesc, PhysicsBackend, PhysicsSettings, QueryFilter, Ray};
use engine_physics_jolt::JoltBackend;

fn world() -> JoltBackend {
    JoltBackend::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap()
}

fn collider(shapes: Vec<Shape>) -> Collider {
    Collider {
        shapes: shapes
            .into_iter()
            .enumerate()
            .map(|(id, shape)| ColliderShape {
                id: id as u32,
                shape,
                surface: 0,
            })
            .collect(),
        ..Collider::default()
    }
}

/// Where a ray from far along `from` (towards the origin) first hits, along `from`.
fn reach(w: &JoltBackend, from: Vec3) -> f32 {
    let ray = Ray {
        origin: from * 100.0,
        direction: -from * 200.0,
    };
    w.raycast(&ray, QueryFilter::default())
        .unwrap()
        .point
        .dot(from)
}

/// Builds `c`, places it by `transform` as a static body, and returns the world.
fn placed_world(c: &Collider, transform: Affine3A) -> (JoltBackend, bool) {
    let mut w = world();
    let shape = build(&mut w, c).unwrap();
    let p = placed(&mut w, c, shape, transform, 1e-3).unwrap().unwrap();
    let mut body = BodyDesc::fixed(p.shape, p.position);
    body.rotation = p.rotation;
    w.create_body(&body).unwrap();
    (w, p.made)
}

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

#[test]
fn a_scale_jolt_takes_is_not_rebuilt() {
    let c = collider(vec![
        Shape::Box {
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_extents: Vec3::splat(0.5),
        },
        Shape::Hull {
            points: (0..8)
                .map(|i| {
                    Vec3::new(
                        (i & 1) as f32,
                        ((i >> 1) & 1) as f32 - 0.5,
                        ((i >> 2) & 1) as f32 - 0.5,
                    ) + Vec3::new(2.0, 0.0, 0.0)
                })
                .collect(),
        },
    ]);
    let (w, made) = placed_world(&c, Affine3A::from_scale(Vec3::new(2.0, 3.0, 1.0)));
    assert!(made);
    // The hull's far end, the box's top and its other side.
    assert!(close(reach(&w, Vec3::X), 6.0, 2e-3));
    assert!(close(reach(&w, Vec3::Y), 1.5, 2e-3));
    assert!(close(reach(&w, -Vec3::X), 1.0, 2e-3));
    // No scale: the collider's own shape.
    let (_, made) = placed_world(
        &c,
        Affine3A::from_rotation_translation(Quat::from_rotation_y(1.0), Vec3::ONE),
    );
    assert!(!made);
}

#[test]
fn a_squashed_sphere_is_rebuilt_within_the_tolerance() {
    let c = collider(vec![Shape::Sphere {
        center: Vec3::ZERO,
        radius: 0.5,
    }]);
    let (w, made) = placed_world(&c, Affine3A::from_scale(Vec3::new(2.0, 1.0, 0.5)));
    assert!(made);
    assert!(
        close(reach(&w, Vec3::X), 1.0, 0.01),
        "{}",
        reach(&w, Vec3::X)
    );
    assert!(close(reach(&w, Vec3::Y), 0.5, 0.01));
    assert!(close(reach(&w, Vec3::Z), 0.25, 0.01));
    let diagonal = Vec3::new(1.0, 1.0, 0.0).normalize();
    // On the ellipse x²/1 + y²/0.25 = 1 along the diagonal: t = 1/sqrt(0.5 + 2).
    let expected = (1.0f32 / (0.5 + 2.0)).sqrt();
    assert!(
        close(reach(&w, diagonal), expected, 0.01),
        "{}",
        reach(&w, diagonal)
    );
}

#[test]
fn a_shear_is_baked_into_the_shape() {
    // A unit box turned 45° about Z under a parent stretched along X: a parallelepiped.
    let c = collider(vec![Shape::Box {
        center: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        half_extents: Vec3::splat(0.5),
    }]);
    let transform = Affine3A::from_scale(Vec3::new(2.0, 1.0, 1.0))
        * Affine3A::from_rotation_z(core::f32::consts::FRAC_PI_4);
    let (w, made) = placed_world(&c, transform);
    assert!(made);
    let half_diagonal = core::f32::consts::FRAC_1_SQRT_2;
    assert!(
        close(reach(&w, Vec3::X), 2.0 * half_diagonal, 2e-3),
        "{}",
        reach(&w, Vec3::X)
    );
    assert!(
        close(reach(&w, Vec3::Y), half_diagonal, 2e-3),
        "{}",
        reach(&w, Vec3::Y)
    );
    assert!(close(reach(&w, Vec3::Z), 0.5, 2e-3));
}

#[test]
fn a_zero_scale_disables_the_collider() {
    let c = collider(vec![Shape::Sphere {
        center: Vec3::ZERO,
        radius: 0.5,
    }]);
    let mut w = world();
    let shape = build(&mut w, &c).unwrap();
    assert!(
        placed(
            &mut w,
            &c,
            shape,
            Affine3A::from_scale(Vec3::new(1.0, 0.0, 1.0)),
            1e-3
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn a_mirrored_mesh_collides_from_outside() {
    // A closed cube mesh around the origin.
    let vertices: Vec<Vec3> = (0..8)
        .map(|i| {
            Vec3::new((i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32)
                - Vec3::splat(0.5)
        })
        .collect();
    let triangles = vec![
        [0, 2, 1],
        [1, 2, 3],
        [4, 5, 6],
        [5, 7, 6],
        [0, 1, 4],
        [1, 5, 4],
        [2, 6, 3],
        [3, 6, 7],
        [0, 4, 2],
        [2, 4, 6],
        [1, 3, 5],
        [3, 7, 5],
    ];
    let c = collider(vec![Shape::Mesh {
        vertices,
        triangles,
        surfaces: Vec::new(),
    }]);
    let (w, _) = placed_world(&c, Affine3A::from_scale(Vec3::new(-2.0, 1.0, 1.0)));
    let ray = Ray {
        origin: Vec3::new(10.0, 0.1, 0.1),
        direction: Vec3::new(-20.0, 0.0, 0.0),
    };
    let hit = w.raycast(&ray, QueryFilter::default()).unwrap();
    assert!(close(hit.point.x, 1.0, 2e-3), "{hit:?}");
    assert!(hit.normal.x > 0.9, "the face points outward: {hit:?}");
}

#[test]
fn a_baked_collider_restores_what_was_built() {
    let c = collider(vec![
        Shape::Capsule {
            center: Vec3::new(0.0, 1.0, 0.0),
            rotation: Quat::from_rotation_z(core::f32::consts::FRAC_PI_2),
            half_height: 1.0,
            radius: 0.25,
        },
        Shape::Cylinder {
            center: Vec3::new(0.0, -1.0, 0.0),
            rotation: Quat::IDENTITY,
            half_height: 0.5,
            radius: 0.5,
        },
    ]);
    let mut w = world();
    let baked = bake(&mut w, &c).unwrap().unwrap();
    assert!(baked.castable);
    assert_eq!(Some(baked.format), w.baked_shape_format());
    let mut restored = world();
    let shape = restored
        .create_baked_shape(&baked.bytes, baked.castable)
        .unwrap();
    restored
        .create_body(&BodyDesc::fixed(shape, Vec3::ZERO))
        .unwrap();
    // The capsule lies along X at height 1: its end at x = 1.25.
    let ray = Ray {
        origin: Vec3::new(10.0, 1.0, 0.0),
        direction: Vec3::new(-20.0, 0.0, 0.0),
    };
    assert!(close(
        restored
            .raycast(&ray, QueryFilter::default())
            .unwrap()
            .point
            .x,
        1.25,
        2e-3
    ));
    assert!(close(reach(&restored, -Vec3::Y), 1.5, 2e-3));
}

/// What each of three rays down onto the surfaces test's collider reports.
fn surfaces_hit(w: &JoltBackend) -> [Option<u16>; 3] {
    [
        Vec3::new(2.0, 10.0, -2.0),
        Vec3::new(-2.0, 10.0, 2.0),
        Vec3::new(20.0, 10.0, 0.0),
    ]
    .map(|origin| {
        let hit = w
            .raycast(
                &Ray {
                    origin,
                    direction: Vec3::new(0.0, -20.0, 0.0),
                },
                QueryFilter::default(),
            )
            .unwrap();
        w.surface_of(hit.body, hit.part)
    })
}

#[test]
fn surfaces_reach_the_hits_built_baked_and_scaled() {
    // A floor of two triangles, one with a surface of its own (3), one taking its shape's
    // (9); and a box made of 5.
    let mut c = collider(vec![
        Shape::Mesh {
            vertices: vec![
                Vec3::new(-5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, 5.0),
                Vec3::new(-5.0, 0.0, 5.0),
            ],
            triangles: vec![[0, 2, 1], [0, 3, 2]],
            surfaces: vec![3, u16::MAX],
        },
        Shape::Box {
            center: Vec3::new(20.0, 1.0, 0.0),
            rotation: Quat::IDENTITY,
            half_extents: Vec3::ONE,
        },
    ]);
    c.shapes[0].surface = 9;
    c.shapes[1].surface = 5;
    let expected = [Some(3), Some(9), Some(5)];

    let (w, _) = placed_world(&c, Affine3A::IDENTITY);
    assert_eq!(surfaces_hit(&w), expected, "built");

    let baked = bake(&mut world(), &c).unwrap().unwrap();
    let mut restored = world();
    let shape = restored
        .create_baked_shape(&baked.bytes, baked.castable)
        .unwrap();
    restored
        .create_body(&BodyDesc::fixed(shape, Vec3::ZERO))
        .unwrap();
    assert_eq!(surfaces_hit(&restored), expected, "baked");

    // Stretched along Z: the rays still land on the same pieces.
    let (w, _) = placed_world(&c, Affine3A::from_scale(Vec3::new(1.0, 1.0, 1.5)));
    assert_eq!(surfaces_hit(&w), expected, "scaled");

    // Without surfaces, none.
    for s in &mut c.shapes {
        s.surface = u16::MAX;
    }
    if let Shape::Mesh { surfaces, .. } = &mut c.shapes[0].shape {
        surfaces.clear();
    }
    let (w, _) = placed_world(&c, Affine3A::IDENTITY);
    assert_eq!(surfaces_hit(&w), [None; 3], "none");
}
