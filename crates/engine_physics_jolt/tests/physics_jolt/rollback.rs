//! State save/restore for rollback (ADR 0066): restoring and re-simulating gives
//! bit-identical states, a 10-frame rollback reproduces the original frames, and
//! snapshots of another world are refused.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Quat, Vec3};
use engine_physics::scenes;
use engine_physics::{
    BodyDesc, CharacterDesc, CharacterId, CharacterMotor, MotionType, PhysicsBackend, PhysicsError,
    PhysicsSettings, PhysicsSnapshot, ShapeDesc,
};
use engine_physics_jolt::JoltBackend;

const DT: f32 = 1.0 / 60.0;

/// A game-like world: a falling pile, a hanging chain, a kinematic platform and two
/// characters (one with a proxy body).
struct Game {
    world: JoltBackend,
    platform: engine_physics::BodyId,
    characters: [CharacterId; 2],
    motor: CharacterMotor,
}

impl Game {
    fn new(threads: u32) -> Self {
        let mut world = JoltBackend::new(&PhysicsSettings {
            threads,
            single_thread_below: 0,
            ..PhysicsSettings::default()
        })
        .unwrap();
        scenes::pile(&mut world, 300).unwrap();
        // Chain anchored high above the pile.
        let link = world
            .create_shape(&ShapeDesc::Capsule {
                half_height: 0.3,
                radius: 0.1,
            })
            .unwrap();
        let mut previous = None;
        for i in 0..10 {
            let body = world
                .create_body(&BodyDesc {
                    rotation: Quat::from_rotation_z(core::f32::consts::FRAC_PI_2),
                    ..BodyDesc::dynamic(link, Vec3::new(30.4 + i as f32 * 0.8, 20.0, 0.0))
                })
                .unwrap();
            world
                .create_constraint(&engine_physics::ConstraintDesc::Point {
                    a: body,
                    b: previous,
                    anchor: Vec3::new(30.0 + i as f32 * 0.8, 20.0, 0.0),
                })
                .unwrap();
            previous = Some(body);
        }
        let slab = world
            .create_shape(&ShapeDesc::Box {
                half_extents: Vec3::new(2.0, 0.1, 2.0),
            })
            .unwrap();
        let platform = world
            .create_body(&BodyDesc {
                motion: MotionType::Kinematic,
                ..BodyDesc::dynamic(slab, Vec3::new(-30.0, 1.0, 0.0))
            })
            .unwrap();
        let characters = [
            world
                .create_character(&CharacterDesc::capsule(
                    Vec3::new(-30.0, 1.1, 0.0),
                    1.8,
                    0.3,
                ))
                .unwrap(),
            world
                .create_character(&CharacterDesc {
                    proxy_layer: Some(engine_physics::Layer(1)),
                    ..CharacterDesc::capsule(Vec3::new(-20.0, 0.0, 5.0), 1.8, 0.3)
                })
                .unwrap(),
        ];
        Self {
            world,
            platform,
            characters,
            motor: CharacterMotor::default(),
        }
    }

    /// Frame `frame` with its "input": a function of the frame number only.
    fn frame(&mut self, frame: u32) {
        let t = frame as f32 * DT;
        let target = Vec3::new(-30.0 + (t * 0.7).sin() * 3.0, 1.0 + t * 0.1, 0.0);
        self.world
            .move_kinematic(self.platform, target, Quat::from_rotation_y(t * 0.2), DT);
        for (i, &c) in self.characters.iter().enumerate() {
            let walk = Vec3::new(
                ((frame / 20 + i as u32) % 3) as f32 - 1.0,
                0.0,
                ((frame / 30) % 2) as f32 * 2.0 - 1.0,
            ) * 2.5;
            let jump = frame % 45 == 10 + i as u32;
            let state = self.world.character(c).unwrap();
            let v = self.motor.velocity(&state, walk, jump, DT);
            self.world.move_character(c, v, DT).unwrap();
        }
        self.world.step(DT, 1).unwrap();
    }
}

