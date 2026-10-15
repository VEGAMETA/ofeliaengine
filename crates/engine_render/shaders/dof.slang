// Depth of field (ADR 0095, dof.rs, docs/research/depth-of-field.md): the passes of AMD
// FidelityFX DOF 1.1, ported from FidelityFX SDK v1.1.4 (`sdk/include/FidelityFX/gpu/dof`,
// MIT, Copyright (C) 2024 Advanced Micro Devices, Inc.; THIRD_PARTY_NOTICES.md), modified:
//
// - Where FidelityFX keeps signed circles of confusion (its depth tiles and its mips), these
//   passes keep the inverse depth q = near / distance (1 at the near plane). A pixel's near
//   and far radii are two separate non-negative values computed from it, each zero outside
//   its own field (`near_radius`, `far_radius`); no value stands for both (the research
//   note's patent check).
// - The image blurred is the one the tone mapper reads, after antialiasing and upscaling;
//   the depth is the scene's at render size, read under each pixel.
// - The blur's ring count, and so the mip it samples, is the same for its whole workgroup
//   (FidelityFX: per wave): the mip's texture is picked by a uniform index.
// - The composite writes an image of its own: no workgroup reads pixels another writes.
// - The near field starts at a radius of one pixel (`NEAR_START`), not at the plane in
//   focus.
// - No ring merging (FidelityFX reads inner rings of flat tiles from coarser mips: a
//   bright point's disk becomes a cone).
// - 32-bit arithmetic only (FidelityFX's half-precision variants are left out).
//
// Radii are in pixels of the half-size image the gather runs on.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(1, 0)]] [vk::image_format("rg16f")] RWTexture2D<float2> g_tile_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint* max_tile_radius;   // the largest reach of a tile, in tiles (atomic max)
    uint color;              // sampled: the image blurred
    uint depth;              // sampled: the scene's depth (render size)
    uint tiles;              // per 8 × 8 pixels: (nearest q, farthest q); storage, then sampled
    uint dilated;            // the tiles spread by their radii; storage, then sampled
    uint mips[4];            // half size and below: color and q; storage, then sampled
    uint near_field;         // half size: near color and opacity; storage, then sampled
    uint far_field;          // half size: far color and weight; storage, then sampled
    uint output;             // storage: the composite
    uint linear_sampler;
    uint point_sampler;
    uint max_rings;          // most rings of the gather (quality)
    uint2 size;              // the image blurred
    uint2 half_size;         // mip 0
    uint2 depth_size;        // the depth
    float depth_scale;       // q = 1 - depth * depth_scale
    float coc_slope;         // the thin lens: near radius = slope * q - coc, far = coc - slope * q
    float coc;
    float limit;             // the largest radius
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

// Pixels of the image per tile side.
static const uint TILE = 8;

// The near field starts at this radius (one half-size pixel; the composite's fixed blur
// covers the radii below). FidelityFX starts it at 0: then a surface barely in front of
// the plane in focus joins the near field with the full weight of a sharp pixel (1 / r²)
// and an opacity of 1, and a blurred object in front of it no longer spreads over it (a
// hard silhouette, Sponza's curtains).
static const float NEAR_START = 1.0;

float depth_q(float depth)
{
    return 1.0 - depth * push.depth_scale;
}

// A pixel's radius in the near field: positive in front of the plane in focus, else 0.
float near_radius(float q)
{
    return clamp(push.coc_slope * q - push.coc, 0.0, push.limit);
}

// Its radius in the far field: positive behind the plane in focus, else 0.
float far_radius(float q)
{
    return clamp(push.coc - push.coc_slope * q, 0.0, push.limit);
}

// The radius of its circle of confusion, whichever field (at most one is not 0).
float coc_radius(float q)
{
    return near_radius(q) + far_radius(q);
}

// How far apart two pixels' circles of confusion are, in radius across the plane in focus.
float coc_distance(float q0, float q1)
{
    return abs(near_radius(q0) - near_radius(q1)) + abs(far_radius(q0) - far_radius(q1));
}

// A radius in tiles that it reaches (+1 for diagonals and bilinear reads).
uint2 radius_in_tiles(float2 radius)
{
    return uint2(ceil(radius / float(TILE / 2))) + 1;
}

float scene_depth_at(int2 pixel)
{
    int2 p = int2((float2(pixel) + 0.5) * float2(push.depth_size) / float2(push.size));
    p = clamp(p, int2(0, 0), int2(push.depth_size) - 1);
    return g_textures[push.depth].Load(int3(p, 0)).r;
}

// ---- Tiles ------------------------------------------------------------------------------

// The nearest and farthest q of each tile, and the largest reach of any tile.
[shader("compute")]
[numthreads(8, 8, 1)]
void dofTiles(uint3 id : SV_DispatchThreadID)
{
    uint2 tiles = (push.size + TILE - 1) / TILE;
    bool inside = all(id.xy < tiles);
    float lowest = 1.0;
    float highest = 0.0;
    if (inside)
    {
        Texture2D depth = g_textures[push.depth];
        SamplerState s = g_samplers[push.point_sampler];
        float2 rcp_size = 1.0 / float2(push.size);
        // Each gather takes the depth texels around the corner between four pixels.
        for (uint y = 0; y < TILE; y += 2)
        {
            for (uint x = 0; x < TILE; x += 2)
            {
                float2 uv = saturate((float2(id.xy * TILE + uint2(x, y)) + 1.0) * rcp_size);
                float4 d = depth.GatherRed(s, uv);
                lowest = min(lowest, min(min(d.x, d.y), min(d.z, d.w)));
                highest = max(highest, max(max(d.x, d.y), max(d.z, d.w)));
            }
        }
    }
    float nearest = depth_q(lowest);
    float farthest = depth_q(highest);
    uint2 reach = radius_in_tiles(float2(coc_radius(nearest), coc_radius(farthest)));
    uint widest = WaveActiveMax(inside ? max(reach.x, reach.y) : 0u);
    if (WaveIsFirstLane())
        InterlockedMax(push.max_tile_radius[0], widest);
    if (inside)
        g_tile_images[push.tiles][id.xy] = float2(nearest, farthest);
}

