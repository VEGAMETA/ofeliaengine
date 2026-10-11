// The scene for ray queries (ADR 0087): the top-level structure's instances written from
// the GPU scene's (`tlasInstances`), and rays cast in a batch (`castRays`: tests, picking).

#include "rays_common.slang"

// `InstanceData` in forward/mod.rs (64 bytes): the rows of world_from_object, the
// material's slot, the batch.
struct Instance
{
    float4 rows[3];
    uint material;
    uint batch;
    uint2 _pad;
};

// `BatchRay` in rays.rs (16 bytes): the bottom-level structure of the batch's mesh (0:
// none, the instance is inactive), the instance mask and flags.
struct BatchRay
{
    uint2 blas;
    uint mask;
    uint flags;
};

// `InstancePush` in rays.rs.
struct InstancePush
{
    Instance* instances;
    BatchRay* batches;
    uint4* out;              // VkAccelerationStructureInstanceKHR, 64 bytes each
    uint count;              // slots
    uint batch_count;
};

[[vk::push_constant]]
ConstantBuffer<InstancePush> instance_push;

// One instance per slot of the GPU scene (holes and meshes without a structure inactive):
// the transform's rows as they are, the custom index the slot.
[shader("compute")]
[numthreads(64, 1, 1)]
void tlasInstances(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= instance_push.count)
        return;
    Instance inst = loadAligned<16>(instance_push.instances + id.x);
    BatchRay b = (BatchRay)0;
    if (inst.batch < instance_push.batch_count)
        b = loadAligned<16>(instance_push.batches + inst.batch);
    uint4* o = instance_push.out + id.x * 4;
    storeAligned<16>(o, asuint(inst.rows[0]));
    storeAligned<16>(o + 1, asuint(inst.rows[1]));
    storeAligned<16>(o + 2, asuint(inst.rows[2]));
    // instanceCustomIndex (24) | mask (8); shader binding table offset (24) | flags (8);
    // the bottom-level structure's address.
    storeAligned<16>(o + 3, uint4((id.x & 0xFFFFFFu) | (b.mask << 24), b.flags << 24, b.blas.x, b.blas.y));
}

// `RayCast` and `RayHit` in rays.rs.
struct RayCast
{
    float4 origin;           // w: the nearest distance
    float4 direction;        // w: the farthest distance
};

struct RayHit
{
    float t;                 // -1: no hit
    uint slot;               // the GPU scene's slot
    uint primitive;          // the triangle in the mesh's level its structure was built from
    uint front;              // 1: the triangle's front face
    float2 barycentrics;
    uint2 _pad;
};

// `CastPush` in rays.rs.
struct CastPush
{
    uint2 accel;
    RayCast* rays;
    RayHit* hits;
    uint count;
    uint mask;
};

[[vk::push_constant]]
ConstantBuffer<CastPush> cast_push;

// The nearest hit of each ray (opaque: no candidates to process).
[shader("compute")]
[numthreads(64, 1, 1)]
void castRays(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= cast_push.count)
        return;
    RayCast c = loadAligned<16>(cast_push.rays + id.x);
    RayDesc ray;
    ray.Origin = c.origin.xyz;
    ray.TMin = c.origin.w;
    ray.Direction = c.direction.xyz;
    ray.TMax = c.direction.w;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> q;
    q.TraceRayInline(accel_at(cast_push.accel), 0, cast_push.mask, ray);
    while (q.Proceed())
    {
    }
    RayHit hit = (RayHit)0;
    hit.t = -1.0;
    if (q.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
    {
        hit.t = q.CommittedRayT();
        hit.slot = q.CommittedInstanceID();
        hit.primitive = q.CommittedPrimitiveIndex();
        hit.front = q.CommittedTriangleFrontFace() ? 1u : 0u;
        hit.barycentrics = q.CommittedTriangleBarycentrics();
    }
    storeAligned<16>(cast_push.hits + id.x, hit);
}

// `CameraPush` in rays.rs.
struct CameraPush
{
    uint2 accel;
    float4x4* world_from_clip;   // the camera's inverse view-projection
    float* out;                  // per pixel, the distance to the nearest hit (-1: none)
    uint2 size;
    uint mask;
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<CameraPush> camera_push;

// A ray through every pixel's center from the camera's near plane: the scene's distance
// image as rays see it (tests against the rasterized depth, ray throughput).
[shader("compute")]
[numthreads(8, 4, 1)]
void castCameraRays(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= camera_push.size))
        return;
    float4x4 m = loadAligned<16>(camera_push.world_from_clip);
    float2 ndc = (float2(id.xy) + 0.5) / float2(camera_push.size) * 2.0 - 1.0;
    float4 near = mul(m, float4(ndc, 0.0, 1.0));
    float4 far = mul(m, float4(ndc, 1.0, 1.0));
    float3 origin = near.xyz / near.w;
    RayDesc ray;
    ray.Origin = origin;
    ray.TMin = 0.0;
    ray.Direction = normalize(far.xyz / far.w - origin);
    ray.TMax = 1e30;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> q;
    q.TraceRayInline(accel_at(camera_push.accel), 0, camera_push.mask, ray);
    while (q.Proceed())
    {
    }
    camera_push.out[id.y * camera_push.size.x + id.x] =
        q.CommittedStatus() == COMMITTED_TRIANGLE_HIT ? q.CommittedRayT() : -1.0;
}
