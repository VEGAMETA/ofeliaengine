// Motion blur (ADR 0096, motion_blur.rs, docs/research/motion-blur.md): Guertin, McGuire
// and Nowrouzezahrai, "A Fast and Stable Feature-Aware Motion Blur Filter" (HPG 2014),
// after McGuire et al., "A Reconstruction Filter for Plausible Motion Blur" (I3D 2012),
// written from the papers (no code taken):
//
// 1. `mbTileMax`: each pixel's velocity and depth into one texel (the gather's samples
//    read one), and the largest velocity of each r × r tile (r: the largest blur);
// 2. `mbNeighborMax`: the largest of the 3 × 3 tiles around, a diagonal tile only if it
//    blurs towards the center tile;
// 3. `mbGather`: each pixel's N samples along the neighbourhood's dominant velocity and its
//    own, alternately, weighted by depth (who is in front), by reach (cones and cylinders
//    of the velocities) and by direction (dot products of the directions involved).
//
// Velocities are half the blur's extent, in pixels of the image: the shutter's share of the
// frame's motion, the streak running from p − v to p + v. They are clamped to [0.5, r].
// Depth is linear (distance from the camera, positive).

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(1, 0)]] [vk::image_format("rg16f")] RWTexture2D<float2> g_tile_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint color;          // sampled: the image blurred
    uint motion;         // sampled: motion vectors (render size; uv now − last frame)
    uint depth;          // sampled: the depth (render size; 0 near, 1 far)
    uint tiles;          // the largest velocity per tile; storage, then sampled
    uint neighbors;      // the largest in each tile's neighbourhood; storage, then sampled
    uint output;         // storage: the blurred image
    uint linear_sampler;
    uint samples;        // N
    uint2 size;          // the image
    uint2 render_size;   // the motion vectors and the depth
    uint2 tile_count;
    uint tile;           // r: pixels per tile side, the largest blur
    float exposure;      // half the shutter's share of the frame: v = exposure · motion
    float near;          // linear depth from the depth buffer: near / (1 − d · depth_scale)
    float depth_scale;
    float jitter;        // τ ψ / N: the samples' jitter, in sample spacings
    uint vz;             // per pixel: velocity, linear depth, speed; storage, then sampled
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

// The paper's constants: κ (the center weight's bias) and γ (pixels of speed over which
// the center's direction turns from across the dominant motion to its own).
static const float KAPPA = 40.0;
static const float GAMMA = 1.5;

uint2 render_texel(uint2 pixel)
{
    uint2 t = uint2((float2(pixel) + 0.5) * float2(push.render_size) / float2(push.size));
    return min(t, push.render_size - 1);
}

// A pixel's velocity: half its streak, in pixels, clamped to [0.5, r] (0 stays 0).
float2 velocity_at(uint2 pixel)
{
    float2 motion = g_textures[push.motion].Load(int3(render_texel(pixel), 0)).xy;
    float2 v = motion * float2(push.size) * push.exposure;
    float len = length(v);
    if (len < 1e-4)
        return float2(0.0, 0.0);
    return v * (clamp(len, 0.5, float(push.tile)) / len);
}

float linear_depth_at(uint2 pixel)
{
    float d = g_textures[push.depth].Load(int3(render_texel(pixel), 0)).r;
    return push.near / max(1.0 - d * push.depth_scale, 1e-6);
}

// ---- Tiles ------------------------------------------------------------------------------

groupshared float2 g_largest[64];

