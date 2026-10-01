//! 2D physics on Box2D v3 (ADR 0028).
//!
//! A deliberately small layer: rigid bodies (static, kinematic, dynamic) with box, circle
//! and capsule shapes on up to 64 query layers, stepping, transforms, velocities,
//! kinematic targets, begin-contact events, ray and shape casts, overlaps and a capsule
//! character mover ([`Mover2d`], ADR 0066). Joints, polygons, chains, sensors,
//! save/restore and multithreading come when a game needs them.
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
            layer: i32,
        );
        pub(crate) fn e2d_body_add_circle(
            body: u64,
            radius: f32,
            density: f32,
            friction: f32,
            restitution: f32,
            layer: i32,
        );
        pub(crate) fn e2d_body_add_capsule(
            body: u64,
            half_length: f32,
            radius: f32,
            density: f32,
            friction: f32,
            restitution: f32,
            layer: i32,
        );
        pub(crate) fn e2d_body_set_target(body: u64, x: f32, y: f32, angle: f32, dt: f32);
        pub(crate) fn e2d_cast_ray(
            world: u32,
            ox: f32,
            oy: f32,
            tx: f32,
            ty: f32,
            layers: u64,
            ignore: u64,
            closest: i32,
            hits: *mut f32,
            bodies: *mut u64,
            capacity: i32,
        ) -> i32;
        pub(crate) fn e2d_cast_shape(
            world: u32,
            kind: i32,
            a: f32,
            b: f32,
            x: f32,
            y: f32,
            angle: f32,
            tx: f32,
            ty: f32,
            layers: u64,
            ignore: u64,
            closest: i32,
            hits: *mut f32,
            bodies: *mut u64,
            capacity: i32,
        ) -> i32;
        pub(crate) fn e2d_overlap_shape(
            world: u32,
            kind: i32,
            a: f32,
            b: f32,
            x: f32,
            y: f32,
            angle: f32,
            layers: u64,
            ignore: u64,
            bodies: *mut u64,
            capacity: i32,
        ) -> i32;
        pub(crate) fn e2d_mover_move(
            world: u32,
            input: *const f32,
            layers: u64,
            out: *mut f32,
            ground_body: *mut u64,
        );
        pub(crate) fn e2d_body_transform(body: u64, out: *mut f32);
        pub(crate) fn e2d_body_set_transform(body: u64, x: f32, y: f32, angle: f32);
        pub(crate) fn e2d_body_velocity(body: u64, out: *mut f32);
        pub(crate) fn e2d_body_set_velocity(body: u64, vx: f32, vy: f32, angular: f32);
    }
}

use core::cell::RefCell;
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
    /// A capsule along the body's y axis: a segment from `-half_length` to `half_length`
    /// with rounded ends of `radius`.
    Capsule {
        /// Half the length of the segment (without the caps).
        half_length: f32,
        /// Radius.
        radius: f32,
    },
}

impl Shape {
    /// The shim's kind and two parameters, if the dimensions are valid.
    fn raw(self) -> Option<(i32, f32, f32)> {
        let positive = |v: f32| v.is_finite() && v > 0.0;
        match self {
            Self::Circle { radius } if positive(radius) => Some((0, radius, 0.0)),
            Self::Box { half_extents: h } if positive(h.x) && positive(h.y) => Some((1, h.x, h.y)),
            Self::Capsule {
                half_length,
                radius,
            } if positive(radius) && half_length.is_finite() && half_length >= 0.0 => {
                Some((2, half_length, radius))
            }
            _ => None,
        }
    }
}

/// Which shapes a 2D query or mover sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryFilter2d {
    /// Bit `n` set: shapes on layer `n` ([`World2d::add_shape_on_layer`]) are included.
    pub layers: u64,
    /// A body to skip (the one casting).
    pub ignore: Option<BodyId>,
}

impl Default for QueryFilter2d {
    fn default() -> Self {
        Self {
            layers: u64::MAX,
            ignore: None,
        }
    }
}

/// A hit of a 2D ray or shape cast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit2d {
    /// The body hit.
    pub body: BodyId,
    /// Fraction of the translation (`0..=1`) at the hit.
    pub fraction: f32,
    /// Hit point.
    pub point: Vec2,
    /// Surface normal at the hit.
    pub normal: Vec2,
}

