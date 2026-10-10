// HDR panoramas as an environment's sky (environment.rs, ADR 0083):
//
// - `panoramaCube` turns an equirectangular panorama, its RGBE texels in a buffer as `.hdr`
//   files hold them, into the faces of a background cube (mip 0; `downsample` in
//   environment.slang fills the rest), in the panorama's own frame: whoever reads the cube
//   turns their directions back (`turned_back`), so turning the panorama remakes nothing
//   but the lighting;
// - `panoramaLight` fills the environment's lighting cube (mip 0, in the world's frame)
//   from the background cube, the sun's cone filled with the sky around it: the sun lights
//   the scene as a directional light then, and the lighting cube keeps no spike for its
//   filtering to smear.
//
// No code was copied; see docs/research/hdri-environments.md.

[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2DArray<float4> g_cube_faces[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

#include "cube_common.slang"

struct PanoramaPush
{
    uint* texels;            // `panoramaCube`: width × height RGBE pixels, rows from the top (+Y)
    uint width;
    uint height;
    float2 turn;             // `panoramaLight`: sine and cosine of the panorama's turn about +Y
    uint target;             // storage image being written (mip 0)
    uint size;               // its face size
    float4 sun;              // `panoramaLight`: xyz towards the sun, w the cosine of its cone (2: none)
    float4 sun_sky;          // `panoramaLight`: rgb, the radiance put inside the cone
    uint source;             // `panoramaLight`: the background cube, sampled
    uint sampler;            // trilinear, clamp
    float level;             // `panoramaLight`: the background's mip it reads
    uint samples;            // samples per side of a texel (samples² per texel)
};

[[vk::push_constant]]
ConstantBuffer<PanoramaPush> push;

static const float PI = 3.14159265;

// An RGBE pixel (red in the low byte) as linear RGB: the mantissas' centres,
// (m + 0.5) / 256 · 2^(e − 128); 0 for exponent 0.
float3 rgbe(uint p)
{
    uint e = p >> 24;
    if (e == 0)
        return 0.0;
    float3 m = float3(float(p & 0xFFu), float((p >> 8) & 0xFFu), float((p >> 16) & 0xFFu)) + 0.5;
    return m * exp2(float(e) - 136.0);
}

// The panorama's radiance along direction `p` in its own frame: its texels filtered
// bilinearly, wrapping around in u, clamped at the poles. `p` falls at
// u = 0.5 + atan2(p.x, −p.z) / 2π (the middle faces −Z) and v = acos(p.y) / π (the top row
// faces +Y).
float3 panorama(float3 p)
{
    float2 uv = float2(0.5 + atan2(p.x, -p.z) / (2.0 * PI), acos(clamp(p.y, -1.0, 1.0)) / PI);
    float2 xy = uv * float2(push.width, push.height) - 0.5;
    float2 base = floor(xy);
    float2 f = xy - base;
    int w = int(push.width);
    int h = int(push.height);
    // u lies in 0..1, so the left column is −1 at the least.
    uint x0 = uint((int(base.x) + w) % w);
    uint x1 = uint((int(base.x) + 1 + w) % w);
    uint y0 = uint(clamp(int(base.y), 0, h - 1)) * push.width;
    uint y1 = uint(clamp(int(base.y) + 1, 0, h - 1)) * push.width;
    float3 top = lerp(rgbe(push.texels[y0 + x0]), rgbe(push.texels[y0 + x1]), f.x);
    float3 bottom = lerp(rgbe(push.texels[y1 + x0]), rgbe(push.texels[y1 + x1]), f.x);
    return lerp(top, bottom, f.y);
}

// Mip 0 of the background cube: samples² samples of the panorama per texel.
[shader("compute")]
[numthreads(8, 8, 1)]
void panoramaCube(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    uint n = push.samples;
    float3 sum = 0.0;
    for (uint k = 0; k < n * n; k++)
    {
        float2 offset = (float2(k % n, k / n) + 0.5) / float(n);
        sum += panorama(face_direction_at(float2(id.xy) + offset, id.z, push.size));
    }
    // Half floats end at 65,504: a brighter sun saturates there (white on screen; its
    // light, taken out as a directional light, is not lost).
    g_cube_faces[push.target][id] = float4(min(sum / float(n * n), 65504.0), 1.0);
}

// Mip 0 of the environment's lighting cube: samples² samples of the background cube's mip
// `level` per texel (the panorama turned by `turn`), those within the sun's cone (in the
// world) replaced by the sky around it.
[shader("compute")]
[numthreads(8, 8, 1)]
void panoramaLight(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    TextureCube background = g_cubes[push.source];
    SamplerState s = g_samplers[push.sampler];
    uint n = push.samples;
    float3 sum = 0.0;
    for (uint k = 0; k < n * n; k++)
    {
        float2 offset = (float2(k % n, k / n) + 0.5) / float(n);
        float3 d = face_direction_at(float2(id.xy) + offset, id.z, push.size);
        if (dot(d, push.sun.xyz) > push.sun.w)
            sum += push.sun_sky.rgb;
        else
            sum += background.SampleLevel(s, turned_back(d, push.turn), push.level).rgb;
    }
    g_cube_faces[push.target][id] = float4(sum / float(n * n), 1.0);
}
