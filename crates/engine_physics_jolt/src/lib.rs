//! Jolt Physics 5.6 backend for `engine_physics` (CLAUDE.md §8 P1, ADR 0018): the
//! reference and fallback the engine's own solver is measured against.
//!
//! Jolt and its C API (JoltC) are vendored under `third_party/` and compiled by this
//! crate's build script; the Rust side binds the subset it uses (`ffi`).
//!
//! Mapping:
//! - Object layers: each engine [`Layer`](engine_physics::Layer) becomes two Jolt object layers, static and
//!   moving, so static-static pairs never reach the broad phase; two broad-phase layers
//!   (non-moving, moving).
//! - Contact callbacks (called from Jolt's worker threads) write into a preallocated array
//!   through an atomic index, without locks or allocation; after the step the events are
//!   sorted by `(kind, a, b)` and de-duplicated, so their order is deterministic.
//! - Custom gravity vectors and gravity fields are applied by a step listener at every
//!   collision step, to awake bodies, as a velocity change (Jolt's own gravity factor is
//!   zero for those bodies).
//! - Jolt's own job system (a thread pool) runs the simulation; `PhysicsSettings::threads
//!   = 0` keeps everything on the calling thread. Routing Jolt's jobs through
//!   `engine_core::jobs` is a follow-up (ADR 0018).

mod ffi;

use core::cell::{Cell, UnsafeCell};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr::null;
use core::sync::atomic::{AtomicUsize, Ordering};
use engine_core::math::{Quat, Vec3};
use engine_physics::{
    BodyDesc, BodyId, ConstraintDesc, ConstraintId, ContactEvent, ContactKind, GravityField,
    GravityMode, HASH_SEED, MotionQuality, MotionType, PhysicsBackend, PhysicsError,
    PhysicsSettings, PhysicsStats, QueryFilter, Ray, RayHit, ShapeDesc, ShapeId, hash_words,
};
use ffi::*;
use std::collections::BTreeMap;
use std::sync::{Mutex, Once};