/// A capsule character moved by game code (ADR 0066), after Box2D 3.1's mover: it slides
/// along what it hits, does not walk up slopes steeper than `max_slope`, detects the
/// ground and stays on it walking down slopes and steps. It is not a body: bodies do not
/// collide with it, and it does not push them. Plain data: copy it to save it for
/// rollback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mover2d {
    /// Position of the feet (the bottom of the capsule).
    pub position: Vec2,
    /// Velocity after the last move (what remained after sliding).
    pub velocity: Vec2,
    /// Total height of the capsule (at least `2 × radius`).
    pub height: f32,
    /// Radius of the capsule.
    pub radius: f32,
    /// Steepest walkable slope, in radians from horizontal.
    pub max_slope: f32,
    /// How far down it is pulled to stay on the ground (m); 0 turns it off.
    pub snap_to_ground: f32,
    /// What it collides with.
    pub filter: QueryFilter2d,
    /// Where it stands after the last move, if on walkable ground.
    pub ground: Option<Ground2d>,
}

/// The walkable ground under a [`Mover2d`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ground2d {
    /// The body it stands on.
    pub body: BodyId,
    /// Surface normal.
    pub normal: Vec2,
    /// Contact point.
    pub point: Vec2,
    /// Velocity of the ground at the contact point (moving platforms).
    pub velocity: Vec2,
}

impl Mover2d {
    /// A mover standing at `position`, `height` tall with `radius`, 50° slope limit, 0.3 m
    /// ground snap, colliding with every layer.
    pub fn new(position: Vec2, height: f32, radius: f32) -> Self {
        Self {
            position,
            velocity: Vec2::ZERO,
            height,
            radius,
            max_slope: 50f32.to_radians(),
            snap_to_ground: 0.3,
            filter: QueryFilter2d::default(),
            ground: None,
        }
    }
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
    /// Layers are `0..64`.
    InvalidLayer,
}

