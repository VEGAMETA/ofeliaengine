//! Hand-written bindings to the part of the JoltC API the backend uses (third_party/joltc,
//! `joltc.h`). Struct layouts are checked against the C compiler's by `tests/layout.rs`
//! through `csrc/layout.cpp`. Single precision (`JPH_RVec3` = `JPH_Vec3`).

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use core::ffi::c_void;

macro_rules! opaque {
    ($($name:ident),* $(,)?) => {
        $(
            #[repr(C)]
            pub(crate) struct $name {
                _private: [u8; 0],
                _not_send: core::marker::PhantomData<*mut u8>,
            }
        )*
    };
}

opaque!(
    JPH_JobSystem,
    JPH_PhysicsSystem,
    JPH_BroadPhaseLayerInterface,
    JPH_ObjectLayerPairFilter,
    JPH_ObjectVsBroadPhaseLayerFilter,
    JPH_BodyInterface,
    JPH_BodyLockInterface,
    JPH_NarrowPhaseQuery,
    JPH_BroadPhaseLayerFilter,
    JPH_ObjectLayerFilter,
    JPH_BodyFilter,
    JPH_ShapeFilter,
    JPH_Shape,
    JPH_ShapeSettings,
    JPH_BodyCreationSettings,
    JPH_Body,
    JPH_Constraint,
    JPH_ContactListener,
    JPH_ContactManifold,
    JPH_ContactSettings,
    JPH_PhysicsStepListener,
    JPH_SharedMutex,
);

pub(crate) type JPH_BodyID = u32;
pub(crate) type JPH_SubShapeID = u32;
pub(crate) type JPH_ObjectLayer = u32;
pub(crate) type JPH_BroadPhaseLayer = u8;
pub(crate) type JPH_Bool = u32;

pub(crate) const JPH_INVALID_BODY_ID: JPH_BodyID = 0xFFFF_FFFF;

// Enums are C enums forced to 32 bits (`_Force32 = 0x7fffffff`).
pub(crate) const JPH_MOTION_TYPE_STATIC: u32 = 0;
pub(crate) const JPH_MOTION_TYPE_KINEMATIC: u32 = 1;
pub(crate) const JPH_MOTION_TYPE_DYNAMIC: u32 = 2;
pub(crate) const JPH_ACTIVATION_ACTIVATE: u32 = 0;
pub(crate) const JPH_ACTIVATION_DONT_ACTIVATE: u32 = 1;
pub(crate) const JPH_MOTION_QUALITY_DISCRETE: u32 = 0;
pub(crate) const JPH_MOTION_QUALITY_LINEAR_CAST: u32 = 1;
pub(crate) const JPH_OVERRIDE_MASS_CALCULATE_INERTIA: u32 = 1;
pub(crate) const JPH_CONSTRAINT_SPACE_WORLD: u32 = 1;
pub(crate) const JPH_BODY_TYPE_RIGID: u32 = 0;
pub(crate) const JPH_VALIDATE_ACCEPT_ALL: u32 = 0;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JPH_Vec3 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JPH_Vec4 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
    pub(crate) w: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JPH_Quat {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
    pub(crate) w: f32,
}

/// `JPH_RMat4` in single precision: `JPH_Mat4`, four columns.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JPH_RMat4 {
    pub(crate) column: [JPH_Vec4; 4],
}

pub(crate) type JPH_RVec3 = JPH_Vec3;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JPH_RayCastResult {
    pub(crate) bodyID: JPH_BodyID,
    pub(crate) fraction: f32,
    pub(crate) subShapeID2: JPH_SubShapeID,
}

#[repr(C)]
pub(crate) struct JPH_PhysicsSystemSettings {
    pub(crate) maxBodies: u32,
    pub(crate) numBodyMutexes: u32,
    pub(crate) maxBodyPairs: u32,
    pub(crate) maxContactConstraints: u32,
    pub(crate) _padding: u32,
    pub(crate) broadPhaseLayerInterface: *mut JPH_BroadPhaseLayerInterface,
    pub(crate) objectLayerPairFilter: *mut JPH_ObjectLayerPairFilter,
    pub(crate) objectVsBroadPhaseLayerFilter: *mut JPH_ObjectVsBroadPhaseLayerFilter,
}

#[repr(C)]
pub(crate) struct JPH_PhysicsStepListenerContext {
    pub(crate) deltaTime: f32,
    pub(crate) isFirstStep: JPH_Bool,
    pub(crate) isLastStep: JPH_Bool,
    pub(crate) physicsSystem: *mut JPH_PhysicsSystem,
}

#[repr(C)]
pub(crate) struct JPH_PhysicsStepListener_Procs {
    pub(crate) OnStep: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            context: *const JPH_PhysicsStepListenerContext,
        ),
    >,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct JPH_SubShapeIDPair {
    pub(crate) Body1ID: JPH_BodyID,
    pub(crate) subShapeID1: JPH_SubShapeID,
    pub(crate) Body2ID: JPH_BodyID,
    pub(crate) subShapeID2: JPH_SubShapeID,
}

