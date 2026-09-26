// Sizes and field offsets of the JoltC structs the Rust bindings mirror, so tests can
// check that both sides agree (engine_physics_jolt/src/ffi.rs).
#include "joltc.h"
#include <cstddef>

#define SIZE(T) extern "C" size_t engine_joltc_sizeof_##T(void) { return sizeof(T); }
#define OFFSET(T, F) extern "C" size_t engine_joltc_offsetof_##T##_##F(void) { return offsetof(T, F); }

SIZE(JPH_Vec3)
SIZE(JPH_Quat)
SIZE(JPH_RMat4)
SIZE(JPH_RayCastResult)
SIZE(JPH_PhysicsSystemSettings)
OFFSET(JPH_PhysicsSystemSettings, broadPhaseLayerInterface)
SIZE(JPH_PhysicsStepListenerContext)
SIZE(JPH_SubShapeIDPair)
SIZE(JPH_IndexedTriangle)
SIZE(JPH_CollideShapeSettings)
SIZE(JPH_CollideShapeResult)
OFFSET(JPH_CollideShapeResult, bodyID2)
SIZE(JPH_ConstraintSettings)
SIZE(JPH_FixedConstraintSettings)
OFFSET(JPH_FixedConstraintSettings, point1)
OFFSET(JPH_FixedConstraintSettings, axisY2)
SIZE(JPH_PointConstraintSettings)
OFFSET(JPH_PointConstraintSettings, point2)
SIZE(JPH_HingeConstraintSettings)
OFFSET(JPH_HingeConstraintSettings, limitsMin)
OFFSET(JPH_HingeConstraintSettings, motorSettings)
SIZE(JPH_DistanceConstraintSettings)
OFFSET(JPH_DistanceConstraintSettings, minDistance)
SIZE(JPH_SliderConstraintSettings)
OFFSET(JPH_SliderConstraintSettings, limitsMin)
OFFSET(JPH_SliderConstraintSettings, motorSettings)
SIZE(JPH_BodyLockRead)
SIZE(JobSystemThreadPoolConfig)
SIZE(JPH_JobSystemConfig)
OFFSET(JPH_JobSystemConfig, maxConcurrency)
SIZE(JPH_MassProperties)
