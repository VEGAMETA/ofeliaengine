//! Physics interface (CLAUDE.md §8, ADR 0018): the [`PhysicsBackend`] trait and the types
//! every backend shares, so Jolt (the reference, `engine_physics_jolt`) and our own solver
//! are interchangeable and measured on the same scenes ([`scenes`]).
//!
//! Design points:
//! - **Handles, not pointers**: [`BodyId`], [`ShapeId`], [`ConstraintId`] are opaque and
//!   backend-defined.
//! - **Shapes are shared, immutable resources**, created once and used by many bodies.
//! - **Events are arrays read after the step** ([`PhysicsBackend::contact_events`]), sorted
//!   deterministically, never callbacks into game code from worker threads.
//! - **Gravity is first-class**: world gravity, a per-body factor, a per-body vector, or
//!   the sum of [`GravityField`]s (attractor points, directional volumes), evaluated by the
//!   backend every sub-step.
//! - **Batched queries** ([`PhysicsBackend::raycast_batch`]) for AI and sensors.
//! - **Determinism**: same binary, same API call order, same results;
//!   [`PhysicsBackend::state_hash`] checks it.

use engine_core::math::{Quat, Vec3};

pub mod scenes;

/// A rigid body. Opaque, backend-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BodyId(pub u64);

/// A collision shape. Opaque, backend-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShapeId(pub u64);

/// A constraint (joint). Opaque, backend-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConstraintId(pub u64);

/// A collision layer, `0..PhysicsSettings::layers`. Which layers collide is set by
/// [`PhysicsSettings::collides`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Layer(pub u8);

/// How a body moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionType {
    /// Never moves (level geometry).
    Static,
    /// Moved by the game (velocity or target transform); pushes dynamic bodies, is not
    /// pushed.
    Kinematic,
    /// Simulated.
    #[default]
    Dynamic,
}

/// Collision detection for fast bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionQuality {
    /// Positions at step ends only (fast; thin or fast objects may tunnel).
    #[default]
    Discrete,
    /// Continuous: sweeps the body along its motion (bullets, fast small objects).
    Continuous,
}

/// Which gravity a body feels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum GravityMode {
    /// World gravity × factor (1 = normal, 0 = none, -1 = falls up).
    Scaled(f32),
    /// A fixed vector for this body (m/s²), for example toward a planet's surface normal.
    Custom(Vec3),
    /// The sum of the world's [`GravityField`]s at the body's center of mass.
    Fields,
    /// World gravity.
    #[default]
    World,
}

/// A region of gravity for bodies in [`GravityMode::Fields`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GravityField {
    /// Pulls toward `center`: `strength / max(distance, min_distance)²` (m/s²), zero
    /// beyond `radius`. A planet or black hole.
    Point {
        /// Attractor position.
        center: Vec3,
        /// Acceleration at one meter (m³/s²; `G × mass`).
        strength: f32,
        /// Distance below which the pull stops growing (avoids the singularity).
        min_distance: f32,
        /// Range.
        radius: f32,
    },
    /// Constant acceleration inside an axis-aligned box (a gravity lift, a wind tunnel).
    Box {
        /// Minimum corner.
        min: Vec3,
        /// Maximum corner.
        max: Vec3,
        /// Acceleration inside.
        acceleration: Vec3,
    },
}

impl GravityField {
    /// Acceleration at `point`.
    pub fn acceleration_at(&self, point: Vec3) -> Vec3 {
        match *self {
            Self::Point {
                center,
                strength,
                min_distance,
                radius,
            } => {
                let to_center = center - point;
                let distance = to_center.length();
                if distance > radius || distance <= 1e-9 {
                    return Vec3::ZERO;
                }
                let d = distance.max(min_distance);
                to_center / distance * (strength / (d * d))
            }
            Self::Box {
                min,
                max,
                acceleration,
            } => {
                if point.cmpge(min).all() && point.cmple(max).all() {
                    acceleration
                } else {
                    Vec3::ZERO
                }
            }
        }
    }
}

/// A collision shape to create.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeDesc<'a> {
    /// Sphere around the origin.
    Sphere {
        /// Radius.
        radius: f32,
    },
    /// Box around the origin.
    Box {
        /// Half the size on each axis.
        half_extents: Vec3,
    },
    /// Capsule along Y: a cylinder of `half_height` × 2 with hemispherical caps.
    Capsule {
        /// Half the length of the cylindrical part.
        half_height: f32,
        /// Radius.
        radius: f32,
    },
    /// Cylinder along Y.
    Cylinder {
        /// Half the height.
        half_height: f32,
        /// Radius.
        radius: f32,
    },
    /// Convex hull of points.
    ConvexHull {
        /// Points (at least 4, not coplanar).
        points: &'a [Vec3],
    },
    /// Triangle mesh (static geometry; dynamic bodies should use convex shapes).
    TriangleMesh {
        /// Vertices.
        vertices: &'a [Vec3],
        /// Triangles as vertex indices, counter-clockwise seen from outside.
        triangles: &'a [[u32; 3]],
    },
}

