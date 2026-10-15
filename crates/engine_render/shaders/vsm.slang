// Virtual shadow maps for the sun (ADR 0093 step 3): levels of 4096 × 4096 virtual
// texels around the camera, each covering twice the last, cut into pages of 64 × 64
// texels; only the pages a frame's pixels need are backed, by pages of one pool kept
// across frames.
//
// Addresses: a level's page `p` (absolute, in the light's xy) is valid while it lies in
// the level's window of 64 × 64 pages (`Level::first_page`), and its entry in the
// level's page table is `p mod 64` per axis, so the table scrolls with the window: the
// rows and columns whose page changed are freed when the window moves (`wrapped`).
//
// The bookkeeping, in the order a frame runs it (step 3a):
// - `vsmBeginFrame`: a frame's flags cleared, wrapped entries freed;
// - `vsmMark`: each pixel of the depth buffer flags the page its surface needs, at the
//   level whose texel is about its footprint; the first to flag an unbacked page appends
//   its request;
// - `vsmFindPages`: the pool's free pages, and those no pixel needed for `keep_frames`;
//   when the light turned, the needed pages are dirty and the others freed;
// - `vsmAllocate`: requests take free pages, then old ones (their entries lose them),
//   then fail;
// - `vsmCollectDirty`, `vsmArgs`, `vsmClear`: the dirty pages, cleared to the far plane.
//
// Page table entries (32 bits): the flags below over the physical page's index (16
// bits). The meta table holds, per physical page, the entry it backs and the last frame
// a pixel needed it.
//
// The design follows Timberdoodle's virtual shadow maps (Apache-2.0; M. Sakmáry; see
// docs/research/virtual-shadow-maps.md); the code is written for this engine.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("r32ui")] RWTexture2D<uint> g_pool_images[];

static const uint PAGE = 64;
static const uint TABLE = 64;
static const uint ENTRIES = TABLE * TABLE;
static const uint MAX_LEVELS = 16;

static const uint ALLOCATED = 1u << 31;
static const uint NEEDED = 1u << 30;
static const uint DIRTY = 1u << 29;
static const uint FAILED = 1u << 28;
// Flags of one frame: a page is dirty in the frame that must draw it.
static const uint FRAME_FLAGS = NEEDED | DIRTY | FAILED;
static const uint PHYSICAL = 0xFFFFu;
// Meta: the entry a physical page backs is valid.
static const uint OWNED = 1u << 31;
// The far plane, as the depth's bits (1.0).
static const uint FAR_DEPTH = 0x3F800000u;

// `LevelData` in vsm.rs (32 bytes).
struct Level
{
    int2 first_page;   // the window's first page, in absolute page coordinates
    float page_scale;  // pages per metre across the light
    uint flags;        // bit 0: the pages were drawn for another light
    uint4 wrapped;     // columns (xy: bits 0-31, 32-63) and rows (zw) whose pages changed
};

// `FrameData` in vsm.rs (656 bytes). A depth buffer's clip position `c` (as drawn, its
// jitter included) is at `clip_to_view · c` from the camera and at `clip_to_light · c`
// across the light, both over `clip_to_view[3] · c`.
struct Frame
{
    float4 clip_to_light[2];    // rows: clip → light xy (homogeneous)
    float4 clip_to_view[4];     // rows: clip → world relative to the camera (homogeneous)
    float2 ndc_per_pixel;       // 2 / the depth buffer's size
    uint2 depth_size;           // the depth buffer's size
    float level_offset;         // log2 of (a pixel's footprint per metre / the first texel), plus the bias
    uint levels;                // levels in use
    uint frame;                 // this frame's number
    uint pool_pages;            // physical pages in the pool
    uint keep_frames;           // frames a page no pixel needs stays allocated
    uint max_requests;          // room in the request list
    uint _pad[2];
    Level level[MAX_LEVELS];
};

struct VsmPush
{
    Frame* frame;
    uint* table;        // levels × 64 × 64 entries
    uint2* meta;        // per physical page: x the entry it backs (OWNED | index), y the last frame needed
    uint* requests;     // [0] count, then entry indices
    uint* lists;        // [0] free, [1] old, [2] dirty counts; [4..6] the clear pass's dispatch;
                        // then the free, old and dirty pages (pool_pages each)
    uint pool;          // bindless: the pool (storage)
    uint depth;         // bindless: the depth buffer (sampled)
};

[[vk::push_constant]]
ConstantBuffer<VsmPush> push;

// The free list's, old list's and dirty list's first page in `lists`.
static const uint LISTS = 8;
uint free_at(Frame* f) { return LISTS; }
uint old_at(Frame* f) { return LISTS + f.pool_pages; }
uint dirty_at(Frame* f) { return LISTS + 2 * f.pool_pages; }

