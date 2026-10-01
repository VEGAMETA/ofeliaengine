//! Character controllers on flat ground, stairs, slopes and a moving platform; pushing,
//! blocking, contacts and proxy bodies (ADR 0066).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Quat, Vec3};
use engine_physics::scenes;
use engine_physics::{
    BodyDesc, BodyId, CharacterDesc, CharacterId, CharacterMotor, GroundState, Layer, MotionType,
    PhysicsBackend, PhysicsSettings, QueryFilter, Ray, ShapeDesc,
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

/// A 1.8 m capsule of radius 0.3 with the default limits (50°, 0.4 m steps).
fn character(w: &mut JoltBackend, feet: Vec3) -> CharacterId {
    w.create_character(&CharacterDesc::capsule(feet, 1.8, 0.3))
        .unwrap()
}

/// One fixed update: the motor's velocity from `walk`, the move, the world step.
fn tick(w: &mut JoltBackend, c: CharacterId, motor: &CharacterMotor, walk: Vec3) {
    let state = w.character(c).unwrap();
    let velocity = motor.velocity(&state, walk, false, DT);
    w.move_character(c, velocity, DT).unwrap();
    w.step(DT, 1).unwrap();
}

fn run(w: &mut JoltBackend, c: CharacterId, walk: Vec3, seconds: f32) {
    let motor = CharacterMotor::default();
    for _ in 0..(seconds / DT).round() as u32 {
        tick(w, c, &motor, walk);
    }
}

fn fixed_box(w: &mut JoltBackend, center: Vec3, half: Vec3, rotation: Quat) -> BodyId {
    let shape = w
        .create_shape(&ShapeDesc::Box { half_extents: half })
        .unwrap();
    w.create_body(&BodyDesc {
        rotation,
        ..BodyDesc::fixed(shape, center)
    })
    .unwrap()
}

#[test]
fn falls_lands_stands_and_walks() {
    let mut w = world();
    let ground = scenes::ground(&mut w).unwrap();
    let c = character(&mut w, Vec3::new(0.0, 2.0, 0.0));
    assert_eq!(w.character(c).unwrap().ground, GroundState::InAir);
    run(&mut w, c, Vec3::ZERO, 1.5);
    let s = w.character(c).unwrap();
    println!("landed: {s:?}");
    assert!(s.is_grounded());
    assert_eq!(s.ground_body, Some(ground));
    assert!((s.ground_normal - Vec3::Y).length() < 1e-3);
    // The feet rest on the ground (within Jolt's 2 cm character padding).
    assert!(s.position.y.abs() < 0.03, "{}", s.position);
    // 3 m/s for 2 s.
    let start = s.position.x;
    run(&mut w, c, Vec3::new(3.0, 0.0, 0.0), 2.0);
    let s = w.character(c).unwrap();
    println!("walked to {}", s.position);
    assert!((s.position.x - start - 6.0).abs() < 0.05, "{}", s.position);
    assert!(s.position.y.abs() < 0.03 && s.is_grounded());
}

/// Eight 0.25 m steps with 0.5 m treads from x = 2: the top is 2 m up, from x = 5.5.
fn stairs(w: &mut JoltBackend) {
    for i in 0..8 {
        let start = 2.0 + 0.5 * i as f32;
        let top = 0.25 * (i + 1) as f32;
        let end = 10.0;
        fixed_box(
            w,
            Vec3::new((start + end) * 0.5, top * 0.5, 0.0),
            Vec3::new((end - start) * 0.5, top * 0.5, 2.0),
            Quat::IDENTITY,
        );
    }
}

#[test]
fn climbs_stairs_up_to_its_step_height() {
    let mut w = world();
    scenes::ground(&mut w).unwrap();
    stairs(&mut w);
    let c = character(&mut w, Vec3::ZERO);
    run(&mut w, c, Vec3::new(2.0, 0.0, 0.0), 4.0);
    let s = w.character(c).unwrap();
    println!("on the stairs: {s:?}");
    assert!(s.position.x > 6.0, "{}", s.position);
    assert!((s.position.y - 2.0).abs() < 0.03, "{}", s.position);
    assert!(s.is_grounded());

    // Without stair climbing it stops at the first step: the capsule's radius before it.
    let mut w = world();
    scenes::ground(&mut w).unwrap();
    stairs(&mut w);
    let c = w
        .create_character(&CharacterDesc {
            step_height: 0.0,
            ..CharacterDesc::capsule(Vec3::ZERO, 1.8, 0.3)
        })
        .unwrap();
    run(&mut w, c, Vec3::new(2.0, 0.0, 0.0), 4.0);
    let s = w.character(c).unwrap();
    println!("blocked: {s:?}");
    assert!((s.position.x - 1.7).abs() < 0.05, "{}", s.position);
    assert!(s.position.y.abs() < 0.03);
}

/// A ramp rising along +x at `degrees`, starting at x = 1 on the ground.
fn ramp(w: &mut JoltBackend, degrees: f32) {
    let angle = degrees.to_radians();
    let length = 20.0;
    let thickness = 0.5;
    // Centre of a slab whose top surface passes through (1, 0, 0).
    let along = Vec3::new(angle.cos(), angle.sin(), 0.0);
    let normal = Vec3::new(-angle.sin(), angle.cos(), 0.0);
    let center = Vec3::new(1.0, 0.0, 0.0) + along * (length * 0.5) - normal * (thickness * 0.5);
    fixed_box(
        w,
        center,
        Vec3::new(length * 0.5, thickness * 0.5, 3.0),
        Quat::from_rotation_z(angle),
    );
}

#[test]
fn walks_up_gentle_slopes_but_not_steep_ones() {
    for (degrees, walkable) in [(30.0f32, true), (60.0, false)] {
        let mut w = world();
        scenes::ground(&mut w).unwrap();
        ramp(&mut w, degrees);
        let c = character(&mut w, Vec3::ZERO);
        run(&mut w, c, Vec3::new(2.0, 0.0, 0.0), 3.0);
        let s = w.character(c).unwrap();
        println!("{degrees}°: {s:?}");
        if walkable {
            // About 5 m along the ramp in 3 s at 2 m/s horizontal: well up the slope.
            assert!(s.position.y > 2.0, "{}", s.position);
            assert!(s.is_grounded());
            let expected = (s.position.x - 1.0) * degrees.to_radians().tan();
            assert!(
                (s.position.y - expected).abs() < 0.1,
                "on the surface: {}",
                s.position
            );
        } else {
            assert!(s.position.y < 0.3, "climbed a 60° slope: {}", s.position);
            assert!(s.position.x < 1.3, "{}", s.position);
        }
    }
}

#[test]
fn rides_a_moving_platform() {
    let mut w = world();
    scenes::ground(&mut w).unwrap();
    let platform_shape = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(2.0, 0.1, 2.0),
        })
        .unwrap();
    let start = Vec3::new(0.0, 0.9, 0.0);
    let platform = w
        .create_body(&BodyDesc {
            motion: MotionType::Kinematic,
            ..BodyDesc::dynamic(platform_shape, start)
        })
        .unwrap();
    let c = character(&mut w, Vec3::new(0.0, 1.0, 0.0));
    let motor = CharacterMotor::default();
    let steps = 120;
    for i in 1..=steps {
        // 1 m/s along x, rising 0.25 m/s.
        let t = i as f32 * DT;
        let target = start + Vec3::new(t, 0.25 * t, 0.0);
        w.move_kinematic(platform, target, Quat::IDENTITY, DT);
        tick(&mut w, c, &motor, Vec3::ZERO);
    }
    let s = w.character(c).unwrap();
    let (p, _) = w.transform(platform).unwrap();
    println!("platform at {p}, character {s:?}");
    assert_eq!(s.ground_body, Some(platform));
    assert!(s.is_grounded());
    assert!((s.ground_velocity - Vec3::new(1.0, 0.25, 0.0)).length() < 1e-3);
    // Carried along: 2 m in x, standing on the platform's top (y = p.y + 0.1).
    assert!((s.position.x - 2.0).abs() < 0.05, "{}", s.position);
    assert!((s.position.y - (p.y + 0.1)).abs() < 0.03, "{}", s.position);
}