/// A rigid body to create.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyDesc {
    /// Its shape.
    pub shape: ShapeId,
    /// Position of the shape's origin.
    pub position: Vec3,
    /// Orientation.
    pub rotation: Quat,
    /// Initial linear velocity (m/s).
    pub linear_velocity: Vec3,
    /// Initial angular velocity (rad/s).
    pub angular_velocity: Vec3,
    /// Static, kinematic or dynamic.
    pub motion: MotionType,
    /// Discrete or continuous collision detection.
    pub quality: MotionQuality,
    /// Collision layer.
    pub layer: Layer,
    /// Friction coefficient (0 = ice).
    pub friction: f32,
    /// Restitution (0 = no bounce, 1 = elastic).
    pub restitution: f32,
    /// Linear velocity damping per second.
    pub linear_damping: f32,
    /// Angular velocity damping per second.
    pub angular_damping: f32,
    /// Gravity.
    pub gravity: GravityMode,
    /// Mass override in kg (otherwise from the shape's volume at 1000 kg/m³).
    pub mass: Option<f32>,
    /// Reports overlaps as events but does not collide.
    pub sensor: bool,
    /// May go to sleep when at rest.
    pub allow_sleeping: bool,
    /// Game data returned in events and query hits.
    pub user_data: u64,
}

impl BodyDesc {
    /// A dynamic body with default material at `position`.
    pub fn dynamic(shape: ShapeId, position: Vec3) -> Self {
        Self {
            shape,
            position,
            rotation: Quat::IDENTITY,
            linear_velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            motion: MotionType::Dynamic,
            quality: MotionQuality::Discrete,
            layer: Layer(1),
            friction: 0.5,
            restitution: 0.0,
            linear_damping: 0.05,
            angular_damping: 0.05,
            gravity: GravityMode::World,
            mass: None,
            sensor: false,
            allow_sleeping: true,
            user_data: 0,
        }
    }

    /// A static body at `position` on layer 0.
    pub fn fixed(shape: ShapeId, position: Vec3) -> Self {
        Self {
            motion: MotionType::Static,
            layer: Layer(0),
            ..Self::dynamic(shape, position)
        }
    }
}

/// A constraint between two bodies (`b` may be `None`: attached to the world). Anchors and
/// axes are in world space at creation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstraintDesc {
    /// Removes all relative motion.
    Fixed {
        /// First body.
        a: BodyId,
        /// Second body, or the world.
        b: Option<BodyId>,
    },
    /// Ball joint at `anchor`.
    Point {
        /// First body.
        a: BodyId,
        /// Second body, or the world.
        b: Option<BodyId>,
        /// Joint position.
        anchor: Vec3,
    },
    /// Rotation about `axis` through `anchor`, optionally limited to `limits` radians.
    Hinge {
        /// First body.
        a: BodyId,
        /// Second body, or the world.
        b: Option<BodyId>,
        /// Joint position.
        anchor: Vec3,
        /// Rotation axis (unit).
        axis: Vec3,
        /// Minimum and maximum angle, if limited.
        limits: Option<(f32, f32)>,
    },
    /// Keeps `anchor_a` and `anchor_b` between `min` and `max` apart (a rope when
    /// `min = 0`, a rod when equal).
    Distance {
        /// First body.
        a: BodyId,
        /// Second body, or the world.
        b: Option<BodyId>,
        /// Point on `a`.
        anchor_a: Vec3,
        /// Point on `b`.
        anchor_b: Vec3,
        /// Minimum distance.
        min: f32,
        /// Maximum distance.
        max: f32,
    },
    /// Translation along `axis` only, optionally limited.
    Slider {
        /// First body.
        a: BodyId,
        /// Second body, or the world.
        b: Option<BodyId>,
        /// Joint position.
        anchor: Vec3,
        /// Slide axis (unit).
        axis: Vec3,
        /// Minimum and maximum offset, if limited.
        limits: Option<(f32, f32)>,
    },
}

