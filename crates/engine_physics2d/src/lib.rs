//! 2D physics on Box2D v3 (ADR 0028).
//!
//! A deliberately small first cut: rigid bodies (static, kinematic, dynamic) with box and
//! circle shapes, stepping, transforms, velocities and begin-contact events. Joints,
//! polygons, chains, queries, sensors and multithreading come when a game needs them.
//!
//! Box2D is vendored as C source (`third_party/box2d`, v3.1.1, MIT) and reached through
//! `csrc/shim.c`, a flat C API with integer ids and plain arguments, so the Rust side does
//! not mirror Box2D's structs.
//!
//! Box2D runs single-threaded on the calling thread here. Its results are identical for
//! the same binary and the same sequence of calls.
//!
//! ```
//! use engine_core::math::Vec2;
//! use engine_physics2d::{BodyKind, Material, Shape, World2d};
//!
//! let mut world = World2d::new(Vec2::new(0.0, -10.0));
//! let ground = world.create_body(BodyKind::Static, Vec2::ZERO, 0.0);
//! world.add_shape(ground, Shape::Box { half_extents: Vec2::new(50.0, 1.0) }, Material::default())?;
//! let crate_ = world.create_body(BodyKind::Dynamic, Vec2::new(0.0, 4.0), 0.0);
//! world.add_shape(crate_, Shape::Box { half_extents: Vec2::splat(0.5) }, Material::default())?;
//! for _ in 0..120 {
//!     world.step(1.0 / 60.0, 4);
//! }
//! let y = world.transform(crate_).unwrap().position.y;
//! assert!((y - 1.5).abs() < 0.02, "resting on the ground: {y}");
//! # Ok::<(), engine_physics2d::Physics2dError>(())
//! ```

mod ffi {
    unsafe extern "C" {
        pub(crate) fn e2d_world_create(gx: f32, gy: f32) -> u32;
        pub(crate) fn e2d_world_destroy(world: u32);
        pub(crate) fn e2d_world_step(world: u32, dt: f32, substeps: i32);
        pub(crate) fn e2d_world_awake_bodies(world: u32) -> i32;
        pub(crate) fn e2d_world_begin_contacts(world: u32, out: *mut u64, capacity: i32) -> i32;
        pub(crate) fn e2d_body_create(world: u32, kind: i32, x: f32, y: f32, angle: f32) -> u64;
        pub(crate) fn e2d_body_destroy(body: u64);
        pub(crate) fn e2d_body_valid(body: u64) -> i32;
        pub(crate) fn e2d_body_add_box(
            body: u64,
            hx: f32,
            hy: f32,
            density: f32,
            friction: f32,
            restitution: f32,
        );
        pub(crate) fn e2d_body_add_circle(
            body: u64,
            radius: f32,
            density: f32,
            friction: f32,
            restitution: f32,
        );
        pub(crate) fn e2d_body_transform(body: u64, out: *mut f32);
        pub(crate) fn e2d_body_set_transform(body: u64, x: f32, y: f32, angle: f32);
        pub(crate) fn e2d_body_velocity(body: u64, out: *mut f32);
        pub(crate) fn e2d_body_set_velocity(body: u64, vx: f32, vy: f32, angular: f32);
    }
}

use engine_core::math::Vec2;
use std::sync::Mutex;

/// Box2D keeps its worlds in a process-wide table that `b2CreateWorld` and
/// `b2DestroyWorld` change without locking; creating or destroying worlds on several
/// threads at once (parallel tests, editor previews) takes turns here. Stepping
/// different worlds concurrently is safe: each step touches only its own world.
static WORLDS: Mutex<()> = Mutex::new(());

/// How a body moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// Never moves (ground, walls).
    Static,
    /// Moved by setting its velocity; not affected by forces or collisions.
    Kinematic,
    /// Simulated.
    Dynamic,
}

/// A body handle. Stale handles (destroyed bodies) are detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyId(u64);