// One tile of the dilation: it spreads its nearest q when its near radius reaches the tile,
// its farthest q when its radius there reaches.
void dilate_step(inout float nearest, inout float farthest, int x, int y, int r, int2 tiles, int2 tile)
{
    if (x * x + y * y <= r * r)
    {
        int2 t = tile + int2(x, y);
        if (all(t >= 0) && all(t < tiles))
        {
            float2 range = g_textures[push.tiles].Load(int3(t, 0)).xy;
            uint2 reach = radius_in_tiles(float2(coc_radius(range.x), coc_radius(range.y)));
            int d2 = x * x + y * y;
            if (d2 < int(reach.x * reach.x))
                nearest = max(nearest, range.x);
            if (d2 < int(reach.y * reach.y))
                farthest = min(farthest, range.y);
        }
    }
}

// The tiles dilated by scatter-as-gather over the largest reach.
[shader("compute")]
[numthreads(8, 8, 1)]
void dofDilate(uint3 id : SV_DispatchThreadID)
{
    int2 tiles = int2((push.size + TILE - 1) / TILE);
    int2 tile = int2(id.xy);
    if (any(tile >= tiles))
        return;
    int r = int(push.max_tile_radius[0]);
    float2 own = g_textures[push.tiles].Load(int3(tile, 0)).xy;
    float nearest = own.x;
    float farthest = own.y;
    // The kernel's extremes by themselves: the square below would test many in vain.
    if (r > 0)
    {
        dilate_step(nearest, farthest, -r, 0, r, tiles, tile);
        dilate_step(nearest, farthest, r, 0, r, tiles, tile);
        dilate_step(nearest, farthest, 0, -r, r, tiles, tile);
        dilate_step(nearest, farthest, 0, r, r, tiles, tile);
    }
    for (int x = -r + 1; x < r; x++)
    {
        for (int y = -r + 1; y < r; y++)
        {
            if (x == 0 && y == 0)
                continue;
            dilate_step(nearest, farthest, x, y, r, tiles, tile);
        }
    }
    // A tile in focus hides what is behind it: no far field spreads into it.
    if (coc_radius(own.x) < 0.5 && coc_radius(own.y) < 0.5)
        farthest = own.y;
    g_tile_images[push.dilated][tile] = float2(nearest, farthest);
}

// ---- Mips -------------------------------------------------------------------------------

// A pixel of the image (clamped to it) with its q.
float4 image_pixel(int2 p)
{
    p = clamp(p, int2(0, 0), int2(push.size) - 1);
    float3 c = g_textures[push.color].Load(int3(p, 0)).rgb;
    // HDR sources may hold infinities or NaN after a bad frame: never let them spread.
    c = min(max(c, 0.0), 65000.0);
    return float4(c, depth_q(scene_depth_at(p)));
}

// A 2 × 2 quad down, bilateral: each pixel weighs by how close its circle of confusion is
// to the first's (nothing beyond one pixel of radius); the first's q is kept.
float4 reduce(float4 v0, float4 v1, float4 v2, float4 v3)
{
    float w1 = saturate(1.0 - coc_distance(v0.a, v1.a));
    float w2 = saturate(1.0 - coc_distance(v0.a, v2.a));
    float w3 = saturate(1.0 - coc_distance(v0.a, v3.a));
    float3 color = v0.rgb + w1 * v1.rgb + w2 * v2.rgb + w3 * v3.rgb;
    return float4(color / (1.0 + w1 + w2 + w3), v0.a);
}

uint2 mip_size(uint level)
{
    uint d = 2u << level;
    return (push.size + d - 1) / d;
}

groupshared float4 g_mip1[8][8];
groupshared float4 g_mip2[4][4];

// Four mips of color and q (half size and below): every workgroup reduces 32 × 32 pixels.
[shader("compute")]
[numthreads(8, 8, 1)]
void dofDownsample(uint3 group : SV_GroupID, uint3 thread : SV_GroupThreadID)
{
    float4 level0[2][2];
    for (uint y = 0; y < 2; y++)
    {
        for (uint x = 0; x < 2; x++)
        {
            int2 t = int2(group.xy * 16 + thread.xy * 2 + uint2(x, y));
            int2 p = t * 2;
            float4 v = reduce(image_pixel(p), image_pixel(p + int2(1, 0)),
                image_pixel(p + int2(0, 1)), image_pixel(p + int2(1, 1)));
            level0[y][x] = v;
            if (all(uint2(t) < mip_size(0)))
                g_images[push.mips[0]][t] = v;
        }
    }
    float4 v1 = reduce(level0[0][0], level0[0][1], level0[1][0], level0[1][1]);
    uint2 t1 = group.xy * 8 + thread.xy;
    if (all(t1 < mip_size(1)))
        g_images[push.mips[1]][t1] = v1;
    g_mip1[thread.y][thread.x] = v1;
    GroupMemoryBarrierWithGroupSync();
    if (all(thread.xy < 4))
    {
        uint2 s = thread.xy * 2;
        float4 v2 = reduce(g_mip1[s.y][s.x], g_mip1[s.y][s.x + 1], g_mip1[s.y + 1][s.x],
            g_mip1[s.y + 1][s.x + 1]);
        uint2 t2 = group.xy * 4 + thread.xy;
        if (all(t2 < mip_size(2)))
            g_images[push.mips[2]][t2] = v2;
        g_mip2[thread.y][thread.x] = v2;
    }
    GroupMemoryBarrierWithGroupSync();
    if (all(thread.xy < 2))
    {
        uint2 s = thread.xy * 2;
        float4 v3 = reduce(g_mip2[s.y][s.x], g_mip2[s.y][s.x + 1], g_mip2[s.y + 1][s.x],
            g_mip2[s.y + 1][s.x + 1]);
        uint2 t3 = group.xy * 2 + thread.xy;
        if (all(t3 < mip_size(3)))
            g_images[push.mips[3]][t3] = v3;
    }
}