fn save(world: &JoltBackend) -> PhysicsSnapshot {
    let mut snapshot = PhysicsSnapshot::new();
    world.save_state(&mut snapshot).unwrap();
    snapshot
}

#[test]
fn restore_then_step_is_bit_identical() {
    for threads in [0, 3] {
        let mut game = Game::new(threads);
        for f in 0..30 {
            game.frame(f);
        }
        let start = save(&game.world);
        for f in 30..90 {
            game.frame(f);
        }
        let (hash, bytes) = (game.world.state_hash(), save(&game.world));
        game.world.restore_state(&start).unwrap();
        assert_eq!(save(&game.world).bytes(), start.bytes(), "restore is exact");
        for f in 30..90 {
            game.frame(f);
        }
        println!(
            "{threads} workers: {} bytes per snapshot, hash {hash:#018x}",
            start.len()
        );
        assert_eq!(game.world.state_hash(), hash);
        assert_eq!(save(&game.world).hash(), bytes.hash());
        assert_eq!(save(&game.world).bytes(), bytes.bytes());
    }
}

#[test]
fn ten_frame_rollback_reproduces_every_frame() {
    let mut game = Game::new(0);
    // Run 60 frames keeping the last ten snapshots (state before frame f in ring[f % 10])
    // and every frame's hash after it.
    let mut ring: Vec<PhysicsSnapshot> = (0..10).map(|_| PhysicsSnapshot::new()).collect();
    let mut hashes = Vec::new();
    for f in 0..60 {
        game.world.save_state(&mut ring[f as usize % 10]).unwrap();
        game.frame(f);
        hashes.push((game.world.state_hash(), save(&game.world).hash()));
    }
    // A late input for frame 50 arrives: roll back ten frames and re-simulate. With the
    // same inputs, every frame matches the first run.
    game.world.restore_state(&ring[50 % 10]).unwrap();
    assert_eq!(
        (game.world.state_hash(), save(&game.world).hash()),
        hashes[49],
        "back at the end of frame 49"
    );
    for f in 50..60 {
        game.world.save_state(&mut ring[f as usize % 10]).unwrap();
        game.frame(f);
        assert_eq!(
            (game.world.state_hash(), save(&game.world).hash()),
            hashes[f as usize],
            "frame {f}"
        );
    }
    // And a fresh world with the same calls agrees too (same binary).
    let mut fresh = Game::new(0);
    for f in 0..60 {
        fresh.frame(f);
    }
    assert_eq!(fresh.world.state_hash(), hashes[59].0);
}

#[test]
fn snapshots_of_other_worlds_are_refused() {
    let mut a = Game::new(0);
    a.frame(0);
    let snapshot = save(&a.world);
    // One more body: other numbers of bodies.
    let shape = a
        .world
        .create_shape(&ShapeDesc::Sphere { radius: 0.5 })
        .unwrap();
    let extra = a
        .world
        .create_body(&BodyDesc::dynamic(shape, Vec3::new(0.0, 50.0, 0.0)))
        .unwrap();
    assert!(matches!(
        a.world.restore_state(&snapshot),
        Err(PhysicsError::SnapshotMismatch(_))
    ));
    // Removing it again makes the snapshot fit.
    a.world.destroy_body(extra);
    a.world.restore_state(&snapshot).unwrap();
    // Not a snapshot at all; a truncated one.
    assert!(a.world.restore_state(&PhysicsSnapshot::new()).is_err());
    let mut truncated = PhysicsSnapshot::new();
    truncated
        .buffer_mut()
        .extend_from_slice(&snapshot.bytes()[..snapshot.len() / 2]);
    assert!(matches!(
        a.world.restore_state(&truncated),
        Err(PhysicsError::SnapshotMismatch(_))
    ));
}