/// A collision shape, centred on the body's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// An axis-aligned box in body space.
    Box {
        /// Half the width and height.
        half_extents: Vec2,
    },
    /// A circle.
    Circle {
        /// Radius.
        radius: f32,
    },
}

/// Surface and mass properties of a shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// Mass per square metre.
    pub density: f32,
    /// Coulomb friction (0: ice).
    pub friction: f32,
    /// Bounciness (0: no bounce, 1: elastic).
    pub restitution: f32,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            density: 1.0,
            friction: 0.6,
            restitution: 0.0,
        }
    }
}

/// Position and rotation of a body.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transform2d {
    /// World position of the body's origin.
    pub position: Vec2,
    /// Rotation in radians, counter-clockwise.
    pub angle: f32,
}

/// Linear and angular velocity of a body.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Velocity2d {
    /// Metres per second.
    pub linear: Vec2,
    /// Radians per second, counter-clockwise.
    pub angular: f32,
}

/// Two bodies started touching during the last step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactBegin {
    /// First body.
    pub a: BodyId,
    /// Second body.
    pub b: BodyId,
}

/// Errors from the 2D world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Physics2dError {
    /// The body was destroyed (or never existed in this world).
    InvalidBody,
    /// Shape dimensions must be positive and finite.
    InvalidShape,
}

impl core::fmt::Display for Physics2dError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidBody => write!(f, "the body does not exist"),
            Self::InvalidShape => write!(f, "shape dimensions must be positive and finite"),
        }
    }
}

impl std::error::Error for Physics2dError {}

/// A Box2D world.
pub struct World2d {
    id: u32,
    /// Begin-contact pairs of the last step.
    contacts: Vec<ContactBegin>,
    /// Raw pairs from the shim (reused).
    raw: Vec<u64>,
}

// SAFETY: a world is only reached through `&mut self` or `&self` of this handle (Box2D
// state is per world); world creation and destruction, which touch Box2D's global table,
// are serialized by `WORLDS`.
unsafe impl Send for World2d {}

impl World2d {
    /// An empty world with the given gravity (metres per second squared; Box2D's
    /// default is (0, -10)).
    pub fn new(gravity: Vec2) -> Self {
        let _guard = WORLDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: plain creation; destroyed in `drop`.
        let id = unsafe { ffi::e2d_world_create(gravity.x, gravity.y) };
        Self {
            id,
            contacts: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// Creates a body at `position`, rotated by `angle` radians. Add shapes to give it
    /// mass and collision.
    pub fn create_body(&mut self, kind: BodyKind, position: Vec2, angle: f32) -> BodyId {
        let kind = match kind {
            BodyKind::Static => 0,
            BodyKind::Kinematic => 1,
            BodyKind::Dynamic => 2,
        };
        // SAFETY: the world is alive.
        BodyId(unsafe { ffi::e2d_body_create(self.id, kind, position.x, position.y, angle) })
    }

    fn check(&self, body: BodyId) -> Result<u64, Physics2dError> {
        // SAFETY: validity check on any id value (Box2D checks the world and generation).
        if unsafe { ffi::e2d_body_valid(body.0) } != 0 {
            Ok(body.0)
        } else {
            Err(Physics2dError::InvalidBody)
        }
    }

    /// Destroys a body and its shapes.
    pub fn destroy_body(&mut self, body: BodyId) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        // SAFETY: the body is valid.
        unsafe { ffi::e2d_body_destroy(raw) };
        Ok(())
    }

    /// Attaches a shape to a body.
    pub fn add_shape(
        &mut self,
        body: BodyId,
        shape: Shape,
        material: Material,
    ) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        let Material {
            density,
            friction,
            restitution,
        } = material;
        let positive = |v: f32| v.is_finite() && v > 0.0;
        match shape {
            Shape::Box { half_extents: h } => {
                if !(positive(h.x) && positive(h.y)) {
                    return Err(Physics2dError::InvalidShape);
                }
                // SAFETY: the body is valid; dimensions are positive.
                unsafe { ffi::e2d_body_add_box(raw, h.x, h.y, density, friction, restitution) };
            }
            Shape::Circle { radius } => {
                if !positive(radius) {
                    return Err(Physics2dError::InvalidShape);
                }
                // SAFETY: the body is valid; the radius is positive.
                unsafe { ffi::e2d_body_add_circle(raw, radius, density, friction, restitution) };
            }
        }
        Ok(())
    }

