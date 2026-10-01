//! Character controllers (ADR 0066).
//!
//! A character is a capsule moved by game code, not by the solver: every fixed update the
//! game decides a velocity (input, gravity, jumps; [`CharacterMotor`] does the usual
//! part), calls [`PhysicsBackend::move_character`](crate::PhysicsBackend::move_character)
//! before the world step, and reads the result. The backend does the collision work:
//!
//! - **collide and slide**: blocked motion slides along walls and floors;
//! - **slope limit**: slopes steeper than [`CharacterDesc::max_slope`] act as walls;
//! - **step up**: steps up to [`CharacterDesc::step_height`] are climbed without
//!   jumping;
//! - **stick to the ground**: walking down slopes and steps up to
//!   [`CharacterDesc::snap_to_ground`] keeps the character on the ground instead of
//!   launching it;
//! - **ground detection**: [`CharacterState::ground`], the ground's normal, body and
//!   velocity (moving platforms);
//! - **pushing**: dynamic bodies in the way are pushed with at most
//!   [`CharacterDesc::max_push_force`]; characters block each other.
//!
//! The character is not a body: the solver, rays and other bodies do not see it unless it
//! has a proxy body ([`CharacterDesc::proxy_layer`]), a kinematic capsule that follows it.

use crate::{BodyId, Layer};
use engine_core::math::{Quat, Vec3};

/// A character controller. Opaque, backend-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CharacterId(pub u64);

/// A character to create: an upright capsule whose bottom is at `position`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterDesc {
    /// Position of the feet (the bottom of the capsule).
    pub position: Vec3,
    /// Orientation; usually a rotation about `up` only.
    pub rotation: Quat,
    /// Up direction (unit). Ground is what supports the character against it.
    pub up: Vec3,
    /// Total height of the capsule, caps included (at least `2 × radius`).
    pub height: f32,
    /// Radius of the capsule.
    pub radius: f32,
    /// Steepest walkable slope, in radians from horizontal. Steeper surfaces block like
    /// walls and do not count as ground.
    pub max_slope: f32,
    /// Highest step climbed without jumping (m); 0 turns stair climbing off.
    pub step_height: f32,
    /// How far down the character is pulled to stay on the ground when walking down
    /// slopes and steps (m); 0 turns it off.
    pub snap_to_ground: f32,
    /// Mass (kg), for pushing bodies and pressing on the ground.
    pub mass: f32,
    /// Largest force (N) with which the character pushes dynamic bodies.
    pub max_push_force: f32,
    /// Collision layer: which bodies the character collides with
    /// ([`PhysicsSettings::collides`](crate::PhysicsSettings::collides)).
    pub layer: Layer,
    /// A kinematic proxy body on this layer follows the character, so rays, shape casts,
    /// sensors and fast bodies see it. It pushes dynamic bodies on layers its layer
    /// collides with like any kinematic body (with infinite mass), so give it a layer that
    /// collides only with what should feel the character that way. `None`: no proxy.
    pub proxy_layer: Option<Layer>,
    /// Game data, returned in contacts.
    pub user_data: u64,
}

impl CharacterDesc {
    /// A capsule character `height` tall with `radius`, feet at `position`, Y up, 50°
    /// slope limit, 0.4 m steps, 0.5 m ground snap, 70 kg pushing with up to 100 N, on
    /// layer 1 without a proxy body.
    pub fn capsule(position: Vec3, height: f32, radius: f32) -> Self {
        Self {
            position,
            rotation: Quat::IDENTITY,
            up: Vec3::Y,
            height,
            radius,
            max_slope: 50f32.to_radians(),
            step_height: 0.4,
            snap_to_ground: 0.5,
            mass: 70.0,
            max_push_force: 100.0,
            layer: Layer(1),
            proxy_layer: None,
            user_data: 0,
        }
    }
}

/// Where a character stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GroundState {
    /// On walkable ground.
    OnGround,
    /// On a slope steeper than [`CharacterDesc::max_slope`]: it slides down.
    OnSteepGround,
    /// Touching something below, but nothing supports it (for example wedged between
    /// two walls).
    NotSupported,
    /// Not touching anything below.
    #[default]
    InAir,
}

/// A character after its last move.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CharacterState {
    /// Position of the feet.
    pub position: Vec3,
    /// Orientation.
    pub rotation: Quat,
    /// Velocity after collisions (m/s): what remained of the requested velocity after
    /// sliding along what it hit.
    pub velocity: Vec3,
    /// Where it stands.
    pub ground: GroundState,
    /// Normal of the ground (zero in the air).
    pub ground_normal: Vec3,
    /// Contact point with the ground.
    pub ground_point: Vec3,
    /// The body it stands on.
    pub ground_body: Option<BodyId>,
    /// Velocity of the ground at the contact point: a moving platform's motion.
    pub ground_velocity: Vec3,
    /// The proxy body, if the character has one ([`CharacterDesc::proxy_layer`]).
    pub proxy: Option<BodyId>,
}

impl CharacterState {
    /// On walkable ground.
    pub fn is_grounded(&self) -> bool {
        self.ground == GroundState::OnGround
    }
}

/// Something a character touched during its last move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterContact {
    /// The body touched, if it was a body.
    pub body: Option<BodyId>,
    /// The character touched, if it was another character.
    pub character: Option<CharacterId>,
    /// Contact point.
    pub point: Vec3,
    /// Contact normal, pointing toward the character.
    pub normal: Vec3,
    /// The contact blocked the motion (as opposed to being merely near).
    pub collided: bool,
    /// The other body is a sensor (reported, never blocks).
    pub sensor: bool,
}

