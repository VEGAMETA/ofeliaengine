// Ground-truth ambient occlusion, GTAO (ADR 0058): Jimenez, Wu, Pesce, Jarabo, "Practical
// Realtime Strategies for Accurate Indirect Occlusion" (SIGGRAPH 2016), in the structure
// of Intel's XeGTAO (MIT; its documentation and the paper were followed, see
// docs/research/ambient-occlusion.md):
//
// 1. aoDepths: the depth buffer as view-space depth, with four coarser levels in an atlas
//    (level 0 at the left, levels 1-4 stacked at its right). Coarser levels average the
//    texels near the farthest of their 2 × 2, so thin foreground edges do not grow halos.
// 2. aoMain: per pixel, a few slices around the view vector (their angle rotated per
//    pixel and per frame); along each slice, samples on both sides find the horizons
//    (the highest occluders, fading out at the effect radius); the visible arc between
//    them, projected onto the normal, integrates the cosine-weighted visibility exactly.
//    Far samples read coarser depth levels. The normal comes from the depth, from the
//    neighbours on the side without an edge. Writes the visibility (÷ 1.5 when a blur
//    follows) and the edges (two bits per side in one byte).
// 3. aoDenoise: a 3 × 3 blur that does not cross the edges, from gathers; the last pass
//    restores the scale. The temporal resolve (TAA) averages the per-frame noise further.
//
// View space here: x right, y up, z forward (depth, positive), in the camera's units.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("r32f")] RWTexture2D<float> g_depths[];
[[vk::binding(1, 0)]] [vk::image_format("r8")] RWTexture2D<float> g_visibility[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const float PI = 3.14159265358979;
static const float HALF_PI = 1.57079632679490;
static const uint LEVELS = 5;

struct AoPush
{
    float2 view_mul;     // view position = (uv × view_mul + view_add) × depth
    float2 view_add;
    float2 pixel_size;   // 1 / size
    uint2 size;          // the depth target's size in pixels
    float depth_a;       // view depth = depth_b / (depth + depth_a)
    float depth_b;
    float radius;        // effect radius in view units
    float falloff_mul;   // sample weight = saturate(distance × falloff_mul + falloff_add)
    float falloff_add;
    float power;         // visibility = visibility ^ power
    uint slices;
    uint steps;          // per side of a slice
    uint frame;          // temporal noise index
    uint source;         // sampled: the depth target (aoDepths), the visibility (aoDenoise)
    uint target;         // storage: the depth atlas (aoDepths), the visibility (others)
    uint edges;          // aoMain: storage, aoDenoise: sampled
    uint depths;         // sampled: the depth atlas
    float mip_scale;     // a sample `d` pixels away reads level floor(log2(d × mip_scale))
    uint level2_y;       // rows where depth levels 2-4 start in the atlas (level 1: row 0)
    uint level3_y;
    uint level4_y;
    uint sampler;        // aoDenoise: nearest, clamp to edge, for gathers of 2 × 2 texels
    float output_scale;  // the written visibility's scale (see OCCLUSION_TERM_SCALE)
};

// Before the blur the visibility is stored divided by this (XeGTAO's
// XE_GTAO_OCCLUSION_TERM_SCALE): a noisy estimate may exceed 1, and clipping it in the
// 8-bit target before averaging would darken open surfaces. The last pass scales it back.
static const float OCCLUSION_TERM_SCALE = 1.5;

[[vk::push_constant]]
ConstantBuffer<AoPush> push;

// Size of depth level `k` (level 0: the target).
uint2 level_size(uint k)
{
    return ((push.size - 1) >> k) + 1;
}

// Where depth level `k` starts in the atlas.
uint2 level_origin(uint k)
{
    if (k == 0)
        return uint2(0, 0);
    uint y = k == 1 ? 0 : (k == 2 ? push.level2_y : (k == 3 ? push.level3_y : push.level4_y));
    return uint2(push.size.x, y);
}

// ---------------------------------------------------------------------------------------
// aoDepths: one group reduces 16 × 16 pixels to their four coarser levels.

groupshared float g_level[8][8];

// The average of four depths, weighted towards the farthest: nearer depths fade out in
// front of it as occluders fade out at the effect radius, scaled to three quarters of it
// (XeGTAO's depth MIP filter).
float depth_filter(float a, float b, float c, float d)
{
    float far = max(max(a, b), max(c, d));
    float4 depths = float4(a, b, c, d);
    float4 weights = saturate((far - depths) * (push.falloff_mul / 0.75) + push.falloff_add);
    return dot(weights, depths) / dot(weights, 1.0);
}

float linear_depth(uint2 p)
{
    p = min(p, push.size - 1);
    float d = g_textures[NonUniformResourceIndex(push.source)].Load(int3(p, 0)).r;
    return push.depth_b / (d + push.depth_a);
}

void write_level(uint k, uint2 p, float depth)
{
    if (all(p < level_size(k)))
        g_depths[push.target][level_origin(k) + p] = depth;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void aoDepths(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID)
{
    // Level 0: 2 × 2 pixels per thread; level 1: their filtered depth.
    uint2 p = group.xy * 16 + local.xy * 2;
    float d00 = linear_depth(p);
    float d10 = linear_depth(p + uint2(1, 0));
    float d01 = linear_depth(p + uint2(0, 1));
    float d11 = linear_depth(p + uint2(1, 1));
    write_level(0, p, d00);
    write_level(0, p + uint2(1, 0), d10);
    write_level(0, p + uint2(0, 1), d01);
    write_level(0, p + uint2(1, 1), d11);
    float level = depth_filter(d00, d10, d01, d11);
    write_level(1, group.xy * 8 + local.xy, level);
    g_level[local.y][local.x] = level;
    // Levels 2-4 from the group's shared values: 4 × 4, 2 × 2, 1 × 1 threads; level
    // k - 1's values sit 2^(k - 2) apart in the shared array.
    for (uint k = 2; k < LEVELS; k++)
    {
        GroupMemoryBarrierWithGroupSync();
        uint step = 1u << (k - 2);
        bool active = all((local.xy % (step * 2)) == 0);
        float value = 0.0;
        if (active)
        {
            value = depth_filter(
                g_level[local.y][local.x],
                g_level[local.y][local.x + step],
                g_level[local.y + step][local.x],
                g_level[local.y + step][local.x + step]);
            write_level(k, (group.xy * 16 >> k) + local.xy / (step * 2), value);
        }
        GroupMemoryBarrierWithGroupSync();
        if (active)
            g_level[local.y][local.x] = value;
    }
}

// ---------------------------------------------------------------------------------------
// aoMain

// View depth at pixel `p` (of level 0) from depth level `k`, clamped to the level.
float depth_at(Texture2D depths, int2 p, uint k)
{
    int2 last = int2((push.size - 1) >> k);
    p = clamp(p >> k, int2(0, 0), last) + int2(level_origin(k));
    return depths.Load(int3(p, 0)).r;
}

float3 view_position(float2 uv, float depth)
{
    return float3((uv * push.view_mul + push.view_add) * depth, depth);
}

// 1 where a neighbour lies on the same surface, towards 0 across a depth edge: the depth
// differences, allowing for the slope across the pixel (Jimenez, and XeGTAO's edges).
float4 depth_edges(float center, float left, float right, float top, float bottom)
{
    float4 edges = float4(left, right, top, bottom) - center;
    float slope_lr = (edges.y - edges.x) * 0.5;
    float slope_tb = (edges.w - edges.z) * 0.5;
    float4 adjusted = edges + float4(slope_lr, -slope_lr, slope_tb, -slope_tb);
    edges = min(abs(edges), abs(adjusted));
    return saturate(1.25 - edges / (center * 0.011));
}

// The four edges in one byte, two bits each (XeGTAO's packing): left, right, top, bottom
// from the high bits down.
float pack_edges(float4 edges)
{
    return dot(round(edges * 3.0), float4(64.0, 16.0, 4.0, 1.0)) / 255.0;
}

float4 unpack_edges(float packed)
{
    uint bits = uint(packed * 255.0 + 0.5);
    return float4((bits >> 6) & 3, (bits >> 4) & 3, (bits >> 2) & 3, bits & 3) / 3.0;
}

// Index of `p` along a Hilbert curve over 64 × 64 pixels: neighbouring pixels get
// well-spread noise values from a low-discrepancy sequence.
uint hilbert_index(uint2 p)
{
    uint index = 0;
    for (uint level = 32; level > 0; level /= 2)
    {
        uint rx = (p.x & level) > 0 ? 1 : 0;
        uint ry = (p.y & level) > 0 ? 1 : 0;
        index += level * level * ((3 * rx) ^ ry);
        if (ry == 0)
        {
            if (rx == 1)
                p = level - 1 - p;
            p = p.yx;
        }
    }
    return index;
}

float fast_acos(float x)
{
    // Eberly's approximation, as in the paper (error below 7e-5 rad).
    float a = abs(x);
    float r = (-0.0187293 * a + 0.0742610) * a - 0.2121144;
    r = (r * a + 1.5707288) * sqrt(1.0 - a);
    return x >= 0.0 ? r : PI - r;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void aoMain(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    int2 pixel = int2(id.xy);
    float2 uv = (float2(pixel) + 0.5) * push.pixel_size;
    Texture2D depths = g_textures[NonUniformResourceIndex(push.depths)];
    int2 last = int2(push.size) - 1;
    float center_z = depths.Load(int3(pixel, 0)).r;
    float left_z = depths.Load(int3(clamp(pixel + int2(-1, 0), int2(0, 0), last), 0)).r;
    float right_z = depths.Load(int3(clamp(pixel + int2(1, 0), int2(0, 0), last), 0)).r;
    float top_z = depths.Load(int3(clamp(pixel + int2(0, -1), int2(0, 0), last), 0)).r;
    float bottom_z = depths.Load(int3(clamp(pixel + int2(0, 1), int2(0, 0), last), 0)).r;
    float4 edges = depth_edges(center_z, left_z, right_z, top_z, bottom_z);
    g_visibility[push.edges][pixel] = pack_edges(edges);

    float3 center = view_position(uv, center_z);
    float3 view = normalize(-center);
    // The normal from the neighbours, favouring the pairs without an edge.
    float3 l = normalize(view_position(uv - float2(push.pixel_size.x, 0.0), left_z) - center);
    float3 r = normalize(view_position(uv + float2(push.pixel_size.x, 0.0), right_z) - center);
    float3 t = normalize(view_position(uv - float2(0.0, push.pixel_size.y), top_z) - center);
    float3 b = normalize(view_position(uv + float2(0.0, push.pixel_size.y), bottom_z) - center);
    float4 accept = saturate(edges.xzyw * edges.zywx + 0.01);
    float3 normal = accept.x * cross(l, t) + accept.y * cross(t, r) + accept.z * cross(r, b)
                  + accept.w * cross(b, l);
    normal = normalize(normal);
    if (dot(normal, view) < 0.0)
        normal = -normal;

    // The effect radius in pixels at this depth; too small to see: nothing occluded.
    float pixel_extent = center_z * abs(push.view_mul.x) * push.pixel_size.x;
    float screen_radius = push.radius / pixel_extent;
    if (screen_radius < 1.0)
    {
        g_visibility[push.target][pixel] = push.output_scale;
        return;
    }
    const float too_close = 1.3; // pixels: nearer samples would read the pixel itself
    float min_s = too_close / screen_radius;

    // Noise: rotation of the slices and offset of the steps, per pixel and per frame.
    uint index = hilbert_index(id.xy % 64) + 288 * (push.frame % 64);
    float2 noise = frac(0.5 + float(index) * float2(0.75487766624669276, 0.56984029099805327));

    // Slice directions on screen (y down): the first at noise × π / slices, each next one
    // rotated by π / slices.
    float slice_angle = PI / float(push.slices);
    float2 rotation = float2(cos(slice_angle), sin(slice_angle));
    float phi = noise.x * slice_angle;
    float2 omega = float2(cos(phi), -sin(phi));
    float visibility = 0.0;
    for (uint slice = 0; slice < push.slices; slice++)
    {
        float3 direction = float3(omega.x, -omega.y, 0.0);
        float3 ortho = direction - dot(direction, view) * view;
        float3 axis = normalize(cross(ortho, view));
        float3 projected = normal - axis * dot(normal, axis);
        float projected_length = length(projected);
        float sign_n = dot(ortho, projected) >= 0.0 ? 1.0 : -1.0;
        float cos_n = saturate(dot(projected, view) / max(projected_length, 1e-6));
        float n = sign_n * fast_acos(cos_n);
        float sin_n = sin(n);
        // Horizons start at the tangent plane (the hemisphere's edge) on each side:
        // cos(n + π/2) and cos(n - π/2).
        float low0 = -sin_n;
        float low1 = sin_n;
        float horizon0 = low0;
        float horizon1 = low1;
        for (uint step = 0; step < push.steps; step++)
        {
            float step_noise = frac(noise.y + float(slice + step * push.steps) * 0.6180339887);
            float s = (float(step) + step_noise) / float(push.steps);
            s = s * s + min_s; // denser near the pixel
            float distance = s * screen_radius;
            int2 offset = int2(round(distance * omega));
            // Far samples read coarser levels: floor(log2(distance × mip_scale)), at most 4.
            uint k = min(firstbithigh(uint(distance * push.mip_scale) | 1u), LEVELS - 1);
            float2 uv_offset = float2(offset) * push.pixel_size;
            float3 delta0 = view_position(uv + uv_offset, depth_at(depths, pixel + offset, k)) - center;
            float3 delta1 = view_position(uv - uv_offset, depth_at(depths, pixel - offset, k)) - center;
            float squared0 = max(dot(delta0, delta0), 1e-12);
            float squared1 = max(dot(delta1, delta1), 1e-12);
            float inverse0 = rsqrt(squared0);
            float inverse1 = rsqrt(squared1);
            float weight0 = saturate(squared0 * inverse0 * push.falloff_mul + push.falloff_add);
            float weight1 = saturate(squared1 * inverse1 * push.falloff_mul + push.falloff_add);
            horizon0 = max(horizon0, lerp(low0, dot(delta0, view) * inverse0, weight0));
            horizon1 = max(horizon1, lerp(low1, dot(delta1, view) * inverse1, weight1));
        }
        // A little of the full hemisphere for normals nearly in the slice's axis.
        projected_length = lerp(projected_length, 1.0, 0.05);
        float h0 = -fast_acos(horizon1);
        float h1 = fast_acos(horizon0);
        h0 = n + clamp(h0 - n, -HALF_PI, HALF_PI);
        h1 = n + clamp(h1 - n, -HALF_PI, HALF_PI);
        float arc0 = (cos_n + 2.0 * h0 * sin_n - cos(2.0 * h0 - n)) / 4.0;
        float arc1 = (cos_n + 2.0 * h1 * sin_n - cos(2.0 * h1 - n)) / 4.0;
        visibility += projected_length * (arc0 + arc1);
        omega = float2(omega.x * rotation.x + omega.y * rotation.y,
                       omega.y * rotation.x - omega.x * rotation.y);
    }
    visibility /= float(push.slices);
    // Small effect radii (a few pixels) find little: fade towards open (XeGTAO).
    visibility += saturate((10.0 - screen_radius) / 100.0) * 0.5 / float(push.slices);
    // Never fully black: a little ambient light always reaches (as XeGTAO).
    visibility = max(pow(max(visibility, 0.0), push.power), 0.03);
    g_visibility[push.target][pixel] = saturate(visibility * push.output_scale);
}

// ---------------------------------------------------------------------------------------
// aoDenoise

[shader("compute")]
[numthreads(8, 8, 1)]
void aoDenoise(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    int2 pixel = int2(id.xy);
    Texture2D visibility = g_textures[NonUniformResourceIndex(push.source)];
    Texture2D edge_map = g_textures[NonUniformResourceIndex(push.edges)];
    SamplerState point = g_samplers[push.sampler];
    // Gathers of 2 × 2 texels (clamped at the target's edges) at the corners around the
    // pixel's centre; gather order: x (0, 1), y (1, 1), z (1, 0), w (0, 0).
    float2 corner = float2(pixel) * push.pixel_size;
    float4 edges_up_left = edge_map.GatherRed(point, corner);
    float4 edges_down_right = edge_map.GatherRed(point, corner, int2(1, 1));
    float4 edges = unpack_edges(edges_up_left.y);
    // The neighbours' own edges towards this pixel: a pair blends only if both agree.
    float left = unpack_edges(edges_up_left.x).y;
    float top = unpack_edges(edges_up_left.z).w;
    float right = unpack_edges(edges_down_right.z).x;
    float bottom = unpack_edges(edges_down_right.x).z;
    float4 w = edges * float4(left, right, top, bottom);
    float4 up_left = visibility.GatherRed(point, corner);
    float4 down_right = visibility.GatherRed(point, corner, int2(1, 1));
    float4 up_right = visibility.GatherRed(point, corner, int2(1, 0));
    float4 down_left = visibility.GatherRed(point, corner, int2(0, 1));
    float v = up_left.y;
    float4 neighbours = float4(up_left.x, down_right.z, up_left.z, down_right.x); // l r t b
    float4 corners = float4(up_left.w, up_right.z, down_left.x, down_right.y);   // tl tr bl br
    float sum = v + dot(w, neighbours);
    float total = 1.0 + dot(w, 1.0);
    // Diagonals through two edges.
    float4 dw = 0.5 * float4(w.x * w.z, w.y * w.z, w.x * w.w, w.y * w.w);
    sum += dot(dw, corners);
    total += dot(dw, 1.0);
    g_visibility[push.target][pixel] = saturate(sum / total * push.output_scale);
}
