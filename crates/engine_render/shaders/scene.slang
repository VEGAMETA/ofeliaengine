// Persistent GPU scene (ADR 0064): the instance slots of `GpuScene` live in two device
// buffers, this frame's (`instances`) and the one before (`previous`, for motion
// vectors), in the `Instance` layout of forward.slang. Each frame the CPU uploads only
// what changed: one `Update` per touched slot and the new instance data, and one thread
// per update applies it here. A slot appears at most once per frame, so threads never
// touch the same slot.
//
// - moved: previous = current, current = data (last frame's transform is remembered);
// - added or teleported: previous = current = data (no motion this frame);
// - settled (moved last frame, not this one): previous = current;
// - removed: current = data (a hole, parked in the scene's sink batch); previous kept.

struct Instance
{
    float4 row0;
    float4 row1;
    float4 row2;
    uint material;
    uint batch;
    uint _pad0;
    uint _pad1;
};

struct Update
{
    uint slot;
    uint current;  // index into `data`, or KEEP
    uint previous; // index into `data`, KEEP or COPY (the slot's current value)
    uint _pad;
};

// `KEEP` and `COPY` in gpu_scene.rs.
static const uint KEEP = 0xFFFFFFFFu;
static const uint COPY = 0xFFFFFFFEu;

// `ScatterPush` in gpu_scene.rs (32 bytes).
struct ScatterPush
{
    Instance* instances;
    Instance* previous;
    Update* updates;
    Instance* data;
    uint count;
    uint _pad0;
    uint _pad1;
    uint _pad2;
};

[[vk::push_constant]]
ConstantBuffer<ScatterPush> push;

[shader("compute")]
[numthreads(64, 1, 1)]
void sceneScatter(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= push.count)
        return;
    Update u = push.updates[id.x];
    if (u.previous == COPY)
        push.previous[u.slot] = push.instances[u.slot];
    else if (u.previous != KEEP)
        push.previous[u.slot] = push.data[u.previous];
    if (u.current != KEEP)
        push.instances[u.slot] = push.data[u.current];
}
