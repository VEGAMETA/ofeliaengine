// The sun's virtual shadow pages (ADR 0093 step 3), shared by their bookkeeping
// (`vsm.slang`) and the passes that draw into them and read them: constants, the frame's
// data and addressing.
//
// Levels of 4096 × 4096 virtual texels around the camera, each covering twice the last,
// cut into pages of 64 × 64 texels. A level's page `p` (absolute, in pages across the
// light) lies in its window of 64 × 64 pages from `VsmLevel::first_page`, and its entry in
// the level's page table is `p mod 64` per axis: the table scrolls with the window. A
// level's virtual texel `t` (absolute) is texel `t mod 64` of page `t / 64`.

static const uint VSM_PAGE = 64;          // texels of a page, per axis
static const uint VSM_TABLE = 64;         // pages of a level's window, per axis
static const uint VSM_ENTRIES = VSM_TABLE * VSM_TABLE;
static const uint VSM_MAX_LEVELS = 16;
// Texels around a pixel's sample that its filter and normal offset may read: the pages
// within it are backed too (`MARK_MARGIN_TEXELS` in vsm.rs).
static const float VSM_MARGIN = 4.0;

// Cells of a level's hierarchy of dirty pages: its 64 × 64 pages by their place in the
// window, then each coarser level of them (2 × 2 cells into one) down to 1 × 1, a `uint`
// each, 1 where a dirty page is under it; then (from `VSM_HIERARCHY_ROWS`, a cell of
// padding before it) the dirty pages as bits, a row of the window in two words: page
// (x, y) is bit x % 32 of word 2y + x / 32 (`HIERARCHY_CELLS` in vsm.rs).
static const uint VSM_HIERARCHY_MIPS = 7;
static const uint VSM_HIERARCHY_ROWS = 5462;
static const uint VSM_HIERARCHY_CELLS = VSM_HIERARCHY_ROWS + 2 * VSM_TABLE;

// The first cell of mip `m` of a level's hierarchy.
uint vsm_hierarchy_mip(uint m)
{
    // 4096 + 1024 + 256 + 64 + 16 + 4 cells before mip 6.
    return (4096u - (4096u >> (2 * m))) * 4u / 3u;
}

// Page table entries: these flags over the physical page's index.
static const uint VSM_ALLOCATED = 1u << 31;
static const uint VSM_NEEDED = 1u << 30;
static const uint VSM_DIRTY = 1u << 29;
static const uint VSM_FAILED = 1u << 28;
static const uint VSM_PHYSICAL = 0xFFFFu;

// `LevelData` in vsm.rs (32 bytes).
struct VsmLevel
{
    int2 first_page;   // the window's first page, in absolute page coordinates
    float page_scale;  // pages per metre across the light
    uint flags;        // bit 0: the pages were drawn for another light
    uint4 wrapped;     // columns (xy: bits 0-31, 32-63) and rows (zw) whose pages changed
};

// `FrameData` in vsm.rs (736 bytes). A depth buffer's clip position `c` (as drawn, its
// jitter included) is at `clip_to_view · c` from the camera and at `clip_to_light · c`
// across the light, both over `clip_to_view[3] · c`.
struct VsmFrame
{
    float4 clip_to_light[2];    // rows: clip → light xy (homogeneous)
    float4 clip_to_view[4];     // rows: clip → world relative to the camera (homogeneous)
    float4 light_rows[3];       // rows: world → light (xy across the light, z against it)
    float4 light_eye;           // xyz: the camera in light space
    float2 ndc_per_pixel;       // 2 / the depth buffer's size
    uint2 depth_size;           // the depth buffer's size
    float level_offset;         // log2 of (a pixel's footprint per metre / the first texel), plus the bias
    uint levels;                // levels in use
    uint frame;                 // this frame's number
    uint pool_pages;            // physical pages in the pool
    uint keep_frames;           // frames a page no pixel needs stays allocated
    uint max_requests;          // room in the request list
    float depth_scale;          // a point's depth in the pages: −light.z × scale + offset
    float depth_offset;
    VsmLevel level[VSM_MAX_LEVELS];
    float4* moved;              // what moved since the pages were drawn: xy min, zw max across the light (metres)
    uint moved_count;
    uint moved_pad;
};

// The level a surface `distance_squared` from the camera wants: its texel about its
// pixel's footprint (rounded to the finer level), clamped to the levels in use.
uint vsm_level_for(VsmFrame* f, float distance_squared)
{
    float k = 0.5 * log2(max(distance_squared, 1e-8)) + f.level_offset;
    return uint(clamp(floor(k), 0.0, float(f.levels - 1)));
}

// Where `light_xy` (metres across the light) is at `level`, in its absolute virtual
// texels.
float2 vsm_texel(VsmFrame* f, uint level, float2 light_xy)
{
    return light_xy * (f.level[level].page_scale * float(VSM_PAGE));
}

// The absolute page holding absolute virtual texel position `t`.
int2 vsm_page_of(float2 t)
{
    return int2(floor(t * (1.0 / float(VSM_PAGE))));
}

// Whether absolute page `page` lies in `level`'s window.
bool vsm_in_window(VsmFrame* f, uint level, int2 page)
{
    return all(uint2(page - f.level[level].first_page) < VSM_TABLE);
}

