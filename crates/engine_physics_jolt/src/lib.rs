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
//! - Jolt's own job system (a thread pool) runs large simulations. Before each step the
//!   world picks: fewer awake bodies than `PhysicsSettings::single_thread_below` run on
//!   the calling thread, more on the pool (created on first use, so small worlds never
//!   start threads). `threads = 0` keeps everything on the calling thread. Jolt's results
//!   do not depend on the thread count. Routing Jolt's jobs through `engine_core::jobs`
//!   is a follow-up (ADR 0018).
//! - Queries (ADR 0066) go through JoltC's narrow-phase queries with collector callbacks
//!   that write into reusable buffers; results are sorted, because Jolt reports narrow-
//!   phase hits in an order that is not guaranteed.
//! - Characters, state save/restore and the allocation counter go through our own small
//!   C++ extension over Jolt's C++ API (`csrc/ext.cpp`): Jolt's `CharacterVirtual` with
//!   `ExtendedUpdate`, its `StateRecorder` over the caller's buffer, and wrappers around
//!   Jolt's allocation hooks.

mod ffi;

use core::cell::RefCell;
use core::cell::{Cell, UnsafeCell};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr::null;
use core::sync::atomic::{AtomicUsize, Ordering};
use engine_core::jobs::{ForeignJobs, ThreadPool};
use engine_core::math::Mat4;
use engine_core::math::{Quat, Vec3};
use engine_physics::{
    BodyDesc, BodyId, CharacterContact, CharacterDesc, CharacterId, CharacterState, ConstraintDesc,
    ConstraintId, ContactEvent, ContactKind, GravityField, GravityMode, GroundState, HASH_SEED,
    MotionQuality, MotionType, PhysicsBackend, PhysicsError, PhysicsSettings, PhysicsSnapshot,
    PhysicsStats, QueryFilter, Ray, RayHit, ShapeCast, ShapeDesc, ShapeHit, ShapeId, hash_words,
};
use ffi::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Once};

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

fn mat4(position: Vec3, rotation: Quat) -> JPH_RMat4 {
    let m = Mat4::from_rotation_translation(rotation, position);
    let column = |c: engine_core::math::Vec4| JPH_Vec4 {
        x: c.x,
        y: c.y,
        z: c.z,
        w: c.w,
    };
    JPH_RMat4 {
        column: [
            column(m.x_axis),
            column(m.y_axis),
            column(m.z_axis),
            column(m.w_axis),
        ],
    }
}

fn arr3(a: [f32; 3]) -> Vec3 {
    Vec3::from_array(a)
}

fn body_or_none(raw: u32) -> Option<BodyId> {
    (raw != ENGINE_JOLT_NONE).then_some(BodyId(u64::from(raw)))
}

fn character_state(raw: &EngineJoltCharacterState) -> CharacterState {
    CharacterState {
        position: arr3(raw.position),
        rotation: Quat::from_array(raw.rotation),
        velocity: arr3(raw.velocity),
        ground: match raw.ground_state {
            0 => GroundState::OnGround,
            1 => GroundState::OnSteepGround,
            2 => GroundState::NotSupported,
            _ => GroundState::InAir,
        },
        ground_normal: arr3(raw.ground_normal),
        ground_point: arr3(raw.ground_point),
        ground_body: body_or_none(raw.ground_body),
        ground_velocity: arr3(raw.ground_velocity),
        proxy: body_or_none(raw.proxy_body),
    }
}

/// First four bytes of a Jolt backend snapshot ("OEJ1"), then the numbers of bodies,
/// constraints and characters, then Jolt's state.
const SNAPSHOT_MAGIC: u32 = u32::from_le_bytes(*b"OEJ1");
const SNAPSHOT_HEADER: usize = 16;

static INIT: Once = Once::new();

