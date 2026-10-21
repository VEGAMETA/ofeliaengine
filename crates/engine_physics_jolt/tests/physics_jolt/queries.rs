//! Shape casts, all-hit rays, overlaps and kinematic moves against geometry with known
//! answers (ADR 0066).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Quat, Vec3};
use engine_physics::scenes;
use engine_physics::{
    BodyDesc, BodyId, Layer, MotionType, PhysicsBackend, PhysicsSettings, QueryFilter, Ray,
    ShapeCast, ShapeDesc, ShapeId,
};
use engine_physics_jolt::JoltBackend;

const DT: f32 = 1.0 / 60.0;

fn world() -> JoltBackend {
    JoltBackend::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap()
}

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

fn down_from(w: &JoltBackend, shape: ShapeId, rotation: Quat) -> f32 {
    let cast = ShapeCast {
        shape,
        position: Vec3::new(0.0, 10.0, 0.0),
        rotation,
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    let hit = w.cast_shape(&cast, QueryFilter::default()).unwrap();
    assert!((hit.normal - Vec3::Y).length() < 1e-3, "{hit:?}");
    assert!(hit.point.y.abs() < 1e-3, "contact on the ground: {hit:?}");
    assert_eq!(hit.penetration, 0.0);
    hit.fraction
}

#[test]
fn shapes_cast_down_onto_the_ground() {
    let mut w = world();
    let ground = scenes::ground(&mut w).unwrap();
    let sphere = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(0.5, 0.3, 0.7),
        })
        .unwrap();
    let capsule = w
        .create_shape(&ShapeDesc::Capsule {
            half_height: 0.5,
            radius: 0.25,
        })
        .unwrap();
    let cylinder = w
        .create_shape(&ShapeDesc::Cylinder {
            half_height: 0.4,
            radius: 0.3,
        })
        .unwrap();
    // A prism with a flat bottom at y = -0.3, off its center of mass (the cast places the
    // shape by its origin, not its center of mass).
    let prism = w
        .create_shape(&ShapeDesc::ConvexHull {
            points: &[
                Vec3::new(-0.5, -0.3, -0.5),
                Vec3::new(0.5, -0.3, -0.5),
                Vec3::new(0.0, -0.3, 0.5),
                Vec3::new(-0.5, 0.9, -0.5),
                Vec3::new(0.5, 0.9, -0.5),
                Vec3::new(0.0, 0.9, 0.5),
            ],
            convex_radius: engine_physics::HULL_RADIUS,
        })
        .unwrap();
    // From y = 10 down 20 m: the fraction is (10 - lowest point depth) / 20.
    let cases = [
        ("sphere", sphere, Quat::IDENTITY, 0.5),
        ("box", cube, Quat::IDENTITY, 0.3),
        (
            "box on its side",
            cube,
            Quat::from_rotation_z(90f32.to_radians()),
            0.5,
        ),
        ("capsule upright", capsule, Quat::IDENTITY, 0.75),
        (
            "capsule lying",
            capsule,
            Quat::from_rotation_z(90f32.to_radians()),
            0.25,
        ),
        ("cylinder", cylinder, Quat::IDENTITY, 0.4),
        ("convex hull", prism, Quat::IDENTITY, 0.3),
    ];
    for (name, shape, rotation, depth) in cases {
        let fraction = down_from(&w, shape, rotation);
        let expected = (10.0 - depth) / 20.0;
        println!("{name}: {fraction} (expected {expected})");
        assert!(
            close(fraction, expected, 1e-4),
            "{name}: {fraction} vs {expected}"
        );
    }
    // The ground is on layer 0: excluding it, nothing is hit.
    let cast = ShapeCast {
        shape: sphere,
        position: Vec3::new(0.0, 10.0, 0.0),
        rotation: Quat::IDENTITY,
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    assert!(w.cast_shape(&cast, QueryFilter::layers(1 << 1)).is_none());
    assert!(
        w.cast_shape(&cast, QueryFilter::default().ignoring(ground))
            .is_none()
    );
    // Too short to reach it.
    let short = ShapeCast {
        direction: Vec3::new(0.0, -9.0, 0.0),
        ..cast
    };
    assert!(w.cast_shape(&short, QueryFilter::default()).is_none());
}

#[test]
fn sphere_against_sphere_matches_the_analytic_contact() {
    let mut w = world();
    let big = w.create_shape(&ShapeDesc::Sphere { radius: 2.0 }).unwrap();
    let target = w
        .create_body(&BodyDesc {
            user_data: 7,
            ..BodyDesc::fixed(big, Vec3::ZERO)
        })
        .unwrap();
    let small = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    // Centre path y = 1: contact when the centres are 2.5 apart, at x = -sqrt(2.5² - 1²).
    let cast = ShapeCast {
        shape: small,
        position: Vec3::new(-10.0, 1.0, 0.0),
        rotation: Quat::IDENTITY,
        direction: Vec3::new(20.0, 0.0, 0.0),
    };
    let hit = w.cast_shape(&cast, QueryFilter::default()).unwrap();
    let x = -(2.5f32 * 2.5 - 1.0).sqrt();
    let normal = Vec3::new(x, 1.0, 0.0) / 2.5;
    println!("{hit:?}");
    assert_eq!((hit.body, hit.user_data), (target, 7));
    assert!(
        close(hit.fraction, (x + 10.0) / 20.0, 1e-4),
        "{}",
        hit.fraction
    );
    assert!((hit.normal - normal).length() < 1e-3, "{}", hit.normal);
    assert!((hit.point - normal * 2.0).length() < 1e-3, "{}", hit.point);
}