/// The usual way to turn input into a character velocity: walk on the ground, jump, fall,
/// steer in the air, ride moving platforms. Stateless, so rollback needs nothing beyond
/// the physics snapshot (the character's velocity is part of it).
///
/// ```
/// use engine_core::math::Vec3;
/// use engine_physics::{CharacterMotor, CharacterState, GroundState};
///
/// let motor = CharacterMotor::default();
/// let standing = CharacterState { ground: GroundState::OnGround, ..Default::default() };
/// let v = motor.velocity(&standing, Vec3::new(3.0, 0.0, 0.0), false, 1.0 / 60.0);
/// assert_eq!(v.x, 3.0);
/// assert!(v.y < 0.0, "gravity keeps it pressed to the ground");
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterMotor {
    /// Gravity (m/s²).
    pub gravity: Vec3,
    /// Up direction (unit; the character's [`CharacterDesc::up`]).
    pub up: Vec3,
    /// Speed added along `up` by a jump (m/s).
    pub jump_speed: f32,
    /// How much input steers in the air, `0..=1`: 0 keeps the momentum of the jump, 1
    /// steers as on the ground.
    pub air_control: f32,
    /// Ride moving platforms and inherit their velocity when jumping off.
    pub follow_ground: bool,
}

impl Default for CharacterMotor {
    /// Earth gravity along -Y, 5 m/s jumps (about 1.3 m), half air control, platforms
    /// followed.
    fn default() -> Self {
        Self {
            gravity: Vec3::new(0.0, -9.81, 0.0),
            up: Vec3::Y,
            jump_speed: 5.0,
            air_control: 0.5,
            follow_ground: true,
        }
    }
}

impl CharacterMotor {
    /// The velocity to pass to
    /// [`move_character`](crate::PhysicsBackend::move_character): `walk` is the desired
    /// velocity along the ground (its part along `up` is ignored), `jump` jumps if the
    /// character stands on walkable ground, `state` is the character's state before the
    /// move. After the method of Jolt's `CharacterVirtualTest` sample.
    pub fn velocity(&self, state: &CharacterState, walk: Vec3, jump: bool, dt: f32) -> Vec3 {
        let up = self.up.normalize_or(Vec3::Y);
        let walk = walk - up * walk.dot(up);
        let vertical_speed = state.velocity.dot(up);
        let ground_velocity = if self.follow_ground {
            state.ground_velocity
        } else {
            Vec3::ZERO
        };
        // Not when moving away from the ground (the first frames of a jump).
        let toward_ground = vertical_speed - ground_velocity.dot(up) < 0.1;
        let mut velocity = if state.is_grounded() && toward_ground {
            let mut v = ground_velocity + walk;
            if jump {
                v += up * self.jump_speed;
            }
            v
        } else {
            let horizontal = state.velocity - up * vertical_speed;
            let control = self.air_control.clamp(0.0, 1.0);
            up * vertical_speed + horizontal + (walk - horizontal) * control
        };
        // Also on the ground: it keeps the character pressed down (ground detection,
        // platforms moving down).
        velocity += self.gravity * dt;
        velocity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    fn grounded(velocity: Vec3, ground_velocity: Vec3) -> CharacterState {
        CharacterState {
            velocity,
            ground: GroundState::OnGround,
            ground_normal: Vec3::Y,
            ground_velocity,
            ..CharacterState::default()
        }
    }

    #[test]
    fn walks_jumps_and_rides_platforms() {
        let motor = CharacterMotor::default();
        let walk = Vec3::new(2.0, 7.0, 0.0);
        // Walking: input along the ground only, plus one step of gravity.
        let v = motor.velocity(&grounded(Vec3::ZERO, Vec3::ZERO), walk, false, DT);
        assert_eq!(v, Vec3::new(2.0, -9.81 * DT, 0.0));
        // Jumping adds the jump speed.
        let v = motor.velocity(&grounded(Vec3::ZERO, Vec3::ZERO), walk, true, DT);
        assert!((v.y - (5.0 - 9.81 * DT)).abs() < 1e-6);
        // On a platform moving at 1 m/s in z, the platform's velocity is added.
        let v = motor.velocity(
            &grounded(Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0)),
            walk,
            false,
            DT,
        );
        assert_eq!((v.x, v.z), (2.0, 1.0));
        // Not following platforms: ignored.
        let still = CharacterMotor {
            follow_ground: false,
            ..motor
        };
        let v = still.velocity(&grounded(Vec3::ZERO, Vec3::Z), walk, false, DT);
        assert_eq!(v.z, 0.0);
    }

    #[test]
    fn falls_and_steers_in_the_air() {
        let motor = CharacterMotor {
            air_control: 0.25,
            ..CharacterMotor::default()
        };
        let falling = CharacterState {
            velocity: Vec3::new(4.0, -3.0, 0.0),
            ..CharacterState::default()
        };
        let v = motor.velocity(&falling, Vec3::ZERO, true, DT);
        // No jump in the air; a quarter of the way to the input (stop); gravity.
        assert!((v.x - 3.0).abs() < 1e-6);
        assert!((v.y - (-3.0 - 9.81 * DT)).abs() < 1e-6);
        // Leaving the ground upward (the frame after a jump) is not "grounded" yet.
        let rising = grounded(Vec3::new(0.0, 5.0, 0.0), Vec3::ZERO);
        let v = motor.velocity(&rising, Vec3::ZERO, true, DT);
        assert!(
            (v.y - (5.0 - 9.81 * DT)).abs() < 1e-6,
            "no double jump: {v}"
        );
    }
}