impl core::fmt::Display for Physics2dError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidBody => write!(f, "the body does not exist"),
            Self::InvalidShape => write!(f, "shape dimensions must be positive and finite"),
            Self::InvalidLayer => write!(f, "layers are 0..64"),
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
    /// Reused query buffers: 5 floats and a body per hit.
    query_hits: RefCell<(Vec<f32>, Vec<u64>)>,
    /// Reused result of closest-hit queries.
    closest: RefCell<Vec<Hit2d>>,
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
            query_hits: RefCell::new((Vec::new(), Vec::new())),
            closest: RefCell::new(Vec::with_capacity(1)),
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

    /// Attaches a shape to a body, on layer 0.
    pub fn add_shape(
        &mut self,
        body: BodyId,
        shape: Shape,
        material: Material,
    ) -> Result<(), Physics2dError> {
        self.add_shape_on_layer(body, shape, material, 0)
    }

    /// Attaches a shape to a body on a query layer (`0..64`): queries and movers see it
    /// when their filter includes the layer. It collides with every layer.
    pub fn add_shape_on_layer(
        &mut self,
        body: BodyId,
        shape: Shape,
        material: Material,
        layer: u8,
    ) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        if layer >= 64 {
            return Err(Physics2dError::InvalidLayer);
        }
        let Material {
            density,
            friction,
            restitution,
        } = material;
        let (kind, a, b) = shape.raw().ok_or(Physics2dError::InvalidShape)?;
        let layer = i32::from(layer);
        // SAFETY: the body is valid; the dimensions are positive (checked by `raw`).
        unsafe {
            match kind {
                0 => ffi::e2d_body_add_circle(raw, a, density, friction, restitution, layer),
                1 => ffi::e2d_body_add_box(raw, a, b, density, friction, restitution, layer),
                _ => ffi::e2d_body_add_capsule(raw, a, b, density, friction, restitution, layer),
            }
        }
        Ok(())
    }

    /// Moves a kinematic body so it reaches `target` at the end of the next step of `dt`
    /// seconds (its velocity is set for that, so it carries and pushes what it touches).
    pub fn move_kinematic(
        &mut self,
        body: BodyId,
        target: Transform2d,
        dt: f32,
    ) -> Result<(), Physics2dError> {
        let raw = self.check(body)?;
        if dt > 0.0 {
            // SAFETY: the body is valid.
            unsafe {
                ffi::e2d_body_set_target(
                    raw,
                    target.position.x,
                    target.position.y,
                    target.angle,
                    dt,
                )
            };
        }
        Ok(())
    }

    /// Runs a cast through the shim with the reused buffers, growing them when a query
    /// finds more hits than they hold; returns the hits in the buffers' order.
    fn cast(
        &self,
        closest: bool,
        out: &mut Vec<Hit2d>,
        mut query: impl FnMut(*mut f32, *mut u64, i32, i32) -> i32,
    ) {
        let mut buffers = self.query_hits.borrow_mut();
        let (hits, bodies) = &mut *buffers;
        loop {
            let capacity = bodies.len();
            let count = query(
                hits.as_mut_ptr(),
                bodies.as_mut_ptr(),
                capacity as i32,
                i32::from(closest),
            )
            .max(0) as usize;
            if count <= capacity {
                out.extend((0..count).map(|i| {
                    let h = &hits[i * 5..i * 5 + 5];
                    Hit2d {
                        body: BodyId(bodies[i]),
                        fraction: h[0],
                        point: Vec2::new(h[1], h[2]),
                        normal: Vec2::new(h[3], h[4]),
                    }
                }));
                return;
            }
            let grown = count.next_power_of_two().max(16);
            hits.resize(grown * 5, 0.0);
            bodies.resize(grown, 0);
        }
    }

    /// Sorts hits by fraction then body, keeping the first hit per body.
    fn sort_hits(hits: &mut Vec<Hit2d>, start: usize) {
        let tail = &mut hits[start..];
        tail.sort_unstable_by(|a, b| {
            a.body
                .0
                .cmp(&b.body.0)
                .then(a.fraction.total_cmp(&b.fraction))
        });
        let mut end = start;
        for i in start..hits.len() {
            if end == start || hits[end - 1].body != hits[i].body {
                hits[end] = hits[i];
                end += 1;
            }
        }
        hits.truncate(end);
        hits[start..].sort_unstable_by(|a, b| {
            a.fraction
                .total_cmp(&b.fraction)
                .then(a.body.0.cmp(&b.body.0))
        });
    }

    /// The closest hit of a ray from `origin` along `translation` (its length is the
    /// range).
    pub fn raycast(&self, origin: Vec2, translation: Vec2, filter: QueryFilter2d) -> Option<Hit2d> {
        let mut out = self.closest.borrow_mut();
        out.clear();
        self.ray(origin, translation, filter, true, &mut out);
        out.first().copied()
    }

    /// Every hit of a ray (one per body), appended to `out` sorted by fraction, then body.
    pub fn raycast_all(
        &self,
        origin: Vec2,
        translation: Vec2,
        filter: QueryFilter2d,
        out: &mut Vec<Hit2d>,
    ) {
        let start = out.len();
        self.ray(origin, translation, filter, false, out);
        Self::sort_hits(out, start);
    }

    fn ray(
        &self,
        origin: Vec2,
        translation: Vec2,
        filter: QueryFilter2d,
        closest: bool,
        out: &mut Vec<Hit2d>,
    ) {
        let ignore = filter.ignore.map_or(0, |b| b.0);
        self.cast(closest, out, |hits, bodies, capacity, closest| {
            // SAFETY: the world is alive; the buffers hold `capacity` hits.
            unsafe {
                ffi::e2d_cast_ray(
                    self.id,
                    origin.x,
                    origin.y,
                    translation.x,
                    translation.y,
                    filter.layers,
                    ignore,
                    closest,
                    hits,
                    bodies,
                    capacity,
                )
            }
        });
    }

    /// The first hit of `shape` placed at `position`, rotated by `angle`, swept along
    /// `translation`. `None` also for invalid shape dimensions.
    pub fn cast_shape(
        &self,
        shape: Shape,
        position: Vec2,
        angle: f32,
        translation: Vec2,
        filter: QueryFilter2d,
    ) -> Option<Hit2d> {
        let mut out = self.closest.borrow_mut();
        out.clear();
        self.sweep(shape, position, angle, translation, filter, true, &mut out);
        out.first().copied()
    }

    /// Every hit of a swept shape (one per body), appended to `out` sorted by fraction,
    /// then body.
    pub fn cast_shape_all(
        &self,
        shape: Shape,
        position: Vec2,
        angle: f32,
        translation: Vec2,
        filter: QueryFilter2d,
        out: &mut Vec<Hit2d>,
    ) {
        let start = out.len();
        self.sweep(shape, position, angle, translation, filter, false, out);
        Self::sort_hits(out, start);
    }

    #[allow(clippy::too_many_arguments)]
    fn sweep(
        &self,
        shape: Shape,
        position: Vec2,
        angle: f32,
        translation: Vec2,
        filter: QueryFilter2d,
        closest: bool,
        out: &mut Vec<Hit2d>,
    ) {
        let Some((kind, a, b)) = shape.raw() else {
            return;
        };
        let ignore = filter.ignore.map_or(0, |b| b.0);
        self.cast(closest, out, |hits, bodies, capacity, closest| {
            // SAFETY: the world is alive; the buffers hold `capacity` hits.
            unsafe {
                ffi::e2d_cast_shape(
                    self.id,
                    kind,
                    a,
                    b,
                    position.x,
                    position.y,
                    angle,
                    translation.x,
                    translation.y,
                    filter.layers,
                    ignore,
                    closest,
                    hits,
                    bodies,
                    capacity,
                )
            }
        });
    }

    /// Bodies whose shapes overlap `shape` at `position`, rotated by `angle`, appended to
    /// `out` (once each, sorted by id).
    pub fn overlap_shape(
        &self,
        shape: Shape,
        position: Vec2,
        angle: f32,
        filter: QueryFilter2d,
        out: &mut Vec<BodyId>,
    ) {
        let Some((kind, a, b)) = shape.raw() else {
            return;
        };
        let ignore = filter.ignore.map_or(0, |b| b.0);
        let start = out.len();
        let mut buffers = self.query_hits.borrow_mut();
        let bodies = &mut buffers.1;
        loop {
            let capacity = bodies.len();
            // SAFETY: the world is alive; `bodies` holds `capacity` ids.
            let count = unsafe {
                ffi::e2d_overlap_shape(
                    self.id,
                    kind,
                    a,
                    b,
                    position.x,
                    position.y,
                    angle,
                    filter.layers,
                    ignore,
                    bodies.as_mut_ptr(),
                    capacity as i32,
                )
            }
            .max(0) as usize;
            if count <= capacity {
                out.extend(bodies[..count].iter().map(|&b| BodyId(b)));
                break;
            }
            bodies.resize(count.next_power_of_two().max(16), 0);
        }
        out[start..].sort_unstable_by_key(|b| b.0);
        let mut end = start;
        for i in start..out.len() {
            if end == start || out[end - 1] != out[i] {
                out[end] = out[i];
                end += 1;
            }
        }
        out.truncate(end);
    }

    /// Moves `mover` with `velocity` (m/s, including gravity and jumps) for `dt` seconds:
    /// collide and slide, slope limit, ground detection and snapping. To ride moving
    /// platforms, add `mover.ground`'s velocity to the velocity you pass. Call it before
    /// [`World2d::step`].
    pub fn move_mover(&self, mover: &mut Mover2d, velocity: Vec2, dt: f32) {
        engine_profiler::zone!("physics2d::move_mover");
        let height = mover.height.max(2.0 * mover.radius);
        let input = [
            mover.position.x,
            mover.position.y,
            velocity.x,
            velocity.y,
            dt.max(0.0),
            mover.radius,
            height,
            mover.max_slope.cos(),
            mover.snap_to_ground.max(0.0),
            if mover.ground.is_some() { 1.0 } else { 0.0 },
        ];
        let mut out = [0.0f32; 11];
        let mut ground_body = 0u64;
        // SAFETY: the world is alive; `input` holds 10 floats and `out` 11.
        unsafe {
            ffi::e2d_mover_move(
                self.id,
                input.as_ptr(),
                mover.filter.layers,
                out.as_mut_ptr(),
                &mut ground_body,
            )
        };
        mover.position = Vec2::new(out[0], out[1]);
        mover.velocity = Vec2::new(out[2], out[3]);
        mover.ground = (out[10] != 0.0).then(|| Ground2d {
            body: BodyId(ground_body),
            normal: Vec2::new(out[4], out[5]),
            point: Vec2::new(out[6], out[7]),
            velocity: Vec2::new(out[8], out[9]),
        });
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