/// Three 1 m boxes on the x axis at 5, 10 and 15 (the middle one on layer 1), one off the
/// path at z = 5.
fn row_of_boxes(w: &mut JoltBackend) -> [BodyId; 3] {
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let a = w
        .create_body(&BodyDesc::fixed(cube, Vec3::new(5.0, 0.0, 0.0)))
        .unwrap();
    let b = w
        .create_body(&BodyDesc {
            layer: Layer(1),
            ..BodyDesc::fixed(cube, Vec3::new(10.0, 0.0, 0.0))
        })
        .unwrap();
    let c = w
        .create_body(&BodyDesc::fixed(cube, Vec3::new(15.0, 0.0, 0.0)))
        .unwrap();
    w.create_body(&BodyDesc::fixed(cube, Vec3::new(10.0, 0.0, 5.0)))
        .unwrap();
    [a, b, c]
}

#[test]
fn every_hit_in_sweep_order_with_filters() {
    let mut w = world();
    let [a, b, c] = row_of_boxes(&mut w);
    let ball = w.create_shape(&ShapeDesc::Sphere { radius: 0.25 }).unwrap();
    let cast = ShapeCast {
        shape: ball,
        position: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        direction: Vec3::new(20.0, 0.0, 0.0),
    };
    let mut hits = Vec::new();
    w.cast_shape_all(&cast, QueryFilter::default(), &mut hits);
    let got: Vec<(BodyId, f32)> = hits.iter().map(|h| (h.body, h.fraction)).collect();
    println!("{got:?}");
    assert_eq!(got.len(), 3);
    for ((body, fraction), (expected_body, expected)) in
        got.iter().zip([(a, 0.2125), (b, 0.4625), (c, 0.7125)])
    {
        assert_eq!(*body, expected_body);
        assert!(close(*fraction, expected, 1e-4), "{fraction} vs {expected}");
    }
    assert!(hits.iter().all(|h| (h.normal + Vec3::X).length() < 1e-3));

    // Rays: the same, without the radius.
    let ray = Ray {
        origin: Vec3::ZERO,
        direction: Vec3::new(20.0, 0.0, 0.0),
    };
    let mut ray_hits = Vec::new();
    w.raycast_all(&ray, QueryFilter::default(), &mut ray_hits);
    let got: Vec<(BodyId, f32)> = ray_hits.iter().map(|h| (h.body, h.fraction)).collect();
    assert_eq!(got.len(), 3);
    for ((body, fraction), (expected_body, expected)) in
        got.iter().zip([(a, 0.225), (b, 0.475), (c, 0.725)])
    {
        assert_eq!(*body, expected_body);
        assert!(close(*fraction, expected, 1e-5), "{fraction} vs {expected}");
    }
    assert!((ray_hits[1].point - Vec3::new(9.5, 0.0, 0.0)).length() < 1e-4);
    assert!((ray_hits[1].normal + Vec3::X).length() < 1e-4);

    // Layer 1 (the middle box) excluded; the first box ignored.
    hits.clear();
    w.cast_shape_all(&cast, QueryFilter::layers(1 << 0), &mut hits);
    assert_eq!(hits.iter().map(|h| h.body).collect::<Vec<_>>(), [a, c]);
    let closest = w
        .cast_shape(&cast, QueryFilter::default().ignoring(a))
        .unwrap();
    assert_eq!(closest.body, b);
    ray_hits.clear();
    w.raycast_all(&ray, QueryFilter::default().ignoring(b), &mut ray_hits);
    assert_eq!(ray_hits.iter().map(|h| h.body).collect::<Vec<_>>(), [a, c]);
    let hit = w.raycast(&ray, QueryFilter::default().ignoring(a)).unwrap();
    assert_eq!(hit.body, b);

    // Starting inside the first box: reported at fraction 0 with the penetration depth
    // (the sphere's centre at the box's centre: 0.5 + 0.25).
    let inside = ShapeCast {
        position: Vec3::new(5.0, 0.0, 0.0),
        ..cast
    };
    let hit = w.cast_shape(&inside, QueryFilter::default()).unwrap();
    println!("starting inside: {hit:?}");
    assert_eq!((hit.body, hit.fraction), (a, 0.0));
    assert!(close(hit.penetration, 0.75, 1e-3), "{}", hit.penetration);
}

