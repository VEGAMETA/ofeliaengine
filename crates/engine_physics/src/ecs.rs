//! Physics in the ECS (ADR 0066): components that tie entities to bodies and characters,
//! a fixed-step clock, and the sync between the physics world and [`Transform`]s.
//!
//! The simulation runs at a fixed rate; rendering runs at whatever rate the display
//! gives. Per rendered frame:
//!
//! ```text
//! steps = clock.advance(frame_seconds)
//! repeat steps times:
//!     game logic, characters (move_character)
//!     sync.push(world, physics, clock.dt())     // kinematic targets
//!     physics.step(clock.dt(), 1)
//!     sync.pull(world, physics)                 // poses of the last two steps
//! sync.interpolate(world, clock.alpha())        // Transform between them
//! ```
//!
//! Interpolating between the last two steps shows motion one step late but smooth at any
//! frame rate; drawing the latest step directly stutters whenever the number of steps per
//! frame changes. All components are plain data, so ECS snapshots carry them for rollback
//! next to [`PhysicsSnapshot`](crate::PhysicsSnapshot).
//!
//! Bodies are in world space: entities with [`RigidBody`] or [`Character`] should be
//! hierarchy roots. [`PhysicsSync::interpolate`] skips entities with a [`Parent`].

use crate::{BodyId, CharacterId, PhysicsBackend};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Parent, QueryState, Transform, With, Without, World, pod_component};

/// The entity is driven by this rigid body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct RigidBody {
    /// [`BodyId`] value.
    pub id: u64,
}

impl RigidBody {
    /// For `body`.
    pub const fn new(body: BodyId) -> Self {
        Self { id: body.0 }
    }

    /// The body.
    pub const fn body(self) -> BodyId {
        BodyId(self.id)
    }
}

/// The entity is driven by this character controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Character {
    /// [`CharacterId`] value.
    pub id: u64,
}

impl Character {
    /// For `character`.
    pub const fn new(character: CharacterId) -> Self {
        Self { id: character.0 }
    }

    /// The character.
    pub const fn character(self) -> CharacterId {
        CharacterId(self.id)
    }
}

/// Pose of the body or character after the last two fixed steps, written by
/// [`PhysicsSync::pull`] and blended by [`PhysicsSync::interpolate`].
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PhysicsPose {
    /// Orientation one step earlier.
    pub previous_rotation: Quat,
    /// Orientation after the last step.
    pub rotation: Quat,
    /// Position one step earlier.
    pub previous_position: Vec3,
    /// Position after the last step.
    pub position: Vec3,
    _padding: [u32; 2],
}

impl PhysicsPose {
    /// Resting at a pose (both steps equal): use it when spawning or teleporting, so the
    /// first frames do not blend from somewhere else.
    pub const fn at(position: Vec3, rotation: Quat) -> Self {
        Self {
            previous_rotation: rotation,
            rotation,
            previous_position: position,
            position,
            _padding: [0; 2],
        }
    }

    /// Records a new step.
    pub fn push(&mut self, position: Vec3, rotation: Quat) {
        self.previous_position = self.position;
        self.previous_rotation = self.rotation;
        self.position = position;
        self.rotation = rotation;
    }

    /// The pose a fraction `alpha` (`0..=1`) of the way from the previous step to the
    /// last.
    pub fn blend(&self, alpha: f32) -> (Vec3, Quat) {
        let alpha = alpha.clamp(0.0, 1.0);
        (
            self.previous_position.lerp(self.position, alpha),
            // Normalized lerp: within one step the rotation is small, and nlerp is the
            // cheaper, monotonic choice there. It takes the short way round.
            self.previous_rotation.lerp(self.rotation, alpha),
        )
    }
}

impl Default for PhysicsPose {
    fn default() -> Self {
        Self::at(Vec3::ZERO, Quat::IDENTITY)
    }
}

/// A kinematic body's target for the next step: [`PhysicsSync::push`] moves it there
/// with [`PhysicsBackend::move_kinematic`].
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct KinematicTarget {
    /// Target orientation.
    pub rotation: Quat,
    /// Target position.
    pub position: Vec3,
    _padding: u32,
}