fn v3(v: Vec3) -> JPH_Vec3 {
    JPH_Vec3 {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

fn from_v3(v: JPH_Vec3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn quat(q: Quat) -> JPH_Quat {
    JPH_Quat {
        x: q.x,
        y: q.y,
        z: q.z,
        w: q.w,
    }
}

fn from_quat(q: JPH_Quat) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

static INIT: Once = Once::new();

/// JoltC steps every world with one process-wide temporary allocator (a stack allocator,
/// not thread-safe), so steps of different worlds must not overlap. Uncontended with the
/// usual single world; worlds stepping on several threads take turns.
static STEP_LOCK: Mutex<()> = Mutex::new(());

/// Registers Jolt's types and allocator once per process (never shut down: other worlds
/// may be alive, and the process exit frees it), and installs the callback tables every
/// world shares (each world passes its own user data).
fn init_jolt() {
    INIT.call_once(|| {
        // SAFETY: first and only initialization; the procedure tables are 'static.
        unsafe {
            JPH_Init();
            JPH_ContactListener_SetProcs(&CONTACT_PROCS);
            JPH_PhysicsStepListener_SetProcs(&STEP_PROCS);
            JPH_ObjectLayerFilter_SetProcs(&LAYER_FILTER_PROCS);
            JPH_BodyFilter_SetProcs(&BODY_FILTER_PROCS);
        }
    });
}

#[derive(Clone, Copy)]
struct RawEvent {
    kind: ContactKind,
    a: u32,
    b: u32,
    point: Vec3,
    normal: Vec3,
    sensor: bool,
}

/// State the callbacks reach through their user-data pointer. Boxed so its address is
/// stable. During a step only the event array and its counter are written (atomically
/// reserved slots); gravity data is read only. Query state is used by `&self` queries on
/// the owning thread only.
struct Shared {
    events: Box<[UnsafeCell<MaybeUninit<RawEvent>>]>,
    event_count: AtomicUsize,
    gravity_bodies: Vec<(JPH_BodyID, GravityMode)>,
    fields: Vec<GravityField>,
    body_interface_no_lock: *mut JPH_BodyInterface,
    query_layers: Cell<u64>,
    query_sensors: Cell<bool>,
}

// SAFETY: accessed by Jolt worker threads during `step` only through `push_event` (disjoint
// slots reserved atomically) and immutable reads; the `Cell`s are used on the owning
// thread outside steps.
unsafe impl Sync for Shared {}

impl Shared {
    fn push_event(&self, event: RawEvent) {
        let slot = self.event_count.fetch_add(1, Ordering::Relaxed);
        if let Some(cell) = self.events.get(slot) {
            // SAFETY: the slot index was reserved by this thread alone.
            unsafe { (*cell.get()).write(event) };
        }
    }
}

static CONTACT_PROCS: JPH_ContactListener_Procs = JPH_ContactListener_Procs {
    OnContactValidate: None,
    OnContactAdded: Some(on_contact_added),
    OnContactPersisted: None,
    OnContactRemoved: Some(on_contact_removed),
};

unsafe extern "C" fn on_contact_added(
    user_data: *mut c_void,
    body1: *const JPH_Body,
    body2: *const JPH_Body,
    manifold: *const JPH_ContactManifold,
    _settings: *mut JPH_ContactSettings,
) {
    // SAFETY: Jolt passes valid bodies and manifold for the callback's duration; the user
    // data is the world's `Shared`, alive while the world exists.
    unsafe {
        let shared = &*user_data.cast::<Shared>();
        let (id1, id2) = (JPH_Body_GetID(body1), JPH_Body_GetID(body2));
        let sensor = JPH_Body_IsSensor(body1) || JPH_Body_IsSensor(body2);
        let mut normal = JPH_Vec3::default();
        JPH_ContactManifold_GetWorldSpaceNormal(manifold, &mut normal);
        let mut point = JPH_Vec3::default();
        if JPH_ContactManifold_GetPointCount(manifold) > 0 {
            JPH_ContactManifold_GetWorldSpaceContactPointOn1(manifold, 0, &mut point);
        }
        let (a, b, normal) = if id1 <= id2 {
            (id1, id2, from_v3(normal))
        } else {
            (id2, id1, -from_v3(normal))
        };
        shared.push_event(RawEvent {
            kind: ContactKind::Begin,
            a,
            b,
            point: from_v3(point),
            normal,
            sensor,
        });
    }
}

unsafe extern "C" fn on_contact_removed(user_data: *mut c_void, pair: *const JPH_SubShapeIDPair) {
    // SAFETY: as above.
    unsafe {
        let shared = &*user_data.cast::<Shared>();
        let pair = &*pair;
        shared.push_event(RawEvent {
            kind: ContactKind::End,
            a: pair.Body1ID.min(pair.Body2ID),
            b: pair.Body1ID.max(pair.Body2ID),
            point: Vec3::ZERO,
            normal: Vec3::ZERO,
            sensor: false,
        });
    }
}

static STEP_PROCS: JPH_PhysicsStepListener_Procs = JPH_PhysicsStepListener_Procs {
    OnStep: Some(on_step),
};

/// Custom gravity and gravity fields, every collision step.
unsafe extern "C" fn on_step(
    user_data: *mut c_void,
    context: *const JPH_PhysicsStepListenerContext,
) {
    // SAFETY: the user data is the world's `Shared`; the no-lock body interface may be used
    // from a step listener (this is the only listener, so no other listener touches these
    // bodies concurrently).
    unsafe {
        let shared = &*user_data.cast::<Shared>();
        let dt = (*context).deltaTime;
        let bi = shared.body_interface_no_lock;
        for &(body, mode) in &shared.gravity_bodies {
            if !JPH_BodyInterface_IsActive(bi, body) {
                continue;
            }
            let acceleration = match mode {
                GravityMode::Custom(g) => g,
                GravityMode::Fields => {
                    let mut com = JPH_Vec3::default();
                    JPH_BodyInterface_GetCenterOfMassPosition(bi, body, &mut com);
                    let com = from_v3(com);
                    shared
                        .fields
                        .iter()
                        .fold(Vec3::ZERO, |sum, f| sum + f.acceleration_at(com))
                }
                GravityMode::World | GravityMode::Scaled(_) => continue,
            };
            let mut dv = v3(acceleration * dt);
            JPH_BodyInterface_AddLinearVelocity(bi, body, &mut dv);
        }
    }
}

static LAYER_FILTER_PROCS: JPH_ObjectLayerFilter_Procs = JPH_ObjectLayerFilter_Procs {
    ShouldCollide: Some(layer_filter),
};

unsafe extern "C" fn layer_filter(user_data: *mut c_void, layer: JPH_ObjectLayer) -> bool {
    // SAFETY: the user data is the world's `Shared`; queries run on its thread.
    let shared = unsafe { &*user_data.cast::<Shared>() };
    (shared.query_layers.get() >> (layer / 2)) & 1 != 0
}

static BODY_FILTER_PROCS: JPH_BodyFilter_Procs = JPH_BodyFilter_Procs {
    ShouldCollide: Some(body_filter),
    ShouldCollideLocked: Some(body_filter_locked),
};

unsafe extern "C" fn body_filter(_user_data: *mut c_void, _body: JPH_BodyID) -> bool {
    true
}

unsafe extern "C" fn body_filter_locked(user_data: *mut c_void, body: *const JPH_Body) -> bool {
    // SAFETY: as above; the body is locked by Jolt for the call.
    unsafe {
        let shared = &*user_data.cast::<Shared>();
        shared.query_sensors.get() || !JPH_Body_IsSensor(body)
    }
}

unsafe extern "C" fn collect_body(
    context: *mut c_void,
    result: *const JPH_CollideShapeResult,
) -> f32 {
    // SAFETY: the context is the `Vec<BodyId>` passed by `overlap_sphere` for this call.
    unsafe {
        let out = &mut *context.cast::<Vec<BodyId>>();
        out.push(BodyId(u64::from((*result).bodyID2)));
    }
    // Keep the early-out fraction: collect every hit.
    f32::MAX
}

struct BodyInfo {
    gravity: GravityMode,
}

/// A Jolt physics world.
pub struct JoltBackend {
    system: *mut JPH_PhysicsSystem,
    body_interface: *mut JPH_BodyInterface,
    lock_interface: *const JPH_BodyLockInterface,
    query: *const JPH_NarrowPhaseQuery,
    job_system: *mut JPH_JobSystem,
    // The layer tables belong to `system` (JoltC frees them with it).
    contact_listener: *mut JPH_ContactListener,
    step_listener: *mut JPH_PhysicsStepListener,
    layer_filter: *mut JPH_ObjectLayerFilter,
    body_filter: *mut JPH_BodyFilter,
    unit_sphere: *mut JPH_Shape,
    shared: Box<Shared>,
    shapes: Vec<*mut JPH_Shape>,
    constraints: Vec<*mut JPH_Constraint>,
    bodies: BTreeMap<JPH_BodyID, BodyInfo>,
    gravity_dirty: bool,
    events: Vec<ContactEvent>,
    stats: PhysicsStats,
    name: String,
}

impl core::fmt::Debug for JoltBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JoltBackend")
            .field("bodies", &self.bodies.len())
            .field("shapes", &self.shapes.len())
            .finish()
    }
}

impl JoltBackend {
    /// Creates a world.
    pub fn new(settings: &PhysicsSettings) -> Result<Self, PhysicsError> {
        init_jolt();
        let layers = u32::from(settings.layers.clamp(1, 32));
        let object_layers = layers * 2;
        // SAFETY: object creation through the C API; every pointer is checked or owned by
        // the returned value, which destroys them in `Drop`.
        unsafe {
            let bp_interface = JPH_BroadPhaseLayerInterfaceTable_Create(object_layers, 2);
            let pair_filter = JPH_ObjectLayerPairFilterTable_Create(object_layers);
            for a in 0..object_layers {
                JPH_BroadPhaseLayerInterfaceTable_MapObjectToBroadPhaseLayer(
                    bp_interface,
                    a,
                    (a % 2) as JPH_BroadPhaseLayer,
                );
                for b in a..object_layers {
                    let (la, lb) = ((a / 2) as usize, (b / 2) as usize);
                    let mask_a = settings.collides.get(la).copied().unwrap_or(0);
                    let static_pair = a % 2 == 0 && b % 2 == 0;
                    if !static_pair && (mask_a >> lb) & 1 != 0 {
                        JPH_ObjectLayerPairFilterTable_EnableCollision(pair_filter, a, b);
                    }
                }
            }
            let object_vs_bp = JPH_ObjectVsBroadPhaseLayerFilterTable_Create(
                bp_interface,
                2,
                pair_filter,
                object_layers,
            );
            let system = JPH_PhysicsSystem_Create(&JPH_PhysicsSystemSettings {
                maxBodies: settings.max_bodies,
                numBodyMutexes: 0,
                maxBodyPairs: settings.max_body_pairs,
                maxContactConstraints: settings.max_contact_constraints,
                _padding: 0,
                broadPhaseLayerInterface: bp_interface,
                objectLayerPairFilter: pair_filter,
                objectVsBroadPhaseLayerFilter: object_vs_bp,
            });
            if system.is_null() {
                JPH_ObjectVsBroadPhaseLayerFilter_Destroy(object_vs_bp);
                JPH_ObjectLayerPairFilter_Destroy(pair_filter);
                JPH_BroadPhaseLayerInterface_Destroy(bp_interface);
                return Err(PhysicsError::Full("physics system"));
            }
            let job_system = JPH_JobSystemThreadPool_Create(&JobSystemThreadPoolConfig {
                maxJobs: 2048,
                maxBarriers: 8,
                numThreads: settings.threads as i32,
            });
            let capacity = (settings.max_contact_constraints as usize).max(1024) * 2;
            let mut shared = Box::new(Shared {
                events: (0..capacity)
                    .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                    .collect(),
                event_count: AtomicUsize::new(0),
                gravity_bodies: Vec::new(),
                fields: Vec::new(),
                body_interface_no_lock: JPH_PhysicsSystem_GetBodyInterfaceNoLock(system),
                query_layers: Cell::new(u64::MAX),
                query_sensors: Cell::new(false),
            });
            let user_data = (&raw mut *shared).cast::<c_void>();
            let contact_listener = JPH_ContactListener_Create(user_data);
            JPH_PhysicsSystem_SetContactListener(system, contact_listener);
            let step_listener = JPH_PhysicsStepListener_Create(user_data);
            JPH_PhysicsSystem_AddStepListener(system, step_listener);
            let gravity = v3(settings.gravity);
            JPH_PhysicsSystem_SetGravity(system, &gravity);
            let world = Self {
                system,
                body_interface: JPH_PhysicsSystem_GetBodyInterface(system),
                lock_interface: JPH_PhysicsSystem_GetBodyLockInterface(system),
                query: JPH_PhysicsSystem_GetNarrowPhaseQuery(system),
                job_system,
                contact_listener,
                step_listener,
                layer_filter: JPH_ObjectLayerFilter_Create(user_data),
                body_filter: JPH_BodyFilter_Create(user_data),
                unit_sphere: JPH_SphereShape_Create(1.0),
                shared,
                shapes: Vec::new(),
                constraints: Vec::new(),
                bodies: BTreeMap::new(),
                gravity_dirty: false,
                events: Vec::new(),
                stats: PhysicsStats::default(),
                name: String::from("Jolt Physics 5.6.0 (JoltC)"),
            };
            Ok(world)
        }
    }

    fn body_ptr_for_constraint(&self, body: Option<BodyId>) -> Result<*mut JPH_Body, PhysicsError> {
        match body {
            // SAFETY: Jolt's static world body, valid for the process.
            None => Ok(unsafe { JPH_Body_GetFixedToWorldBody() }),
            Some(id) => {
                let raw = id.0 as JPH_BodyID;
                if !self.bodies.contains_key(&raw) {
                    return Err(PhysicsError::InvalidHandle);
                }
                let mut lock = JPH_BodyLockRead {
                    lockInterface: null(),
                    mutex: core::ptr::null_mut(),
                    body: null(),
                };
                // SAFETY: locks, reads the pointer, unlocks. Bodies are not destroyed while
                // this `&self` call runs; Jolt's constraint creation only stores the pointer
                // (bodies outlive their constraints by contract of `destroy_body`).
                unsafe {
                    JPH_BodyLockInterface_LockRead(self.lock_interface, raw, &mut lock);
                    let body = lock.body.cast_mut();
                    JPH_BodyLockInterface_UnlockRead(self.lock_interface, &mut lock);
                    if body.is_null() {
                        Err(PhysicsError::InvalidHandle)
                    } else {
                        Ok(body)
                    }
                }
            }
        }
    }

    fn rebuild_gravity_list(&mut self) {
        let list = &mut self.shared.gravity_bodies;
        list.clear();
        for (&id, info) in &self.bodies {
            if matches!(info.gravity, GravityMode::Custom(_) | GravityMode::Fields) {
                list.push((id, info.gravity));
            }
        }
        self.gravity_dirty = false;
    }

    fn set_query_filter(&self, filter: QueryFilter) {
        self.shared.query_layers.set(filter.layers);
        self.shared.query_sensors.set(filter.sensors);
    }
}

fn jolt_gravity_factor(mode: GravityMode) -> f32 {
    match mode {
        GravityMode::World => 1.0,
        GravityMode::Scaled(factor) => factor,
        GravityMode::Custom(_) | GravityMode::Fields => 0.0,
    }
}

impl PhysicsBackend for JoltBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn set_gravity(&mut self, gravity: Vec3) {
        let g = v3(gravity);
        // SAFETY: valid system.
        unsafe { JPH_PhysicsSystem_SetGravity(self.system, &g) };
    }

    fn set_gravity_fields(&mut self, fields: &[GravityField]) {
        self.shared.fields.clear();
        self.shared.fields.extend_from_slice(fields);
    }

    fn create_shape(&mut self, desc: &ShapeDesc<'_>) -> Result<ShapeId, PhysicsError> {
        // SAFETY: creation calls with valid arguments; settings objects are destroyed after
        // building the shape.
        let shape = unsafe {
            match *desc {
                ShapeDesc::Sphere { radius } => JPH_SphereShape_Create(radius),
                ShapeDesc::Box { half_extents } => {
                    let convex = (half_extents.min_element() * 0.1).min(0.05);
                    JPH_BoxShape_Create(&v3(half_extents), convex)
                }
                ShapeDesc::Capsule {
                    half_height,
                    radius,
                } => JPH_CapsuleShape_Create(half_height, radius),
                ShapeDesc::Cylinder {
                    half_height,
                    radius,
                } => JPH_CylinderShape_Create(half_height, radius),
                ShapeDesc::ConvexHull { points } => {
                    let points: Vec<JPH_Vec3> = points.iter().copied().map(v3).collect();
                    let settings = JPH_ConvexHullShapeSettings_Create(
                        points.as_ptr(),
                        points.len() as u32,
                        0.05,
                    );
                    let shape = JPH_ConvexHullShapeSettings_CreateShape(settings);
                    JPH_ShapeSettings_Destroy(settings);
                    shape
                }
                ShapeDesc::TriangleMesh {
                    vertices,
                    triangles,
                } => {
                    let vertices: Vec<JPH_Vec3> = vertices.iter().copied().map(v3).collect();
                    let triangles: Vec<JPH_IndexedTriangle> = triangles
                        .iter()
                        .map(|t| JPH_IndexedTriangle {
                            i1: t[0],
                            i2: t[1],
                            i3: t[2],
                            materialIndex: 0,
                            userData: 0,
                        })
                        .collect();
                    let settings = JPH_MeshShapeSettings_Create2(
                        vertices.as_ptr(),
                        vertices.len() as u32,
                        triangles.as_ptr(),
                        triangles.len() as u32,
                    );
                    let shape = JPH_MeshShapeSettings_CreateShape(settings);
                    JPH_ShapeSettings_Destroy(settings);
                    shape
                }
            }
        };
        if shape.is_null() {
            return Err(PhysicsError::InvalidShape(format!("{desc:?}")));
        }
        self.shapes.push(shape);
        Ok(ShapeId((self.shapes.len() - 1) as u64))
    }

    fn destroy_shape(&mut self, shape: ShapeId) {
        if let Some(slot) = self.shapes.get_mut(shape.0 as usize)
            && !slot.is_null()
        {
            // SAFETY: releases our reference; bodies hold their own.
            unsafe { JPH_Shape_Destroy(*slot) };
            *slot = core::ptr::null_mut();
        }
    }

    fn create_body(&mut self, desc: &BodyDesc) -> Result<BodyId, PhysicsError> {
        let shape = *self
            .shapes
            .get(desc.shape.0 as usize)
            .filter(|s| !s.is_null())
            .ok_or(PhysicsError::InvalidHandle)?;
        let motion = match desc.motion {
            MotionType::Static => JPH_MOTION_TYPE_STATIC,
            MotionType::Kinematic => JPH_MOTION_TYPE_KINEMATIC,
            MotionType::Dynamic => JPH_MOTION_TYPE_DYNAMIC,
        };
        let object_layer =
            u32::from(desc.layer.0) * 2 + u32::from(desc.motion != MotionType::Static);
        // SAFETY: settings are created, filled, used and destroyed here.
        let id = unsafe {
            let settings = JPH_BodyCreationSettings_Create3(
                shape,
                &v3(desc.position),
                &quat(desc.rotation),
                motion,
                object_layer,
            );
            JPH_BodyCreationSettings_SetLinearVelocity(settings, &v3(desc.linear_velocity));
            JPH_BodyCreationSettings_SetAngularVelocity(settings, &v3(desc.angular_velocity));
            JPH_BodyCreationSettings_SetUserData(settings, desc.user_data);
            JPH_BodyCreationSettings_SetIsSensor(settings, desc.sensor);
            JPH_BodyCreationSettings_SetMotionQuality(
                settings,
                match desc.quality {
                    MotionQuality::Discrete => JPH_MOTION_QUALITY_DISCRETE,
                    MotionQuality::Continuous => JPH_MOTION_QUALITY_LINEAR_CAST,
                },
            );
            JPH_BodyCreationSettings_SetAllowSleeping(settings, desc.allow_sleeping);
            JPH_BodyCreationSettings_SetFriction(settings, desc.friction);
            JPH_BodyCreationSettings_SetRestitution(settings, desc.restitution);
            JPH_BodyCreationSettings_SetLinearDamping(settings, desc.linear_damping);
            JPH_BodyCreationSettings_SetAngularDamping(settings, desc.angular_damping);
            JPH_BodyCreationSettings_SetGravityFactor(settings, jolt_gravity_factor(desc.gravity));
            if let Some(mass) = desc.mass {
                JPH_BodyCreationSettings_SetOverrideMassProperties(
                    settings,
                    JPH_OVERRIDE_MASS_CALCULATE_INERTIA,
                );
                JPH_BodyCreationSettings_SetMassPropertiesOverride(
                    settings,
                    &JPH_MassProperties {
                        mass,
                        inertia: JPH_RMat4::default(),
                    },
                );
            }
            let activation = if desc.motion == MotionType::Static {
                JPH_ACTIVATION_DONT_ACTIVATE
            } else {
                JPH_ACTIVATION_ACTIVATE
            };
            let id = JPH_BodyInterface_CreateAndAddBody(self.body_interface, settings, activation);
            JPH_BodyCreationSettings_Destroy(settings);
            id
        };
        if id == JPH_INVALID_BODY_ID {
            return Err(PhysicsError::Full("bodies"));
        }
        self.bodies.insert(
            id,
            BodyInfo {
                gravity: desc.gravity,
            },
        );
        if matches!(desc.gravity, GravityMode::Custom(_) | GravityMode::Fields) {
            self.gravity_dirty = true;
        }
        Ok(BodyId(u64::from(id)))
    }

    fn destroy_body(&mut self, body: BodyId) {
        let raw = body.0 as JPH_BodyID;
        if let Some(info) = self.bodies.remove(&raw) {
            if matches!(info.gravity, GravityMode::Custom(_) | GravityMode::Fields) {
                self.gravity_dirty = true;
            }
            // SAFETY: a live body of this system.
            unsafe { JPH_BodyInterface_RemoveAndDestroyBody(self.body_interface, raw) };
        }
    }

    fn transform(&self, body: BodyId) -> Option<(Vec3, Quat)> {
        let raw = body.0 as JPH_BodyID;
        if !self.bodies.contains_key(&raw) {
            return None;
        }
        let (mut p, mut r) = (JPH_Vec3::default(), JPH_Quat::default());
        // SAFETY: a live body.
        unsafe {
            JPH_BodyInterface_GetPositionAndRotation(self.body_interface, raw, &mut p, &mut r)
        };
        Some((from_v3(p), from_quat(r)))
    }

    fn set_transform(&mut self, body: BodyId, position: Vec3, rotation: Quat) {
        let raw = body.0 as JPH_BodyID;
        if self.bodies.contains_key(&raw) {
            // SAFETY: a live body.
            unsafe {
                JPH_BodyInterface_SetPositionAndRotation(
                    self.body_interface,
                    raw,
                    &v3(position),
                    &quat(rotation),
                    JPH_ACTIVATION_ACTIVATE,
                );
            }
        }
    }

    fn velocity(&self, body: BodyId) -> Option<(Vec3, Vec3)> {
        let raw = body.0 as JPH_BodyID;
        if !self.bodies.contains_key(&raw) {
            return None;
        }
        let (mut l, mut a) = (JPH_Vec3::default(), JPH_Vec3::default());
        // SAFETY: a live body.
        unsafe {
            JPH_BodyInterface_GetLinearVelocity(self.body_interface, raw, &mut l);
            JPH_BodyInterface_GetAngularVelocity(self.body_interface, raw, &mut a);
        }
        Some((from_v3(l), from_v3(a)))
    }

    fn set_velocity(&mut self, body: BodyId, linear: Vec3, angular: Vec3) {
        let raw = body.0 as JPH_BodyID;
        if self.bodies.contains_key(&raw) {
            let (mut l, mut a) = (v3(linear), v3(angular));
            // SAFETY: a live body.
            unsafe {
                JPH_BodyInterface_SetLinearAndAngularVelocity(
                    self.body_interface,
                    raw,
                    &mut l,
                    &mut a,
                )
            };
        }
    }

    fn add_impulse(&mut self, body: BodyId, impulse: Vec3) {
        let raw = body.0 as JPH_BodyID;
        if self.bodies.contains_key(&raw) {
            let mut i = v3(impulse);
            // SAFETY: a live body.
            unsafe { JPH_BodyInterface_AddImpulse(self.body_interface, raw, &mut i) };
        }
    }

    fn set_body_gravity(&mut self, body: BodyId, gravity: GravityMode) {
        let raw = body.0 as JPH_BodyID;
        if let Some(info) = self.bodies.get_mut(&raw) {
            info.gravity = gravity;
            self.gravity_dirty = true;
            // SAFETY: a live body.
            unsafe {
                JPH_BodyInterface_SetGravityFactor(
                    self.body_interface,
                    raw,
                    jolt_gravity_factor(gravity),
                )
            };
        }
    }

    fn is_active(&self, body: BodyId) -> bool {
        let raw = body.0 as JPH_BodyID;
        // SAFETY: checked live body.
        self.bodies.contains_key(&raw)
            && unsafe { JPH_BodyInterface_IsActive(self.body_interface, raw) }
    }

    fn create_constraint(&mut self, desc: &ConstraintDesc) -> Result<ConstraintId, PhysicsError> {
        let (a, b) = match *desc {
            ConstraintDesc::Fixed { a, b }
            | ConstraintDesc::Point { a, b, .. }
            | ConstraintDesc::Hinge { a, b, .. }
            | ConstraintDesc::Distance { a, b, .. }
            | ConstraintDesc::Slider { a, b, .. } => (a, b),
        };
        let body1 = self.body_ptr_for_constraint(Some(a))?;
        let body2 = self.body_ptr_for_constraint(b)?;
        let perpendicular = |axis: Vec3| axis.any_orthonormal_vector();
        // SAFETY: settings initialized by Jolt's defaults, then filled in world space;
        // bodies are live (checked above).
        let constraint = unsafe {
            match *desc {
                ConstraintDesc::Fixed { .. } => {
                    let mut s = MaybeUninit::<JPH_FixedConstraintSettings>::zeroed();
                    JPH_FixedConstraintSettings_Init(s.as_mut_ptr());
                    let mut s = s.assume_init();
                    s.autoDetectPoint = true;
                    JPH_FixedConstraint_Create(&s, body1, body2)
                }
                ConstraintDesc::Point { anchor, .. } => {
                    let mut s = MaybeUninit::<JPH_PointConstraintSettings>::zeroed();
                    JPH_PointConstraintSettings_Init(s.as_mut_ptr());
                    let mut s = s.assume_init();
                    s.space = JPH_CONSTRAINT_SPACE_WORLD;
                    s.point1 = v3(anchor);
                    s.point2 = v3(anchor);
                    JPH_PointConstraint_Create(&s, body1, body2)
                }
                ConstraintDesc::Hinge {
                    anchor,
                    axis,
                    limits,
                    ..
                } => {
                    let mut s = MaybeUninit::<JPH_HingeConstraintSettings>::zeroed();
                    JPH_HingeConstraintSettings_Init(s.as_mut_ptr());
                    let mut s = s.assume_init();
                    s.space = JPH_CONSTRAINT_SPACE_WORLD;
                    let axis = axis.normalize_or(Vec3::Y);
                    s.point1 = v3(anchor);
                    s.point2 = v3(anchor);
                    s.hingeAxis1 = v3(axis);
                    s.hingeAxis2 = v3(axis);
                    s.normalAxis1 = v3(perpendicular(axis));
                    s.normalAxis2 = v3(perpendicular(axis));
                    if let Some((min, max)) = limits {
                        s.limitsMin = min;
                        s.limitsMax = max;
                    }
                    JPH_HingeConstraint_Create(&s, body1, body2)
                }
                ConstraintDesc::Distance {
                    anchor_a,
                    anchor_b,
                    min,
                    max,
                    ..
                } => {
                    let mut s = MaybeUninit::<JPH_DistanceConstraintSettings>::zeroed();
                    JPH_DistanceConstraintSettings_Init(s.as_mut_ptr());
                    let mut s = s.assume_init();
                    s.space = JPH_CONSTRAINT_SPACE_WORLD;
                    s.point1 = v3(anchor_a);
                    s.point2 = v3(anchor_b);
                    s.minDistance = min;
                    s.maxDistance = max;
                    JPH_DistanceConstraint_Create(&s, body1, body2)
                }
                ConstraintDesc::Slider {
                    anchor,
                    axis,
                    limits,
                    ..
                } => {
                    let mut s = MaybeUninit::<JPH_SliderConstraintSettings>::zeroed();
                    JPH_SliderConstraintSettings_Init(s.as_mut_ptr());
                    let mut s = s.assume_init();
                    s.space = JPH_CONSTRAINT_SPACE_WORLD;
                    let axis = axis.normalize_or(Vec3::X);
                    s.autoDetectPoint = false;
                    s.point1 = v3(anchor);
                    s.point2 = v3(anchor);
                    s.sliderAxis1 = v3(axis);
                    s.sliderAxis2 = v3(axis);
                    s.normalAxis1 = v3(perpendicular(axis));
                    s.normalAxis2 = v3(perpendicular(axis));
                    if let Some((min, max)) = limits {
                        s.limitsMin = min;
                        s.limitsMax = max;
                    }
                    JPH_SliderConstraint_Create(&s, body1, body2)
                }
            }
        };
        if constraint.is_null() {
            return Err(PhysicsError::Full("constraints"));
        }
        // SAFETY: a new constraint between live bodies.
        unsafe { JPH_PhysicsSystem_AddConstraint(self.system, constraint) };
        self.constraints.push(constraint);
        Ok(ConstraintId((self.constraints.len() - 1) as u64))
    }

    fn destroy_constraint(&mut self, constraint: ConstraintId) {
        if let Some(slot) = self.constraints.get_mut(constraint.0 as usize)
            && !slot.is_null()
        {
            // SAFETY: a constraint added to this system.
            unsafe {
                JPH_PhysicsSystem_RemoveConstraint(self.system, *slot);
                JPH_Constraint_Destroy(*slot);
            }
            *slot = core::ptr::null_mut();
        }
    }

    fn step(&mut self, dt: f32, substeps: u32) -> Result<(), PhysicsError> {
        engine_profiler::zone!("physics::jolt::step");
        if self.gravity_dirty {
            self.rebuild_gravity_list();
        }
        self.shared.event_count.store(0, Ordering::Relaxed);
        let error = {
            let _serialized = STEP_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the system and job system are valid; callbacks reach `shared`, which
            // outlives the call; `STEP_LOCK` keeps JoltC's shared temporary allocator to one
            // step at a time.
            unsafe {
                JPH_PhysicsSystem_Update(self.system, dt, substeps.max(1) as i32, self.job_system)
            }
        };
        // Gather the events written by the callbacks (the step has finished: all writes
        // happened before `Update` returned).
        let written = self
            .shared
            .event_count
            .load(Ordering::Acquire)
            .min(self.shared.events.len());
        self.events.clear();
        for cell in &self.shared.events[..written] {
            // SAFETY: slots below the count were initialized during this step.
            let raw = unsafe { (*cell.get()).assume_init() };
            self.events.push(ContactEvent {
                kind: raw.kind,
                a: BodyId(u64::from(raw.a)),
                b: BodyId(u64::from(raw.b)),
                point: raw.point,
                normal: raw.normal,
                sensor: raw.sensor,
            });
        }
        // Jolt calls back from several threads: sort into a deterministic order, and
        // collapse per-sub-shape duplicates of one body pair.
        self.events.sort_by(|x, y| {
            (x.kind, x.a, x.b)
                .cmp(&(y.kind, y.a, y.b))
                .then(x.point.x.total_cmp(&y.point.x))
                .then(x.point.y.total_cmp(&y.point.y))
                .then(x.point.z.total_cmp(&y.point.z))
        });
        self.events
            .dedup_by(|x, y| (x.kind, x.a, x.b) == (y.kind, y.a, y.b));
        // SAFETY: valid system.
        let (bodies, active) = unsafe {
            (
                JPH_PhysicsSystem_GetNumBodies(self.system),
                JPH_PhysicsSystem_GetNumActiveBodies(self.system, JPH_BODY_TYPE_RIGID),
            )
        };
        self.stats = PhysicsStats {
            bodies,
            active_bodies: active,
            contact_events: self.events.len() as u32,
        };
        if error != 0 {
            return Err(PhysicsError::StepFailed(format!(
                "Jolt update error flags {error:#x} (raise the pair or contact limits)"
            )));
        }
        Ok(())
    }

    fn contact_events(&self) -> &[ContactEvent] {
        &self.events
    }

    fn stats(&self) -> PhysicsStats {
        self.stats
    }

    fn raycast(&self, ray: &Ray, filter: QueryFilter) -> Option<RayHit> {
        self.set_query_filter(filter);
        let mut hit = JPH_RayCastResult::default();
        let origin = v3(ray.origin);
        // SAFETY: valid query object and filters; `hit` is written on success.
        let found = unsafe {
            JPH_NarrowPhaseQuery_CastRay(
                self.query,
                &origin,
                &v3(ray.direction),
                &mut hit,
                null(),
                self.layer_filter,
                self.body_filter,
            )
        };
        if !found {
            return None;
        }
        let point = ray.origin + ray.direction * hit.fraction;
        let mut normal = JPH_Vec3::default();
        let mut lock = JPH_BodyLockRead {
            lockInterface: null(),
            mutex: core::ptr::null_mut(),
            body: null(),
        };
        // SAFETY: lock, read the surface normal, unlock.
        let user_data = unsafe {
            JPH_BodyLockInterface_LockRead(self.lock_interface, hit.bodyID, &mut lock);
            if !lock.body.is_null() {
                JPH_Body_GetWorldSpaceSurfaceNormal(
                    lock.body,
                    hit.subShapeID2,
                    &v3(point),
                    &mut normal,
                );
            }
            JPH_BodyLockInterface_UnlockRead(self.lock_interface, &mut lock);
            JPH_BodyInterface_GetUserData(self.body_interface, hit.bodyID)
        };
        Some(RayHit {
            body: BodyId(u64::from(hit.bodyID)),
            fraction: hit.fraction,
            point,
            normal: from_v3(normal),
            user_data,
        })
    }

    fn overlap_sphere(
        &self,
        center: Vec3,
        radius: f32,
        filter: QueryFilter,
        out: &mut Vec<BodyId>,
    ) {
        self.set_query_filter(filter);
        let start = out.len();
        let mut settings = MaybeUninit::<JPH_CollideShapeSettings>::zeroed();
        let transform = JPH_RMat4 {
            column: [
                JPH_Vec4 {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                    w: 0.0,
                },
                JPH_Vec4 {
                    x: 0.0,
                    y: 1.0,
                    z: 0.0,
                    w: 0.0,
                },
                JPH_Vec4 {
                    x: 0.0,
                    y: 0.0,
                    z: 1.0,
                    w: 0.0,
                },
                JPH_Vec4 {
                    x: center.x,
                    y: center.y,
                    z: center.z,
                    w: 1.0,
                },
            ],
        };
        let mut base_offset = JPH_Vec3::default();
        // SAFETY: settings initialized by Jolt; the callback appends to `out`, which lives
        // for the call.
        unsafe {
            JPH_CollideShapeSettings_Init(settings.as_mut_ptr());
            JPH_NarrowPhaseQuery_CollideShape(
                self.query,
                self.unit_sphere,
                &v3(Vec3::splat(radius)),
                &transform,
                settings.as_ptr(),
                &mut base_offset,
                collect_body,
                (out as *mut Vec<BodyId>).cast(),
                null(),
                self.layer_filter,
                self.body_filter,
                null(),
            );
        }
        out[start..].sort_unstable();
        let mut end = start;
        for i in start..out.len() {
            if end == start || out[end - 1] != out[i] {
                out[end] = out[i];
                end += 1;
            }
        }
        out.truncate(end);
    }

    fn state_hash(&self) -> u64 {
        let mut hash = HASH_SEED;
        for &id in self.bodies.keys() {
            let (mut p, mut r, mut l, mut a) = (
                JPH_Vec3::default(),
                JPH_Quat::default(),
                JPH_Vec3::default(),
                JPH_Vec3::default(),
            );
            // SAFETY: live bodies.
            unsafe {
                JPH_BodyInterface_GetPositionAndRotation(self.body_interface, id, &mut p, &mut r);
                JPH_BodyInterface_GetLinearVelocity(self.body_interface, id, &mut l);
                JPH_BodyInterface_GetAngularVelocity(self.body_interface, id, &mut a);
            }
            hash_words(
                &mut hash,
                [
                    id,
                    p.x.to_bits(),
                    p.y.to_bits(),
                    p.z.to_bits(),
                    r.x.to_bits(),
                    r.y.to_bits(),
                    r.z.to_bits(),
                    r.w.to_bits(),
                    l.x.to_bits(),
                    l.y.to_bits(),
                    l.z.to_bits(),
                    a.x.to_bits(),
                    a.y.to_bits(),
                    a.z.to_bits(),
                ],
            );
        }
        hash
    }
}