// ---- Blur -------------------------------------------------------------------------------

// Accumulators of one ring (the far field's occlusion between rings).
struct Bucket
{
    float4 color;        // rgb: color sum, a: weight sum
    float ring_covg;     // the ring's coverage (average of tile radius / radius, clamped)
    float radius;        // radius of the ring's center
    uint sample_count;
};

// One sample of the mips.
struct Sample
{
    float near;          // its near radius
    float far;           // its far radius
    float3 color;
};

struct InputState
{
    float2 px_coord;     // the kernel's center, in pixels
    float tile_coc;      // the dilated tiles' radius here
    float center_coc;    // the center's radius
    float ring_gap;      // undersampling: ring_gap * rings = tile radius
    uint mip_level;      // from the radius and the ring cap
    bool near_field;     // the center is in the near field
    uint n_samples;
    uint n_rings;
    float covg_factor;
    float covg_bias;
};

struct Accumulators
{
    Bucket prev;
    Bucket curr;
    float4 near_color;
    float4 fill_color;
};

// a / b, 0 where b is 0 (FidelityFX takes D3D's saturate(NaN) = 0 for it; SPIR-V's clamp
// of NaN is undefined).
float ratio(float a, float b)
{
    return b > 0.0 ? a / b : 0.0;
}

// Merges the current bucket into the previous one. Opacity: the share of the current
// ring's samples it holds.
void merge_buckets(inout Accumulators acc, float opacity)
{
    float prev_rc = saturate(ratio(acc.prev.ring_covg, float(acc.prev.sample_count)));
    float curr_rc = saturate(ratio(acc.curr.ring_covg, float(acc.curr.sample_count)));
    // The occlusion is the area where both buckets' rings (radius, width = coverage)
    // intersect, over the previous bucket's ring area.
    float prev_outer = saturate(acc.prev.radius + prev_rc);
    float prev_inner = acc.prev.radius - prev_rc;
    float curr_outer = saturate(acc.curr.radius + curr_rc);
    float curr_inner = acc.curr.radius - curr_rc;
    float ins_outer = min(prev_outer, curr_outer);
    float ins_inner = max(prev_inner, curr_inner);
    // An empty previous area occludes nothing (its sums are 0).
    float occlusion = ins_outer < ins_inner ? 0.0
        : saturate(ratio(ins_outer * ins_outer - sign(ins_inner) * ins_inner * ins_inner,
            prev_outer * prev_outer - sign(prev_inner) * prev_inner * prev_inner));
    float factor = 1.0 - opacity * occlusion;
    acc.prev.color = acc.prev.color * factor + acc.curr.color;
    // A radius that roughly covers both rings, the coverage up to either bound.
    float new_radius = 0.5 * (max(prev_outer, curr_outer) + min(prev_inner, curr_inner));
    float new_covg = 0.5 * (max(prev_outer, curr_outer) - min(prev_inner, curr_inner));
    acc.prev.sample_count = uint(float(acc.prev.sample_count) * factor) + acc.curr.sample_count;
    acc.prev.ring_covg = float(acc.prev.sample_count) * new_covg;
    acc.prev.radius = new_radius;
}

// The inverse of a sample's area (1 / (π r²), π ≈ 4), 1 within its own pixel.
float dof_weight(float radius)
{
    float inv = 1.0 / max(radius, 1e-6);
    return saturate(inv * inv / 4.0);
}

// The radius of a sample's projection on the aperture, relative to the tile's radius and
// the kernel's diameter in samples, biased for the gaps between rings.
float coverage(InputState ins, float radius)
{
    return saturate(ins.covg_factor / max(radius, 1e-6) + ins.covg_bias);
}

// Sample positions: an affine step (rotation about the center) from one to the next.
struct Stream
{
    float cos_theta;
    float sin_theta_x_aspect;
    float m_sin_theta_x_r_aspect;
    float2 translation;
    float2 position;
};

float2 advance(inout Stream state)
{
    float2 pos = state.position;
    float x = state.cos_theta * pos.x + state.m_sin_theta_x_r_aspect * pos.y;
    float y = state.sin_theta_x_aspect * pos.x + state.cos_theta * pos.y;
    state.position = float2(x, y) + state.translation;
    return pos;
}