#[test]
fn pushes_bodies_up_to_its_strength() {
    for (force, pushed) in [(400.0, true), (0.0, false)] {
        let mut w = world();
        scenes::ground(&mut w).unwrap();
        let shape = w
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::splat(0.5),
            })
            .unwrap();
        let crate_body = w
            .create_body(&BodyDesc {
                mass: Some(20.0),
                friction: 0.2,
                ..BodyDesc::dynamic(shape, Vec3::new(1.5, 0.5, 0.0))
            })
            .unwrap();
        let c = w
            .create_character(&CharacterDesc {
                max_push_force: force,
                ..CharacterDesc::capsule(Vec3::ZERO, 1.8, 0.3)
            })
            .unwrap();
        run(&mut w, c, Vec3::new(1.5, 0.0, 0.0), 2.0);
        let (p, _) = w.transform(crate_body).unwrap();
        let s = w.character(c).unwrap();
        println!(
            "push force {force} N: crate at {p}, character at {}",
            s.position
        );
        let mut contacts = Vec::new();
        w.character_contacts(c, &mut contacts);
        assert!(
            contacts
                .iter()
                .any(|k| k.body == Some(crate_body) && k.collided),
            "{contacts:?}"
        );
        if pushed {
            assert!(p.x > 2.5, "{p}");
        } else {
            assert!((p.x - 1.5).abs() < 0.02, "{p}");
            assert!(s.position.x < 0.75, "{}", s.position);
        }
    }
}