impl Drop for JoltBackend {
    fn drop(&mut self) {
        // SAFETY: destroys everything this world created: constraints and bodies before
        // the system (which also frees the layer tables it owns), listeners after it.
        unsafe {
            for &constraint in &self.constraints {
                if !constraint.is_null() {
                    JPH_PhysicsSystem_RemoveConstraint(self.system, constraint);
                    JPH_Constraint_Destroy(constraint);
                }
            }
            for &id in self.bodies.keys() {
                JPH_BodyInterface_RemoveAndDestroyBody(self.body_interface, id);
            }
            JPH_PhysicsSystem_Destroy(self.system);
            JPH_ContactListener_Destroy(self.contact_listener);
            JPH_PhysicsStepListener_Destroy(self.step_listener);
            JPH_ObjectLayerFilter_Destroy(self.layer_filter);
            JPH_BodyFilter_Destroy(self.body_filter);
            for &shape in &self.shapes {
                if !shape.is_null() {
                    JPH_Shape_Destroy(shape);
                }
            }
            JPH_Shape_Destroy(self.unit_sphere);
            JPH_JobSystem_Destroy(self.job_system);
            // The layer tables and filters belong to the physics system from creation on:
            // JoltC's `JPH_PhysicsSystem_Destroy` deleted them.
        }
    }
}