// The samples of ring `ri` (0: the outermost); returns their count.
uint init_stream(out Stream state, InputState ins, float ri)
{
    float rings = float(ins.n_rings);
    uint n = uint(6.25 * (rings - ri)); // about pi / asin(1 / (2 (rings - ri)))
    float r = ins.tile_coc / rings * (rings - ri);
    float2 rcp_half = 1.0 / float2(push.half_size);
    state.position = rcp_half * (ins.px_coord + float2(r, 0.0));
    float theta = 6.2831853 / float(n);
    float s = sin(theta);
    float c = cos(theta);
    float2 aspect = float2(push.half_size) * rcp_half.yx;
    state.cos_theta = c;
    state.sin_theta_x_aspect = s * aspect.x;
    state.m_sin_theta_x_r_aspect = -s * aspect.y;
    state.translation = rcp_half * (ins.px_coord - ins.px_coord.x * float2(c, s) - ins.px_coord.y * float2(-s, c));
    return n;
}

struct RingParams
{
    float distance;      // to the center, in pixels: the ring's radius
    float bucket_border; // far field: between the current and the previous bucket
    float range_near;    // in range of the near field
    float range_far;     // and of the far
    float range_fill;    // main or fallback fill
    float fill_quality;  // how much a background sample fills
};

Sample fetch_sample(InputState ins, inout Stream state)
{
    float2 pos = advance(state);
    // Uniform across the workgroup: the mip's texture by a uniform index.
    float4 t = g_textures[push.mips[ins.mip_level]].SampleLevel(g_samplers[push.point_sampler], pos, 0.0);
    Sample s;
    s.near = near_radius(t.a);
    s.far = far_radius(t.a);
    s.color = t.rgb;
    return s;
}

// How much a sample belongs to the near field (feathered over a pixel).
float near_member(float near)
{
    return saturate(near - NEAR_START);
}

void process_near_sample(InputState ins, Sample s, inout Accumulators acc, RingParams ring)
{
    // Feathered by a pixel.
    float in_range = saturate(s.near - ring.range_near) * near_member(s.near);
    float weight = dof_weight(s.near + s.far);
    // The background behind the center, from samples farther away. Samples of the same
    // surface (a slope of a pixel of radius per pixel of distance) are rejected unless no
    // other is there.
    if (ins.near_field)
    {
        bool fills = s.far > 0.0 ? s.far > -ring.range_fill : s.near < ring.range_fill;
        float rejection = fills ? 1.0 : (1.0 / 2048.0);
        // Nearer samples (in the image) weigh more, falling off quadratically.
        acc.fill_color += float4(s.color, 1.0) * weight * ring.fill_quality * rejection;
    }
    acc.near_color += float4(s.color, 1.0) * weight * in_range;
}

void process_far_sample(InputState ins, Sample s, inout Accumulators acc, RingParams ring)
{
    float in_range = saturate(s.far - ring.range_far);
    float radius = s.near + s.far;
    float covg = coverage(ins, radius);
    float4 color = float4(s.color, 1.0) * dof_weight(radius) * in_range;
    // A near sample always counts in the current ring.
    if (s.near <= 0.0 && s.far >= ring.bucket_border)
    {
        acc.prev.ring_covg += covg;
        acc.prev.color += color;
        acc.prev.sample_count++;
    }
    else
    {
        acc.curr.ring_covg += covg;
        acc.curr.color += color;
        acc.curr.sample_count++;
    }
}

RingParams ring_params(InputState ins, uint ri)
{
    float rings = float(ins.n_rings);
    RingParams ring;
    ring.distance = ins.tile_coc / rings * (rings - float(ri));
    ring.bucket_border = (rings - 1.0 - float(ri) + 2.5) * ins.tile_coc / (0.5 + rings);
    ring.range_near = max(0.0, ring.distance - ins.ring_gap - 0.5);
    ring.range_far = max(0.0, ring.distance - ins.ring_gap);
    ring.range_fill = ins.center_coc - ring.distance;
    ring.fill_quality = (1.0 / ring.distance) * (1.0 / ring.distance);
    return ring;
}

void start_bucket(inout Accumulators acc, InputState ins, uint ri)
{
    acc.curr.color = float4(0.0, 0.0, 0.0, 0.0);
    acc.curr.ring_covg = 0.0;
    acc.curr.radius = float(ins.n_rings - ri) / float(ins.n_rings);
    acc.curr.sample_count = 0;
}

// Both fields; the far field scanned outside-in for its occlusion.
void process_near_far(InputState ins, inout Accumulators acc)
{
    for (uint ri = 0; ri < ins.n_rings; ri++)
    {
        start_bucket(acc, ins, ri);
        Stream stream;
        uint ring_samples = init_stream(stream, ins, float(ri));
        RingParams ring = ring_params(ins, ri);
        for (uint i = 0; i < ring_samples; i++)
        {
            Sample s = fetch_sample(ins, stream);
            process_far_sample(ins, s, acc, ring);
            process_near_sample(ins, s, acc, ring);
        }
        merge_buckets(acc, float(acc.curr.sample_count) / float(ring_samples));
    }
}

// Every sample in the near field.
void process_near_only(InputState ins, inout Accumulators acc)
{
    for (uint ri = 0; ri < ins.n_rings; ri++)
    {
        Stream stream;
        uint ring_samples = init_stream(stream, ins, float(ri));
        RingParams ring = ring_params(ins, ri);
        for (uint i = 0; i < ring_samples; i++)
        {
            Sample s = fetch_sample(ins, stream);
            process_near_sample(ins, s, acc, ring);
        }
    }
}