#[test]
fn overlaps_of_any_shape() {
    let mut w = world();
    let [a, b, c] = row_of_boxes(&mut w);
    let probe = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(3.0, 0.2, 0.2),
        })
        .unwrap();
    let mut found = Vec::new();
    // A 6 m bar along x centred at 7.5 touches the first two boxes.
    w.overlap_shape(
        probe,
        Vec3::new(7.5, 0.0, 0.0),
        Quat::IDENTITY,
        QueryFilter::default(),
        &mut found,
    );
    assert_eq!(found, [a, b]);
    // Turned 90° about y it lies along z and touches only the second box.
    found.clear();
    w.overlap_shape(
        probe,
        Vec3::new(10.0, 0.0, 0.0),
        Quat::from_rotation_y(90f32.to_radians()),
        QueryFilter::default(),
        &mut found,
    );
    assert_eq!(found, [b]);
    // A capsule between the boxes touches nothing; a slightly larger one both neighbours.
    let thin = w
        .create_shape(&ShapeDesc::Capsule {
            half_height: 1.0,
            radius: 1.9,
        })
        .unwrap();
    let wide = w
        .create_shape(&ShapeDesc::Capsule {
            half_height: 1.0,
            radius: 2.1,
        })
        .unwrap();
    found.clear();
    w.overlap_shape(
        thin,
        Vec3::new(12.5, 0.0, 0.0),
        Quat::IDENTITY,
        QueryFilter::default(),
        &mut found,
    );
    assert!(found.is_empty(), "{found:?}");
    w.overlap_shape(
        wide,
        Vec3::new(12.5, 0.0, 0.0),
        Quat::IDENTITY,
        QueryFilter::default(),
        &mut found,
    );
    assert_eq!(found, [b, c]);
}

#[test]
fn meshes_are_targets_but_not_cast() {
    let mut w = world();
    let mesh = w
        .create_shape(&ShapeDesc::TriangleMesh {
            vertices: &[
                Vec3::new(-5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, 5.0),
                Vec3::new(-5.0, 0.0, 5.0),
            ],
            triangles: &[[0, 2, 1], [0, 3, 2]],
            surfaces: &[],
        })
        .unwrap();
    let floor = w.create_body(&BodyDesc::fixed(mesh, Vec3::ZERO)).unwrap();
    let ball = w.create_shape(&ShapeDesc::Sphere { radius: 1.0 }).unwrap();
    let cast = ShapeCast {
        shape: ball,
        position: Vec3::new(1.0, 5.0, 1.0),
        rotation: Quat::IDENTITY,
        direction: Vec3::new(0.0, -10.0, 0.0),
    };
    let hit = w.cast_shape(&cast, QueryFilter::default()).unwrap();
    assert_eq!(hit.body, floor);
    assert!(close(hit.fraction, 0.4, 1e-4), "{}", hit.fraction);
    let as_cast = ShapeCast {
        shape: mesh,
        ..cast
    };
    assert!(w.cast_shape(&as_cast, QueryFilter::default()).is_none());
}

#[test]
fn kinematic_bodies_reach_their_targets_and_push() {
    let mut w = world();
    scenes::ground(&mut w).unwrap();
    let paddle_shape = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(0.25, 0.5, 1.0),
        })
        .unwrap();
    let paddle = w
        .create_body(&BodyDesc {
            motion: MotionType::Kinematic,
            ..BodyDesc::dynamic(paddle_shape, Vec3::new(0.0, 0.5, 0.0))
        })
        .unwrap();
    let crate_shape = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.4),
        })
        .unwrap();
    let crate_body = w
        .create_body(&BodyDesc::dynamic(crate_shape, Vec3::new(1.5, 0.4, 0.0)))
        .unwrap();
    // 2 m/s along x for 2 s, a target per step, turning a quarter turn on the way.
    let steps = 120;
    for i in 1..=steps {
        let t = i as f32 * DT;
        let target = Vec3::new(2.0 * t, 0.5, 0.0);
        let turn = Quat::from_rotation_y(t / 2.0 * 90f32.to_radians() / 2.0);
        w.move_kinematic(paddle, target, turn, DT);
        w.step(DT, 1).unwrap();
        let (p, r) = w.transform(paddle).unwrap();
        assert!((p - target).length() < 1e-4, "step {i}: {p} vs {target}");
        assert!(r.angle_between(turn) < 1e-3, "step {i}");
    }
    // Without a new target it keeps moving at its last velocity.
    w.step(DT, 1).unwrap();
    let (p, _) = w.transform(paddle).unwrap();
    assert!(close(p.x, 4.0 + 2.0 * DT, 1e-3), "{p}");
    // The crate was pushed ahead of the paddle.
    let (c, _) = w.transform(crate_body).unwrap();
    println!("paddle at {p}, crate at {c}");
    assert!(c.x > 4.2, "the crate was not pushed: {c}");
}
