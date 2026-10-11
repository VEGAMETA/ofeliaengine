// Decals through a decal buffer (ADR 0082): after the depth prepass, each pixel's decals,
// found as the local lights are (z-bins and tile bitmasks, ADR 0045), are projected onto
// the surface the depth buffer holds there and composited, in the decals' order, into
// three render-size targets that the shading's decal variants blend in before lighting:
//
// - albedo: rgb premultiplied by coverage, a the coverage (the shading's albedo becomes
//   albedo × (1 − a) + rgb);
// - normal: a world-space normal premultiplied by coverage, w the coverage;
// - surface: roughness and metalness premultiplied by coverage, b the coverage.
//
// The surface's position and normal come from the depth (the normal from the smaller of
// the two differences along each axis, so edges do not bend it), which is what lets a
// decal wrap a corner: each face takes it by its own angle to the decal. No code was
// copied; see docs/research/decals.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba8")] RWTexture2D<float4> g_unorm_images[];
[[vk::binding(1, 0)]] [vk::image_format("rgba8_snorm")] RWTexture2D<float4> g_snorm_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

// `DecalData` in decals.rs (144 bytes).
struct Decal
{
    float4 decal_from_world[3]; // rows of the affine map from the world to the box [-0.5, 0.5]³
    float4 tint;                // albedo multiplier (linear) and opacity
    float4 axis_x;              // the decal's axes in the world, unit: the normal map's frame
    float4 axis_y;
    float4 axis_z;              // it projects along −z onto surfaces facing +z
    uint albedo;                // bindless texture (alpha: coverage), or NO_TEXTURE
    uint normal;                // bindless tangent-space normal map (xy), or NO_TEXTURE
    float roughness;
    float metalness;
    float surface;              // how much of the surface's roughness and metalness it replaces
    float normal_strength;
    float fade_full;            // cosine between the surface normal and +z: full above
    float fade_none;            // none below
};

// `DecalFrame` in decals.rs (128 bytes).
struct DecalFrame
{
    float4x4 world_from_clip;   // of the depth as drawn (jittered)
    float4 near_center;         // xyz: the view axis's origin; w: z-bins per unit of depth
    float4 forward;             // xyz: the view axis (unit); w: unused
    uint2 size;                 // the depth's size in pixels
    uint tiles_x;
    uint words;                 // 32-bit words of a tile's bitmask
    uint sampler;               // trilinear, clamp
    uint count;                 // decals
    uint2 _pad;
};

