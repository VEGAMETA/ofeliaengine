// Decals through a decal buffer (ADR 0082): after the depth prepass, each pixel's decals,
// those in both its tile's bitmask and its z-bin's (the decals whose boxes' screen
// rectangles and depth ranges reach them), are projected onto the surface the depth
// buffer holds there and composited, in the decals' order, into two render-size targets
// that the shading's decal variants blend in before lighting:
//
// - albedo (rgba8): the square root of rgb premultiplied by coverage, a the coverage (the
//   shading's albedo becomes albedo × (1 − a) + rgb²: dark colors keep their steps);
// - surface (rg32ui): x the normal's change premultiplied by coverage (three signed
//   10-bit components, added to the shading's normal); y roughness and metalness
//   premultiplied by coverage, then the coverage (10 bits each).
//
// Only tiles some decal reaches are evaluated and written (`DecalFrame::occupied`, the
// bits the shading tests too), so the targets cost what the decals cover. A wave walks the
// union of its pixels' decals (as `local_lights` in forward.slang), so each decal's data
// is read once per wave, through scalar loads.
//
// The surface's position and normal come from the depth (the normal from the smaller of
// the two differences along each axis, so edges do not bend it), which is what lets a
// decal wrap a corner: each face takes it by its own angle to the decal, and a normal
// map is laid in the face's own plane. No code was copied; see docs/research/decals.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba8")] RWTexture2D<float4> g_unorm_images[];
[[vk::binding(1, 0)]] [vk::image_format("rg32ui")] RWTexture2D<uint2> g_uint2_images[];
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
    float normal_scale;         // scales the normal map's xy (glTF's normalTexture.scale)
    float fade_full;            // cosine between the surface normal and +z: full above
    float fade_none;            // none below
};

// `DecalFrame` in decals.rs (128 bytes).
struct DecalFrame
{
    float4x4 world_from_clip;   // of the depth as drawn (jittered)
    float4 bin_row;             // a clip point v's z-bin: dot(bin_row, v) / dot(world_from_clip[3], v)
    float2 ndc_per_pixel;       // 2 / size
    float2 _pad;
    uint2 size;                 // the depth's size in pixels
    uint tiles_x;
    uint words;                 // 32-bit words of a tile's bitmask
    uint sampler;               // trilinear, clamp
    uint count;                 // decals
    uint* occupied;             // per tile a bit: some decal's rectangle reaches it
};

