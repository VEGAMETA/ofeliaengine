// Virtual shadow maps for the sun (ADR 0093 step 3): the pages' bookkeeping. Levels,
// pages and their addresses: `vsm_common.slang`.
//
// The bookkeeping, in the order a frame runs it:
// - `vsmBeginFrame`: a frame's flags cleared, wrapped entries freed, the hierarchy of
//   dirty pages emptied;
// - `vsmMark`: each pixel of the depth buffer flags the pages its surface needs (the one
//   of its sample and those within `VSM_MARGIN` texels, which its filter may read), at
//   the level whose texel is about its footprint; the first to flag an unbacked page
//   appends its request;
// - `vsmFindPages`: the pool's free pages, and those no pixel needed for `keep_frames`;
//   when the light turned, the needed pages are dirty and the others freed;
// - `vsmAllocate`: requests take free pages, then old ones (their entries lose them),
//   then fail;
// - `vsmCollectDirty`, `vsmArgs`, `vsmClear`: the dirty pages listed, set in the
//   hierarchy the casters are culled with, and cleared to the far plane.
//
// After the casters are drawn into the dirty pages (`forward.slang`), `vsmVisibility`
// writes the sun's visibility per pixel into the sun's mask.
//
// Page table entries (32 bits): flags over the physical page's index (16 bits). The meta
// table holds, per physical page, the entry it backs and the last frame a pixel needed
// it.
//
// The design follows Timberdoodle's virtual shadow maps (Apache-2.0; M. Sakmáry; see
// docs/research/virtual-shadow-maps.md); the code is written for this engine.

#include "vsm_common.slang"

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("r32ui")] RWTexture2D<uint> g_pool_images[];

// Flags of one frame: a page is dirty in the frame that must draw it.
static const uint FRAME_FLAGS = VSM_NEEDED | VSM_DIRTY | VSM_FAILED;
// Meta: the entry a physical page backs is valid.
static const uint OWNED = 1u << 31;
// The far plane, as the depth's bits (1.0).
static const uint FAR_DEPTH = 0x3F800000u;

struct VsmPush
{
    VsmFrame* frame;
    uint* table;        // levels × 64 × 64 entries
    uint2* meta;        // per physical page: x the entry it backs (OWNED | index), y the last frame needed
    uint* requests;     // [0] count, then entry indices
    uint* lists;        // [0] free, [1] old, [2] dirty counts; [4..6] the clear pass's dispatch;
                        // then the free, old and dirty pages (pool_pages each)
    uint* hierarchy;    // per level, its dirty pages (by window position) and their coarser levels
    uint pool;          // bindless: the pool (storage)
    uint depth;         // bindless: the depth buffer (sampled)
    uint mask;          // bindless: the sun's mask (storage; `vsmVisibility`)
    uint flags;         // `vsmVisibility`: bit 0, multiply into what the mask holds
};

[[vk::push_constant]]
ConstantBuffer<VsmPush> push;

// The free list's, old list's and dirty list's first page in `lists`.
static const uint LISTS = 8;
uint free_at(VsmFrame* f) { return LISTS; }
uint old_at(VsmFrame* f) { return LISTS + f.pool_pages; }
uint dirty_at(VsmFrame* f) { return LISTS + 2 * f.pool_pages; }