#[repr(C)]
pub(crate) struct JPH_ContactListener_Procs {
    pub(crate) OnContactValidate: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            body1: *const JPH_Body,
            body2: *const JPH_Body,
            base_offset: *const JPH_RVec3,
            result: *const JPH_CollideShapeResult,
        ) -> u32,
    >,
    pub(crate) OnContactAdded: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            body1: *const JPH_Body,
            body2: *const JPH_Body,
            manifold: *const JPH_ContactManifold,
            settings: *mut JPH_ContactSettings,
        ),
    >,
    pub(crate) OnContactPersisted: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            body1: *const JPH_Body,
            body2: *const JPH_Body,
            manifold: *const JPH_ContactManifold,
            settings: *mut JPH_ContactSettings,
        ),
    >,
    pub(crate) OnContactRemoved:
        Option<unsafe extern "C" fn(user_data: *mut c_void, pair: *const JPH_SubShapeIDPair)>,
}

#[repr(C)]
pub(crate) struct JPH_ObjectLayerFilter_Procs {
    pub(crate) ShouldCollide:
        Option<unsafe extern "C" fn(user_data: *mut c_void, layer: JPH_ObjectLayer) -> bool>,
}

#[repr(C)]
pub(crate) struct JPH_BodyFilter_Procs {
    pub(crate) ShouldCollide:
        Option<unsafe extern "C" fn(user_data: *mut c_void, body: JPH_BodyID) -> bool>,
    pub(crate) ShouldCollideLocked:
        Option<unsafe extern "C" fn(user_data: *mut c_void, body: *const JPH_Body) -> bool>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JPH_IndexedTriangle {
    pub(crate) i1: u32,
    pub(crate) i2: u32,
    pub(crate) i3: u32,
    pub(crate) materialIndex: u32,
    pub(crate) userData: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_CollideSettingsBase {
    pub(crate) activeEdgeMode: u32,
    pub(crate) collectFacesMode: u32,
    pub(crate) collisionTolerance: f32,
    pub(crate) penetrationTolerance: f32,
    pub(crate) activeEdgeMovementDirection: JPH_Vec3,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_CollideShapeSettings {
    pub(crate) base: JPH_CollideSettingsBase,
    pub(crate) maxSeparationDistance: f32,
    pub(crate) backFaceMode: u32,
}

#[repr(C)]
pub(crate) struct JPH_CollideShapeResult {
    pub(crate) contactPointOn1: JPH_Vec3,
    pub(crate) contactPointOn2: JPH_Vec3,
    pub(crate) penetrationAxis: JPH_Vec3,
    pub(crate) penetrationDepth: f32,
    pub(crate) subShapeID1: JPH_SubShapeID,
    pub(crate) subShapeID2: JPH_SubShapeID,
    pub(crate) bodyID2: JPH_BodyID,
    pub(crate) shape1FaceCount: u32,
    pub(crate) shape1Faces: *mut JPH_Vec3,
    pub(crate) shape2FaceCount: u32,
    pub(crate) shape2Faces: *mut JPH_Vec3,
}

pub(crate) type JPH_CollideShapeCollectorCallback =
    unsafe extern "C" fn(context: *mut c_void, result: *const JPH_CollideShapeResult) -> f32;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_ConstraintSettings {
    pub(crate) enabled: bool,
    pub(crate) constraintPriority: u32,
    pub(crate) numVelocityStepsOverride: u32,
    pub(crate) numPositionStepsOverride: u32,
    pub(crate) drawConstraintSize: f32,
    pub(crate) userData: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_SpringSettings {
    pub(crate) mode: u32,
    pub(crate) frequencyOrStiffness: f32,
    pub(crate) damping: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_MotorSettings {
    pub(crate) springSettings: JPH_SpringSettings,
    pub(crate) minForceLimit: f32,
    pub(crate) maxForceLimit: f32,
    pub(crate) minTorqueLimit: f32,
    pub(crate) maxTorqueLimit: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_FixedConstraintSettings {
    pub(crate) base: JPH_ConstraintSettings,
    pub(crate) space: u32,
    pub(crate) autoDetectPoint: bool,
    pub(crate) point1: JPH_RVec3,
    pub(crate) axisX1: JPH_Vec3,
    pub(crate) axisY1: JPH_Vec3,
    pub(crate) point2: JPH_RVec3,
    pub(crate) axisX2: JPH_Vec3,
    pub(crate) axisY2: JPH_Vec3,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_DistanceConstraintSettings {
    pub(crate) base: JPH_ConstraintSettings,
    pub(crate) space: u32,
    pub(crate) point1: JPH_RVec3,
    pub(crate) point2: JPH_RVec3,
    pub(crate) minDistance: f32,
    pub(crate) maxDistance: f32,
    pub(crate) limitsSpringSettings: JPH_SpringSettings,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_PointConstraintSettings {
    pub(crate) base: JPH_ConstraintSettings,
    pub(crate) space: u32,
    pub(crate) point1: JPH_RVec3,
    pub(crate) point2: JPH_RVec3,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_HingeConstraintSettings {
    pub(crate) base: JPH_ConstraintSettings,
    pub(crate) space: u32,
    pub(crate) point1: JPH_RVec3,
    pub(crate) hingeAxis1: JPH_Vec3,
    pub(crate) normalAxis1: JPH_Vec3,
    pub(crate) point2: JPH_RVec3,
    pub(crate) hingeAxis2: JPH_Vec3,
    pub(crate) normalAxis2: JPH_Vec3,
    pub(crate) limitsMin: f32,
    pub(crate) limitsMax: f32,
    pub(crate) limitsSpringSettings: JPH_SpringSettings,
    pub(crate) maxFrictionTorque: f32,
    pub(crate) motorSettings: JPH_MotorSettings,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JPH_SliderConstraintSettings {
    pub(crate) base: JPH_ConstraintSettings,
    pub(crate) space: u32,
    pub(crate) autoDetectPoint: bool,
    pub(crate) point1: JPH_RVec3,
    pub(crate) sliderAxis1: JPH_Vec3,
    pub(crate) normalAxis1: JPH_Vec3,
    pub(crate) point2: JPH_RVec3,
    pub(crate) sliderAxis2: JPH_Vec3,
    pub(crate) normalAxis2: JPH_Vec3,
    pub(crate) limitsMin: f32,
    pub(crate) limitsMax: f32,
    pub(crate) limitsSpringSettings: JPH_SpringSettings,
    pub(crate) maxFrictionForce: f32,
    pub(crate) motorSettings: JPH_MotorSettings,
}

#[repr(C)]
pub(crate) struct JPH_BodyLockRead {
    pub(crate) lockInterface: *const JPH_BodyLockInterface,
    pub(crate) mutex: *mut JPH_SharedMutex,
    pub(crate) body: *const JPH_Body,
}

#[repr(C)]
pub(crate) struct JobSystemThreadPoolConfig {
    pub(crate) maxJobs: u32,
    pub(crate) maxBarriers: u32,
    pub(crate) numThreads: i32,
}

pub(crate) type JPH_JobFunction = unsafe extern "C" fn(arg: *mut c_void);
pub(crate) type JPH_QueueJobCallback =
    unsafe extern "C" fn(context: *mut c_void, job: JPH_JobFunction, arg: *mut c_void);
pub(crate) type JPH_QueueJobsCallback = unsafe extern "C" fn(
    context: *mut c_void,
    job: JPH_JobFunction,
    args: *mut *mut c_void,
    count: u32,
);

#[repr(C)]
pub(crate) struct JPH_JobSystemConfig {
    pub(crate) context: *mut c_void,
    pub(crate) queueJob: Option<JPH_QueueJobCallback>,
    pub(crate) queueJobs: Option<JPH_QueueJobsCallback>,
    pub(crate) maxConcurrency: u32,
    pub(crate) maxBarriers: u32,
}

#[repr(C)]
pub(crate) struct JPH_MassProperties {
    pub(crate) mass: f32,
    pub(crate) inertia: JPH_RMat4,
}

unsafe extern "C" {
    pub(crate) fn JPH_Init() -> bool;
    pub(crate) fn JPH_Shutdown();

    pub(crate) fn JPH_JobSystemThreadPool_Create(
        config: *const JobSystemThreadPoolConfig,
    ) -> *mut JPH_JobSystem;
    pub(crate) fn JPH_JobSystemCallback_Create(
        config: *const JPH_JobSystemConfig,
    ) -> *mut JPH_JobSystem;
    pub(crate) fn JPH_JobSystem_Destroy(job_system: *mut JPH_JobSystem);

    pub(crate) fn JPH_BroadPhaseLayerInterfaceTable_Create(
        num_object_layers: u32,
        num_broad_phase_layers: u32,
    ) -> *mut JPH_BroadPhaseLayerInterface;
    pub(crate) fn JPH_BroadPhaseLayerInterfaceTable_MapObjectToBroadPhaseLayer(
        bp_interface: *mut JPH_BroadPhaseLayerInterface,
        object_layer: JPH_ObjectLayer,
        broad_phase_layer: JPH_BroadPhaseLayer,
    );
    pub(crate) fn JPH_BroadPhaseLayerInterface_Destroy(
        bp_interface: *mut JPH_BroadPhaseLayerInterface,
    );
    pub(crate) fn JPH_ObjectLayerPairFilterTable_Create(
        num_object_layers: u32,
    ) -> *mut JPH_ObjectLayerPairFilter;
    pub(crate) fn JPH_ObjectLayerPairFilterTable_EnableCollision(
        filter: *mut JPH_ObjectLayerPairFilter,
        layer1: JPH_ObjectLayer,
        layer2: JPH_ObjectLayer,
    );
    pub(crate) fn JPH_ObjectLayerPairFilter_Destroy(filter: *mut JPH_ObjectLayerPairFilter);
    pub(crate) fn JPH_ObjectVsBroadPhaseLayerFilterTable_Create(
        bp_interface: *mut JPH_BroadPhaseLayerInterface,
        num_broad_phase_layers: u32,
        pair_filter: *mut JPH_ObjectLayerPairFilter,
        num_object_layers: u32,
    ) -> *mut JPH_ObjectVsBroadPhaseLayerFilter;
    pub(crate) fn JPH_ObjectVsBroadPhaseLayerFilter_Destroy(
        filter: *mut JPH_ObjectVsBroadPhaseLayerFilter,
    );

    pub(crate) fn JPH_PhysicsSystem_Create(
        settings: *const JPH_PhysicsSystemSettings,
    ) -> *mut JPH_PhysicsSystem;
    pub(crate) fn JPH_PhysicsSystem_Destroy(system: *mut JPH_PhysicsSystem);
    pub(crate) fn JPH_PhysicsSystem_OptimizeBroadPhase(system: *mut JPH_PhysicsSystem);
    pub(crate) fn JPH_PhysicsSystem_Update(
        system: *mut JPH_PhysicsSystem,
        delta_time: f32,
        collision_steps: i32,
        job_system: *mut JPH_JobSystem,
    ) -> u32;
    pub(crate) fn JPH_PhysicsSystem_GetBodyInterface(
        system: *mut JPH_PhysicsSystem,
    ) -> *mut JPH_BodyInterface;
    pub(crate) fn JPH_PhysicsSystem_GetBodyInterfaceNoLock(
        system: *mut JPH_PhysicsSystem,
    ) -> *mut JPH_BodyInterface;
    pub(crate) fn JPH_PhysicsSystem_GetBodyLockInterface(
        system: *const JPH_PhysicsSystem,
    ) -> *const JPH_BodyLockInterface;
    pub(crate) fn JPH_PhysicsSystem_GetNarrowPhaseQuery(
        system: *const JPH_PhysicsSystem,
    ) -> *const JPH_NarrowPhaseQuery;
    pub(crate) fn JPH_PhysicsSystem_SetContactListener(
        system: *mut JPH_PhysicsSystem,
        listener: *mut JPH_ContactListener,
    );
    pub(crate) fn JPH_PhysicsSystem_GetNumBodies(system: *const JPH_PhysicsSystem) -> u32;
    pub(crate) fn JPH_PhysicsSystem_GetNumActiveBodies(
        system: *const JPH_PhysicsSystem,
        body_type: u32,
    ) -> u32;
    pub(crate) fn JPH_PhysicsSystem_SetGravity(
        system: *mut JPH_PhysicsSystem,
        value: *const JPH_Vec3,
    );
    pub(crate) fn JPH_PhysicsSystem_AddConstraint(
        system: *mut JPH_PhysicsSystem,
        constraint: *mut JPH_Constraint,
    );
    pub(crate) fn JPH_PhysicsSystem_RemoveConstraint(
        system: *mut JPH_PhysicsSystem,
        constraint: *mut JPH_Constraint,
    );
    pub(crate) fn JPH_PhysicsSystem_AddStepListener(
        system: *mut JPH_PhysicsSystem,
        listener: *mut JPH_PhysicsStepListener,
    );

    pub(crate) fn JPH_PhysicsStepListener_SetProcs(procs: *const JPH_PhysicsStepListener_Procs);
    pub(crate) fn JPH_PhysicsStepListener_Create(
        user_data: *mut c_void,
    ) -> *mut JPH_PhysicsStepListener;
    pub(crate) fn JPH_PhysicsStepListener_Destroy(listener: *mut JPH_PhysicsStepListener);
    pub(crate) fn JPH_ContactListener_SetProcs(procs: *const JPH_ContactListener_Procs);
    pub(crate) fn JPH_ContactListener_Create(user_data: *mut c_void) -> *mut JPH_ContactListener;
    pub(crate) fn JPH_ContactListener_Destroy(listener: *mut JPH_ContactListener);
    pub(crate) fn JPH_ObjectLayerFilter_SetProcs(procs: *const JPH_ObjectLayerFilter_Procs);
    pub(crate) fn JPH_ObjectLayerFilter_Create(
        user_data: *mut c_void,
    ) -> *mut JPH_ObjectLayerFilter;
    pub(crate) fn JPH_ObjectLayerFilter_Destroy(filter: *mut JPH_ObjectLayerFilter);
    pub(crate) fn JPH_BodyFilter_SetProcs(procs: *const JPH_BodyFilter_Procs);
    pub(crate) fn JPH_BodyFilter_Create(user_data: *mut c_void) -> *mut JPH_BodyFilter;
    pub(crate) fn JPH_BodyFilter_Destroy(filter: *mut JPH_BodyFilter);

    pub(crate) fn JPH_ContactManifold_GetWorldSpaceNormal(
        manifold: *const JPH_ContactManifold,
        result: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_ContactManifold_GetPointCount(manifold: *const JPH_ContactManifold) -> u32;
    pub(crate) fn JPH_ContactManifold_GetWorldSpaceContactPointOn1(
        manifold: *const JPH_ContactManifold,
        index: u32,
        result: *mut JPH_RVec3,
    );

    pub(crate) fn JPH_Body_GetID(body: *const JPH_Body) -> JPH_BodyID;
    pub(crate) fn JPH_Body_IsSensor(body: *const JPH_Body) -> bool;
    pub(crate) fn JPH_Body_GetObjectLayer(body: *const JPH_Body) -> JPH_ObjectLayer;
    pub(crate) fn JPH_Body_GetWorldSpaceSurfaceNormal(
        body: *const JPH_Body,
        sub_shape: JPH_SubShapeID,
        position: *const JPH_RVec3,
        normal: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_Body_GetFixedToWorldBody() -> *mut JPH_Body;

    pub(crate) fn JPH_SphereShape_Create(radius: f32) -> *mut JPH_Shape;
    pub(crate) fn JPH_BoxShape_Create(
        half_extent: *const JPH_Vec3,
        convex_radius: f32,
    ) -> *mut JPH_Shape;
    pub(crate) fn JPH_CapsuleShape_Create(half_height: f32, radius: f32) -> *mut JPH_Shape;
    pub(crate) fn JPH_CylinderShape_Create(half_height: f32, radius: f32) -> *mut JPH_Shape;
    pub(crate) fn JPH_ConvexHullShapeSettings_Create(
        points: *const JPH_Vec3,
        count: u32,
        max_convex_radius: f32,
    ) -> *mut JPH_ShapeSettings;
    pub(crate) fn JPH_ConvexHullShapeSettings_CreateShape(
        settings: *const JPH_ShapeSettings,
    ) -> *mut JPH_Shape;
    pub(crate) fn JPH_MeshShapeSettings_Create2(
        vertices: *const JPH_Vec3,
        vertex_count: u32,
        triangles: *const JPH_IndexedTriangle,
        triangle_count: u32,
    ) -> *mut JPH_ShapeSettings;
    pub(crate) fn JPH_MeshShapeSettings_CreateShape(
        settings: *const JPH_ShapeSettings,
    ) -> *mut JPH_Shape;
    pub(crate) fn JPH_ShapeSettings_Destroy(settings: *mut JPH_ShapeSettings);
    pub(crate) fn JPH_Shape_Destroy(shape: *mut JPH_Shape);

    pub(crate) fn JPH_BodyCreationSettings_Create3(
        shape: *const JPH_Shape,
        position: *const JPH_RVec3,
        rotation: *const JPH_Quat,
        motion_type: u32,
        object_layer: JPH_ObjectLayer,
    ) -> *mut JPH_BodyCreationSettings;
    pub(crate) fn JPH_BodyCreationSettings_Destroy(settings: *mut JPH_BodyCreationSettings);
    pub(crate) fn JPH_BodyCreationSettings_SetLinearVelocity(
        settings: *mut JPH_BodyCreationSettings,
        v: *const JPH_Vec3,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetAngularVelocity(
        settings: *mut JPH_BodyCreationSettings,
        v: *const JPH_Vec3,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetUserData(
        settings: *mut JPH_BodyCreationSettings,
        value: u64,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetIsSensor(
        settings: *mut JPH_BodyCreationSettings,
        value: bool,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetMotionQuality(
        settings: *mut JPH_BodyCreationSettings,
        value: u32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetAllowSleeping(
        settings: *mut JPH_BodyCreationSettings,
        value: bool,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetFriction(
        settings: *mut JPH_BodyCreationSettings,
        value: f32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetRestitution(
        settings: *mut JPH_BodyCreationSettings,
        value: f32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetLinearDamping(
        settings: *mut JPH_BodyCreationSettings,
        value: f32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetAngularDamping(
        settings: *mut JPH_BodyCreationSettings,
        value: f32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetGravityFactor(
        settings: *mut JPH_BodyCreationSettings,
        value: f32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetOverrideMassProperties(
        settings: *mut JPH_BodyCreationSettings,
        value: u32,
    );
    pub(crate) fn JPH_BodyCreationSettings_SetMassPropertiesOverride(
        settings: *mut JPH_BodyCreationSettings,
        mass: *const JPH_MassProperties,
    );

    pub(crate) fn JPH_BodyInterface_CreateAndAddBody(
        bi: *mut JPH_BodyInterface,
        settings: *const JPH_BodyCreationSettings,
        activation: u32,
    ) -> JPH_BodyID;
    pub(crate) fn JPH_BodyInterface_RemoveAndDestroyBody(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
    );
    pub(crate) fn JPH_BodyInterface_GetPositionAndRotation(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        position: *mut JPH_RVec3,
        rotation: *mut JPH_Quat,
    );
    pub(crate) fn JPH_BodyInterface_SetPositionAndRotation(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        position: *const JPH_RVec3,
        rotation: *const JPH_Quat,
        activation: u32,
    );
    pub(crate) fn JPH_BodyInterface_GetCenterOfMassPosition(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        position: *mut JPH_RVec3,
    );
    pub(crate) fn JPH_BodyInterface_GetLinearVelocity(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        v: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_BodyInterface_GetAngularVelocity(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        v: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_BodyInterface_SetLinearAndAngularVelocity(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        linear: *mut JPH_Vec3,
        angular: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_BodyInterface_AddLinearVelocity(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        v: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_BodyInterface_AddImpulse(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        impulse: *mut JPH_Vec3,
    );
    pub(crate) fn JPH_BodyInterface_SetGravityFactor(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
        value: f32,
    );
    pub(crate) fn JPH_BodyInterface_IsActive(
        bi: *const JPH_BodyInterface,
        body: JPH_BodyID,
    ) -> bool;
    pub(crate) fn JPH_BodyInterface_GetUserData(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
    ) -> u64;
    pub(crate) fn JPH_BodyInterface_GetObjectLayer(
        bi: *mut JPH_BodyInterface,
        body: JPH_BodyID,
    ) -> JPH_ObjectLayer;

    pub(crate) fn JPH_BodyLockInterface_LockRead(
        lock_interface: *const JPH_BodyLockInterface,
        body: JPH_BodyID,
        out_lock: *mut JPH_BodyLockRead,
    );
    pub(crate) fn JPH_BodyLockInterface_UnlockRead(
        lock_interface: *const JPH_BodyLockInterface,
        io_lock: *mut JPH_BodyLockRead,
    );

    pub(crate) fn JPH_NarrowPhaseQuery_CastRay(
        query: *const JPH_NarrowPhaseQuery,
        origin: *const JPH_RVec3,
        direction: *const JPH_Vec3,
        hit: *mut JPH_RayCastResult,
        bp_filter: *const JPH_BroadPhaseLayerFilter,
        layer_filter: *const JPH_ObjectLayerFilter,
        body_filter: *const JPH_BodyFilter,
    ) -> bool;
    pub(crate) fn JPH_CollideShapeSettings_Init(settings: *mut JPH_CollideShapeSettings);
    pub(crate) fn JPH_NarrowPhaseQuery_CollideShape(
        query: *const JPH_NarrowPhaseQuery,
        shape: *const JPH_Shape,
        scale: *const JPH_Vec3,
        center_of_mass_transform: *const JPH_RMat4,
        settings: *const JPH_CollideShapeSettings,
        base_offset: *mut JPH_RVec3,
        callback: JPH_CollideShapeCollectorCallback,
        user_data: *mut c_void,
        bp_filter: *const JPH_BroadPhaseLayerFilter,
        layer_filter: *const JPH_ObjectLayerFilter,
        body_filter: *const JPH_BodyFilter,
        shape_filter: *const JPH_ShapeFilter,
    ) -> bool;

    pub(crate) fn JPH_FixedConstraintSettings_Init(settings: *mut JPH_FixedConstraintSettings);
    pub(crate) fn JPH_FixedConstraint_Create(
        settings: *const JPH_FixedConstraintSettings,
        body1: *mut JPH_Body,
        body2: *mut JPH_Body,
    ) -> *mut JPH_Constraint;
    pub(crate) fn JPH_PointConstraintSettings_Init(settings: *mut JPH_PointConstraintSettings);
    pub(crate) fn JPH_PointConstraint_Create(
        settings: *const JPH_PointConstraintSettings,
        body1: *mut JPH_Body,
        body2: *mut JPH_Body,
    ) -> *mut JPH_Constraint;
    pub(crate) fn JPH_HingeConstraintSettings_Init(settings: *mut JPH_HingeConstraintSettings);
    pub(crate) fn JPH_HingeConstraint_Create(
        settings: *const JPH_HingeConstraintSettings,
        body1: *mut JPH_Body,
        body2: *mut JPH_Body,
    ) -> *mut JPH_Constraint;
    pub(crate) fn JPH_DistanceConstraintSettings_Init(
        settings: *mut JPH_DistanceConstraintSettings,
    );
    pub(crate) fn JPH_DistanceConstraint_Create(
        settings: *const JPH_DistanceConstraintSettings,
        body1: *mut JPH_Body,
        body2: *mut JPH_Body,
    ) -> *mut JPH_Constraint;
    pub(crate) fn JPH_SliderConstraintSettings_Init(settings: *mut JPH_SliderConstraintSettings);
    pub(crate) fn JPH_SliderConstraint_Create(
        settings: *const JPH_SliderConstraintSettings,
        body1: *mut JPH_Body,
        body2: *mut JPH_Body,
    ) -> *mut JPH_Constraint;
    pub(crate) fn JPH_Constraint_Destroy(constraint: *mut JPH_Constraint);
}

/// Layout checks against the C compiler (`csrc/layout.cpp`).
#[cfg(test)]
mod layout {
    use super::*;
    use core::mem::{offset_of, size_of};

    macro_rules! c_size {
        ($($f:ident),* $(,)?) => {
            unsafe extern "C" { $(fn $f() -> usize;)* }
        };
    }
    c_size!(
        engine_joltc_sizeof_JPH_Vec3,
        engine_joltc_sizeof_JPH_Quat,
        engine_joltc_sizeof_JPH_RMat4,
        engine_joltc_sizeof_JPH_RayCastResult,
        engine_joltc_sizeof_JPH_PhysicsSystemSettings,
        engine_joltc_offsetof_JPH_PhysicsSystemSettings_broadPhaseLayerInterface,
        engine_joltc_sizeof_JPH_PhysicsStepListenerContext,
        engine_joltc_sizeof_JPH_SubShapeIDPair,
        engine_joltc_sizeof_JPH_IndexedTriangle,
        engine_joltc_sizeof_JPH_CollideShapeSettings,
        engine_joltc_sizeof_JPH_CollideShapeResult,
        engine_joltc_offsetof_JPH_CollideShapeResult_bodyID2,
        engine_joltc_sizeof_JPH_ConstraintSettings,
        engine_joltc_sizeof_JPH_FixedConstraintSettings,
        engine_joltc_offsetof_JPH_FixedConstraintSettings_point1,
        engine_joltc_offsetof_JPH_FixedConstraintSettings_axisY2,
        engine_joltc_sizeof_JPH_PointConstraintSettings,
        engine_joltc_offsetof_JPH_PointConstraintSettings_point2,
        engine_joltc_sizeof_JPH_HingeConstraintSettings,
        engine_joltc_offsetof_JPH_HingeConstraintSettings_limitsMin,
        engine_joltc_offsetof_JPH_HingeConstraintSettings_motorSettings,
        engine_joltc_sizeof_JPH_DistanceConstraintSettings,
        engine_joltc_offsetof_JPH_DistanceConstraintSettings_minDistance,
        engine_joltc_sizeof_JPH_SliderConstraintSettings,
        engine_joltc_offsetof_JPH_SliderConstraintSettings_limitsMin,
        engine_joltc_offsetof_JPH_SliderConstraintSettings_motorSettings,
        engine_joltc_sizeof_JPH_BodyLockRead,
        engine_joltc_sizeof_JobSystemThreadPoolConfig,
        engine_joltc_sizeof_JPH_JobSystemConfig,
        engine_joltc_offsetof_JPH_JobSystemConfig_maxConcurrency,
        engine_joltc_sizeof_JPH_MassProperties,
    );

    #[test]
    fn rust_layouts_match_c() {
        // SAFETY: the functions only return constants.
        let checks: &[(&str, usize, usize)] = unsafe {
            &[
                (
                    "Vec3",
                    size_of::<JPH_Vec3>(),
                    engine_joltc_sizeof_JPH_Vec3(),
                ),
                (
                    "Quat",
                    size_of::<JPH_Quat>(),
                    engine_joltc_sizeof_JPH_Quat(),
                ),
                (
                    "RMat4",
                    size_of::<JPH_RMat4>(),
                    engine_joltc_sizeof_JPH_RMat4(),
                ),
                (
                    "RayCastResult",
                    size_of::<JPH_RayCastResult>(),
                    engine_joltc_sizeof_JPH_RayCastResult(),
                ),
                (
                    "PhysicsSystemSettings",
                    size_of::<JPH_PhysicsSystemSettings>(),
                    engine_joltc_sizeof_JPH_PhysicsSystemSettings(),
                ),
                (
                    "PhysicsSystemSettings.broadPhaseLayerInterface",
                    offset_of!(JPH_PhysicsSystemSettings, broadPhaseLayerInterface),
                    engine_joltc_offsetof_JPH_PhysicsSystemSettings_broadPhaseLayerInterface(),
                ),
                (
                    "PhysicsStepListenerContext",
                    size_of::<JPH_PhysicsStepListenerContext>(),
                    engine_joltc_sizeof_JPH_PhysicsStepListenerContext(),
                ),
                (
                    "SubShapeIDPair",
                    size_of::<JPH_SubShapeIDPair>(),
                    engine_joltc_sizeof_JPH_SubShapeIDPair(),
                ),
                (
                    "IndexedTriangle",
                    size_of::<JPH_IndexedTriangle>(),
                    engine_joltc_sizeof_JPH_IndexedTriangle(),
                ),
                (
                    "CollideShapeSettings",
                    size_of::<JPH_CollideShapeSettings>(),
                    engine_joltc_sizeof_JPH_CollideShapeSettings(),
                ),
                (
                    "CollideShapeResult",
                    size_of::<JPH_CollideShapeResult>(),
                    engine_joltc_sizeof_JPH_CollideShapeResult(),
                ),
                (
                    "CollideShapeResult.bodyID2",
                    offset_of!(JPH_CollideShapeResult, bodyID2),
                    engine_joltc_offsetof_JPH_CollideShapeResult_bodyID2(),
                ),
                (
                    "ConstraintSettings",
                    size_of::<JPH_ConstraintSettings>(),
                    engine_joltc_sizeof_JPH_ConstraintSettings(),
                ),
                (
                    "FixedConstraintSettings",
                    size_of::<JPH_FixedConstraintSettings>(),
                    engine_joltc_sizeof_JPH_FixedConstraintSettings(),
                ),
                (
                    "FixedConstraintSettings.point1",
                    offset_of!(JPH_FixedConstraintSettings, point1),
                    engine_joltc_offsetof_JPH_FixedConstraintSettings_point1(),
                ),
                (
                    "FixedConstraintSettings.axisY2",
                    offset_of!(JPH_FixedConstraintSettings, axisY2),
                    engine_joltc_offsetof_JPH_FixedConstraintSettings_axisY2(),
                ),
                (
                    "PointConstraintSettings",
                    size_of::<JPH_PointConstraintSettings>(),
                    engine_joltc_sizeof_JPH_PointConstraintSettings(),
                ),
                (
                    "PointConstraintSettings.point2",
                    offset_of!(JPH_PointConstraintSettings, point2),
                    engine_joltc_offsetof_JPH_PointConstraintSettings_point2(),
                ),
                (
                    "HingeConstraintSettings",
                    size_of::<JPH_HingeConstraintSettings>(),
                    engine_joltc_sizeof_JPH_HingeConstraintSettings(),
                ),
                (
                    "HingeConstraintSettings.limitsMin",
                    offset_of!(JPH_HingeConstraintSettings, limitsMin),
                    engine_joltc_offsetof_JPH_HingeConstraintSettings_limitsMin(),
                ),
                (
                    "HingeConstraintSettings.motorSettings",
                    offset_of!(JPH_HingeConstraintSettings, motorSettings),
                    engine_joltc_offsetof_JPH_HingeConstraintSettings_motorSettings(),
                ),
                (
                    "DistanceConstraintSettings",
                    size_of::<JPH_DistanceConstraintSettings>(),
                    engine_joltc_sizeof_JPH_DistanceConstraintSettings(),
                ),
                (
                    "DistanceConstraintSettings.minDistance",
                    offset_of!(JPH_DistanceConstraintSettings, minDistance),
                    engine_joltc_offsetof_JPH_DistanceConstraintSettings_minDistance(),
                ),
                (
                    "SliderConstraintSettings",
                    size_of::<JPH_SliderConstraintSettings>(),
                    engine_joltc_sizeof_JPH_SliderConstraintSettings(),
                ),
                (
                    "SliderConstraintSettings.limitsMin",
                    offset_of!(JPH_SliderConstraintSettings, limitsMin),
                    engine_joltc_offsetof_JPH_SliderConstraintSettings_limitsMin(),
                ),
                (
                    "SliderConstraintSettings.motorSettings",
                    offset_of!(JPH_SliderConstraintSettings, motorSettings),
                    engine_joltc_offsetof_JPH_SliderConstraintSettings_motorSettings(),
                ),
                (
                    "BodyLockRead",
                    size_of::<JPH_BodyLockRead>(),
                    engine_joltc_sizeof_JPH_BodyLockRead(),
                ),
                (
                    "JobSystemThreadPoolConfig",
                    size_of::<JobSystemThreadPoolConfig>(),
                    engine_joltc_sizeof_JobSystemThreadPoolConfig(),
                ),
                (
                    "JobSystemConfig",
                    size_of::<JPH_JobSystemConfig>(),
                    engine_joltc_sizeof_JPH_JobSystemConfig(),
                ),
                (
                    "JobSystemConfig.maxConcurrency",
                    offset_of!(JPH_JobSystemConfig, maxConcurrency),
                    engine_joltc_offsetof_JPH_JobSystemConfig_maxConcurrency(),
                ),
                (
                    "MassProperties",
                    size_of::<JPH_MassProperties>(),
                    engine_joltc_sizeof_JPH_MassProperties(),
                ),
            ]
        };
        let mismatches: Vec<_> = checks.iter().filter(|(_, rust, c)| rust != c).collect();
        assert!(
            mismatches.is_empty(),
            "layout mismatches (name, Rust, C): {mismatches:?}"
        );
    }
}
