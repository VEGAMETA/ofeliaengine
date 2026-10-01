//! Physics in the ECS (ADR 0066): a fixed-step loop with kinematic targets pushed from
//! components, poses pulled after each step, interpolated transforms per frame, and the
//! ECS and physics rolled back together.

#![allow(clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Snapshot, Transform, World};
use engine_physics::ecs::{
    Character, FixedStep, KinematicTarget, PhysicsPose, PhysicsSync, RigidBody,
};
use engine_physics::scenes;
use engine_physics::{
    BodyDesc, CharacterDesc, MotionType, PhysicsBackend, PhysicsSettings, PhysicsSnapshot,
    ShapeDesc,
};
use engine_physics_jolt::JoltBackend;

#[test]
fn fixed_steps_kinematic_targets_and_interpolation() {
    let mut physics = JoltBackend::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap();
    scenes::ground(&mut physics).unwrap();
    let ball = physics
        .create_shape(&ShapeDesc::Sphere { radius: 0.5 })
        .unwrap();
    let falling = physics
        .create_body(&BodyDesc {
            linear_damping: 0.0,
            ..BodyDesc::dynamic(ball, Vec3::new(0.0, 20.0, 0.0))
        })
        .unwrap();
    let mover = physics
        .create_body(&BodyDesc {
            motion: MotionType::Kinematic,
            ..BodyDesc::dynamic(ball, Vec3::new(5.0, 3.0, 0.0))
        })
        .unwrap();
    let hero = physics
        .create_character(&CharacterDesc::capsule(Vec3::new(-5.0, 0.0, 0.0), 1.8, 0.3))
        .unwrap();

    let mut ecs = World::new();
    let mut sync = PhysicsSync::new(&mut ecs);
    let ball_entity = ecs.spawn((
        RigidBody::new(falling),
        PhysicsPose::at(Vec3::new(0.0, 20.0, 0.0), Quat::IDENTITY),
        Transform::IDENTITY,
    ));
    let mover_entity = ecs.spawn((
        RigidBody::new(mover),
        PhysicsPose::at(Vec3::new(5.0, 3.0, 0.0), Quat::IDENTITY),
        KinematicTarget::new(Vec3::new(5.0, 3.0, 0.0), Quat::IDENTITY),
        Transform::IDENTITY,
    ));
    let hero_entity = ecs.spawn((
        Character::new(hero),
        PhysicsPose::at(Vec3::new(-5.0, 0.0, 0.0), Quat::IDENTITY),
        Transform::IDENTITY,
    ));

    // 50 Hz simulation, 144 Hz frames, for one second: the kinematic body's target moves
    // 1 m/s along z.
    let mut clock = FixedStep::new(50.0);
    let mut steps_run = 0;
    let mut time = 0.0f32;
    for _ in 0..144 {
        for _ in 0..clock.advance(1.0 / 144.0) {
            steps_run += 1;
            time += clock.dt();
            ecs.get_mut::<KinematicTarget>(mover_entity)
                .unwrap()
                .position = Vec3::new(5.0, 3.0, time);
            sync.push(&mut ecs, &mut physics, clock.dt());
            physics.step(clock.dt(), 1).unwrap();
            sync.pull(&mut ecs, &physics);
        }
        sync.interpolate(&mut ecs, clock.alpha());
        // The rendered transform lies between the last two physics poses.
        let pose = *ecs.get::<PhysicsPose>(ball_entity).unwrap();
        let shown = ecs.get::<Transform>(ball_entity).unwrap().translation;
        let low = pose.position.y.min(pose.previous_position.y) - 1e-5;
        let high = pose.position.y.max(pose.previous_position.y) + 1e-5;
        assert!((low..=high).contains(&shown.y), "{shown} {pose:?}");
    }
    assert!((49..=50).contains(&steps_run), "{steps_run}");
    // The mover followed its target exactly; the pose and the physics agree.
    let pose = ecs.get::<PhysicsPose>(mover_entity).unwrap();
    assert!((pose.position - Vec3::new(5.0, 3.0, time)).length() < 1e-4);
    let (p, _) = physics.transform(falling).unwrap();
    assert_eq!(ecs.get::<PhysicsPose>(ball_entity).unwrap().position, p);
    let hero_pose = ecs.get::<PhysicsPose>(hero_entity).unwrap();
    assert_eq!(
        hero_pose.position,
        physics.character(hero).unwrap().position
    );

    // Roll the ECS and the physics back together, re-run the same steps: same result.
    let mut ecs_saved = Snapshot::new();
    ecs.save(&mut ecs_saved).unwrap();
    let mut physics_saved = PhysicsSnapshot::new();
    physics.save_state(&mut physics_saved).unwrap();
    let step = |ecs: &mut World, physics: &mut JoltBackend, sync: &mut PhysicsSync| {
        for _ in 0..30 {
            sync.push(ecs, physics, 0.02);
            physics.step(0.02, 1).unwrap();
            sync.pull(ecs, physics);
        }
        (ecs.state_hash(), physics.state_hash())
    };
    let first = step(&mut ecs, &mut physics, &mut sync);
    ecs.restore(&ecs_saved).unwrap();
    physics.restore_state(&physics_saved).unwrap();
    assert_eq!(step(&mut ecs, &mut physics, &mut sync), first);
}