// `Push` in decals.rs (56 bytes).
struct Push
{
    DecalFrame* frame;
    Decal* decals;
    uint* bins;                 // Z_BINS: lowest | highest index << 16 (empty: lowest > highest)
    uint* tiles;                // per tile, `words` bitmask words
    uint* rects;                // per decal: min x | min y << 16, max x | max y << 16 (tiles)
    uint depth;                 // bindless sampled
    uint albedo;                // bindless storage (rgba8)
    uint normal;                // bindless storage (rgba8_snorm)
    uint surface;               // bindless storage (rgba8)
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

static const uint TILE = 16;        // LIGHT_TILE
static const uint Z_BINS = 1024;    // lights::Z_BINS
static const uint NO_TEXTURE = 0xFFFFFFFFu;

// One group per decal: its bit is set in the (cleared) bitmasks of every tile of its
// rectangle (as `lightTiles` in forward.slang).
[shader("compute")]
[numthreads(64, 1, 1)]
void decalTiles(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    DecalFrame f = *push.frame;
    uint decal = group.x;
    uint lo = push.rects[decal * 2];
    uint hi = push.rects[decal * 2 + 1];
    uint2 first = uint2(lo & 0xFFFFu, lo >> 16);
    uint2 size = uint2(hi & 0xFFFFu, hi >> 16) - first + 1;
    uint word = decal >> 5;
    uint bit = 1u << (decal & 31u);
    for (uint i = local; i < size.x * size.y; i += 64)
    {
        uint2 t = first + uint2(i % size.x, i / size.x);
        InterlockedOr(push.tiles[(t.y * f.tiles_x + t.x) * f.words + word], bit);
    }
}

// The world position of pixel `xy` at `depth`.
float3 world_at(DecalFrame f, int2 xy, float depth)
{
    float2 ndc = (float2(xy) + 0.5) / float2(f.size) * 2.0 - 1.0;
    float4 p = mul(f.world_from_clip, float4(ndc, depth, 1.0));
    return p.xyz / p.w;
}

float load_depth(DecalFrame f, int2 xy)
{
    return g_textures[push.depth].Load(int3(clamp(xy, int2(0, 0), int2(f.size) - 1), 0)).r;
}

// The decal box's coordinates of a world position.
float3 in_decal(Decal d, float3 p)
{
    float4 q = float4(p, 1.0);
    return float3(dot(d.decal_from_world[0], q), dot(d.decal_from_world[1], q), dot(d.decal_from_world[2], q));
}

[shader("compute")]
[numthreads(8, 8, 1)]
void decalApply(uint3 id : SV_DispatchThreadID)
{
    DecalFrame f = *push.frame;
    if (any(id.xy >= f.size))
        return;
    int2 xy = int2(id.xy);
    float4 albedo = 0.0;
    float4 normal = 0.0;
    float4 surface = 0.0;
    float depth = load_depth(f, xy);
    if (depth < 1.0)
    {
        float3 p = world_at(f, xy, depth);
        float bin = dot(p - f.near_center.xyz, f.forward.xyz) * f.near_center.w;
        uint range = push.bins[min(uint(max(bin, 0.0)), Z_BINS - 1)];
        uint lowest = range & 0xFFFFu;
        uint highest = range >> 16;
        if (lowest <= highest)
        {
            // The surface's normal and the position's steps to the next pixels, from the
            // depth: on each axis the side whose depth is closer (no bend at edges).
            float dl = load_depth(f, xy - int2(1, 0));
            float dr = load_depth(f, xy + int2(1, 0));
            float du = load_depth(f, xy - int2(0, 1));
            float dd = load_depth(f, xy + int2(0, 1));
            float3 dx = abs(dr - depth) < abs(dl - depth)
                ? world_at(f, xy + int2(1, 0), dr) - p
                : p - world_at(f, xy - int2(1, 0), dl);
            float3 dy = abs(dd - depth) < abs(du - depth)
                ? world_at(f, xy + int2(0, 1), dd) - p
                : p - world_at(f, xy - int2(0, 1), du);
            float3 n = normalize(cross(dy, dx));
            if (dot(n, f.forward.xyz) > 0.0)
                n = -n;
            SamplerState s = g_samplers[f.sampler];
            uint tile = (id.y / TILE) * f.tiles_x + id.x / TILE;
            for (uint w = lowest >> 5; w <= (highest >> 5); w++)
            {
                uint bits = push.tiles[tile * f.words + w];
                // Only the decals of this pixel's z-bin.
                uint first = w * 32;
                if (first < lowest)
                    bits &= ~0u << (lowest - first);
                if (highest - first < 31)
                    bits &= (2u << (highest - first)) - 1;
                while (bits != 0)
                {
                    uint index = first + firstbitlow(bits);
                    bits &= bits - 1;
                    Decal d = push.decals[index];
                    float3 q = in_decal(d, p);
                    if (any(abs(q) > 0.5))
                        continue;
                    float facing = dot(n, d.axis_z.xyz);
                    float fade = saturate((facing - d.fade_none) / max(d.fade_full - d.fade_none, 1e-4));
                    if (fade <= 0.0)
                        continue;
                    // Texture coordinates and their steps to the next pixels (for mips).
                    float2 uv = float2(q.x + 0.5, 0.5 - q.y);
                    float3 qx = in_decal(d, p + dx) - q;
                    float3 qy = in_decal(d, p + dy) - q;
                    float2 gx = float2(qx.x, -qx.y);
                    float2 gy = float2(qy.x, -qy.y);
                    float4 color = d.albedo != NO_TEXTURE
                        ? g_textures[d.albedo].SampleGrad(s, uv, gx, gy)
                        : float4(1.0, 1.0, 1.0, 1.0);
                    float a = saturate(color.a * d.tint.a * fade);
                    albedo = float4(albedo.rgb * (1.0 - a) + color.rgb * d.tint.rgb * a, albedo.a * (1.0 - a) + a);
                    if (d.normal != NO_TEXTURE && d.normal_strength > 0.0)
                    {
                        float2 t = g_textures[d.normal].SampleGrad(s, uv, gx, gy).xy * 2.0 - 1.0;
                        float3 tangent = float3(t, sqrt(saturate(1.0 - dot(t, t))));
                        float3 m = normalize(tangent.x * d.axis_x.xyz + tangent.y * d.axis_y.xyz + tangent.z * d.axis_z.xyz);
                        float an = a * saturate(d.normal_strength);
                        normal = float4(normal.xyz * (1.0 - an) + m * an, normal.w * (1.0 - an) + an);
                    }
                    if (d.surface > 0.0)
                    {
                        float as_ = a * saturate(d.surface);
                        surface = float4(surface.rg * (1.0 - as_) + float2(d.roughness, d.metalness) * as_, surface.b * (1.0 - as_) + as_, 0.0);
                    }
                }
            }
        }
    }
    g_unorm_images[push.albedo][id.xy] = albedo;
    g_snorm_images[push.normal][id.xy] = normal;
    g_unorm_images[push.surface][id.xy] = surface;
}