// Every sample in the far field.
void process_far_only(InputState ins, inout Accumulators acc)
{
    for (uint ri = 0; ri < ins.n_rings; ri++)
    {
        start_bucket(acc, ins, ri);
        Stream stream;
        uint ring_samples = init_stream(stream, ins, float(ri));
        RingParams ring = ring_params(ins, ri);
        for (uint i = 0; i < ring_samples; i++)
        {
            Sample s = fetch_sample(ins, stream);
            process_far_sample(ins, s, acc, ring);
        }
        merge_buckets(acc, float(acc.curr.sample_count) / float(ring_samples));
    }
}

// Every sample near, equally weighted. (FidelityFX also merges inner rings here and in
// `process_far_color_only`, reading them from coarser mips. Left out: it turns a bright
// point's disk into a cone, 1.7 times as bright at its center, with a tail 8 pixels past
// its radius; dof.rs's test.)
void process_near_color_only(InputState ins, inout Accumulators acc)
{
    for (uint ri = 0; ri < ins.n_rings; ri++)
    {
        Stream stream;
        uint ring_samples = init_stream(stream, ins, float(ri));
        float sample_dist = ins.tile_coc / float(ins.n_rings) * float(ins.n_rings - ri);
        float range = max(0.0, sample_dist - ins.ring_gap - 0.5);
        float3 sum = 0.0;
        float weights = 0.0;
        for (uint i = 0; i < ring_samples; i++)
        {
            Sample s = fetch_sample(ins, stream);
            float w = saturate(s.near - range) * near_member(s.near);
            sum += s.color * w;
            weights += w;
        }
        acc.near_color.rgb += sum;
        acc.near_color.a += weights;
    }
}

// Every sample far, equally weighted (its radius within half a pixel of the tile's).
void process_far_color_only(InputState ins, inout Accumulators acc)
{
    float n_samples = 0.0;
    for (uint ri = 0; ri < ins.n_rings; ri++)
    {
        Stream stream;
        uint ring_samples = init_stream(stream, ins, float(ri));
        float3 sum = 0.0;
        if (ri == 0)
        {
            // The outermost ring tests its range; the others are in range by construction.
            float sample_dist = ins.tile_coc;
            float range = max(0.0, sample_dist - ins.ring_gap);
            float weights = 0.0;
            for (uint i = 0; i < ring_samples; i++)
            {
                Sample s = fetch_sample(ins, stream);
                float w = saturate(s.far - range);
                sum += s.color * w;
                weights += w;
            }
            n_samples += weights;
        }
        else
        {
            for (uint i = 0; i < ring_samples; i++)
                sum += fetch_sample(ins, stream).color;
            n_samples += float(ring_samples);
        }
        acc.prev.color.rgb += sum;
    }
    acc.prev.color.a = n_samples;
    acc.prev.ring_covg = n_samples;
    acc.prev.sample_count = uint(n_samples);
}

groupshared uint g_ring_count;

