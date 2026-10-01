//! 2D queries, kinematic targets and the character mover against geometry with known
//! answers (ADR 0066).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::Vec2;
use engine_physics2d::{
    BodyId, BodyKind, Material, Mover2d, QueryFilter2d, Shape, Transform2d, World2d,
};

const DT: f32 = 1.0 / 60.0;

fn fixed_box(w: &mut World2d, center: Vec2, half: Vec2, angle: f32, layer: u8) -> BodyId {
    let b = w.create_body(BodyKind::Static, center, angle);
    w.add_shape_on_layer(
        b,
        Shape::Box { half_extents: half },
        Material::default(),
        layer,
    )
    .unwrap();
    b
}

/// A world with zero gravity (queries need no simulation) and a ground whose top is y = 0.
fn world_with_ground() -> (World2d, BodyId) {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let ground = fixed_box(&mut w, Vec2::new(0.0, -1.0), Vec2::new(100.0, 1.0), 0.0, 0);
    (w, ground)
}

#[test]
fn rays_and_shapes_cast_down_onto_the_ground() {
    let (w, ground) = world_with_ground();
    let all = QueryFilter2d::default();
    let hit = w
        .raycast(Vec2::new(1.0, 10.0), Vec2::new(0.0, -20.0), all)
        .unwrap();
    assert_eq!(hit.body, ground);
    assert!((hit.fraction - 0.5).abs() < 1e-5, "{hit:?}");
    assert!((hit.point - Vec2::new(1.0, 0.0)).length() < 1e-4);
    assert!((hit.normal - Vec2::Y).length() < 1e-4);

    let quarter = core::f32::consts::FRAC_PI_2;
    let cases = [
        ("circle", Shape::Circle { radius: 0.5 }, 0.0, 0.5),
        (
            "box",
            Shape::Box {
                half_extents: Vec2::new(0.5, 0.3),
            },
            0.0,
            0.3,
        ),
        (
            "box turned",
            Shape::Box {
                half_extents: Vec2::new(0.5, 0.3),
            },
            quarter,
            0.5,
        ),
        (
            "capsule",
            Shape::Capsule {
                half_length: 0.5,
                radius: 0.25,
            },
            0.0,
            0.75,
        ),
        (
            "capsule lying",
            Shape::Capsule {
                half_length: 0.5,
                radius: 0.25,
            },
            quarter,
            0.25,
        ),
    ];
    for (name, shape, angle, depth) in cases {
        let hit = w
            .cast_shape(
                shape,
                Vec2::new(0.0, 10.0),
                angle,
                Vec2::new(0.0, -20.0),
                all,
            )
            .unwrap();
        let expected = (10.0 - depth) / 20.0;
        println!("{name}: {} (expected {expected})", hit.fraction);
        // Box2D stops casts within its linear slop (5 mm) of contact.
        assert!((hit.fraction - expected).abs() < 3e-4, "{name}: {hit:?}");
        assert!((hit.normal - Vec2::Y).length() < 1e-3, "{name}: {hit:?}");
        assert!(hit.point.y.abs() < 6e-3, "{name}: {hit:?}");
    }
    // Invalid dimensions: nothing.
    assert!(
        w.cast_shape(
            Shape::Circle { radius: 0.0 },
            Vec2::ZERO,
            0.0,
            Vec2::NEG_Y,
            all
        )
        .is_none()
    );
}