/// Serializes JoltC's process-wide state (uncontended with the usual single world):
/// - every world steps with one temporary allocator (a stack allocator, not thread-safe),
///   so steps of different worlds must not overlap;
/// - a map from Jolt systems to JoltC systems is written when a world is created or
///   destroyed and read by step listeners on Jolt's worker threads during a step, so
///   creation and destruction must not overlap another world's step (they did in parallel
///   tests: an access violation when the map rehashed under a reader).
static STEP_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`STEP_LOCK`], also after a panic elsewhere (the guarded state is JoltC's).
fn jolt_lock() -> std::sync::MutexGuard<'static, ()> {
    STEP_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs a Jolt job on the calling thread as soon as it is queued (`threads = 0`).
unsafe extern "C" fn run_inline(_: *mut c_void, job: JPH_JobFunction, arg: *mut c_void) {
    // SAFETY: Jolt passes a job function with its argument.
    unsafe { job(arg) };
}

/// Jolt's jobs on the engine's pool ([`JoltBackend::set_jobs`]): queued through the
/// pool's injector, or run at once when it is full.
unsafe extern "C" fn queue_on_pool(context: *mut c_void, job: JPH_JobFunction, arg: *mut c_void) {
    // SAFETY: the context is the world's `ForeignJobs`, alive as long as this job system.
    let jobs = unsafe { &*context.cast::<ForeignJobs>() };
    // SAFETY: Jolt's jobs run on any thread, and `step` waits for every one.
    if !unsafe { jobs.submit(job, arg) } {
        // SAFETY: as in `run_inline`.
        unsafe { job(arg) };
    }
}

/// Batch form of [`queue_on_pool`].
unsafe extern "C" fn queue_many_on_pool(
    context: *mut c_void,
    job: JPH_JobFunction,
    args: *mut *mut c_void,
    count: u32,
) {
    for i in 0..count as usize {
        // SAFETY: Jolt passes `count` job arguments; the context as above.
        unsafe { queue_on_pool(context, job, *args.add(i)) };
    }
}

/// Batch form of [`run_inline`].
unsafe extern "C" fn run_inline_batch(
    _: *mut c_void,
    job: JPH_JobFunction,
    args: *mut *mut c_void,
    count: u32,
) {
    for i in 0..count as usize {
        // SAFETY: Jolt passes `count` job arguments.
        unsafe { job(*args.add(i)) };
    }
}

/// Registers Jolt's types and allocator once per process (never shut down: other worlds
/// may be alive, and the process exit frees it), and installs the callback tables every
/// world shares (each world passes its own user data).
fn init_jolt() {
    INIT.call_once(|| {
        // SAFETY: first and only initialization; the procedure tables are 'static.
        unsafe {
            JPH_Init();
            // Before any world exists, so every Jolt allocation goes through the counter.
            engine_jolt_count_allocations();
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
    /// A body the current query skips, or `JPH_INVALID_BODY_ID`.
    query_ignore: Cell<JPH_BodyID>,
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

unsafe extern "C" fn body_filter(user_data: *mut c_void, body: JPH_BodyID) -> bool {
    // SAFETY: the user data is the world's `Shared`; queries run on its thread.
    let shared = unsafe { &*user_data.cast::<Shared>() };
    body != shared.query_ignore.get()
}

unsafe extern "C" fn body_filter_locked(user_data: *mut c_void, body: *const JPH_Body) -> bool {
    // SAFETY: as above; the body is locked by Jolt for the call.
    unsafe {
        let shared = &*user_data.cast::<Shared>();
        (shared.query_sensors.get() || !JPH_Body_IsSensor(body))
            && JPH_Body_GetID(body) != shared.query_ignore.get()
    }
}

/// A raw ray hit: body, fraction, sub-shape.
type RawRayHit = (JPH_BodyID, f32, JPH_SubShapeID);

unsafe extern "C" fn collect_ray(context: *mut c_void, result: *const JPH_RayCastResult) -> f32 {
    // SAFETY: the context is the scratch `Vec<RawRayHit>` passed by `raycast_all` for this
    // call; the result is valid for the callback.
    unsafe {
        let out = &mut *context.cast::<Vec<RawRayHit>>();
        let r = &*result;
        out.push((r.bodyID, r.fraction, r.subShapeID2));
    }
    // Keep the early-out fraction: collect every hit.
    f32::MAX
}

/// Jolt's ordering of shape-cast hits: the fraction, or minus the penetration depth for
/// hits the shape starts in (deeper first).
fn early_out(hit: &JPH_ShapeCastResult) -> f32 {
    if hit.fraction > 0.0 {
        hit.fraction
    } else {
        -hit.penetrationDepth
    }
}

/// State of a closest-hit shape cast.
struct ClosestShape {
    best: Option<JPH_ShapeCastResult>,
}

unsafe extern "C" fn collect_closest_shape(
    context: *mut c_void,
    result: *const JPH_ShapeCastResult,
) -> f32 {
    // SAFETY: the context is the `ClosestShape` of this `cast_shape` call; the result is
    // valid for the callback.
    unsafe {
        let state = &mut *context.cast::<ClosestShape>();
        let hit = *result;
        let better = state
            .best
            .as_ref()
            .is_none_or(|best| (early_out(&hit), hit.bodyID2) < (early_out(best), best.bodyID2));
        if better {
            state.best = Some(hit);
        }
        // Only hits closer than the best so far are of interest now.
        state.best.as_ref().map_or(f32::MAX, early_out)
    }
}

unsafe extern "C" fn collect_shape(
    context: *mut c_void,
    result: *const JPH_ShapeCastResult,
) -> f32 {
    // SAFETY: the context is the scratch `Vec<JPH_ShapeCastResult>` of this
    // `cast_shape_all` call.
    unsafe {
        let out = &mut *context.cast::<Vec<JPH_ShapeCastResult>>();
        out.push(*result);
    }
    f32::MAX
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

/// A character controller of this world (owned; destroyed with it).
struct CharacterInfo {
    handle: *mut EngineJoltCharacter,
}

/// A Jolt physics world.
pub struct JoltBackend {
    system: *mut JPH_PhysicsSystem,
    body_interface: *mut JPH_BodyInterface,
    lock_interface: *const JPH_BodyLockInterface,
    query: *const JPH_NarrowPhaseQuery,
    /// Runs every job on the calling thread.
    inline_jobs: *mut JPH_JobSystem,
    /// Jolt's thread pool, created by the first step that needs it (without
    /// [`JoltBackend::set_jobs`]).
    pool_jobs: *mut JPH_JobSystem,
    /// The engine's pool ([`JoltBackend::set_jobs`]): Jolt's jobs handed to it, and the job
    /// system that hands them (its context is the boxed `ForeignJobs`).
    engine_jobs: Option<(Box<ForeignJobs>, *mut JPH_JobSystem)>,
    threads: u32,
    single_thread_below: u32,
    // The layer tables belong to `system` (JoltC frees them with it).
    contact_listener: *mut JPH_ContactListener,
    step_listener: *mut JPH_PhysicsStepListener,
    layer_filter: *mut JPH_ObjectLayerFilter,
    body_filter: *mut JPH_BodyFilter,
    unit_sphere: *mut JPH_Shape,
    shared: Box<Shared>,
    shapes: Vec<*mut JPH_Shape>,
    /// Per shape: it can be swept or overlapped by queries (not a triangle mesh).
    shape_castable: Vec<bool>,
    /// Our C++ extension's per-world state (characters' temporary memory and
    /// character-versus-character collision).
    ext: *mut EngineJoltWorld,
    characters: BTreeMap<u64, CharacterInfo>,
    /// The characters' handles in id order (the order snapshots store them in).
    character_handles: Vec<*mut EngineJoltCharacter>,
    next_character: u32,
    /// Reused buffers of the `&self` queries.
    ray_scratch: RefCell<Vec<RawRayHit>>,
    shape_scratch: RefCell<Vec<JPH_ShapeCastResult>>,
    contact_scratch: RefCell<Vec<EngineJoltCharacterContact>>,
    constraints: Vec<*mut JPH_Constraint>,
    bodies: BTreeMap<JPH_BodyID, BodyInfo>,
    /// World gravity as last set.
    gravity: Vec3,
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
    /// Runs the multithreaded steps' jobs on the engine's pool `jobs` (review 2 §3.10, ADR
    /// 0002 addendum) instead of Jolt's own threads, which then never start: one set of
    /// threads for the frame's work, placed by the engine. `None`: Jolt's own threads
    /// again. Steps below [`PhysicsSettings::single_thread_below`] awake bodies stay on the
    /// calling thread either way; results do not depend on the choice.
    ///
    /// [`PhysicsSettings::single_thread_below`]: engine_physics::PhysicsSettings::single_thread_below
    pub fn set_jobs(&mut self, jobs: Option<Arc<ThreadPool>>) {
        if let Some((foreign, system)) = self.engine_jobs.take() {
            foreign.wait();
            // SAFETY: no step is running (`&mut self`) and its jobs have finished.
            unsafe { JPH_JobSystem_Destroy(system) };
        }
        let Some(pool) = jobs else {
            return;
        };
        let concurrency = pool.num_workers() as u32 + 1;
        // Records for every job Jolt can have queued at once (cMaxPhysicsJobs).
        let foreign = Box::new(ForeignJobs::new(pool, 2048));
        let context = (&raw const *foreign).cast_mut().cast::<c_void>();
        // SAFETY: the context outlives the job system (both dropped together, the job
        // system first).
        let system = unsafe {
            JPH_JobSystemCallback_Create(&JPH_JobSystemConfig {
                context,
                queueJob: Some(queue_on_pool),
                queueJobs: Some(queue_many_on_pool),
                maxConcurrency: concurrency,
                maxBarriers: 8,
            })
        };
        if !system.is_null() {
            self.engine_jobs = Some((foreign, system));
        }
    }

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
            let system = {
                let _serialized = jolt_lock();
                JPH_PhysicsSystem_Create(&JPH_PhysicsSystemSettings {
                    maxBodies: settings.max_bodies,
                    numBodyMutexes: 0,
                    maxBodyPairs: settings.max_body_pairs,
                    maxContactConstraints: settings.max_contact_constraints,
                    _padding: 0,
                    broadPhaseLayerInterface: bp_interface,
                    objectLayerPairFilter: pair_filter,
                    objectVsBroadPhaseLayerFilter: object_vs_bp,
                })
            };
            if system.is_null() {
                JPH_ObjectVsBroadPhaseLayerFilter_Destroy(object_vs_bp);
                JPH_ObjectLayerPairFilter_Destroy(pair_filter);
                JPH_BroadPhaseLayerInterface_Destroy(bp_interface);
                return Err(PhysicsError::Full("physics system"));
            }
            // JoltC maps a thread pool of 0 threads to "all cores". Jolt's own
            // single-threaded job system runs each job as soon as it is queued; the
            // callback job system does the same through `run_inline`.
            let inline_jobs = JPH_JobSystemCallback_Create(&JPH_JobSystemConfig {
                context: core::ptr::null_mut(),
                queueJob: Some(run_inline),
                queueJobs: Some(run_inline_batch),
                maxConcurrency: 1,
                maxBarriers: 8,
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
                query_ignore: Cell::new(JPH_INVALID_BODY_ID),
            });
            let user_data = (&raw mut *shared).cast::<c_void>();
            let contact_listener = JPH_ContactListener_Create(user_data);
            JPH_PhysicsSystem_SetContactListener(system, contact_listener);
            let step_listener = JPH_PhysicsStepListener_Create(user_data);
            JPH_PhysicsSystem_AddStepListener(system, step_listener);
            let gravity = v3(settings.gravity);
            JPH_PhysicsSystem_SetGravity(system, &gravity);
            let ext = engine_jolt_world_create(system);
            let world = Self {
                system,
                body_interface: JPH_PhysicsSystem_GetBodyInterface(system),
                lock_interface: JPH_PhysicsSystem_GetBodyLockInterface(system),
                query: JPH_PhysicsSystem_GetNarrowPhaseQuery(system),
                inline_jobs,
                pool_jobs: core::ptr::null_mut(),
                engine_jobs: None,
                threads: settings.threads,
                single_thread_below: settings.single_thread_below,
                contact_listener,
                step_listener,
                layer_filter: JPH_ObjectLayerFilter_Create(user_data),
                body_filter: JPH_BodyFilter_Create(user_data),
                unit_sphere: JPH_SphereShape_Create(1.0),
                shared,
                shapes: Vec::new(),
                shape_castable: Vec::new(),
                ext,
                characters: BTreeMap::new(),
                character_handles: Vec::new(),
                next_character: 1,
                ray_scratch: RefCell::new(Vec::new()),
                shape_scratch: RefCell::new(Vec::new()),
                contact_scratch: RefCell::new(Vec::new()),
                constraints: Vec::new(),
                bodies: BTreeMap::new(),
                gravity: settings.gravity,
                gravity_dirty: false,
                events: Vec::new(),
                stats: PhysicsStats::default(),
                name: String::from("Jolt Physics 5.6.0 (JoltC)"),
            };
            if world.ext.is_null() {
                // JoltC's private system layout changed (a JoltC update): dropping the
                // world frees what was created.
                return Err(PhysicsError::StepFailed(String::from(
                    "the JoltC physics system layout does not match csrc/ext.cpp",
                )));
            }
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
        self.shared.query_ignore.set(
            filter
                .ignore
                .map_or(JPH_INVALID_BODY_ID, |b| b.0 as JPH_BodyID),
        );
    }

    /// Heap allocations Jolt has made so far, in every world and thread (Jolt allocates
    /// through its own hooks, which Rust's global allocator does not see). For tests and
    /// diagnostics: the difference over a call is what the call allocated.
    pub fn jolt_allocations() -> u64 {
        init_jolt();
        // SAFETY: reads an atomic counter.
        unsafe { engine_jolt_allocations() }
    }

    /// A shape that has not been destroyed.
    fn live_shape(&self, shape: ShapeId) -> Option<*mut JPH_Shape> {
        let ptr = *self.shapes.get(shape.0 as usize)?;
        (!ptr.is_null()).then_some(ptr)
    }

    /// The shape of a cast, if it can be swept.
    fn cast_shape_ptr(&self, shape: ShapeId) -> Option<*mut JPH_Shape> {
        let index = shape.0 as usize;
        let ptr = *self.shapes.get(index)?;
        (!ptr.is_null() && self.shape_castable.get(index).copied().unwrap_or(false)).then_some(ptr)
    }

    fn user_data(&self, body: JPH_BodyID) -> u64 {
        // SAFETY: a body id reported by a query of this world.
        unsafe { JPH_BodyInterface_GetUserData(self.body_interface, body) }
    }

    /// What `body` is made of where a query hit it ([`PhysicsBackend::surface_of`]).
    fn surface(&self, body: JPH_BodyID, sub_shape: JPH_SubShapeID) -> Option<u16> {
        let mut data = 0;
        let mut lock = JPH_BodyLockRead {
            lockInterface: null(),
            mutex: core::ptr::null_mut(),
            body: null(),
        };
        // SAFETY: lock, read the leaf shape's user data, unlock.
        unsafe {
            JPH_BodyLockInterface_LockRead(self.lock_interface, body, &mut lock);
            if !lock.body.is_null() {
                data = engine_jolt_hit_surface(lock.body, sub_shape);
            }
            JPH_BodyLockInterface_UnlockRead(self.lock_interface, &mut lock);
        }
        data.checked_sub(1).and_then(|s| u16::try_from(s).ok())
    }

    /// World-space surface normal of `body` at `point`.
    fn surface_normal(&self, body: JPH_BodyID, sub_shape: JPH_SubShapeID, point: Vec3) -> Vec3 {
        let mut normal = JPH_Vec3::default();
        let mut lock = JPH_BodyLockRead {
            lockInterface: null(),
            mutex: core::ptr::null_mut(),
            body: null(),
        };
        // SAFETY: lock, read the surface normal, unlock.
        unsafe {
            JPH_BodyLockInterface_LockRead(self.lock_interface, body, &mut lock);
            if !lock.body.is_null() {
                JPH_Body_GetWorldSpaceSurfaceNormal(lock.body, sub_shape, &v3(point), &mut normal);
            }
            JPH_BodyLockInterface_UnlockRead(self.lock_interface, &mut lock);
        }
        from_v3(normal)
    }

    fn shape_hit(&self, hit: &JPH_ShapeCastResult) -> ShapeHit {
        let axis = from_v3(hit.penetrationAxis);
        ShapeHit {
            body: BodyId(u64::from(hit.bodyID2)),
            fraction: hit.fraction,
            point: from_v3(hit.contactPointOn2),
            // The axis points from the cast shape into the body hit.
            normal: -axis.normalize_or_zero(),
            penetration: if hit.fraction > 0.0 {
                0.0
            } else {
                hit.penetrationDepth.max(0.0)
            },
            user_data: self.user_data(hit.bodyID2),
            part: hit.subShapeID2,
        }
    }

    /// Runs a shape cast with `callback` collecting into `context`.
    fn run_shape_cast(
        &self,
        shape: *mut JPH_Shape,
        cast: &ShapeCast,
        filter: QueryFilter,
        callback: JPH_CastShapeCollectorCallback,
        context: *mut c_void,
    ) {
        self.set_query_filter(filter);
        let mut settings = MaybeUninit::<JPH_ShapeCastSettings>::zeroed();
        let transform = mat4(cast.position, cast.rotation);
        let mut base_offset = JPH_Vec3::default();
        // SAFETY: settings initialized by JoltC from Jolt's defaults; the callback writes
        // only to `context`, which the caller keeps alive for the call.
        unsafe {
            JPH_ShapeCastSettings_Init(settings.as_mut_ptr());
            let mut settings = settings.assume_init();
            // Starting inside a body: report how deep (costs time only in that case).
            settings.returnDeepestPoint = true;
            JPH_NarrowPhaseQuery_CastShape(
                self.query,
                shape,
                &transform,
                &v3(cast.direction),
                &settings,
                &mut base_offset,
                callback,
                context,
                null(),
                self.layer_filter,
                self.body_filter,
                null(),
            );
        }
    }

    fn character_handle(&self, character: CharacterId) -> Option<*mut EngineJoltCharacter> {
        self.characters.get(&character.0).map(|c| c.handle)
    }

    fn rebuild_character_handles(&mut self) {
        self.character_handles.clear();
        self.character_handles
            .extend(self.characters.values().map(|c| c.handle));
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
        self.gravity = gravity;
        let g = v3(gravity);
        // SAFETY: valid system.
        unsafe { JPH_PhysicsSystem_SetGravity(self.system, &g) };
    }

    fn gravity(&self) -> Vec3 {
        self.gravity
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
                ShapeDesc::ConvexHull {
                    points,
                    convex_radius,
                } => {
                    let points: Vec<JPH_Vec3> = points.iter().copied().map(v3).collect();
                    let settings = JPH_ConvexHullShapeSettings_Create(
                        points.as_ptr(),
                        points.len() as u32,
                        convex_radius.max(0.0),
                    );
                    let shape = JPH_ConvexHullShapeSettings_CreateShape(settings);
                    JPH_ShapeSettings_Destroy(settings);
                    shape
                }
                ShapeDesc::TriangleMesh {
                    vertices,
                    triangles,
                    surfaces,
                } => {
                    let vertices: Vec<JPH_Vec3> = vertices.iter().copied().map(v3).collect();
                    // A triangle's surface + 1 as its user data (0: the shape's own).
                    let triangles: Vec<JPH_IndexedTriangle> = triangles
                        .iter()
                        .enumerate()
                        .map(|(k, t)| JPH_IndexedTriangle {
                            i1: t[0],
                            i2: t[1],
                            i3: t[2],
                            materialIndex: 0,
                            userData: surfaces
                                .get(k)
                                .filter(|&&s| s != u16::MAX)
                                .map_or(0, |&s| u32::from(s) + 1),
                        })
                        .collect();
                    let settings = JPH_MeshShapeSettings_Create2(
                        vertices.as_ptr(),
                        vertices.len() as u32,
                        triangles.as_ptr(),
                        triangles.len() as u32,
                    );
                    if !surfaces.is_empty() {
                        JPH_MeshShapeSettings_SetPerTriangleUserData(settings, true);
                    }
                    let shape = JPH_MeshShapeSettings_CreateShape(settings);
                    JPH_ShapeSettings_Destroy(settings);
                    shape
                }
                ShapeDesc::Compound { children } => {
                    let settings = JPH_StaticCompoundShapeSettings_Create();
                    for child in children {
                        let Some(shape) = self.live_shape(child.shape) else {
                            JPH_ShapeSettings_Destroy(settings);
                            return Err(PhysicsError::InvalidHandle);
                        };
                        JPH_CompoundShapeSettings_AddShape2(
                            settings,
                            &v3(child.position),
                            &quat(child.rotation.normalize()),
                            shape,
                            0,
                        );
                    }
                    let shape = JPH_StaticCompoundShape_Create(settings);
                    JPH_ShapeSettings_Destroy(settings);
                    shape
                }
                ShapeDesc::Scaled { shape, scale } => {
                    let Some(inner) = self.live_shape(shape) else {
                        return Err(PhysicsError::InvalidHandle);
                    };
                    if !JPH_Shape_IsValidScale(inner, &v3(scale)) {
                        return Err(PhysicsError::InvalidShape(format!(
                            "scale {scale} does not suit this shape (rebuild it for the scale)"
                        )));
                    }
                    JPH_ScaledShape_Create(inner, &v3(scale))
                }
            }
        };
        if shape.is_null() {
            return Err(PhysicsError::InvalidShape(format!("{desc:?}")));
        }
        let castable = match *desc {
            ShapeDesc::TriangleMesh { .. } => false,
            ShapeDesc::Compound { children } => children.iter().all(|c| {
                self.shape_castable
                    .get(c.shape.0 as usize)
                    .copied()
                    .unwrap_or(false)
            }),
            ShapeDesc::Scaled { shape, .. } => self
                .shape_castable
                .get(shape.0 as usize)
                .copied()
                .unwrap_or(false),
            _ => true,
        };
        self.shape_castable.push(castable);
        self.shapes.push(shape);
        Ok(ShapeId((self.shapes.len() - 1) as u64))
    }

    fn is_valid_scale(&self, shape: ShapeId, scale: Vec3) -> bool {
        // SAFETY: a live shape (checked) and a vector on the stack.
        self.live_shape(shape)
            .is_some_and(|s| unsafe { JPH_Shape_IsValidScale(s, &v3(scale)) })
    }

    fn baked_shape_format(&self) -> Option<u64> {
        // SAFETY: returns a constant.
        Some(unsafe { engine_jolt_shape_format() })
    }

    fn bake_shape(&self, shape: ShapeId) -> Option<Vec<u8>> {
        let ptr = self.live_shape(shape)?;
        // SAFETY: a live shape; the first call only sizes the buffer.
        let size = unsafe { engine_jolt_shape_save(ptr, core::ptr::null_mut(), 0) };
        let mut bytes = vec![0u8; size];
        // SAFETY: as above, into a buffer of the size it reported.
        let written = unsafe { engine_jolt_shape_save(ptr, bytes.as_mut_ptr(), size) };
        (written == size).then_some(bytes)
    }

    fn create_baked_shape(
        &mut self,
        bytes: &[u8],
        castable: bool,
    ) -> Result<ShapeId, PhysicsError> {
        // SAFETY: reads `bytes` only (trusted, see the trait); null when they are not a
        // whole shape.
        let shape = unsafe { engine_jolt_shape_restore(bytes.as_ptr(), bytes.len()) };
        if shape.is_null() {
            return Err(PhysicsError::InvalidShape("not a baked Jolt shape".into()));
        }
        self.shape_castable.push(castable);
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

    fn surface_of(&self, body: BodyId, part: u32) -> Option<u16> {
        self.surface(body.0 as JPH_BodyID, part)
    }

    fn set_shape_surface(&mut self, shape: ShapeId, surface: u16) {
        if let Some(&ptr) = self.shapes.get(shape.0 as usize)
            && !ptr.is_null()
        {
            // SAFETY: a live shape of ours. Its user data is the surface + 1 (0: none), what
            // a query's leaf shape reports (`engine_jolt_hit_surface`); it survives baking.
            unsafe { JPH_Shape_SetUserData(ptr, u64::from(surface) + 1) };
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
            JPH_BodyCreationSettings_SetEnhancedInternalEdgeRemoval(settings, desc.internal_edges);
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

    fn user_data(&self, body: BodyId) -> u64 {
        let raw = body.0 as JPH_BodyID;
        if !self.bodies.contains_key(&raw) {
            return 0;
        }
        // SAFETY: checked live body; read outside a step on the owning thread.
        unsafe { JPH_BodyInterface_GetUserData(self.body_interface, raw) }
    }

    fn set_user_data(&mut self, body: BodyId, data: u64) {
        let raw = body.0 as JPH_BodyID;
        if self.bodies.contains_key(&raw) {
            // SAFETY: checked live body; written outside a step on the owning thread.
            unsafe { JPH_BodyInterface_SetUserData(self.body_interface, raw, data) };
        }
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

    fn optimize_broad_phase(&mut self) {
        engine_profiler::zone!("physics::jolt::optimize_broad_phase");
        // SAFETY: valid system, no step running (&mut self).
        unsafe { JPH_PhysicsSystem_OptimizeBroadPhase(self.system) };
    }

    fn step(&mut self, dt: f32, substeps: u32) -> Result<(), PhysicsError> {
        engine_profiler::zone!("physics::jolt::step");
        if self.gravity_dirty {
            self.rebuild_gravity_list();
        }
        self.shared.event_count.store(0, Ordering::Relaxed);
        // SAFETY: valid system.
        let awake =
            unsafe { JPH_PhysicsSystem_GetNumActiveBodies(self.system, JPH_BODY_TYPE_RIGID) };
        let use_pool = self.threads > 0 && awake >= self.single_thread_below;
        if use_pool && self.pool_jobs.is_null() && self.engine_jobs.is_none() {
            engine_profiler::zone!("physics::jolt::start_workers");
            // SAFETY: plain creation; destroyed in `drop`.
            self.pool_jobs = unsafe {
                JPH_JobSystemThreadPool_Create(&JobSystemThreadPoolConfig {
                    maxJobs: 2048,
                    maxBarriers: 8,
                    numThreads: self.threads as i32,
                })
            };
        }
        let (jobs, step_threads) = match (&self.engine_jobs, use_pool) {
            (Some((foreign, jobs)), true) => (*jobs, foreign.pool().num_workers() as u32 + 1),
            (None, true) if !self.pool_jobs.is_null() => (self.pool_jobs, self.threads + 1),
            _ => (self.inline_jobs, 1),
        };
        let error = {
            let _serialized = jolt_lock();
            // SAFETY: the system and job system are valid; callbacks reach `shared`, which
            // outlives the call; `STEP_LOCK` keeps JoltC's shared temporary allocator to one
            // step at a time.
            unsafe { JPH_PhysicsSystem_Update(self.system, dt, substeps.max(1) as i32, jobs) }
        };
        if let Some((foreign, _)) = &self.engine_jobs {
            // Jolt's barrier saw every job run; the pool may still hold the queued copies of
            // those the waiting thread ran itself (they only release their job).
            foreign.wait();
        }
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
            step_threads,
        };
        // The ground under the characters moved: refresh its velocity (moving
        // platforms), in id order.
        for &handle in &self.character_handles {
            // SAFETY: a live character of this world.
            unsafe { engine_jolt_character_update_ground_velocity(handle) };
        }
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
        Some(RayHit {
            body: BodyId(u64::from(hit.bodyID)),
            fraction: hit.fraction,
            point,
            normal: self.surface_normal(hit.bodyID, hit.subShapeID2, point),
            user_data: self.user_data(hit.bodyID),
            part: hit.subShapeID2,
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

    fn raycast_all(&self, ray: &Ray, filter: QueryFilter, out: &mut Vec<RayHit>) {
        engine_profiler::zone!("physics::jolt::raycast_all");
        self.set_query_filter(filter);
        let mut raw = self.ray_scratch.borrow_mut();
        raw.clear();
        // SAFETY: valid query and filters; the callback appends to `raw`, alive for the
        // call. No ray-cast settings: Jolt's defaults (convex shapes are solid).
        unsafe {
            JPH_NarrowPhaseQuery_CastRay2(
                self.query,
                &v3(ray.origin),
                &v3(ray.direction),
                null(),
                collect_ray,
                (&raw mut *raw).cast(),
                null(),
                self.layer_filter,
                self.body_filter,
                null(),
            );
        }
        // One hit per body (a mesh can report several faces), the nearest; then in ray
        // order. Jolt's narrow phase reports hits in no guaranteed order.
        raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        raw.dedup_by_key(|h| h.0);
        raw.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        for &(body, fraction, sub_shape) in raw.iter() {
            let point = ray.origin + ray.direction * fraction;
            out.push(RayHit {
                body: BodyId(u64::from(body)),
                fraction,
                point,
                normal: self.surface_normal(body, sub_shape, point),
                user_data: self.user_data(body),
                part: sub_shape,
            });
        }
    }

    fn cast_shape(&self, cast: &ShapeCast, filter: QueryFilter) -> Option<ShapeHit> {
        engine_profiler::zone!("physics::jolt::cast_shape");
        let shape = self.cast_shape_ptr(cast.shape)?;
        let mut state = ClosestShape { best: None };
        self.run_shape_cast(
            shape,
            cast,
            filter,
            collect_closest_shape,
            (&raw mut state).cast(),
        );
        state.best.map(|hit| self.shape_hit(&hit))
    }

    fn cast_shape_all(&self, cast: &ShapeCast, filter: QueryFilter, out: &mut Vec<ShapeHit>) {
        engine_profiler::zone!("physics::jolt::cast_shape_all");
        let Some(shape) = self.cast_shape_ptr(cast.shape) else {
            return;
        };
        let mut raw = self.shape_scratch.borrow_mut();
        raw.clear();
        self.run_shape_cast(shape, cast, filter, collect_shape, (&raw mut *raw).cast());
        // The first hit per body, then in sweep order.
        raw.sort_unstable_by(|a, b| {
            a.bodyID2
                .cmp(&b.bodyID2)
                .then(early_out(a).total_cmp(&early_out(b)))
        });
        raw.dedup_by_key(|h| h.bodyID2);
        raw.sort_unstable_by(|a, b| {
            early_out(a)
                .total_cmp(&early_out(b))
                .then(a.bodyID2.cmp(&b.bodyID2))
        });
        out.extend(raw.iter().map(|hit| self.shape_hit(hit)));
    }

    fn overlap_shape(
        &self,
        shape: ShapeId,
        position: Vec3,
        rotation: Quat,
        filter: QueryFilter,
        out: &mut Vec<BodyId>,
    ) {
        engine_profiler::zone!("physics::jolt::overlap_shape");
        let Some(shape) = self.cast_shape_ptr(shape) else {
            return;
        };
        self.set_query_filter(filter);
        let start = out.len();
        let mut com = JPH_Vec3::default();
        let mut settings = MaybeUninit::<JPH_CollideShapeSettings>::zeroed();
        let mut base_offset = JPH_Vec3::default();
        // SAFETY: a live shape; settings initialized by Jolt; the callback appends to
        // `out`, which lives for the call.
        unsafe {
            JPH_Shape_GetCenterOfMass(shape, &mut com);
            // Jolt places the shape by its center of mass.
            let transform = mat4(position + rotation * from_v3(com), rotation);
            JPH_CollideShapeSettings_Init(settings.as_mut_ptr());
            JPH_NarrowPhaseQuery_CollideShape(
                self.query,
                shape,
                &v3(Vec3::ONE),
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

    fn move_kinematic(&mut self, body: BodyId, position: Vec3, rotation: Quat, dt: f32) {
        let raw = body.0 as JPH_BodyID;
        if self.bodies.contains_key(&raw) && dt > 0.0 {
            let (mut p, mut r) = (v3(position), quat(rotation.normalize()));
            // SAFETY: a live body. Jolt ignores the call for non-kinematic bodies.
            unsafe {
                JPH_BodyInterface_MoveKinematic(self.body_interface, raw, &mut p, &mut r, dt)
            };
        }
    }

    fn create_character(&mut self, desc: &CharacterDesc) -> Result<CharacterId, PhysicsError> {
        let valid = desc.radius > 0.0
            && desc.radius.is_finite()
            && desc.height.is_finite()
            && desc.height >= 2.0 * desc.radius;
        if !valid {
            return Err(PhysicsError::InvalidShape(format!(
                "character capsule {} m tall, radius {} m",
                desc.height, desc.radius
            )));
        }
        let id = self.next_character;
        if id == ENGINE_JOLT_NONE {
            return Err(PhysicsError::Full("characters"));
        }
        let raw = EngineJoltCharacterDesc {
            position: desc.position.to_array(),
            rotation: desc.rotation.to_array(),
            up: desc.up.to_array(),
            half_height: desc.height * 0.5 - desc.radius,
            radius: desc.radius,
            max_slope: desc.max_slope,
            step_height: desc.step_height.max(0.0),
            snap_to_ground: desc.snap_to_ground.max(0.0),
            mass: desc.mass,
            max_push_force: desc.max_push_force.max(0.0),
            id,
            object_layer: u32::from(desc.layer.0) * 2 + 1,
            proxy_layer: desc
                .proxy_layer
                .map_or(ENGINE_JOLT_NONE, |l| u32::from(l.0) * 2 + 1),
            _padding: 0,
            user_data: desc.user_data,
        };
        // SAFETY: the extension world is alive; `raw` is a valid description.
        let handle = unsafe { engine_jolt_character_create(self.ext, &raw) };
        if handle.is_null() {
            return Err(PhysicsError::Full("characters"));
        }
        self.next_character += 1;
        self.characters
            .insert(u64::from(id), CharacterInfo { handle });
        self.rebuild_character_handles();
        Ok(CharacterId(u64::from(id)))
    }

    fn destroy_character(&mut self, character: CharacterId) {
        if let Some(info) = self.characters.remove(&character.0) {
            // SAFETY: a live character of this world, destroyed once.
            unsafe { engine_jolt_character_destroy(self.ext, info.handle) };
            self.rebuild_character_handles();
        }
    }

    fn move_character(
        &mut self,
        character: CharacterId,
        velocity: Vec3,
        dt: f32,
    ) -> Option<CharacterState> {
        engine_profiler::zone!("physics::jolt::move_character");
        let handle = self.character_handle(character)?;
        let mut raw = EngineJoltCharacterState::default();
        let velocity = velocity.to_array();
        // SAFETY: a live character of this world; outside a step (`&mut self`).
        unsafe {
            engine_jolt_character_move(self.ext, handle, velocity.as_ptr(), dt.max(0.0), &mut raw)
        };
        Some(character_state(&raw))
    }

    fn character(&self, character: CharacterId) -> Option<CharacterState> {
        let handle = self.character_handle(character)?;
        let mut raw = EngineJoltCharacterState::default();
        // SAFETY: a live character of this world.
        unsafe { engine_jolt_character_state(handle, &mut raw) };
        Some(character_state(&raw))
    }

    fn set_character_transform(&mut self, character: CharacterId, position: Vec3, rotation: Quat) {
        if let Some(handle) = self.character_handle(character) {
            let (p, r) = (position.to_array(), rotation.to_array());
            // SAFETY: a live character of this world.
            unsafe {
                engine_jolt_character_set_transform(self.ext, handle, p.as_ptr(), r.as_ptr())
            };
        }
    }

    fn character_contacts(&self, character: CharacterId, out: &mut Vec<CharacterContact>) {
        let Some(handle) = self.character_handle(character) else {
            return;
        };
        let mut scratch = self.contact_scratch.borrow_mut();
        loop {
            let capacity = scratch.len();
            // SAFETY: a live character; `scratch` holds `capacity` contacts.
            let count = unsafe {
                engine_jolt_character_contacts(handle, scratch.as_mut_ptr(), capacity as u32)
            } as usize;
            if count <= capacity {
                out.extend(scratch[..count].iter().map(|c| {
                    CharacterContact {
                        body: body_or_none(c.body),
                        character: (c.character != ENGINE_JOLT_NONE)
                            .then_some(CharacterId(u64::from(c.character))),
                        point: arr3(c.point),
                        normal: arr3(c.normal),
                        collided: c.collided != 0,
                        sensor: c.sensor != 0,
                    }
                }));
                return;
            }
            scratch.resize(
                count.next_power_of_two(),
                EngineJoltCharacterContact::default(),
            );
        }
    }

    fn save_state(&self, out: &mut PhysicsSnapshot) -> Result<(), PhysicsError> {
        engine_profiler::zone!("physics::jolt::save_state");
        let live_constraints = self.constraints.iter().filter(|c| !c.is_null()).count();
        let buffer = out.buffer_mut();
        buffer.clear();
        for word in [
            SNAPSHOT_MAGIC,
            self.bodies.len() as u32,
            live_constraints as u32,
            self.characters.len() as u32,
        ] {
            buffer.extend_from_slice(&word.to_le_bytes());
        }
        loop {
            let capacity = buffer.capacity() - SNAPSHOT_HEADER;
            // SAFETY: writes at most `capacity` bytes into the buffer's spare capacity after
            // the header (and reports the full size); the characters are live.
            let size = unsafe {
                engine_jolt_save_state(
                    self.ext,
                    self.character_handles.as_ptr(),
                    self.character_handles.len() as u32,
                    buffer.as_mut_ptr().add(SNAPSHOT_HEADER),
                    capacity,
                )
            };
            if size <= capacity {
                // SAFETY: the extension wrote `size` bytes after the header.
                unsafe { buffer.set_len(SNAPSHOT_HEADER + size) };
                return Ok(());
            }
            // Grow (with headroom for a growing contact count) and save again.
            buffer.reserve(size + size / 8);
        }
    }

    fn restore_state(&mut self, snapshot: &PhysicsSnapshot) -> Result<(), PhysicsError> {
        engine_profiler::zone!("physics::jolt::restore_state");
        let bytes = snapshot.bytes();
        let word = |i: usize| {
            bytes
                .get(i * 4..i * 4 + 4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        };
        if word(0) != Some(SNAPSHOT_MAGIC) {
            return Err(PhysicsError::SnapshotMismatch(
                "not a Jolt backend snapshot",
            ));
        }
        let live_constraints = self.constraints.iter().filter(|c| !c.is_null()).count();
        if word(1) != Some(self.bodies.len() as u32)
            || word(2) != Some(live_constraints as u32)
            || word(3) != Some(self.characters.len() as u32)
        {
            return Err(PhysicsError::SnapshotMismatch(
                "other numbers of bodies, constraints or characters",
            ));
        }
        let state = &bytes[SNAPSHOT_HEADER..];
        // SAFETY: the characters are live and in the order they were saved in (id order;
        // the count matches); `state` is valid for its length.
        let ok = unsafe {
            engine_jolt_restore_state(
                self.ext,
                self.character_handles.as_ptr(),
                self.character_handles.len() as u32,
                state.as_ptr(),
                state.len(),
            )
        };
        self.events.clear();
        if ok {
            Ok(())
        } else {
            Err(PhysicsError::SnapshotMismatch(
                "Jolt could not restore it (other bodies, or damaged bytes)",
            ))
        }
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
        for (&id, info) in &self.characters {
            let mut raw = EngineJoltCharacterState::default();
            // SAFETY: a live character of this world.
            unsafe { engine_jolt_character_state(info.handle, &mut raw) };
            hash_words(
                &mut hash,
                [id as u32]
                    .into_iter()
                    .chain(raw.position.iter().map(|f| f.to_bits()))
                    .chain(raw.rotation.iter().map(|f| f.to_bits()))
                    .chain(raw.velocity.iter().map(|f| f.to_bits())),
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
            for info in self.characters.values() {
                engine_jolt_character_destroy(self.ext, info.handle);
            }
            for &constraint in &self.constraints {
                if !constraint.is_null() {
                    JPH_PhysicsSystem_RemoveConstraint(self.system, constraint);
                    JPH_Constraint_Destroy(constraint);
                }
            }
            for &id in self.bodies.keys() {
                JPH_BodyInterface_RemoveAndDestroyBody(self.body_interface, id);
            }
            {
                let _serialized = jolt_lock();
                JPH_PhysicsSystem_Destroy(self.system);
            }
            if !self.ext.is_null() {
                engine_jolt_world_destroy(self.ext);
            }
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
            JPH_JobSystem_Destroy(self.inline_jobs);
            if !self.pool_jobs.is_null() {
                JPH_JobSystem_Destroy(self.pool_jobs);
            }
            if let Some((foreign, jobs)) = self.engine_jobs.take() {
                // Its jobs release into the job system's records: they finish first.
                foreign.wait();
                JPH_JobSystem_Destroy(jobs);
            }
            // The layer tables and filters belong to the physics system from creation on:
            // JoltC's `JPH_PhysicsSystem_Destroy` deleted them.
        }
    }
}