// The page table entry (its index) of absolute page `page` at `level`.
uint vsm_entry(uint level, int2 page)
{
    uint2 wrapped = uint2(page) & (VSM_TABLE - 1);
    return level * VSM_ENTRIES + wrapped.y * VSM_TABLE + wrapped.x;
}

// Texel `texel` (0 to 63 per axis) of physical page `physical`, in the pool.
uint2 vsm_pool_texel(uint physical, uint2 texel)
{
    return uint2(physical % VSM_TABLE, physical / VSM_TABLE) * VSM_PAGE + texel;
}

// The receiver's offset along its normal, and its depth bias, in texels of its level: the
// shadow passes draw the casters' faces turned away from the light, so lit surfaces stand
// well in front of what their own pages hold.
static const float VSM_NORMAL_OFFSET_TEXELS = 1.0;
static const float VSM_DEPTH_BIAS_TEXELS = 0.5;

// A position relative to the camera, across the light (metres).
float2 vsm_light_xy(VsmFrame* f, float3 c)
{
    return float2(dot(f.light_rows[0].xyz, c), dot(f.light_rows[1].xyz, c)) + f.light_eye.xy;
}

// A position relative to the camera, as a depth in the pages.
float vsm_depth_of(VsmFrame* f, float3 c)
{
    return -(dot(f.light_rows[2].xyz, c) + f.light_eye.z) * f.depth_scale + f.depth_offset;
}

// The level a surface at `c` (relative to the camera; `light_xy` across the light) marks
// and reads: from the one its footprint wants, the first whose window holds its sample and
// `VSM_MARGIN` texels around it (a window sits up to a page off the camera, and a narrow
// field of view picks fine levels for far surfaces). `f.levels` when none does.
uint vsm_level_holding(VsmFrame* f, float3 c, float2 light_xy)
{
    for (uint level = vsm_level_for(f, dot(c, c)); level < f.levels; level++)
    {
        float2 t = vsm_texel(f, level, light_xy);
        if (vsm_in_window(f, level, vsm_page_of(t - VSM_MARGIN)) && vsm_in_window(f, level, vsm_page_of(t + VSM_MARGIN)))
            return level;
    }
    return f.levels;
}

// The table entries of the up to 2 × 2 pages within `VSM_MARGIN` texels of absolute
// virtual texel position `t` at `level` (~0: none), which a sample there may read.
uint4 vsm_margin_entries(uint level, float2 t)
{
    int2 lo = vsm_page_of(t - VSM_MARGIN);
    int2 hi = vsm_page_of(t + VSM_MARGIN);
    uint4 entries = uint4(vsm_entry(level, lo), ~0u, ~0u, ~0u);
    if (hi.x != lo.x)
        entries.y = vsm_entry(level, int2(hi.x, lo.y));
    if (hi.y != lo.y)
        entries.z = vsm_entry(level, int2(lo.x, hi.y));
    if (hi.x != lo.x && hi.y != lo.y)
        entries.w = vsm_entry(level, hi);
    return entries;
}

// Flags page table entry `e` as needed this frame, one at a time (a fragment's: no wave
// operations); the first flag of an unbacked page appends its request.
void vsm_flag_entry(VsmFrame* f, uint* table, uint* requests, uint e)
{
    // Flagged already this frame (by the depth's pixels or another fragment).
    if ((table[e] & VSM_NEEDED) != 0)
        return;
    uint previous;
    InterlockedOr(table[e], VSM_NEEDED, previous);
    if ((previous & (VSM_ALLOCATED | VSM_NEEDED)) == 0)
    {
        uint slot;
        InterlockedAdd(requests[0], 1u, slot);
        if (slot < f.max_requests)
            requests[1 + slot] = e;
    }
}

// The sun's visibility at absolute virtual texel position `t` of `level` for a receiver
// at depth `receiver` in the pages: a 3 × 3 tent filter of comparisons over the 4 × 4
// texels around it, as the cascades' 3 × 3 bilinear comparisons weigh them, each texel
// read through the page table `table` (the footprint spans at most 2 × 2 pages; texels of
// unbacked pages are lit).
float vsm_filter(uint* table, RWTexture2D<uint> pool, uint level, float2 t, float receiver)
{
    // Texel centers lie at half texels: the 4 × 4 texels from `i0 - 1`.
    t -= 0.5;
    float2 i0 = floor(t);
    float2 fr = t - i0;
    int2 first = int2(i0) - 1;
    int2 lo = first >> 6;
    int2 hi = (first + 3) >> 6;
    // Most footprints lie in one page: its entry is read once.
    uint e00 = table[vsm_entry(level, lo)];
    uint e10 = hi.x != lo.x ? table[vsm_entry(level, int2(hi.x, lo.y))] : e00;
    uint e01 = hi.y != lo.y ? table[vsm_entry(level, int2(lo.x, hi.y))] : e00;
    uint e11 = any(hi != lo) ? table[vsm_entry(level, hi)] : e00;
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
            // Read whether the page is backed or not (then from page 0, unused): the 16
            // reads go out together.
            uint2 in_pool = vsm_pool_texel(entry & VSM_PHYSICAL, uint2(texel_at & 63));
            float stored = asfloat(pool[in_pool]);
            float lit = (entry & VSM_ALLOCATED) == 0 || receiver <= stored ? 1.0 : 0.0;
            sum += wx[x] * wy[y] * lit;
        }
    }
    return sum / 9.0;
}