bool wrapped_bit(uint2 mask, uint i)
{
    return ((i < 32 ? mask.x : mask.y) >> (i & 31u) & 1u) != 0;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmBeginFrame(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    uint e = id.x;
    if (e == 0)
    {
        push.requests[0] = 0;
        push.lists[0] = 0;
        push.lists[1] = 0;
        push.lists[2] = 0;
    }
    uint entries = f.levels * VSM_ENTRIES;
    for (uint cell = e; cell < f.levels * VSM_HIERARCHY_CELLS; cell += entries)
        push.hierarchy[cell] = 0;
    if (e >= entries)
        return;
    uint level = e / VSM_ENTRIES;
    uint local = e % VSM_ENTRIES;
    uint x = local % VSM_TABLE;
    uint y = local / VSM_TABLE;
    VsmLevel l = f.level[level];
    uint t = push.table[e] & ~FRAME_FLAGS;
    if (wrapped_bit(l.wrapped.xy, x) || wrapped_bit(l.wrapped.zw, y))
    {
        if ((t & VSM_ALLOCATED) != 0)
            push.meta[t & VSM_PHYSICAL] = uint2(0, 0);
        t = 0;
    }
    push.table[e] = t;
}

// Flags the pages in `entries` (~0: none; each lane's are distinct), one flag per page
// among the wave's pixels: each turn takes the least page any lane still holds, every
// lane holding it crosses it off, and the first of them flags it, so the wave turns once
// per distinct page. The first flag of an unbacked page, across waves, appends its
// request. Called by the whole wave.
void flag(VsmFrame* f, uint4 entries)
{
    uint next = min(min(entries.x, entries.y), min(entries.z, entries.w));
    while (WaveActiveAnyTrue(next != ~0u))
    {
        uint page = WaveActiveMin(next);
        bool holds = any(entries == page);
        // Flagged already this frame (by an earlier wave): nothing to do.
        if (holds && WavePrefixCountBits(holds) == 0 && (push.table[page] & VSM_NEEDED) == 0)
        {
            uint previous;
            InterlockedOr(push.table[page], VSM_NEEDED, previous);
            if ((previous & (VSM_ALLOCATED | VSM_NEEDED)) == 0)
            {
                uint slot;
                InterlockedAdd(push.requests[0], 1u, slot);
                if (slot < f.max_requests)
                    push.requests[1 + slot] = page;
            }
        }
        entries = select(entries == page, ~0u, entries);
        next = min(min(entries.x, entries.y), min(entries.z, entries.w));
    }
}

[shader("compute")]
[numthreads(8, 8, 1)]
void vsmMark(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    // The pages the pixel needs: up to 2 × 2 around its sample (~0: none).
    uint4 entries = uint4(~0u, ~0u, ~0u, ~0u);
    if (all(id.xy < f.depth_size))
    {
        float depth = g_textures[NonUniformResourceIndex(push.depth)].Load(int3(id.xy, 0)).r;
        if (depth < 1.0)
        {
            float4 clip = float4((float2(id.xy) + 0.5) * f.ndc_per_pixel - 1.0, depth, 1.0);
            float w = 1.0 / dot(f.clip_to_view[3], clip);
            float3 view = float3(dot(f.clip_to_view[0], clip), dot(f.clip_to_view[1], clip), dot(f.clip_to_view[2], clip)) * w;
            float2 light_xy = float2(dot(f.clip_to_light[0], clip), dot(f.clip_to_light[1], clip)) * w;
            // A level's window may not hold the sample's whole footprint (it sits up to a
            // page off the camera, and a narrow field of view picks fine levels for far
            // surfaces): the first coarser level whose window does.
            for (uint level = vsm_level_for(f, dot(view, view)); level < f.levels; level++)
            {
                float2 t = vsm_texel(f, level, light_xy);
                int2 lo = vsm_page_of(t - VSM_MARGIN);
                int2 hi = vsm_page_of(t + VSM_MARGIN);
                if (!vsm_in_window(f, level, lo) || !vsm_in_window(f, level, hi))
                    continue;
                entries.x = vsm_entry(level, lo);
                if (hi.x != lo.x)
                    entries.y = vsm_entry(level, int2(hi.x, lo.y));
                if (hi.y != lo.y)
                    entries.z = vsm_entry(level, int2(lo.x, hi.y));
                if (hi.x != lo.x && hi.y != lo.y)
                    entries.w = vsm_entry(level, hi);
                break;
            }
        }
    }
    flag(f, entries);
}

// The entry `meta`'s page backs.
uint owner_entry(uint meta_x)
{
    return meta_x & ~OWNED;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmFindPages(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    uint p = id.x;
    if (p >= f.pool_pages)
        return;
    uint2 m = push.meta[p];
    bool free = (m.x & OWNED) == 0;
    if (!free)
    {
        uint owner = owner_entry(m.x);
        uint t = push.table[owner];
        // Drawn for another light: drawn again if needed, freed if not.
        bool stale = (f.level[owner / VSM_ENTRIES].flags & 1u) != 0;
        if ((t & VSM_NEEDED) != 0)
        {
            push.meta[p].y = f.frame;
            if (stale)
                push.table[owner] = t | VSM_DIRTY;
        }
        else if (stale)
        {
            push.table[owner] = 0;
            push.meta[p] = uint2(0, 0);
            free = true;
        }
        else if (f.frame - m.y > f.keep_frames)
        {
            uint slot;
            InterlockedAdd(push.lists[1], 1u, slot);
            push.lists[old_at(f) + slot] = p;
        }
    }
    if (free)
    {
        uint slot;
        InterlockedAdd(push.lists[0], 1u, slot);
        push.lists[free_at(f) + slot] = p;
    }
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmAllocate(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    uint i = id.x;
    uint count = min(push.requests[0], f.max_requests);
    if (i >= count)
        return;
    uint e = push.requests[1 + i];
    uint free_count = push.lists[0];
    uint old_count = push.lists[1];
    uint p;
    if (i < free_count)
    {
        p = push.lists[free_at(f) + i];
    }
    else if (i - free_count < old_count)
    {
        p = push.lists[old_at(f) + i - free_count];
        // Its entry was not needed this frame, so no request is for it: it loses the page.
        push.table[owner_entry(push.meta[p].x)] = 0;
    }
    else
    {
        // Asked for again next frame.
        push.table[e] = VSM_NEEDED | VSM_FAILED;
        return;
    }
    push.table[e] = VSM_ALLOCATED | VSM_DIRTY | VSM_NEEDED | p;
    push.meta[p] = uint2(OWNED | e, f.frame);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmCollectDirty(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    uint e = id.x;
    if (e >= f.levels * VSM_ENTRIES)
        return;
    uint t = push.table[e];
    if ((t & (VSM_ALLOCATED | VSM_DIRTY)) != (VSM_ALLOCATED | VSM_DIRTY))
        return;
    uint slot;
    InterlockedAdd(push.lists[2], 1u, slot);
    push.lists[dirty_at(f) + slot] = t & VSM_PHYSICAL;
    // The page's place in its window (not in the table, which wraps), and the cells
    // holding it at each coarser level of the hierarchy.
    uint level = e / VSM_ENTRIES;
    uint local = e % VSM_ENTRIES;
    uint2 at = (uint2(local % VSM_TABLE, local / VSM_TABLE) - uint2(f.level[level].first_page)) & (VSM_TABLE - 1);
    uint* cells = push.hierarchy + level * VSM_HIERARCHY_CELLS;
    cells[at.y * VSM_TABLE + at.x] = 1;
    InterlockedOr(cells[VSM_HIERARCHY_ROWS + 2 * at.y + at.x / 32], 1u << (at.x % 32));
    for (uint m = 1; m < VSM_HIERARCHY_MIPS; m++)
    {
        uint side = VSM_TABLE >> m;
        InterlockedOr(cells[vsm_hierarchy_mip(m) + (at.y >> m) * side + (at.x >> m)], 1u);
    }
}

[shader("compute")]
[numthreads(1, 1, 1)]
void vsmArgs(uint3 id : SV_DispatchThreadID)
{
    push.lists[4] = push.lists[2];
    push.lists[5] = 1;
    push.lists[6] = 1;
}

// One workgroup per dirty page; each step of its 8 × 8 threads clears an 8 × 8 block.
[shader("compute")]
[numthreads(8, 8, 1)]
void vsmClear(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID)
{
    VsmFrame* f = push.frame;
    uint p = push.lists[dirty_at(f) + group.x];
    uint2 origin = vsm_pool_texel(p, local.xy);
    RWTexture2D<uint> pool = g_pool_images[NonUniformResourceIndex(push.pool)];
    for (uint y = 0; y < VSM_PAGE; y += 8)
        for (uint x = 0; x < VSM_PAGE; x += 8)
            pool[origin + uint2(x, y)] = FAR_DEPTH;
}

// The receiver's offset along its normal (from the depth), and its depth bias, in texels
// of its level: the shadow pass draws the casters' faces turned away from the light, so
// lit surfaces stand well in front of what their own pages hold.
static const float NORMAL_OFFSET_TEXELS = 1.0;
static const float DEPTH_BIAS_TEXELS = 0.5;

// Pixel `p` of the depth buffer (depth `depth`) relative to the camera.
float3 view_position(VsmFrame* f, int2 p, float depth)
{
    float4 clip = float4((float2(p) + 0.5) * f.ndc_per_pixel - 1.0, depth, 1.0);
    float4 h = float4(dot(f.clip_to_view[0], clip), dot(f.clip_to_view[1], clip), dot(f.clip_to_view[2], clip), dot(f.clip_to_view[3], clip));
    return h.xyz / h.w;
}

// The sun's visibility from the pages (ADR 0093 step 3b), per pixel of the depth buffer.
// The level is the one the pixel marked (its window holds the filter's footprint); the
// receiver moves along its normal (from the depth: on each axis the neighbour nearer in
// depth) by a texel of the level; then a 3 × 3 tent filter of comparisons over 4 × 4
// texels, as the cascades' 3 × 3 bilinear comparisons weigh them, each texel read through
// the page table (the footprint spans at most 2 × 2 pages). Written into the sun's mask
// (R32: a float's bits), or multiplied into what the contact shadows wrote there.
[shader("compute")]
[numthreads(8, 8, 1)]
void vsmVisibility(uint3 id : SV_DispatchThreadID)
{
    VsmFrame* f = push.frame;
    if (any(id.xy >= f.depth_size))
        return;
    Texture2D depths = g_textures[NonUniformResourceIndex(push.depth)];
    RWTexture2D<uint> mask = g_pool_images[NonUniformResourceIndex(push.mask)];
    int2 p = int2(id.xy);
    float depth = depths.Load(int3(p, 0)).r;
    float visible = 1.0;
    if (depth < 1.0)
    {
        float3 c = view_position(f, p, depth);
        // The surface's normal from its neighbours, the nearer one in depth on each axis.
        int2 last = int2(f.depth_size) - 1;
        int2 xs = int2(max(p.x - 1, 0), min(p.x + 1, last.x));
        int2 ys = int2(max(p.y - 1, 0), min(p.y + 1, last.y));
        float3 left = view_position(f, int2(xs.x, p.y), depths.Load(int3(xs.x, p.y, 0)).r);
        float3 right = view_position(f, int2(xs.y, p.y), depths.Load(int3(xs.y, p.y, 0)).r);
        float3 up = view_position(f, int2(p.x, ys.x), depths.Load(int3(p.x, ys.x, 0)).r);
        float3 down = view_position(f, int2(p.x, ys.y), depths.Load(int3(p.x, ys.y, 0)).r);
        float3 dx = abs(dot(right - c, right - c)) < abs(dot(c - left, c - left)) ? right - c : c - left;
        float3 dy = abs(dot(down - c, down - c)) < abs(dot(c - up, c - up)) ? down - c : c - up;
        float3 n = cross(dx, dy);
        n = dot(n, n) > 0.0 ? normalize(n) : float3(0.0, 0.0, 0.0);
        // Facing the camera (`c` points from it to the surface).
        if (dot(n, c) > 0.0)
            n = -n;
        float2 light_xy = float2(dot(f.light_rows[0].xyz, c), dot(f.light_rows[1].xyz, c)) + f.light_eye.xy;
        uint level = f.levels;
        for (uint k = vsm_level_for(f, dot(c, c)); k < f.levels; k++)
        {
            float2 t = vsm_texel(f, k, light_xy);
            if (vsm_in_window(f, k, vsm_page_of(t - VSM_MARGIN)) && vsm_in_window(f, k, vsm_page_of(t + VSM_MARGIN)))
            {
                level = k;
                break;
            }
        }
        if (level < f.levels)
        {
            float texel = 1.0 / (f.level[level].page_scale * float(VSM_PAGE));
            float3 r = c + n * (NORMAL_OFFSET_TEXELS * texel);
            float2 at = float2(dot(f.light_rows[0].xyz, r), dot(f.light_rows[1].xyz, r)) + f.light_eye.xy;
            float receiver = -(dot(f.light_rows[2].xyz, r) + f.light_eye.z) * f.depth_scale + f.depth_offset;
            receiver -= DEPTH_BIAS_TEXELS * texel * f.depth_scale;
            // Texel centers lie at half texels: the 4 × 4 texels from `i0 - 1`.
            float2 t = vsm_texel(f, level, at) - 0.5;
            float2 i0 = floor(t);
            float2 fr = t - i0;
            int2 first = int2(i0) - 1;
            int2 lo = first >> 6;
            int2 hi = (first + 3) >> 6;
            uint e00 = push.table[vsm_entry(level, lo)];
            uint e10 = push.table[vsm_entry(level, int2(hi.x, lo.y))];
            uint e01 = push.table[vsm_entry(level, int2(lo.x, hi.y))];
            uint e11 = push.table[vsm_entry(level, hi)];
            RWTexture2D<uint> pool = g_pool_images[NonUniformResourceIndex(push.pool)];
            float4 wx = float4(1.0 - fr.x, 1.0, 1.0, fr.x);
            float4 wy = float4(1.0 - fr.y, 1.0, 1.0, fr.y);
            float sum = 0.0;
            [unroll]
            for (int y = 0; y < 4; y++)
            {
                [unroll]
                for (int x = 0; x < 4; x++)
                {
                    int2 texel_at = first + int2(x, y);
                    int2 page = texel_at >> 6;
                    uint entry = page.y == lo.y ? (page.x == lo.x ? e00 : e10) : (page.x == lo.x ? e01 : e11);
                    float lit = 1.0;
                    if ((entry & VSM_ALLOCATED) != 0)
                    {
                        uint2 in_pool = vsm_pool_texel(entry & VSM_PHYSICAL, uint2(texel_at & 63));
                        lit = receiver <= asfloat(pool[in_pool]) ? 1.0 : 0.0;
                    }
                    sum += wx[x] * wy[y] * lit;
                }
            }
            visible = sum / 9.0;
        }
    }
    if ((push.flags & 1u) != 0)
        visible *= asfloat(mask[id.xy]);
    mask[id.xy] = asuint(visible);
}
