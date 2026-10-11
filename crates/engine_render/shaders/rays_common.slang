// Ray queries against the scene's acceleration structures (ADR 0087), for any shader that
// traces rays. Not compiled on its own: `#include` it.

// The acceleration structure at a device address (`AccelerationStructure::address`),
// pushed as two words like any other address: no descriptor binding is needed, and the
// two-word form needs no 64-bit integers (SPV_KHR_ray_query's
// OpConvertUToAccelerationStructureKHR).
RaytracingAccelerationStructure accel_at(uint2 address)
{
    return spirv_asm {
        OpExtension "SPV_KHR_ray_query";
        OpCapability RayQueryKHR;
        result: $$RaytracingAccelerationStructure = OpConvertUToAccelerationStructureKHR $address;
    };
}

// Instance masks (`rays.rs`): what a ray's cull mask picks.
static const uint RAY_MASK_OPAQUE = 1u;
static const uint RAY_MASK_MASKED = 2u;     // alpha-tested materials, traced as opaque
static const uint RAY_MASK_BLENDED = 4u;    // transparent materials