bool wrapped_bit(uint2 mask, uint i)
{
    return ((i < 32 ? mask.x : mask.y) >> (i & 31u) & 1u) != 0;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmBeginFrame(uint3 id : SV_DispatchThreadID)
{
    Frame* f = push.frame;
    uint e = id.x;
    if (e == 0)
    {
        push.requests[0] = 0;
        push.lists[0] = 0;
        push.lists[1] = 0;
        push.lists[2] = 0;
    }
    if (e >= f.levels * ENTRIES)
        return;
    uint level = e / ENTRIES;
    uint local = e % ENTRIES;
    uint x = local % TABLE;
    uint y = local / TABLE;
    Level l = f.level[level];
    uint t = push.table[e] & ~FRAME_FLAGS;
    if (wrapped_bit(l.wrapped.xy, x) || wrapped_bit(l.wrapped.zw, y))
    {
        if ((t & ALLOCATED) != 0)
            push.meta[t & PHYSICAL] = uint2(0, 0);
        t = 0;
    }
    push.table[e] = t;
}

// The level a surface `distance_squared` from the camera wants: its texel about its
// pixel's footprint (rounded to the finer level), clamped to the levels in use.
uint level_for(Frame* f, float distance_squared)
{
    float k = 0.5 * log2(max(distance_squared, 1e-8)) + f.level_offset;
    return uint(clamp(floor(k), 0.0, float(f.levels - 1)));
}

// The entry of the page holding `light_xy` at `level`, or ~0 when it lies outside the
// level's window.
uint entry_at(Frame* f, uint level, float2 light_xy)
{
    Level l = f.level[level];
    int2 page = int2(floor(light_xy * l.page_scale));
    if (any(uint2(page - l.first_page) >= TABLE))
        return ~0u;
    uint2 wrapped = uint2(page) & (TABLE - 1);
    return level * ENTRIES + wrapped.y * TABLE + wrapped.x;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void vsmMark(uint3 id : SV_DispatchThreadID)
{
    Frame* f = push.frame;
    uint entry = ~0u;
    if (all(id.xy < f.depth_size))
    {
        float depth = g_textures[NonUniformResourceIndex(push.depth)].Load(int3(id.xy, 0)).r;
        if (depth < 1.0)
        {
            float4 clip = float4((float2(id.xy) + 0.5) * f.ndc_per_pixel - 1.0, depth, 1.0);
            float w = 1.0 / dot(f.clip_to_view[3], clip);
            float3 view = float3(dot(f.clip_to_view[0], clip), dot(f.clip_to_view[1], clip), dot(f.clip_to_view[2], clip)) * w;
            float2 light_xy = float2(dot(f.clip_to_light[0], clip), dot(f.clip_to_light[1], clip)) * w;
            // A level's window may not reach the surface (it sits up to a page off the
            // camera, and a narrow field of view picks fine levels for far surfaces):
            // the first coarser level whose window does.
            for (uint level = level_for(f, dot(view, view)); entry == ~0u && level < f.levels; level++)
                entry = entry_at(f, level, light_xy);
        }
    }
    // One flag per page among a wave's pixels: the lanes still looping take the first
    // one's entry; those holding it leave, the lowest of them flags it. The first flag of
    // an unbacked page, across waves, appends its request.
    bool active = entry != ~0u;
    while (active)
    {
        uint first = WaveReadLaneFirst(entry);
        if (entry == first)
        {
            active = false;
            // Flagged already this frame (by an earlier wave): nothing to do.
            if (WavePrefixCountBits(true) == 0 && (push.table[first] & NEEDED) == 0)
            {
                uint previous;
                InterlockedOr(push.table[first], NEEDED, previous);
                if ((previous & (ALLOCATED | NEEDED)) == 0)
                {
                    uint slot;
                    InterlockedAdd(push.requests[0], 1u, slot);
                    if (slot < f.max_requests)
                        push.requests[1 + slot] = first;
                }
            }
        }
    }
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
    Frame* f = push.frame;
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
        bool stale = (f.level[owner / ENTRIES].flags & 1u) != 0;
        if ((t & NEEDED) != 0)
        {
            push.meta[p].y = f.frame;
            if (stale)
                push.table[owner] = t | DIRTY;
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
    Frame* f = push.frame;
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
        push.table[e] = NEEDED | FAILED;
        return;
    }
    push.table[e] = ALLOCATED | DIRTY | NEEDED | p;
    push.meta[p] = uint2(OWNED | e, f.frame);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void vsmCollectDirty(uint3 id : SV_DispatchThreadID)
{
    Frame* f = push.frame;
    uint e = id.x;
    if (e >= f.levels * ENTRIES)
        return;
    uint t = push.table[e];
    if ((t & (ALLOCATED | DIRTY)) == (ALLOCATED | DIRTY))
    {
        uint slot;
        InterlockedAdd(push.lists[2], 1u, slot);
        push.lists[dirty_at(f) + slot] = t & PHYSICAL;
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
    Frame* f = push.frame;
    uint p = push.lists[dirty_at(f) + group.x];
    uint2 origin = uint2(p % TABLE, p / TABLE) * PAGE + local.xy;
    RWTexture2D<uint> pool = g_pool_images[NonUniformResourceIndex(push.pool)];
    for (uint y = 0; y < PAGE; y += 8)
        for (uint x = 0; x < PAGE; x += 8)
            pool[origin + uint2(x, y)] = FAR_DEPTH;
}