#[test]
fn characters_block_each_other_and_proxies_are_visible() {
    let mut w = world();
    scenes::ground(&mut w).unwrap();
    let walker = character(&mut w, Vec3::ZERO);
    let other = w
        .create_character(&CharacterDesc {
            proxy_layer: Some(Layer(1)),
            user_data: 99,
            ..CharacterDesc::capsule(Vec3::new(2.0, 0.0, 0.0), 1.8, 0.3)
        })
        .unwrap();
    let motor = CharacterMotor::default();
    for _ in 0..120 {
        for (id, walk) in [(walker, Vec3::new(2.0, 0.0, 0.0)), (other, Vec3::ZERO)] {
            let state = w.character(id).unwrap();
            let v = motor.velocity(&state, walk, false, DT);
            w.move_character(id, v, DT).unwrap();
        }
        w.step(DT, 1).unwrap();
    }
    let a = w.character(walker).unwrap();
    let b = w.character(other).unwrap();
    println!("walker {a:?}\nother {b:?}");
    // The walker stops at the other (two radii apart) or nudges it, never passes.
    assert!(
        a.position.x < b.position.x - 0.55,
        "{} {}",
        a.position,
        b.position
    );
    let mut contacts = Vec::new();
    w.character_contacts(walker, &mut contacts);
    assert!(
        contacts.iter().any(|k| k.character == Some(other)),
        "{contacts:?}"
    );

    // A ray at chest height hits the proxy of the other character, not the walker (no
    // proxy); the proxy can be ignored.
    let proxy = b.proxy.unwrap();
    let ray = Ray {
        origin: Vec3::new(-5.0, 1.2, 0.0),
        direction: Vec3::new(20.0, 0.0, 0.0),
    };
    let hit = w.raycast(&ray, QueryFilter::default()).unwrap();
    assert_eq!((hit.body, hit.user_data), (proxy, 99));
    assert!(
        w.raycast(&ray, QueryFilter::default().ignoring(proxy))
            .is_none()
    );
    assert!(a.proxy.is_none());

    // Teleport and destroy.
    w.set_character_transform(walker, Vec3::new(-10.0, 0.0, 0.0), Quat::IDENTITY);
    assert!((w.character(walker).unwrap().position.x + 10.0).abs() < 1e-5);
    assert!(w.character(walker).unwrap().is_grounded());
    w.destroy_character(other);
    assert!(w.character(other).is_none());
    assert!(
        w.raycast(&ray, QueryFilter::default()).is_none(),
        "proxy removed"
    );
    assert!(w.move_character(other, Vec3::ZERO, DT).is_none());
}

#[test]
fn rejects_degenerate_capsules() {
    let mut w = world();
    assert!(
        w.create_character(&CharacterDesc::capsule(Vec3::ZERO, 0.4, 0.3))
            .is_err()
    );
    assert!(
        w.create_character(&CharacterDesc::capsule(Vec3::ZERO, 1.0, 0.0))
            .is_err()
    );
}
