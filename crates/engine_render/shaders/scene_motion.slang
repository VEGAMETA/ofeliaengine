// Instances interpolated between simulation ticks on the GPU (ADR 0064 addendum): a game
// gives each moving instance its transforms of the tick before and of the last tick once
// per tick (`GpuScene::step_motion`, `GpuScene::set_motion`), and every frame one thread
// per moving instance places it between them at the frame's interpolation factor. The
// CPU's work per frame does not grow with the moving instances.
//
// A slot's two transforms live in six planes of rows (transform `t`'s row `k` at
// `(3 t + k) × capacity + slot`), so neighbouring threads read and write whole cache
// lines, and a word per slot after them says which of the two is the last tick's (`to`);
// the other is the tick before's (`from`). A tick writes only the new `to`, over the
// older of the two. Records come the same way: a slot word each, and their rows by row
// (row `r` of record `i` at `r × count + i`).
//
// - `sceneMotion` (one thread per update, after the frame's other scene writes): both
//   transforms of a slot that starts moving, ticked more than once since the last upload,
//   or is new or teleported (`STILL`: what it shows in this frame is also its previous
//   transform, no motion).
// - A record whose handedness differs from the tick before's (`SNAP`: a mirroring that
//   flips) makes both transforms its `to`: blended, the instance would pass through flat,
//   inside-out transforms drawn with its batch's culling.
// - `sceneStep` (one thread per step): the next tick of a slot that keeps moving.
// - `sceneInterpolate` (one thread per moving slot, last): this frame's placement. The
//   scene's two instance buffers swap roles every frame (`scene.slang`), so last frame's
//   placement is already the previous one.

// `Instance` in forward.slang, as four 16-byte words: the three rows of the transform,
// then the material and batch.
struct Instance
{
    uint4 words[4];
};

// In a record's slot word (`gpu_scene.rs`): its `to` goes to the second transform (the
// first holds `from`), the instance is new or teleported, and both transforms are `to`.
static const uint TO_SECOND = 0x80000000u;
static const uint STILL = 0x40000000u;
static const uint SNAP = 0x20000000u;
static const uint SLOT_MASK = 0x1FFFFFFFu;

// `MotionPush` in gpu_scene.rs (64 bytes).
struct MotionPush
{
    Instance* instances;
    Instance* previous;
    float4* motion; // the six planes
    uint* second;   // per slot, after the planes: 1 when `to` is the second transform
    uint* words;    // per record, its slot word; for sceneInterpolate, the moving slots
    float4* rows;   // per record, its rows by row
    uint capacity;
    uint count;
    float alpha;
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<MotionPush> push;

// Where row `k` of a slot's transform `t` is.
float4* plane(uint t, uint k, uint slot)
{
    return push.motion + (3 * t + k) * push.capacity + slot;
}

// Rows are 16-byte aligned: one vector load or store each (a plain pointer access is
// declared 4-byte aligned, which a driver may split).
float4 row(float4* at)
{
    return loadAligned<16>(at);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void sceneMotion(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= push.count)
        return;
    uint word = push.words[id.x];
    uint slot = word & SLOT_MASK;
    uint to = (word & TO_SECOND) != 0 ? 1 : 0;
    bool snap = (word & SNAP) != 0;
    [unroll]
    for (uint k = 0; k < 3; k++)
    {
        float4 b = row(push.rows + (3 + k) * push.count + id.x);
        float4 a = snap ? b : row(push.rows + k * push.count + id.x);
        storeAligned<16>(plane(1 - to, k, slot), a);
        storeAligned<16>(plane(to, k, slot), b);
        // As `sceneInterpolate` places it.
        if ((word & STILL) != 0)
            storeAligned<16>(&push.previous[slot].words[k], asuint(a + (b - a) * push.alpha));
    }
    push.second[slot] = to;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void sceneStep(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= push.count)
        return;
    uint word = push.words[id.x];
    uint slot = word & SLOT_MASK;
    uint to = (word & TO_SECOND) != 0 ? 1 : 0;
    bool snap = (word & SNAP) != 0;
    [unroll]
    for (uint k = 0; k < 3; k++)
    {
        float4 b = row(push.rows + k * push.count + id.x);
        storeAligned<16>(plane(to, k, slot), b);
        if (snap)
            storeAligned<16>(plane(1 - to, k, slot), b);
    }
    push.second[slot] = to;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void sceneInterpolate(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= push.count)
        return;
    uint slot = push.words[id.x];
    uint to = push.second[slot];
    // Only the transform's rows: the material and batch words stay as written.
    [unroll]
    for (uint k = 0; k < 3; k++)
    {
        float4 a = row(plane(1 - to, k, slot));
        float4 b = row(plane(to, k, slot));
        // `a + (b - a) * t`, as `RenderExtract`'s CPU blend.
        storeAligned<16>(&push.instances[slot].words[k], asuint(a + (b - a) * push.alpha));
    }
}