// The near and far fields at half size. FidelityFX's blur, per pixel of mip 0.
[shader("compute")]
[numthreads(8, 8, 1)]
void dofBlur(uint3 id : SV_DispatchThreadID, uint index : SV_GroupIndex)
{
    uint2 pixel = id.xy;
    bool inside = all(pixel < push.half_size);
    if (index == 0)
        g_ring_count = 0;

    // The dilated tiles here (interpolated between tiles).
    float2 rcp_half = 1.0 / float2(push.half_size);
    float2 dilated = g_textures[push.dilated].SampleLevel(g_samplers[push.linear_sampler],
        float2(pixel) * rcp_half, 0.0).xy;
    float tile_rad = max(coc_radius(dilated.x), coc_radius(dilated.y));

    // Rings: one sample per pixel of the kernel's radius, capped; a coarser mip covers the
    // rings left out. The same for the whole workgroup.
    GroupMemoryBarrierWithGroupSync();
    uint wave_rings = WaveActiveMax(inside ? uint(ceil(tile_rad)) : 0u);
    if (WaveIsFirstLane())
        InterlockedMax(g_ring_count, wave_rings);
    GroupMemoryBarrierWithGroupSync();
    float ideal_rings = float(g_ring_count);

    // Copying is good enough when the whole wave is in focus.
    bool copy = WaveActiveAllTrue(!inside || tile_rad < 0.5);

    InputState ins;
    ins.n_rings = max(uint(ideal_rings), 1u);
    ins.mip_level = 0;
    if (ideal_rings > float(push.max_rings))
    {
        ins.n_rings = push.max_rings;
        // Every halving of the rings, one mip coarser.
        ins.mip_level = min(uint(log2(ideal_rings / float(push.max_rings))), 3u);
    }
    ins.ring_gap = ideal_rings / float(ins.n_rings) - 1.0;
    ins.tile_coc = tile_rad;

    // The top-left pixel's center in its quad, moved by a fixed hash against banding.
    float2 texcoord = float2(pixel) + float2(0.25, 0.25);
    {
        // hash22 adapted from https://www.shadertoy.com/view/4djSRW, Copyright (c) 2014
        // David Hoskins, MIT (THIRD_PARTY_NOTICES.md).
        float3 p3 = frac(texcoord.xyx * float3(0.1031, 0.1030, 0.0973));
        p3 += dot(p3, p3.yzx + 33.33);
        texcoord += frac((p3.xx + p3.yz) * p3.zy) * 0.5 - 0.25;
    }
    ins.px_coord = texcoord;

    uint2 load = min(pixel, push.half_size - 1);
    float4 center = g_textures[push.mips[0]].Load(int3(load, 0));
    float center_near = near_radius(center.a);
    ins.near_field = center_near > NEAR_START;
    ins.center_coc = center_near + far_radius(center.a);
    // Due to rounding, likely over-approximated: that is fine.
    ins.n_samples = uint(6.25 * 0.5 * float(ins.n_rings * (ins.n_rings + 1)));
    ins.covg_factor = WaveReadLaneFirst(0.5 / float(ins.n_rings) * ins.tile_coc);
    ins.covg_bias = WaveReadLaneFirst(0.5 / float(ins.n_rings));

    // Whether any sample can be in the near field (the nearest tile depth not clearly far)
    // or in the far field (the farthest not clearly near).
    bool needs_near = !copy && WaveActiveAnyTrue(inside && far_radius(dilated.x) < 1.0);
    bool needs_far = !copy && WaveActiveAnyTrue(inside && near_radius(dilated.y) < 1.0);
    bool color_only = WaveActiveAllTrue(!inside || coc_distance(dilated.x, dilated.y) < 0.5);

    Accumulators acc;
    acc.prev.color = float4(0.0, 0.0, 0.0, 0.0);
    acc.prev.ring_covg = 0.0;
    acc.prev.radius = 0.0;
    acc.prev.sample_count = 0;
    acc.curr = acc.prev;
    float center_weight = dof_weight(ins.center_coc);
    float center_covg = coverage(ins, ins.center_coc);
    float4 center_color = float4(center.rgb, 1.0);
    acc.near_color = float4(0.0, 0.0, 0.0, 0.0);
    acc.fill_color = float4(0.0, 0.0, 0.0, 0.0);
    if (ins.near_field)
    {
        // The center covers everything inside the innermost ring.
        float inner_ring = max(1.0, ins.tile_coc / float(ins.n_rings) - exp2(float(ins.mip_level)));
        float near_center_weight = inner_ring * inner_ring;
        // A center radius within a pixel of the near field's start: split between the
        // near field and the fill.
        float near_part = near_member(ins.center_coc);
        float fill_part = 1.0 - near_part;
        acc.near_color = center_color * center_weight * near_center_weight * near_part;
        acc.fill_color = center_color * fill_part;
    }

    if (needs_near && needs_far)
        process_near_far(ins, acc);
    else if (needs_near)
    {
        if (color_only)
            process_near_color_only(ins, acc);
        else
            process_near_only(ins, acc);
    }
    else if (needs_far)
    {
        if (color_only)
            process_far_color_only(ins, acc);
        else
            process_far_only(ins, acc);
    }
    else
    {
        if (inside)
        {
            g_images[push.far_field][pixel] = float4(center.rgb, 1.0);
            g_images[push.near_field][pixel] = float4(0.0, 0.0, 0.0, 0.0);
        }
        return;
    }

    // The center.
    acc.curr.ring_covg = ins.near_field ? 0.0 : center_covg;
    acc.curr.color = ins.near_field ? float4(0.0, 0.0, 0.0, 0.0) : center_color * center_weight;
    acc.curr.radius = 0.0;
    acc.curr.sample_count = 1;
    merge_buckets(acc, 1.0);
    if (ins.near_field)
        acc.prev.color += acc.fill_color;

    float fg_opacity = (!needs_far && color_only) ? 1.0
        : saturate(acc.near_color.a / (dof_weight(ins.tile_coc) * float(ins.n_samples)));
    float4 far_out = acc.prev.color.a > 0.0 ? acc.prev.color / acc.prev.color.a : float4(0.0, 0.0, 0.0, 0.0);
    float3 near_rgb = acc.near_color.a > 0.0 ? acc.near_color.rgb / acc.near_color.a : float3(0.0, 0.0, 0.0);
    float4 near_out = float4(near_rgb, fg_opacity);
    if (inside)
    {
        g_images[push.far_field][pixel] = far_out;
        g_images[push.near_field][pixel] = near_out;
    }
}

// ---- Composite --------------------------------------------------------------------------

float median3(float a, float b, float c)
{
    return max(min(a, b), min(max(a, b), c));
}

float median9(float a, float b, float c, float d, float e, float f, float g, float h, float i)
{
    float hi_lo = max(max(min(min(a, b), c), min(min(d, e), f)), min(min(g, h), i));
    float mi_mi = median3(median3(a, b, c), median3(d, e, f), median3(g, h, i));
    float lo_hi = min(min(max(max(a, b), c), max(max(d, e), f)), max(max(g, h), i));
    return median3(hi_lo, mi_mi, lo_hi);
}

float cubic_spline(float x)
{
    return x * x * (3.0 - 2.0 * x);
}

static const uint COMBINE_TILE = 8;
static const uint ROW_PITCH = COMBINE_TILE + 3; // +2 for the 3 × 3 filter, +1 for bilinear
static const uint COMBINE_AREA = ROW_PITCH * ROW_PITCH;

groupshared float g_near_luma[COMBINE_AREA];
groupshared float g_far_luma[COMBINE_AREA];
groupshared float4 g_near[COMBINE_AREA];
groupshared float4 g_far[COMBINE_AREA];
groupshared float3 g_full[18 * 18];