/// A ray: `origin + direction × t` for `t` in `0..=1` (the direction's length is the
/// range).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    /// Start.
    pub origin: Vec3,
    /// Direction × length.
    pub direction: Vec3,
}

/// The closest hit of a ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    /// The body hit.
    pub body: BodyId,
    /// Fraction of the ray (`0..=1`) at the hit.
    pub fraction: f32,
    /// Hit position.
    pub point: Vec3,
    /// Surface normal at the hit.
    pub normal: Vec3,
    /// The body's user data.
    pub user_data: u64,
}

/// Which bodies a query sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryFilter {
    /// Bit `n` set: layer `n` is included.
    pub layers: u64,
    /// Include sensors.
    pub sensors: bool,
}

impl Default for QueryFilter {
    fn default() -> Self {
        Self {
            layers: u64::MAX,
            sensors: false,
        }
    }
}

/// What happened between two bodies during a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContactKind {
    /// Started touching (or a sensor started overlapping).
    Begin,
    /// Stopped touching.
    End,
}

/// A contact event. `a < b`; events are sorted by `(kind, a, b)` so their order does not
/// depend on thread scheduling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactEvent {
    /// Begin or end.
    pub kind: ContactKind,
    /// The body with the smaller id.
    pub a: BodyId,
    /// The other body.
    pub b: BodyId,
    /// A contact point (begin only; zero for end).
    pub point: Vec3,
    /// Contact normal from `a` to `b` (begin only).
    pub normal: Vec3,
    /// Whether either body is a sensor.
    pub sensor: bool,
}

/// World creation settings.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicsSettings {
    /// Maximum bodies.
    pub max_bodies: u32,
    /// Maximum body pairs the broad phase tracks.
    pub max_body_pairs: u32,
    /// Maximum contact constraints per step.
    pub max_contact_constraints: u32,
    /// Number of layers (at most 32).
    pub layers: u8,
    /// `collides[a]` bit `b`: layers `a` and `b` collide (keep it symmetric).
    pub collides: Vec<u32>,
    /// Worker threads for the backend (0: on the calling thread only, fully
    /// deterministic scheduling).
    pub threads: u32,
    /// World gravity.
    pub gravity: Vec3,
}

impl Default for PhysicsSettings {
    /// Two layers: 0 static geometry, 1 moving objects; everything collides except
    /// static with static.
    fn default() -> Self {
        Self {
            max_bodies: 65_536,
            max_body_pairs: 65_536,
            max_contact_constraints: 32_768,
            layers: 2,
            collides: vec![0b10, 0b11],
            threads: 0,
            gravity: Vec3::new(0.0, -9.81, 0.0),
        }
    }
}

/// Counts after the last step.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PhysicsStats {
    /// Bodies in the world.
    pub bodies: u32,
    /// Bodies awake.
    pub active_bodies: u32,
    /// Contact events of the last step.
    pub contact_events: u32,
}

/// Errors from a backend.
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicsError {
    /// The world is full (bodies, constraints).
    Full(&'static str),
    /// A shape could not be built (degenerate hull, empty mesh...).
    InvalidShape(String),
    /// A handle does not refer to a live object.
    InvalidHandle,
    /// The step could not complete (too many pairs or contacts for the settings).
    StepFailed(String),
}

impl core::fmt::Display for PhysicsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full(what) => write!(f, "physics world is full ({what})"),
            Self::InvalidShape(why) => write!(f, "invalid shape: {why}"),
            Self::InvalidHandle => write!(f, "invalid physics handle"),
            Self::StepFailed(why) => write!(f, "physics step failed: {why}"),
        }
    }
}

impl std::error::Error for PhysicsError {}

/// A physics world. Object safe: the engine may hold `Box<dyn PhysicsBackend>` and switch
/// backends at startup (and benchmarks switch between them).
pub trait PhysicsBackend {
    /// Backend name and version, for reports.
    fn name(&self) -> &str;

    /// Sets world gravity.
    fn set_gravity(&mut self, gravity: Vec3);
    /// Replaces the gravity fields seen by bodies in [`GravityMode::Fields`].
    fn set_gravity_fields(&mut self, fields: &[GravityField]);

