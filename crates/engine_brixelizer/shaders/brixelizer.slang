// Brixelizer GI in the engine (ADR 0090): what the SDK needs that the forward renderer does
// not make, and what the GI shading (ADR 0088) reads from what the SDK makes.
//
// - `brixelizerInputs`: from the view's depth after the prepass, the depth Brixelizer GI
//   reads (inverted: 1 at the near plane, 0 at the far plane and the sky) and the surfaces'
//   normals, as probe GI's resolve takes them (`giResolve` in engine_render's gi.slang): no
//   normal maps, both GIs see the same surfaces. Roughness 1 in the normals' alpha (no
//   specular rays).
// - `brixelizerIrradiance`: the SDK's diffuse light (irradiance), with each pixel's
//   distance from the camera (0: the sky), as `gi_irradiance` in forward.slang reads it.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("r32f")] RWTexture2D<float> g_depth_images[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];

// `ScreenFrame` in lib.rs (112 bytes), in the upload ring.
struct ScreenFrame
{
    float4x4 world_from_clip; // this frame's, without the jitter
    float4 eye;               // xyz: the camera
    uint2 size;               // the view's
    float2 ndc_per_pixel;     // 2 / size
    uint _pad0;
    uint _pad1;
    uint _pad2;
    uint _pad3;
};

// `Push` in lib.rs (24 bytes).
struct Push
{
    ScreenFrame* frame;
    uint depth;   // the view's depth (sampled)
    uint source;  // inputs: unused; irradiance: the SDK's diffuse light (sampled)
    uint target0; // inputs: the inverted depth (r32f); irradiance: the GI shading's (rgba16f)
    uint target1; // inputs: the normals (rgba16f); irradiance: unused
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

float load_depth(ScreenFrame* f, int2 xy)
{
    xy = clamp(xy, int2(0, 0), int2(f.size) - 1);
    return g_textures[push.depth].Load(int3(xy, 0)).r;
}

float4 column(float4x4 m, uint i)
{
    return float4(m[0][i], m[1][i], m[2][i], m[3][i]);
}

float3 position(float4x4 m, ScreenFrame* f, int2 xy, float depth, out float4 h)
{
    float2 ndc = (float2(xy) + 0.5) * f.ndc_per_pixel + float2(-1.0, -1.0);
    h = mul(m, float4(ndc, depth, 1.0));
    return h.xyz / h.w;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void brixelizerInputs(uint3 id : SV_DispatchThreadID)
{
    ScreenFrame* f = push.frame;
    if (any(id.xy >= f.size))
        return;
    int2 xy = int2(id.xy);
    float depth = load_depth(f, xy);
    g_depth_images[push.target0][id.xy] = 1.0 - depth;
    if (depth >= 1.0)
    {
        g_images[push.target1][id.xy] = float4(0.0, 1.0, 0.0, 1.0);
        return;
    }
    float4x4 m = loadAligned<16>(&f.world_from_clip);
    float4 h;
    float3 p = position(m, f, xy, depth, h);
    // On each axis the side whose depth is closer, so edges do not bend the normal.
    float dl = load_depth(f, xy - int2(1, 0));
    float dr = load_depth(f, xy + int2(1, 0));
    float du = load_depth(f, xy - int2(0, 1));
    float dd = load_depth(f, xy + int2(0, 1));
    float sx = abs(dr - depth) < abs(dl - depth) ? 1.0 : -1.0;
    float sy = abs(dd - depth) < abs(du - depth) ? 1.0 : -1.0;
    float4 along_x = column(m, 0) * f.ndc_per_pixel.x;
    float4 along_y = column(m, 1) * f.ndc_per_pixel.y;
    float4 along_z = column(m, 2);
    float4 hx = h + along_x * sx + along_z * ((sx > 0.0 ? dr : dl) - depth);
    float4 hy = h + along_y * sy + along_z * ((sy > 0.0 ? dd : du) - depth);
    float3 dx = (hx.xyz / hx.w - p) * sx;
    float3 dy = (hy.xyz / hy.w - p) * sy;
    float3 n = cross(dy, dx);
    n = n / max(length(n), 1e-20);
    float3 eye = loadAligned<16>(&f.eye).xyz;
    if (dot(n, eye - p) < 0.0)
        n = -n;
    g_images[push.target1][id.xy] = float4(n, 1.0);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void brixelizerIrradiance(uint3 id : SV_DispatchThreadID)
{
    ScreenFrame* f = push.frame;
    if (any(id.xy >= f.size))
        return;
    int2 xy = int2(id.xy);
    float depth = load_depth(f, xy);
    if (depth >= 1.0)
    {
        g_images[push.target0][id.xy] = 0.0;
        return;
    }
    float4x4 m = loadAligned<16>(&f.world_from_clip);
    float4 h;
    float3 p = position(m, f, xy, depth, h);
    float3 eye = loadAligned<16>(&f.eye).xyz;
    float3 diffuse = g_textures[push.source].Load(int3(xy, 0)).rgb;
    g_images[push.target0][id.xy] = float4(max(diffuse, 0.0), length(eye - p));
}