float luma(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

// A fixed blur of about three pixels' diameter (a circle's coverage of a 3 × 3 grid).
float3 blur3x3(uint base)
{
    const float corner = 0.5453;
    const float side = 0.9717;
    const float weights = 1.0 + 4.0 * corner + 4.0 * side;
    float3 sum = g_full[base + 19]
        + corner * (g_full[base] + g_full[base + 2] + g_full[base + 36] + g_full[base + 38])
        + side * (g_full[base + 1] + g_full[base + 18] + g_full[base + 20] + g_full[base + 37]);
    return sum / weights;
}

float4 final_combine(uint2 coord, uint2 rel, float4 bg, float4 fg, float min_fg_w)
{
    uint base = rel.x + 18 * rel.y;
    float3 full = g_full[base + 19];
    float3 fixed_blur = blur3x3(base);
    // The background expanded around edges.
    if (bg.a > 0.0)
        bg.rgb /= bg.a;
    // A foreground texel of zero weight makes the interpolation invalid.
    if (min_fg_w == 0.0)
        fg.a = 0.0;
    float c = 2.0 * coc_radius(depth_q(scene_depth_at(int2(coord)))); // in pixels of the image
    float c1 = saturate(c - 0.5); // sharp to the fixed blur
    float c2 = saturate(c - 1.5); // to the half-size background
    if (bg.a == 0.0)
        c2 = 0.0;
    float3 combined = lerp(full, fixed_blur, c1);
    combined = lerp(combined, bg.rgb, c2);
    combined = lerp(combined, fg.rgb, cubic_spline(fg.a));
    return float4(combined, 1.0);
}

float4 filter_far(uint base)
{
    // The 3 × 3 median of luminance sets the center's.
    float med = median9(g_far_luma[base], g_far_luma[base + 1], g_far_luma[base + 2],
        g_far_luma[base + ROW_PITCH], g_far_luma[base + ROW_PITCH + 1], g_far_luma[base + ROW_PITCH + 2],
        g_far_luma[base + 2 * ROW_PITCH], g_far_luma[base + 2 * ROW_PITCH + 1], g_far_luma[base + 2 * ROW_PITCH + 2]);
    uint idx = base + ROW_PITCH + 1;
    float3 col = g_far[idx].rgb;
    float factor = clamp(ratio(med, g_far_luma[idx]), 0.0, 2.0);
    // A pixel on a corner (five black neighbours) keeps its color.
    if (med == 0.0)
        factor = 1.0;
    return float4(col * factor, g_far[idx].a);
}

float near_alpha(uint idx, uint x, uint y)
{
    return g_near[idx + x + ROW_PITCH * y].a;
}

float near_luma_at(uint idx, uint x, uint y)
{
    return g_near_luma[idx + x + ROW_PITCH * y];
}

float4 filter_near(uint base)
{
    float med = median9(near_luma_at(base, 0, 0), near_luma_at(base, 1, 0), near_luma_at(base, 2, 0),
        near_luma_at(base, 0, 1), near_luma_at(base, 1, 1), near_luma_at(base, 2, 1),
        near_luma_at(base, 0, 2), near_luma_at(base, 1, 2), near_luma_at(base, 2, 2));
    float avg_alpha = (1.0 / 9.0) * (near_alpha(base, 0, 0) + near_alpha(base, 1, 0) + near_alpha(base, 2, 0)
        + near_alpha(base, 0, 1) + near_alpha(base, 1, 1) + near_alpha(base, 2, 1)
        + near_alpha(base, 0, 2) + near_alpha(base, 1, 2) + near_alpha(base, 2, 2));
    uint idx = base + ROW_PITCH + 1;
    if (near_alpha(idx, 0, 0) < 0.01)
    {
        // The center weighs nothing: take a corner's color.
        uint best = base;
        if (near_luma_at(base, 2, 0) > near_luma_at(best, 0, 0))
            best = base + 2;
        if (near_luma_at(base, 0, 2) > near_luma_at(best, 0, 0))
            best = base + 2 * ROW_PITCH;
        if (near_luma_at(base, 2, 2) > near_luma_at(best, 0, 0))
            best = base + 2 * ROW_PITCH + 2;
        idx = best;
    }
    float3 col = g_near[idx].rgb;
    float factor = med > 0.0 ? clamp(ratio(med, g_near_luma[idx]), 0.0, 2.0) : 1.0;
    return float4(col * factor, avg_alpha);
}

// The dilated tiles of a composite group (2 × 2 tiles): the nearest q and the farthest.
float2 group_tiles(uint2 group)
{
    int2 last = int2((push.size + TILE - 1) / TILE) - 1;
    int2 t = int2(group * 2);
    float2 a = g_textures[push.dilated].Load(int3(min(t, last), 0)).xy;
    float2 b = g_textures[push.dilated].Load(int3(min(t + int2(0, 1), last), 0)).xy;
    float2 c = g_textures[push.dilated].Load(int3(min(t + int2(1, 0), last), 0)).xy;
    float2 d = g_textures[push.dilated].Load(int3(min(t + int2(1, 1), last), 0)).xy;
    return float2(max(max(a.x, b.x), max(c.x, d.x)), min(min(a.y, b.y), min(c.y, d.y)));
}

void store_output(uint2 coord, float4 color)
{
    if (all(coord < push.size))
        g_images[push.output][coord] = color;
}

void combine_sharp_only(uint2 group, uint2 thread)
{
    uint2 base = 16 * group + thread;
    for (uint y = 0; y < 2; y++)
    {
        for (uint x = 0; x < 2; x++)
        {
            uint2 p = base + uint2(8 * x, 8 * y);
            uint2 load = min(p, push.size - 1);
            float4 c = g_textures[push.color].Load(int3(load, 0));
            store_output(p, float4(c.rgb, 1.0));
        }
    }
}

void fetch_full_color(uint2 group, uint gix)
{
    for (uint iter = 0; iter < 6; iter++)
    {
        uint i = (gix + iter * 64) % (18 * 18);
        int2 coord = int2(group * 16) + int2(int(i % 18) - 1, int(i / 18) - 1);
        coord = clamp(coord, int2(0, 0), int2(push.size) - 1);
        g_full[i] = g_textures[push.color].Load(int3(coord, 0)).rgb;
    }
}

// The filter's extra column (threads 0..8) and row (9..16) around the 8 × 8 texels, as a
// base index (FidelityFX's arithmetic, spelled out).
uint extra_index(uint gix)
{
    return gix <= COMBINE_TILE ? COMBINE_TILE + ROW_PITCH * gix
        : (gix - COMBINE_TILE - 1) + ROW_PITCH * COMBINE_TILE;
}

void fetch_fields(uint2 group, uint gix, bool near)
{
    for (uint iter = 0; iter < 2; iter++)
    {
        // With the modulo some texels are fetched twice: cheaper than waiting twice.
        uint i = (gix + iter * 64) % COMBINE_AREA;
        int2 coord = int2(group * COMBINE_TILE) + int2(int(i % ROW_PITCH) - 1, int(i / ROW_PITCH) - 1);
        coord = clamp(coord, int2(0, 0), int2(push.half_size) - 1);
        float4 far = g_textures[push.far_field].Load(int3(coord, 0));
        g_far_luma[i] = luma(far.rgb);
        g_far[i] = far;
        if (near)
        {
            float4 n = g_textures[push.near_field].Load(int3(coord, 0));
            g_near_luma[i] = luma(n.rgb);
            g_near[i] = n;
        }
    }
}

void combine(uint2 id, uint2 thread, uint2 group, uint gix, bool near)
{
    fetch_fields(group, gix, near);
    GroupMemoryBarrierWithGroupSync();

    uint base = thread.x + thread.y * ROW_PITCH;
    // One more round of filtering around the edge.
    uint base2 = extra_index(gix);
    bool extra = gix < (2 * COMBINE_TILE + 1);
    float4 far = filter_far(base);
    float4 far2 = extra ? filter_far(base2) : float4(0.0, 0.0, 0.0, 0.0);
    float4 near_c = float4(0.0, 0.0, 0.0, 0.0);
    float4 near2 = float4(0.0, 0.0, 0.0, 0.0);
    if (near)
    {
        near_c = filter_near(base);
        near2 = extra ? filter_near(base2) : float4(0.0, 0.0, 0.0, 0.0);
    }
    GroupMemoryBarrierWithGroupSync();

    // The filtered texels, for interpolation.
    g_far[base] = far;
    if (near)
        g_near[base] = near_c;
    if (extra)
    {
        g_far[base2] = far2;
        if (near)
            g_near[base2] = near2;
    }
    GroupMemoryBarrierWithGroupSync();

    float min_fg_w = 0.0;
    float4 near_tr = float4(0.0, 0.0, 0.0, 0.0);
    float4 near_bl = near_tr;
    float4 near_br = near_tr;
    if (near)
    {
        // A foreground texel of zero weight makes the interpolation invalid.
        min_fg_w = min(near_c.a, min(min(g_near[base + 1].a, g_near[base + ROW_PITCH].a), g_near[base + ROW_PITCH + 1].a));
        near_tr = 0.5 * near_c + 0.5 * g_near[base + 1];
        near_bl = 0.5 * near_c + 0.5 * g_near[base + ROW_PITCH];
        near_br = 0.5 * near_tr + 0.25 * g_near[base + ROW_PITCH] + 0.25 * g_near[base + ROW_PITCH + 1];
    }
    float4 far_tr = 0.5 * far + 0.5 * g_far[base + 1];
    float4 far_bl = 0.5 * far + 0.5 * g_far[base + ROW_PITCH];
    float4 far_br = 0.5 * far_tr + 0.25 * g_far[base + ROW_PITCH] + 0.25 * g_far[base + ROW_PITCH + 1];

    uint2 coord = 2 * id;
    uint2 rel = 2 * thread;
    store_output(coord, final_combine(coord, rel, far, near_c, min_fg_w));
    store_output(coord + uint2(1, 0), final_combine(coord + uint2(1, 0), rel + uint2(1, 0), far_tr, near_tr, min_fg_w));
    store_output(coord + uint2(1, 1), final_combine(coord + uint2(1, 1), rel + uint2(1, 1), far_br, near_br, min_fg_w));
    store_output(coord + uint2(0, 1), final_combine(coord + uint2(0, 1), rel + uint2(0, 1), far_bl, near_bl, min_fg_w));
}

// The fields upscaled and blended over the image: every workgroup writes 16 × 16 pixels.
[shader("compute")]
[numthreads(8, 8, 1)]
void dofComposite(uint3 id : SV_DispatchThreadID, uint3 thread : SV_GroupThreadID,
    uint3 group : SV_GroupID, uint gix : SV_GroupIndex)
{
    float2 tiles = group_tiles(group.xy);
    // Halved for the resolution, then 2 pixels: the blur's threshold and a small bias.
    bool near_needed = far_radius(tiles.x) < 1.025;
    bool all_sharp = coc_radius(tiles.x) < 0.25 && coc_radius(tiles.y) < 0.25;
    if (all_sharp)
    {
        combine_sharp_only(group.xy, thread.xy);
        return;
    }
    fetch_full_color(group.xy, gix);
    combine(id.xy, thread.xy, group.xy, gix, near_needed);
}
