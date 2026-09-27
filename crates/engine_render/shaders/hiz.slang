// Depth pyramid (ADR 0035): level k holds the farthest depth of every 2^k × 2^k pixel
// block of a depth target. Level sizes round up (⌈size / 2^k⌉), and reads clamp to the
// edge, so the last row and column of a level cover what is left. The levels are stored
// one after the other in one buffer, for the occlusion test in forward.slang. Level 1 is
// computed but not stored when there are more levels: its 2 MB at 1080p were a fifth of
// the memory traffic, and level 2 serves the occlusion test as well.
//
// Every workgroup reduces a 64 × 64 tile of the level below (the depth target itself for
// level 1) to up to six levels L .. L + 5 (32², 16², 8², 4², 2², 1² per tile), through
// group shared memory. See hiz.rs for how the levels are split over dispatches.
//
// Values past the edge of a level (inside the last tile) come from clamped reads, so
// they repeat edge values and never change a maximum.

[[vk::binding(0, 0)]] Texture2D g_textures[];

// Offsets are passed per dispatch and indexed with constants only: indexing a push
// constant array with a run-time value makes compilers copy the array per thread
// (measured on ref-1's Radeon iGPU).
struct HizPush
{
    float* pyramid;     // the stored levels, one after the other
    uint2 depth_size;   // size of the depth target in pixels
    uint texture;       // bindless index of the depth target (read when first == 1)
    uint levels;        // levels in the pyramid; the last one is 1 × 1
    uint first;         // L: the first level this dispatch writes
    uint input_offset;  // offset of level L - 1 in floats (unused for the depth target)
    uint4 out_a;        // offsets of levels L .. L + 3
    uint2 out_b;        // offsets of levels L + 4 and L + 5
    uint count;         // levels this dispatch writes, 1 to 6
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<HizPush> push;

// Offset of level L + k; k is a constant wherever this is called.
uint out_offset(uint k)
{
    return k < 4 ? push.out_a[k] : push.out_b[k - 4];
}

// Size of a level; level 0 is the depth target.
uint2 level_size(uint level)
{
    return level == 0 ? push.depth_size : ((push.depth_size - 1) >> level) + 1;
}

// A texel of the input level (L - 1), clamped to its edge.
float read_input(uint2 p)
{
    uint level = push.first - 1;
    uint2 size = level_size(level);
    p = min(p, size - 1);
    if (level == 0)
        return g_textures[NonUniformResourceIndex(push.texture)].Load(int3(p, 0)).r;
    return push.pyramid[push.input_offset + p.y * size.x + p.x];
}

// Writes texel p of level L + k.
void write_texel(uint k, uint2 p, float d)
{
    uint level = push.first + k;
    if (level > push.levels)
        return;
    uint2 size = level_size(level);
    if (any(p >= size))
        return;
    push.pyramid[out_offset(k) + p.y * size.x + p.x] = d;
}

groupshared float g_values[32][32];

[shader("compute")]
[numthreads(16, 16, 1)]
void hizReduce(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID)
{
    // Level L: the group's 32 × 32 texels in four 16 × 16 quarters, one texel per thread
    // each. Neighbouring threads handle neighbouring texels, so the reads of the level
    // below and the writes of level L are contiguous (building level 1 is limited by
    // memory bandwidth on small GPUs).
    [unroll]
    for (uint quarter = 0; quarter < 4; quarter++)
    {
        uint2 c = uint2(quarter & 1, quarter >> 1) * 16 + local.xy;
        uint2 p = (group.xy * 32 + c) * 2;
        float d = max(
            max(read_input(p), read_input(p + uint2(1, 0))),
            max(read_input(p + uint2(0, 1)), read_input(p + uint2(1, 1))));
        // Level 1 is only stored when it is the whole pyramid (a target of 2 × 2 or less).
        if (push.first != 1 || push.levels == 1)
            write_texel(0, group.xy * 32 + c, d);
        g_values[c.y][c.x] = d;
    }
    GroupMemoryBarrierWithGroupSync();

    // Levels L + 1 .. L + 5 through group shared memory: 16², 8², 4², 2² and 1 threads.
    [unroll]
    for (uint k = 1; k <= 5; k++)
    {
        // Uniform: every thread leaves together, barriers stay matched.
        if (k >= push.count)
            break;
        uint side = 32 >> k;
        bool active = all(local.xy < side);
        float d = 0.0;
        if (active)
        {
            uint2 q = local.xy * 2;
            d = max(
                max(g_values[q.y][q.x], g_values[q.y][q.x + 1]),
                max(g_values[q.y + 1][q.x], g_values[q.y + 1][q.x + 1]));
            write_texel(k, group.xy * side + local.xy, d);
        }
        GroupMemoryBarrierWithGroupSync();
        if (active)
            g_values[local.y][local.x] = d;
        GroupMemoryBarrierWithGroupSync();
    }
}