impl KinematicTarget {
    /// A target pose.
    pub const fn new(position: Vec3, rotation: Quat) -> Self {
        Self {
            rotation,
            position,
            _padding: 0,
        }
    }
}

pod_component!(RigidBody, Character, PhysicsPose, KinematicTarget);

/// A fixed-rate clock for the simulation: how many steps each rendered frame runs and
/// how far between the last two steps the frame is.
///
/// ```
/// use engine_physics::ecs::FixedStep;
///
/// let mut clock = FixedStep::new(60.0);
/// assert_eq!(clock.advance(1.0 / 30.0), 2); // a 30 fps frame runs two 60 Hz steps
/// assert_eq!(clock.advance(1.0 / 120.0), 0); // a 120 fps frame runs none...
/// assert!((clock.alpha() - 0.5).abs() < 1e-4); // ...and is drawn half way
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedStep {
    dt: f64,
    accumulated: f64,
    /// Most steps one frame runs; the rest of the time is dropped, so a long hitch (a
    /// breakpoint, loading) does not trigger a burst of catch-up steps that would make
    /// the next frame slow too (the "spiral of death").
    pub max_steps: u32,
}

impl FixedStep {
    /// `hz` steps per second, at most 8 per frame.
    pub fn new(hz: f64) -> Self {
        Self {
            dt: 1.0 / hz.max(1e-3),
            accumulated: 0.0,
            max_steps: 8,
        }
    }

    /// Seconds per step.
    pub fn dt(&self) -> f32 {
        self.dt as f32
    }

    /// Adds a frame's duration; returns the number of steps to run now.
    pub fn advance(&mut self, frame_seconds: f32) -> u32 {
        // A step boundary that is a rounding error away still counts.
        const EPSILON: f64 = 1e-9;
        self.accumulated += f64::from(frame_seconds.max(0.0));
        let mut steps = 0;
        while self.accumulated + EPSILON >= self.dt {
            if steps == self.max_steps {
                self.accumulated = 0.0;
                break;
            }
            self.accumulated = (self.accumulated - self.dt).max(0.0);
            steps += 1;
        }
        steps
    }

    /// How far the time since the last step is into the next one, `0..1`: the blend
    /// factor for [`PhysicsSync::interpolate`].
    pub fn alpha(&self) -> f32 {
        (self.accumulated / self.dt).clamp(0.0, 1.0) as f32
    }

    /// Seconds accumulated toward the next step: what a save keeps of the clock beside
    /// the tick (ADR 0104).
    pub fn remainder(&self) -> f64 {
        self.accumulated
    }

    /// Sets the seconds accumulated toward the next step (a save loaded), less than a step.
    pub fn set_remainder(&mut self, seconds: f64) {
        self.accumulated = seconds.clamp(0.0, self.dt * (1.0 - 1e-9));
    }
}

/// Moves data between the ECS and a physics world. Create it once per ECS world; it keeps
/// its queries and buffers, so steady-state syncing does not allocate.
pub struct PhysicsSync {
    bodies: QueryState<(&'static RigidBody, &'static mut PhysicsPose)>,
    body_ids: QueryState<&'static RigidBody, With<PhysicsPose>>,
    characters: QueryState<(&'static Character, &'static mut PhysicsPose)>,
    kinematic: QueryState<(&'static RigidBody, &'static KinematicTarget)>,
    interpolated: QueryState<(&'static PhysicsPose, &'static mut Transform), Without<Parent>>,
    ids: Vec<BodyId>,
    poses: Vec<(Vec3, Quat)>,
}

impl PhysicsSync {
    /// Creates the queries in `world`.
    pub fn new(world: &mut World) -> Self {
        Self {
            bodies: QueryState::new(world),
            body_ids: QueryState::new(world),
            characters: QueryState::new(world),
            kinematic: QueryState::new(world),
            interpolated: QueryState::new(world),
            ids: Vec::new(),
            poses: Vec::new(),
        }
    }

