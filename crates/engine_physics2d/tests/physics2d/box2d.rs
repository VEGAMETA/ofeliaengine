//! Box2D backend: resting contact, a stable pyramid, contact events, handle checks and
//! determinism for the same binary.

#![allow(clippy::unwrap_used)]

use engine_core::math::Vec2;
use engine_physics2d::{
    BodyId, BodyKind, Material, Physics2dError, Shape, Transform2d, Velocity2d, World2d,
};

const DT: f32 = 1.0 / 60.0;

fn ground(w: &mut World2d) -> BodyId {
    let g = w.create_body(BodyKind::Static, Vec2::new(0.0, -1.0), 0.0);
    w.add_shape(
        g,
        Shape::Box {
            half_extents: Vec2::new(100.0, 1.0),
        },
        Material::default(),
    )
    .unwrap();
    g
}

/// A pyramid of unit boxes, `rows` at the base.
fn pyramid(w: &mut World2d, rows: u32) -> Vec<BodyId> {
    let mut bodies = Vec::new();
    for row in 0..rows {
        for i in 0..rows - row {
            let x = i as f32 + row as f32 * 0.5 - rows as f32 * 0.5;
            let y = 0.5 + row as f32;
            let b = w.create_body(BodyKind::Dynamic, Vec2::new(x, y), 0.0);
            w.add_shape(
                b,
                Shape::Box {
                    half_extents: Vec2::splat(0.5),
                },
                Material::default(),
            )
            .unwrap();
            bodies.push(b);
        }
    }
    bodies
}

#[test]
fn a_pyramid_stays_standing_and_falls_asleep() {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    ground(&mut w);
    let bodies = pyramid(&mut w, 20);
    let top = *bodies.last().unwrap();
    let start = w.transform(top).unwrap().position;
    for _ in 0..600 {
        w.step(DT, 4);
    }
    let end = w.transform(top).unwrap().position;
    assert!(
        (end - start).length() < 0.05,
        "top moved from {start} to {end}"
    );
    assert_eq!(w.awake_bodies(), 0, "the settled pyramid sleeps");
}

#[test]
fn a_ball_lands_and_reports_one_contact() {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let g = ground(&mut w);
    let ball = w.create_body(BodyKind::Dynamic, Vec2::new(0.0, 5.0), 0.0);
    w.add_shape(ball, Shape::Circle { radius: 0.5 }, Material::default())
        .unwrap();
    let mut begins = Vec::new();
    for _ in 0..180 {
        w.step(DT, 4);
        begins.extend_from_slice(w.contact_begins());
    }
    assert_eq!(begins.len(), 1);
    let pair = [begins[0].a, begins[0].b];
    assert!(pair.contains(&g) && pair.contains(&ball));
    let y = w.transform(ball).unwrap().position.y;
    assert!((y - 0.5).abs() < 0.02, "{y}");
}

#[test]
fn velocities_teleports_and_stale_handles() {
    let mut w = World2d::new(Vec2::ZERO);
    let b = w.create_body(BodyKind::Kinematic, Vec2::ZERO, 0.0);
    w.set_velocity(
        b,
        Velocity2d {
            linear: Vec2::new(2.0, 0.0),
            angular: 1.0,
        },
    )
    .unwrap();
    for _ in 0..60 {
        w.step(DT, 4);
    }
    let t = w.transform(b).unwrap();
    assert!(
        (t.position.x - 2.0).abs() < 1e-3 && (t.angle - 1.0).abs() < 1e-3,
        "{t:?}"
    );
    w.set_transform(
        b,
        Transform2d {
            position: Vec2::new(-3.0, 1.0),
            angle: 0.0,
        },
    )
    .unwrap();
    assert_eq!(w.transform(b).unwrap().position, Vec2::new(-3.0, 1.0));
    assert_eq!(
        w.add_shape(b, Shape::Circle { radius: -1.0 }, Material::default()),
        Err(Physics2dError::InvalidShape)
    );
    w.destroy_body(b).unwrap();
    assert_eq!(w.transform(b), Err(Physics2dError::InvalidBody));
    assert_eq!(w.destroy_body(b), Err(Physics2dError::InvalidBody));
}

fn pile_hash() -> u64 {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    ground(&mut w);
    let mut bodies = Vec::new();
    for i in 0..200 {
        let x = (i % 10) as f32 * 1.1 - 5.0 + (i / 10) as f32 * 0.05;
        let b = w.create_body(
            BodyKind::Dynamic,
            Vec2::new(x, 2.0 + (i / 10) as f32 * 1.2),
            0.1 * i as f32,
        );
        let shape = if i % 2 == 0 {
            Shape::Circle { radius: 0.5 }
        } else {
            Shape::Box {
                half_extents: Vec2::new(0.5, 0.3),
            }
        };
        w.add_shape(b, shape, Material::default()).unwrap();
        bodies.push(b);
    }
    for _ in 0..240 {
        w.step(DT, 4);
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bodies {
        let t = w.transform(b).unwrap();
        for v in [t.position.x, t.position.y, t.angle] {
            h = (h ^ u64::from(v.to_bits())).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

#[test]
fn deterministic_for_the_same_binary() {
    // Worlds on other test threads are created and stepped concurrently meanwhile.
    let a = pile_hash();
    let b = std::thread::spawn(pile_hash).join().unwrap();
    assert_eq!(a, b);
}