// Each pixel's velocity and depth, and the largest velocity of each tile: a workgroup per
// tile, each thread a part of it.
[shader("compute")]
[numthreads(8, 8, 1)]
void mbTileMax(uint3 group : SV_GroupID, uint3 thread : SV_GroupThreadID, uint index : SV_GroupIndex)
{
    uint2 origin = group.xy * push.tile;
    uint step = (push.tile + 7) / 8;
    float2 largest = float2(0.0, 0.0);
    for (uint y = 0; y < step; y++)
    {
        for (uint x = 0; x < step; x++)
        {
            // Interleaved: neighbouring threads read and write neighbouring pixels.
            uint2 local = thread.xy + uint2(x, y) * 8;
            uint2 p = origin + local;
            if (all(local < push.tile) && all(p < push.size))
            {
                float2 v = velocity_at(p);
                g_images[push.vz][p] = float4(v, linear_depth_at(p), length(v));
                if (dot(v, v) > dot(largest, largest))
                    largest = v;
            }
        }
    }
    g_largest[index] = largest;
    GroupMemoryBarrierWithGroupSync();
    for (uint stride = 32; stride > 0; stride >>= 1)
    {
        if (index < stride)
        {
            float2 other = g_largest[index + stride];
            if (dot(other, other) > dot(g_largest[index], g_largest[index]))
                g_largest[index] = other;
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (index == 0)
        g_tile_images[push.tiles][group.xy] = g_largest[0];
}

// The largest velocity that can reach each tile: of the 3 × 3 around, a diagonal tile
// only if its streak runs towards the center tile (Guertin et al., Section 4.4).
[shader("compute")]
[numthreads(8, 8, 1)]
void mbNeighborMax(uint3 id : SV_DispatchThreadID)
{
    int2 tile = int2(id.xy);
    if (any(id.xy >= push.tile_count))
        return;
    float2 largest = float2(0.0, 0.0);
    for (int y = -1; y <= 1; y++)
    {
        for (int x = -1; x <= 1; x++)
        {
            int2 t = tile + int2(x, y);
            if (any(t < 0) || any(t >= int2(push.tile_count)))
                continue;
            float2 v = g_textures[push.tiles].Load(int3(t, 0)).xy;
            float len2 = dot(v, v);
            if (len2 <= dot(largest, largest))
                continue;
            // A diagonal tile's streak (either way along it) must head within 45° of the
            // center.
            if (x != 0 && y != 0 && abs(dot(v, normalize(float2(-x, -y)))) < 0.70710678 * sqrt(len2))
                continue;
            largest = v;
        }
    }
    g_tile_images[push.neighbors][id.xy] = largest;
}

// ---- Gather -----------------------------------------------------------------------------

// Interleaved gradient noise (Jimenez 2014): a fixed value in [0, 1) per pixel.
float pixel_noise(float2 pixel)
{
    return frac(52.9829189 * frac(dot(pixel, float2(0.06711056, 0.00583715))));
}

// How much a (depth) is in front of b, relative to their depths (Guertin et al.): 1 when a
// is nearer or at b, 0 once a lies twice as far.
float in_front(float a, float b)
{
    return saturate(1.0 - (a - b) / min(a, b));
}

float cone(float distance, float speed)
{
    return saturate(1.0 - distance / speed);
}

float cylinder(float distance, float speed)
{
    return 1.0 - smoothstep(0.95 * speed, 1.05 * speed, distance);
}

float2 safe_normalize(float2 v, float2 fallback)
{
    float len = length(v);
    return len > 1e-4 ? v / len : fallback;
}

// The blur of each pixel.
[shader("compute")]
[numthreads(8, 8, 1)]
void mbGather(uint3 id : SV_DispatchThreadID)
{
    uint2 p = id.xy;
    if (any(p >= push.size))
        return;
    float2 rcp_size = 1.0 / float2(push.size);
    Texture2D color = g_textures[push.color];
    SamplerState s = g_samplers[push.linear_sampler];
    float4 center = color.Load(int3(p, 0));

    // The neighbourhood's dominant velocity, the lookup moved into the next tile (never a
    // diagonal one) with a probability that falls off linearly from 1/2 at the tile's
    // border to 0 at its center (Guertin et al., Section 4.2).
    float noise = pixel_noise(float2(p));
    float tile_noise = pixel_noise(float2(p) + float2(17.0, 53.0));
    uint2 tile = p / push.tile;
    float2 local = (float2(p % push.tile) + 0.5) / float(push.tile);
    float2 border = min(local, 1.0 - local);
    int2 offset = int2(0, 0);
    if (border.x < border.y)
    {
        if (tile_noise < 0.5 - border.x)
            offset.x = local.x < 0.5 ? -1 : 1;
    }
    else if (tile_noise < 0.5 - border.y)
        offset.y = local.y < 0.5 ? -1 : 1;
    int2 lookup = clamp(int2(tile) + offset, int2(0, 0), int2(push.tile_count) - 1);
    float2 vmax = g_textures[push.neighbors].Load(int3(lookup, 0)).xy;
    float vmax_len = length(vmax);
    if (vmax_len <= 0.5)
    {
        g_images[push.output][p] = float4(center.rgb, 1.0);
        return;
    }

    float2 wn = vmax / vmax_len;
    Texture2D vz = g_textures[push.vz];
    float4 center_vz = vz.Load(int3(p, 0));
    float2 vc = center_vz.xy;
    float vc_len = max(center_vz.w, 0.5);
    float2 wp = float2(-wn.y, wn.x);
    if (dot(wp, vc) < 0.0)
        wp = -wp;
    // The center's direction: across the dominant motion when it is still, its own once it
    // moves (over γ pixels of speed).
    float2 wc = safe_normalize(lerp(wp, safe_normalize(vc, wp), saturate((vc_len - 0.5) / GAMMA)), wp);
    float zc = center_vz.z;

    float n = float(push.samples);
    // The center weight, normalized against N (Section 4.3).
    float total = n / (KAPPA * vc_len);
    float3 sum = center.rgb * total;
    float j = (noise * 2.0 - 1.0) * push.jitter;
    for (uint i = 0; i < push.samples; i++)
    {
        float t = lerp(-1.0, 1.0, (float(i) + j + 1.0) / (n + 1.0));
        // Samples alternate between the dominant velocity and the pixel's own.
        float2 d = (i & 1) != 0 ? vc : vmax;
        float2 offset_px = t * d;
        // The distance the weights see is along the dominant velocity for both kinds of
        // samples (the paper's T = t ‖v_max‖): the samples along a still pixel's own
        // velocity gather where they land, but weigh as if spread along the dominant one.
        float dist = abs(t) * vmax_len;
        float2 sp = float2(p) + 0.5 + offset_px;
        uint2 sq = uint2(clamp(sp, float2(0.0, 0.0), float2(push.size) - 1.0));
        float4 sample_vz = vz.Load(int3(sq, 0));
        float2 vs = sample_vz.xy;
        float vs_len = max(sample_vz.w, 0.5);
        float zs = sample_vz.z;
        float3 cs = color.SampleLevel(s, sp * rcp_size, 0.0).rgb;

        // Who is in front: the sample over the pixel, or the pixel over the sample.
        float f = in_front(zs, zc);
        float b = in_front(zc, zs);
        // Directions: the sampling line against the center's direction and the sample's.
        float2 dn = safe_normalize(d, wn);
        float wa = abs(dot(wc, dn));
        float wb = abs(dot(safe_normalize(vs, dn), dn));
        // A sample blurring over the pixel; the pixel's blur showing the sample behind it;
        // both blurred together.
        float w = f * cone(dist, vs_len) * wb
            + b * cone(dist, vc_len) * wa
            + cylinder(dist, min(vs_len, vc_len)) * max(wa, wb) * 2.0;
        total += w;
        sum += cs * w;
    }
    g_images[push.output][p] = float4(sum / total, 1.0);
}