    /// Before a step of `dt` seconds: moves every body with a [`KinematicTarget`] toward
    /// it.
    pub fn push(&mut self, world: &mut World, physics: &mut dyn PhysicsBackend, dt: f32) {
        engine_profiler::zone!("physics::sync::push");
        self.kinematic.for_each_readonly(world, |(body, target)| {
            physics.move_kinematic(body.body(), target.position, target.rotation, dt);
        });
    }

    /// After a step: records the new pose of every body and character in its
    /// [`PhysicsPose`] (one batched read for the bodies).
    pub fn pull(&mut self, world: &mut World, physics: &dyn PhysicsBackend) {
        engine_profiler::zone!("physics::sync::pull");
        self.ids.clear();
        let ids = &mut self.ids;
        self.body_ids
            .for_each_readonly(world, |body| ids.push(body.body()));
        self.poses.clear();
        self.poses
            .resize(self.ids.len(), (Vec3::ZERO, Quat::IDENTITY));
        physics.transforms(&self.ids, &mut self.poses);
        // The same canonical order as the read above (no structural change in between).
        let mut poses = self.poses.iter();
        self.bodies.for_each(world, |(_, pose)| {
            if let Some(&(position, rotation)) = poses.next() {
                pose.push(position, rotation);
            }
        });
        self.characters.for_each(world, |(character, pose)| {
            if let Some(state) = physics.character(character.character()) {
                pose.push(state.position, state.rotation);
            }
        });
    }

    /// Every rendered frame: sets the [`Transform`] translation and rotation of every root
    /// entity with a [`PhysicsPose`] to the pose `alpha` of the way from the previous step
    /// to the last ([`FixedStep::alpha`]). Scale is left alone.
    pub fn interpolate(&mut self, world: &mut World, alpha: f32) {
        engine_profiler::zone!("physics::sync::interpolate");
        self.interpolated
            .for_each_chunk(world, |(poses, transforms)| {
                for (pose, transform) in poses.iter().zip(transforms.iter_mut()) {
                    let (position, rotation) = pose.blend(alpha);
                    transform.translation = position;
                    transform.rotation = rotation;
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_step_counts_steps_and_drops_hitches() {
        let mut clock = FixedStep::new(50.0);
        assert_eq!(clock.advance(0.01), 0);
        assert!((clock.alpha() - 0.5).abs() < 1e-6);
        assert_eq!(clock.advance(0.01), 1);
        assert!(clock.alpha() < 1e-6);
        // Sixty frames of 1/60 s at 50 Hz: exactly fifty steps.
        let mut clock = FixedStep::new(50.0);
        let steps: u32 = (0..60).map(|_| clock.advance(1.0 / 60.0)).sum();
        assert_eq!(steps, 50);
        // A two-second hitch runs at most `max_steps` and forgets the rest.
        assert_eq!(clock.advance(2.0), 8);
        assert_eq!(clock.advance(0.0), 0);
    }

    #[test]
    fn interpolation_blends_the_last_two_steps_of_roots() {
        let mut world = World::new();
        let mut sync = PhysicsSync::new(&mut world);
        let mut pose = PhysicsPose::at(Vec3::ZERO, Quat::IDENTITY);
        pose.push(
            Vec3::new(2.0, 0.0, 0.0),
            Quat::from_rotation_y(core::f32::consts::FRAC_PI_2),
        );
        let root = world.spawn((pose, Transform::from_scale(Vec3::splat(3.0))));
        let child = world.spawn((pose, Transform::IDENTITY));
        world.set_parent(child, Some(root)).unwrap();
        sync.interpolate(&mut world, 0.25);
        let t = *world.get::<Transform>(root).unwrap();
        assert!((t.translation - Vec3::new(0.5, 0.0, 0.0)).length() < 1e-6);
        let (axis, angle) = t.rotation.to_axis_angle();
        assert!((axis - Vec3::Y).length() < 1e-4, "{axis}");
        // Normalized lerp over a (large) 90° step: within 1° of slerp.
        assert!(
            (angle - core::f32::consts::FRAC_PI_8).abs() < 0.02,
            "{angle}"
        );
        assert_eq!(t.scale, Vec3::splat(3.0), "scale untouched");
        // Children keep their local transform.
        assert_eq!(*world.get::<Transform>(child).unwrap(), Transform::IDENTITY);
    }
}