#[test]
fn every_hit_layers_ignore_and_overlaps() {
    let mut w = World2d::new(Vec2::ZERO);
    let a = fixed_box(&mut w, Vec2::new(5.0, 0.0), Vec2::splat(0.5), 0.0, 0);
    let b = fixed_box(&mut w, Vec2::new(10.0, 0.0), Vec2::splat(0.5), 0.0, 1);
    let c = fixed_box(&mut w, Vec2::new(15.0, 0.0), Vec2::splat(0.5), 0.0, 0);
    fixed_box(&mut w, Vec2::new(10.0, 5.0), Vec2::splat(0.5), 0.0, 0);
    let all = QueryFilter2d::default();
    let mut hits = Vec::new();
    w.raycast_all(Vec2::ZERO, Vec2::new(20.0, 0.0), all, &mut hits);
    let got: Vec<(BodyId, f32)> = hits.iter().map(|h| (h.body, h.fraction)).collect();
    println!("{got:?}");
    assert_eq!(got.len(), 3);
    for ((body, fraction), (eb, ef)) in got.iter().zip([(a, 0.225), (b, 0.475), (c, 0.725)]) {
        assert_eq!(*body, eb);
        assert!((fraction - ef).abs() < 1e-5);
    }
    hits.clear();
    let ball = Shape::Circle { radius: 0.25 };
    w.cast_shape_all(ball, Vec2::ZERO, 0.0, Vec2::new(20.0, 0.0), all, &mut hits);
    let got: Vec<BodyId> = hits.iter().map(|h| h.body).collect();
    assert_eq!(got, [a, b, c]);
    assert!((hits[0].fraction - 0.2125).abs() < 3e-4, "{:?}", hits[0]);
    // Layer 1 excluded; the first box ignored.
    let layer0 = QueryFilter2d { layers: 1, ..all };
    hits.clear();
    w.raycast_all(Vec2::ZERO, Vec2::new(20.0, 0.0), layer0, &mut hits);
    assert_eq!(hits.iter().map(|h| h.body).collect::<Vec<_>>(), [a, c]);
    let skip_a = QueryFilter2d {
        ignore: Some(a),
        ..all
    };
    assert_eq!(
        w.raycast(Vec2::ZERO, Vec2::new(20.0, 0.0), skip_a)
            .unwrap()
            .body,
        b
    );
    assert_eq!(
        w.cast_shape(ball, Vec2::ZERO, 0.0, Vec2::new(20.0, 0.0), skip_a)
            .unwrap()
            .body,
        b
    );
    // Overlaps: a 6 m bar between the first two boxes touches both; turned upright at
    // x = 10 it touches the middle box and the one above it.
    let bar = Shape::Box {
        half_extents: Vec2::new(3.0, 0.2),
    };
    let mut found = Vec::new();
    w.overlap_shape(bar, Vec2::new(7.5, 0.0), 0.0, all, &mut found);
    let mut expected = vec![a, b];
    expected.sort_by_key(|x| format!("{x:?}"));
    found.sort_by_key(|x| format!("{x:?}"));
    assert_eq!(found, expected);
    found.clear();
    w.overlap_shape(
        bar,
        Vec2::new(10.0, 2.5),
        core::f32::consts::FRAC_PI_2,
        all,
        &mut found,
    );
    assert_eq!(found.len(), 2);
    assert!(found.contains(&b));
    found.clear();
    w.overlap_shape(
        Shape::Circle { radius: 1.9 },
        Vec2::new(12.5, 0.0),
        0.0,
        all,
        &mut found,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn kinematic_targets_are_reached() {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let k = w.create_body(BodyKind::Kinematic, Vec2::ZERO, 0.0);
    w.add_shape(
        k,
        Shape::Box {
            half_extents: Vec2::splat(0.5),
        },
        Material::default(),
    )
    .unwrap();
    for i in 1..=60 {
        let t = i as f32 * DT;
        let target = Transform2d {
            position: Vec2::new(2.0 * t, t),
            angle: t,
        };
        w.move_kinematic(k, target, DT).unwrap();
        w.step(DT, 4);
        let now = w.transform(k).unwrap();
        assert!(
            (now.position - target.position).length() < 1e-4,
            "{i}: {now:?}"
        );
        // Box2D integrates rotations approximately (normalized first order): within 2%.
        assert!(
            (now.angle - target.angle).abs() < 0.02 * target.angle,
            "{i}: {now:?}"
        );
    }
}

/// One fixed update of a mover: walk at `walk` m/s, gravity unless standing, platform
/// velocity while standing.
fn tick(w: &mut World2d, m: &mut Mover2d, walk: f32) {
    let mut v = Vec2::new(
        walk,
        if m.ground.is_some() {
            0.0
        } else {
            m.velocity.y
        },
    );
    v.y -= 10.0 * DT;
    if let Some(ground) = m.ground {
        v += ground.velocity;
    }
    w.move_mover(m, v, DT);
    w.step(DT, 4);
}

#[test]
fn mover_lands_walks_and_stops_at_walls() {
    let (mut w, ground) = world_with_ground();
    fixed_box(&mut w, Vec2::new(4.0, 2.0), Vec2::new(0.5, 2.0), 0.0, 0);
    let mut m = Mover2d::new(Vec2::new(0.0, 1.0), 1.8, 0.3);
    for _ in 0..60 {
        tick(&mut w, &mut m, 0.0);
    }
    println!("landed: {m:?}");
    let g = m.ground.unwrap();
    assert_eq!(g.body, ground);
    assert!((g.normal - Vec2::Y).length() < 1e-3);
    assert!(m.position.y.abs() < 0.01, "{m:?}");
    for _ in 0..30 {
        tick(&mut w, &mut m, 3.0);
    }
    assert!((m.position.x - 1.5).abs() < 0.02, "{m:?}");
    // The wall's face is at x = 3.5: the capsule stops a radius before it.
    for _ in 0..90 {
        tick(&mut w, &mut m, 3.0);
    }
    println!("at the wall: {m:?}");
    assert!((m.position.x - 3.2).abs() < 0.02, "{m:?}");
    assert!(m.ground.is_some());
    assert!(
        m.velocity.x.abs() < 1e-3,
        "velocity into the wall is clipped: {m:?}"
    );
}

#[test]
fn mover_respects_the_slope_limit() {
    for (degrees, walkable) in [(30.0f32, true), (60.0, false)] {
        let (mut w, _) = world_with_ground();
        let angle = degrees.to_radians();
        let (length, thickness) = (20.0, 0.5);
        let along = Vec2::new(angle.cos(), angle.sin());
        let normal = Vec2::new(-angle.sin(), angle.cos());
        let center = Vec2::new(1.0, 0.0) + along * (length * 0.5) - normal * (thickness * 0.5);
        fixed_box(
            &mut w,
            center,
            Vec2::new(length * 0.5, thickness * 0.5),
            angle,
            0,
        );
        let mut m = Mover2d::new(Vec2::ZERO, 1.8, 0.3);
        for _ in 0..180 {
            tick(&mut w, &mut m, 2.0);
        }
        println!("{degrees}°: {m:?}");
        if walkable {
            assert!(m.position.y > 2.0, "{m:?}");
            assert!(m.ground.is_some());
        } else {
            assert!(m.position.y < 0.5, "climbed {degrees}°: {m:?}");
        }
    }
}

#[test]
fn mover_snaps_down_steps_and_rides_platforms() {
    // A 0.2 m ledge ending at x = 2: walking off it with a ground snap keeps the mover on
    // the ground; without, it is airborne for a few frames.
    let mut airborne = Vec::new();
    for snap in [0.3f32, 0.0] {
        let (mut w, _) = world_with_ground();
        fixed_box(&mut w, Vec2::new(-3.0, 0.1), Vec2::new(5.0, 0.1), 0.0, 0);
        let mut m = Mover2d {
            snap_to_ground: snap,
            ..Mover2d::new(Vec2::new(0.0, 0.2), 1.8, 0.3)
        };
        for _ in 0..10 {
            tick(&mut w, &mut m, 0.0);
        }
        let mut frames = 0;
        for _ in 0..90 {
            tick(&mut w, &mut m, 4.0);
            frames += u32::from(m.ground.is_none());
        }
        assert!(m.position.x > 5.0 && m.position.y.abs() < 0.01, "{m:?}");
        airborne.push(frames);
    }
    println!(
        "frames airborne after the ledge: with snap {}, without {}",
        airborne[0], airborne[1]
    );
    // The capsule's round bottom rolls over the ledge's corner for a few frames either
    // way (the corner's normal is steeper than the slope limit: not ground); snapping
    // shortens that.
    assert!(airborne[0] < airborne[1], "{airborne:?}");

    // A platform moving 1 m/s along x carries a mover that adds the ground velocity.
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let platform = w.create_body(BodyKind::Kinematic, Vec2::new(0.0, 1.0), 0.0);
    w.add_shape(
        platform,
        Shape::Box {
            half_extents: Vec2::new(2.0, 0.1),
        },
        Material::default(),
    )
    .unwrap();
    let mut m = Mover2d::new(Vec2::new(0.0, 1.1), 1.8, 0.3);
    for i in 1..=120 {
        let t = i as f32 * DT;
        w.move_kinematic(
            platform,
            Transform2d {
                position: Vec2::new(t, 1.0),
                angle: 0.0,
            },
            DT,
        )
        .unwrap();
        tick(&mut w, &mut m, 0.0);
    }
    println!("on the platform: {m:?}");
    let g = m.ground.unwrap();
    assert_eq!(g.body, platform);
    assert!((g.velocity - Vec2::X).length() < 1e-3, "{g:?}");
    assert!((m.position.x - 2.0).abs() < 0.05, "{m:?}");
    assert!((m.position.y - 1.1).abs() < 0.01, "{m:?}");
}

#[test]
fn mover_is_deterministic() {
    let run = || {
        let (mut w, _) = world_with_ground();
        fixed_box(&mut w, Vec2::new(4.0, 0.3), Vec2::new(0.5, 0.3), 0.4, 0);
        let mut m = Mover2d::new(Vec2::new(0.0, 2.0), 1.8, 0.3);
        for i in 0..240 {
            tick(&mut w, &mut m, ((i / 40) % 3) as f32 * 2.0 - 2.0);
        }
        (
            m.position.x.to_bits(),
            m.position.y.to_bits(),
            m.velocity.x.to_bits(),
        )
    };
    assert_eq!(run(), run());
}
