// Persistent GPU scene (ADR 0064): the instance slots of `GpuScene` live in two device
// buffers, this frame's (`instances`) and last frame's (`previous`, for motion vectors),
// in the `Instance` layout of forward.slang. The two swap roles every frame, so last
// frame's values are this frame's previous ones without a copy. Each frame the CPU
// uploads only what changed: one `Update` per touched slot and the new instance data,
// and one thread per update applies it here. A slot appears at most once per frame, so
// threads never touch the same slot.
//
// - moved: current = data (its last frame's transform is already the previous one);
// - added, teleported or removed: previous = current = data (no motion this frame; a
//   removed instance's hole, parked in the scene's sink batch, is in both buffers, so the
//   next swap cannot bring the instance back);
// - settled (changed last frame, not this one): current = previous (this frame's buffer
//   held the frame before's values).

// `Instance` in forward.slang, moved as four 16-byte words (vector loads and stores).
struct Instance
{
    uint4 words[4];
};

struct Update
{
    uint slot;
    uint current;  // index into `data`, KEEP or SETTLE (the slot's previous value)
    uint previous; // index into `data`, or KEEP
    uint _pad;
};

// `KEEP` and `SETTLE` in gpu_scene.rs.
static const uint KEEP = 0xFFFFFFFFu;
static const uint SETTLE = 0xFFFFFFFEu;

// `ScatterPush` in gpu_scene.rs (48 bytes).
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
    if (u.current == SETTLE)
        push.instances[u.slot] = push.previous[u.slot];
    else if (u.current != KEEP)
        push.instances[u.slot] = push.data[u.current];
    if (u.previous != KEEP)
        push.previous[u.slot] = push.data[u.previous];
}