    /// Creates a shape.
    fn create_shape(&mut self, desc: &ShapeDesc<'_>) -> Result<ShapeId, PhysicsError>;
    /// Releases the caller's reference; bodies using it keep it alive.
    fn destroy_shape(&mut self, shape: ShapeId);

    /// Creates a body and adds it to the world (awake unless static).
    fn create_body(&mut self, desc: &BodyDesc) -> Result<BodyId, PhysicsError>;
    /// Removes and frees a body (and its constraints must already be gone).
    fn destroy_body(&mut self, body: BodyId);

    /// Position and orientation.
    fn transform(&self, body: BodyId) -> Option<(Vec3, Quat)>;
    /// Teleports a body (wakes it).
    fn set_transform(&mut self, body: BodyId, position: Vec3, rotation: Quat);
    /// Linear and angular velocity.
    fn velocity(&self, body: BodyId) -> Option<(Vec3, Vec3)>;
    /// Sets linear and angular velocity (wakes it).
    fn set_velocity(&mut self, body: BodyId, linear: Vec3, angular: Vec3);
    /// Applies an impulse at the center of mass (N·s).
    fn add_impulse(&mut self, body: BodyId, impulse: Vec3);
    /// Changes a body's gravity mode.
    fn set_body_gravity(&mut self, body: BodyId, gravity: GravityMode);
    /// Whether the body is awake.
    fn is_active(&self, body: BodyId) -> bool;

    /// Reads many transforms at once (for the renderer / ECS sync). `out` has one entry
    /// per body; missing bodies leave their entry unchanged.
    fn transforms(&self, bodies: &[BodyId], out: &mut [(Vec3, Quat)]) {
        for (body, slot) in bodies.iter().zip(out) {
            if let Some(t) = self.transform(*body) {
                *slot = t;
            }
        }
    }

    /// Creates a constraint.
    fn create_constraint(&mut self, desc: &ConstraintDesc) -> Result<ConstraintId, PhysicsError>;
    /// Removes a constraint.
    fn destroy_constraint(&mut self, constraint: ConstraintId);

    /// Advances the simulation by `dt` seconds in `substeps` collision steps.
    fn step(&mut self, dt: f32, substeps: u32) -> Result<(), PhysicsError>;
    /// Contact events of the last step, sorted.
    fn contact_events(&self) -> &[ContactEvent];
    /// Counts after the last step.
    fn stats(&self) -> PhysicsStats;

    /// The closest hit along `ray`.
    fn raycast(&self, ray: &Ray, filter: QueryFilter) -> Option<RayHit>;
    /// The closest hit of every ray (`out.len() == rays.len()`).
    fn raycast_batch(&self, rays: &[Ray], filter: QueryFilter, out: &mut [Option<RayHit>]) {
        for (ray, slot) in rays.iter().zip(out) {
            *slot = self.raycast(ray, filter);
        }
    }
    /// Bodies whose shapes overlap a sphere, appended to `out`, sorted by id.
    fn overlap_sphere(&self, center: Vec3, radius: f32, filter: QueryFilter, out: &mut Vec<BodyId>);

    /// A hash of every body's position, orientation and velocities (bit-exact), in body
    /// id order: equal hashes after the same steps mean a deterministic simulation.
    fn state_hash(&self) -> u64;
}

/// FNV-1a over 32-bit words: for [`PhysicsBackend::state_hash`] implementations.
pub fn hash_words(hash: &mut u64, words: impl IntoIterator<Item = u32>) {
    for word in words {
        for byte in word.to_le_bytes() {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

/// Starting value for [`hash_words`].
pub const HASH_SEED: u64 = 0xCBF2_9CE4_8422_2325;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_field_falls_off_with_the_square_of_distance() {
        let field = GravityField::Point {
            center: Vec3::ZERO,
            strength: 100.0,
            min_distance: 1.0,
            radius: 50.0,
        };
        let at_2 = field.acceleration_at(Vec3::new(2.0, 0.0, 0.0));
        let at_4 = field.acceleration_at(Vec3::new(4.0, 0.0, 0.0));
        assert!((at_2 - Vec3::new(-25.0, 0.0, 0.0)).length() < 1e-4);
        assert!((at_2.length() / at_4.length() - 4.0).abs() < 1e-4);
        // Clamped near the center, zero beyond the radius.
        assert!((field.acceleration_at(Vec3::new(0.5, 0.0, 0.0)).length() - 100.0).abs() < 1e-3);
        assert_eq!(field.acceleration_at(Vec3::new(60.0, 0.0, 0.0)), Vec3::ZERO);
    }

    #[test]
    fn box_field_is_constant_inside() {
        let field = GravityField::Box {
            min: Vec3::splat(-1.0),
            max: Vec3::splat(1.0),
            acceleration: Vec3::Y * 20.0,
        };
        assert_eq!(field.acceleration_at(Vec3::ZERO), Vec3::Y * 20.0);
        assert_eq!(field.acceleration_at(Vec3::splat(2.0)), Vec3::ZERO);
    }
}