    /// Advances the world by `dt` seconds with `substeps` solver substeps (Box2D
    /// recommends 4).
    pub fn step(&mut self, dt: f32, substeps: u32) {
        engine_profiler::zone!("physics2d::step");
        // SAFETY: the world is alive.
        unsafe { ffi::e2d_world_step(self.id, dt, substeps.max(1) as i32) };
        // Gather begin-contact events; grow the buffer once if the step had more.
        loop {
            let capacity = self.raw.len() / 2;
            // SAFETY: `raw` has room for `capacity` pairs.
            let count = unsafe {
                ffi::e2d_world_begin_contacts(self.id, self.raw.as_mut_ptr(), capacity as i32)
            } as usize;
            if count <= capacity {
                self.contacts.clear();
                self.contacts
                    .extend(self.raw[..count * 2].chunks_exact(2).map(|p| ContactBegin {
                        a: BodyId(p[0]),
                        b: BodyId(p[1]),
                    }));
                break;
            }
            self.raw.resize(count.next_power_of_two() * 2, 0);
        }
    }

    /// Bodies that started touching during the last step, in Box2D's order (which is
    /// deterministic).
    pub fn contact_begins(&self) -> &[ContactBegin] {
        &self.contacts
    }

    /// Number of awake bodies.
    pub fn awake_bodies(&self) -> u32 {
        // SAFETY: the world is alive.
        unsafe { ffi::e2d_world_awake_bodies(self.id) }.max(0) as u32
    }

    /// Position and rotation of a body.
    pub fn transform(&self, body: BodyId) -> Result<Transform2d, Physics2dError> {
        let raw = self.check(body)?;
        let mut out = [0.0f32; 3];
        // SAFETY: the body is valid; `out` holds three floats.
        unsafe { ffi::e2d_body_transform(raw, out.as_mut_ptr()) };
        Ok(Transform2d {
            position: Vec2::new(out[0], out[1]),
            angle: out[2],
        })
    }

    /// Teleports a body.
    pub fn set_transform(
        &mut self,
        body: BodyId,
        transform: Transform2d,
    ) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        let Transform2d { position, angle } = transform;
        // SAFETY: the body is valid.
        unsafe { ffi::e2d_body_set_transform(raw, position.x, position.y, angle) };
        Ok(())
    }

    /// Linear and angular velocity of a body.
    pub fn velocity(&self, body: BodyId) -> Result<Velocity2d, Physics2dError> {
        let raw = self.check(body)?;
        let mut out = [0.0f32; 3];
        // SAFETY: the body is valid; `out` holds three floats.
        unsafe { ffi::e2d_body_velocity(raw, out.as_mut_ptr()) };
        Ok(Velocity2d {
            linear: Vec2::new(out[0], out[1]),
            angular: out[2],
        })
    }

    /// Sets a body's velocity (wakes it).
    pub fn set_velocity(
        &mut self,
        body: BodyId,
        velocity: Velocity2d,
    ) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        let Velocity2d { linear, angular } = velocity;
        // SAFETY: the body is valid.
        unsafe { ffi::e2d_body_set_velocity(raw, linear.x, linear.y, angular) };
        Ok(())
    }
}

impl Drop for World2d {
    fn drop(&mut self) {
        let _guard = WORLDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: the world is alive and destroyed once.
        unsafe { ffi::e2d_world_destroy(self.id) };
    }
}
