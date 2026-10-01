// The motion of skinned meshes (ADR 0068): after the forward pass of a view with motion
// vectors, the skinned instances are drawn again with depth test EQUAL (the placement
// below is bit-identical to `clip_position` in forward.slang, so exactly the pixels the
// forward pass drew from them pass) and their motion, from each vertex's previous skinned
// position (written by skinning.slang), replaces what the forward pass wrote, which knew
// only the instance's rigid motion. The forward shaders are untouched: views and
// materials without skinned meshes pay nothing.

// The view of a skinned motion pass (`MotionView` in skinning.rs).
struct MotionView
{
    float4x4 clip_from_world;          // jittered, as the forward pass drew
    float4x4 previous_clip_from_world; // last frame's, unjittered
    float4 jitter;                     // xy: this frame's jitter in NDC
    uint2 size;                        // of the motion target
    uint2 _pad;
};

// A skinned instance of the motion pass (`MotionInstance` in skinning.rs, 112 bytes).
struct MotionInstance
{
    float4 row0;          // world_from_object rows, as forward.slang's `Instance`
    float4 row1;
    float4 row2;
    float4 previous0;     // last frame's
    float4 previous1;
    float4 previous2;
    float* previous;      // last frame's skinned positions
    uint first_vertex;
    uint _pad;
};

// `MotionPush` in skinning.rs (24 bytes).
struct MotionPush
{
    MotionView* view;
    MotionInstance* instances;
    float* positions;
};

[[vk::push_constant]]
ConstantBuffer<MotionPush> push;

struct MotionOut
{
    float4 position : SV_Position;
    float4 previous_clip : TEXCOORD0;
};

// One instance per draw (`first_instance`); the vertex index includes the draw's vertex
// offset, the instance's first vertex in the pool.
[shader("vertex")]
MotionOut skinnedMotionVertex(uint vertex : SV_VulkanVertexID, uint instance : SV_VulkanInstanceID)
{
    MotionInstance i = push.instances[instance];
    float* pp = push.positions + vertex * 3;
    float3 p = float3(pp[0], pp[1], pp[2]);
    // `clip_position` of forward.slang, operation for operation.
    precise float3 world = float3(
        i.row0.x * p.x + i.row0.y * p.y + i.row0.z * p.z + i.row0.w,
        i.row1.x * p.x + i.row1.y * p.y + i.row1.z * p.z + i.row1.w,
        i.row2.x * p.x + i.row2.y * p.y + i.row2.z * p.z + i.row2.w);
    float4x4 m = transpose(push.view.clip_from_world);
    precise float4 clip = m[0] * world.x + m[1] * world.y + m[2] * world.z + m[3];
    uint local = vertex - i.first_vertex;
    float3 q = float3(i.previous[local * 3], i.previous[local * 3 + 1], i.previous[local * 3 + 2]);
    float3 before = float3(
        dot(i.previous0.xyz, q) + i.previous0.w,
        dot(i.previous1.xyz, q) + i.previous1.w,
        dot(i.previous2.xyz, q) + i.previous2.w);
    MotionOut o;
    o.position = clip;
    o.previous_clip = mul(push.view.previous_clip_from_world, float4(before, 1.0));
    return o;
}

// `motion` of forward.slang: the pixel's position now (jitter removed) minus where its
// surface was last frame, in texture coordinates.
[shader("fragment")]
float2 skinnedMotionFragment(MotionOut input) : SV_Target0
{
    MotionView view = push.view[0];
    float2 uv = input.position.xy / float2(view.size) - view.jitter.xy * 0.5;
    if (input.previous_clip.w <= 1e-6)
        return float2(0.0, 0.0);
    float2 previous = input.previous_clip.xy / input.previous_clip.w * 0.5 + 0.5;
    return uv - previous;
}