// `Push` in decals.rs (56 bytes).
struct Push
{
    DecalFrame* frame;
    Decal* decals;
    uint* bins;                 // per z-bin (DECAL_BINS), `words` bitmask words
    uint* tiles;                // per tile, `words` bitmask words
    uint* rects;                // per decal: min x | min y << 16, max x | max y << 16 (tiles)
    uint depth;                 // bindless sampled
    uint albedo;                // bindless storage (rgba8)
    uint surface;               // bindless storage (rg32ui)
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

static const uint TILE = 16;        // LIGHT_TILE
static const uint DECAL_BINS = 256; // decals::DECAL_BINS
static const uint NO_TEXTURE = 0xFFFFFFFFu;

// One group per decal: its bit is set in the (cleared) bitmasks of every tile of its
// rectangle (as `lightTiles` in forward.slang).
[shader("compute")]
[numthreads(64, 1, 1)]
void decalTiles(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    DecalFrame f = loadAligned<16>(push.frame);
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

// The clip-space point of pixel `xy` at `depth`.
float4 clip_at(DecalFrame f, int2 xy, float depth)
{
    return float4((float2(xy) + 0.5) * f.ndc_per_pixel - 1.0, depth, 1.0);
}

float4 column(float4x4 m, uint j)
{
    return float4(m[0][j], m[1][j], m[2][j], m[3][j]);
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

float3 safe_normalize(float3 v)
{
    return v * rsqrt(max(dot(v, v), 1e-12));
}

// Three components in [-1, 1] as signed 10-bit integers (`unpack_snorm10` in
// forward.slang): 0 stays 0.
uint pack_snorm10(float3 v)
{
    int3 q = int3(round(clamp(v, -1.0, 1.0) * 511.0));
    return (uint(q.x) & 1023u) | (uint(q.y) & 1023u) << 10 | (uint(q.z) & 1023u) << 20;
}

// Three components in [0, 1] in 10 bits each (`unpack_unorm10` in forward.slang).
uint pack_unorm10(float3 v)
{
    uint3 q = uint3(round(saturate(v) * 1023.0));
    return q.x | q.y << 10 | q.z << 20;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void decalApply(uint3 id : SV_DispatchThreadID, uint3 group : SV_GroupID)
{
    DecalFrame f = loadAligned<16>(push.frame);
    // The group lies in one tile: tiles no decal reaches are neither evaluated nor
    // written (the shading does not read them).
    uint tile = (group.y / 2) * f.tiles_x + group.x / 2;
    if ((f.occupied[tile >> 5] & (1u << (tile & 31u))) == 0)
        return;
    if (any(id.xy >= f.size))
        return;
    int2 xy = int2(id.xy);
    float4 albedo = 0.0;
    float3 delta = 0.0;     // the normal's change
    float3 surface = 0.0;   // roughness, metalness, coverage
    float depth = load_depth(f, xy);
    bool surfaced = depth < 1.0;
    float4 v = clip_at(f, xy, depth);
    uint bin = min(uint(max(dot(f.bin_row, v) / dot(f.world_from_clip[3], v), 0.0)), DECAL_BINS - 1);
    // The position (homogeneous: the neighbours' follow from it), once a decal's z-bin and
    // tile hold the pixel; the surface's normal and the position's steps to the next
    // pixels, from the depth (on each axis the side whose depth is closer: no bend at
    // edges), once a decal's box holds it.
    bool placed = false;
    bool shaped = false;
    float4 h = 0.0;
    float3 p = 0.0;
    float3 n = 0.0;
    float3 dx = 0.0;
    float3 dy = 0.0;
    SamplerState s = g_samplers[f.sampler];
    for (uint w = 0; w < f.words; w++)
    {
        uint mask = surfaced ? push.tiles[tile * f.words + w] & push.bins[bin * f.words + w] : 0u;
        // The wave walks the union of its lanes' decals, in their order.
        uint merged = WaveActiveBitOr(mask);
        while (merged != 0)
        {
            uint bit = firstbitlow(merged);
            merged &= merged - 1u;
            Decal d = loadAligned<16>(push.decals + (w * 32 + bit));
            if (((mask >> bit) & 1u) == 0)
                continue;
            if (!placed)
            {
                placed = true;
                h = mul(f.world_from_clip, v);
                p = h.xyz / h.w;
            }
            float3 q = in_decal(d, p);
            if (any(abs(q) > 0.5))
                continue;
            if (!shaped)
            {
                shaped = true;
                float dl = load_depth(f, xy - int2(1, 0));
                float dr = load_depth(f, xy + int2(1, 0));
                float du = load_depth(f, xy - int2(0, 1));
                float dd = load_depth(f, xy + int2(0, 1));
                float sx = abs(dr - depth) < abs(dl - depth) ? 1.0 : -1.0;
                float sy = abs(dd - depth) < abs(du - depth) ? 1.0 : -1.0;
                float4 along_x = column(f.world_from_clip, 0) * f.ndc_per_pixel.x;
                float4 along_y = column(f.world_from_clip, 1) * f.ndc_per_pixel.y;
                float4 along_z = column(f.world_from_clip, 2);
                float4 hx = h + along_x * sx + along_z * ((sx > 0.0 ? dr : dl) - depth);
                float4 hy = h + along_y * sy + along_z * ((sy > 0.0 ? dd : du) - depth);
                dx = (hx.xyz / hx.w - p) * sx;
                dy = (hy.xyz / hy.w - p) * sy;
                // Towards the eye: against the pixel's ray (from its point on the near
                // plane, z = 0), not the view axis, which a surface seen from the side
                // can face.
                float4 h0 = h - along_z * depth;
                n = safe_normalize(cross(dy, dx));
                if (dot(n, p - h0.xyz / h0.w) > 0.0)
                    n = -n;
            }
            float facing = dot(n, d.axis_z.xyz);
            float fade = saturate((facing - d.fade_none) / max(d.fade_full - d.fade_none, 1e-4));
            if (fade <= 0.0)
                continue;
            // Texture coordinates and their steps to the next pixels (for mips).
            float2 uv = float2(q.x + 0.5, 0.5 - q.y);
            float2 gx = float2(dot(d.decal_from_world[0].xyz, dx), -dot(d.decal_from_world[1].xyz, dx));
            float2 gy = float2(dot(d.decal_from_world[0].xyz, dy), -dot(d.decal_from_world[1].xyz, dy));
            float4 color = d.albedo != NO_TEXTURE
                ? g_textures[d.albedo].SampleGrad(s, uv, gx, gy)
                : float4(1.0, 1.0, 1.0, 1.0);
            float a = saturate(color.a * d.tint.a * fade);
            albedo = float4(albedo.rgb * (1.0 - a) + color.rgb * d.tint.rgb * a, albedo.a * (1.0 - a) + a);
            if (d.normal != NO_TEXTURE && d.normal_scale != 0.0)
            {
                float2 t = g_textures[d.normal].SampleGrad(s, uv, gx, gy).xy * 2.0 - 1.0;
                float z = sqrt(saturate(1.0 - dot(t, t)));
                t *= d.normal_scale;
                // The decal's axes laid in this face's plane: the normal map's frame.
                float3 tx = safe_normalize(d.axis_x.xyz - n * dot(n, d.axis_x.xyz));
                float3 ty = d.axis_y.xyz - n * dot(n, d.axis_y.xyz);
                ty = safe_normalize(ty - tx * dot(tx, ty));
                float3 m = safe_normalize(t.x * tx + t.y * ty + z * n);
                delta = delta * (1.0 - a) + (m - n) * a;
            }
            if (d.surface > 0.0)
            {
                float c = a * saturate(d.surface);
                surface = float3(surface.xy * (1.0 - c) + float2(d.roughness, d.metalness) * c, surface.z * (1.0 - c) + c);
            }
        }
    }
    g_unorm_images[push.albedo][id.xy] = float4(sqrt(albedo.rgb), albedo.a);
    g_uint2_images[push.surface][id.xy] = uint2(pack_snorm10(delta), pack_unorm10(surface));
}
