// Image-based lighting (environment.rs, ADR 0046): what the sky sends towards a surface from
// every direction, prepared once per change of the sky rather than per pixel.
//
// - `skyCube` writes the sky's radiance into the faces of a cube map (mip 0), and
//   `downsample` fills its mip chain (a 2 × 2 average per texel).
// - `prefilter` convolves it with the GGX lobe for the roughness of each mip of a second
//   cube (Karis 2013, split sum), importance-sampling with filtered importance sampling
//   (Krivanek and Colbert 2008): each sample reads the mip whose texels are as large as the
//   sample's solid angle, so few samples stay smooth.
// - `irradiance` projects the radiance onto 9 spherical harmonics, convolved with the
//   clamped cosine (Ramamoorthi and Hanrahan 2001): diffuse light for any normal in 9
//   multiply-adds.
// - `dfg` integrates the split sum's second factor (Filament's formulation): for n·v and
//   roughness, x = ∫ Gv · Fc and y = ∫ Gv, so specular = prefiltered · mix(x, y, f0) and
//   1 + f0 · (1 / y − 1) compensates for multiple scattering.
//
// No code was copied; see docs/research/image-based-lighting.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2DArray<float4> g_cube_faces[];
[[vk::binding(1, 0)]] [vk::image_format("rg16f")] RWTexture2D<float2> g_luts[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    float4 zenith;           // rgb: the sky straight up
    float4 horizon;          // rgb: the sky at the horizon
    float4 ground;           // rgb: below the horizon
    float4* sh;              // `irradiance`: 9 coefficients out
    uint source;             // sampled cube (all mips)
    uint target;             // storage image being written (one mip)
    uint read;               // `downsample`: storage image of the mip above
    uint sampler;            // trilinear, clamp
    uint size;               // face (or LUT) size being written
    uint level;              // `prefilter`: mip being written
    uint levels;             // `prefilter`: mips of the prefiltered cube
    uint source_size;        // face size of the source's mip 0
    uint samples;            // importance samples per texel
    uint mode;               // `skyCube`: 0 the gradient, 1 debug: the direction as color
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

static const float PI = 3.14159265;

// The direction through the center of texel `xy` of cube face `face` at size `size`
// (Vulkan's cube map convention: faces +X, −X, +Y, −Y, +Z, −Z; v grows downwards).
float3 face_direction(uint2 xy, uint face, uint size)
{
    float2 uv = (float2(xy) + 0.5) / float(size) * 2.0 - 1.0;
    float3 d;
    switch (face)
    {
    case 0: d = float3(1.0, -uv.y, -uv.x); break;
    case 1: d = float3(-1.0, -uv.y, uv.x); break;
    case 2: d = float3(uv.x, 1.0, uv.y); break;
    case 3: d = float3(uv.x, -1.0, -uv.y); break;
    case 4: d = float3(uv.x, -uv.y, 1.0); break;
    default: d = float3(-uv.x, -uv.y, -1.0); break;
    }
    return normalize(d);
}

// The sky's radiance along `d` (+Y up): zenith to horizon above, horizon to ground below.
float3 sky(float3 d)
{
    if (push.mode == 1)
        return d * 0.5 + 0.5;
    if (d.y >= 0.0)
        return lerp(push.horizon.rgb, push.zenith.rgb, pow(d.y, 0.5));
    return lerp(push.horizon.rgb, push.ground.rgb, saturate(-d.y * 5.0));
}

[shader("compute")]
[numthreads(8, 8, 1)]
void skyCube(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    g_cube_faces[push.target][id] = float4(sky(face_direction(id.xy, id.z, push.size)), 1.0);
}

// Mip `size` from the mip above (twice the size): the average of each 2 × 2 block.
[shader("compute")]
[numthreads(8, 8, 1)]
void downsample(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    RWTexture2DArray<float4> above = g_cube_faces[push.read];
    uint3 p = uint3(id.xy * 2, id.z);
    float4 sum = above[p] + above[p + uint3(1, 0, 0)] + above[p + uint3(0, 1, 0)] + above[p + uint3(1, 1, 0)];
    g_cube_faces[push.target][id] = sum * 0.25;
}

// The i-th of n points of the Hammersley set.
float2 hammersley(uint i, uint n)
{
    return float2((float(i) + 0.5) / float(n), float(reversebits(i)) * 2.3283064365386963e-10);
}

// A GGX half vector around +Z for roughness `a` (α = perceptual²).
float3 ggx_half(float2 xi, float a)
{
    float phi = 2.0 * PI * xi.x;
    float cos_theta = sqrt((1.0 - xi.y) / (1.0 + (a * a - 1.0) * xi.y));
    float sin_theta = sqrt(1.0 - cos_theta * cos_theta);
    return float3(sin_theta * cos(phi), sin_theta * sin(phi), cos_theta);
}

// An orthonormal basis around `n` (Duff et al. 2017).
void basis(float3 n, out float3 t, out float3 b)
{
    float s = n.z >= 0.0 ? 1.0 : -1.0;
    float a = -1.0 / (s + n.z);
    float c = n.x * n.y * a;
    t = float3(1.0 + s * n.x * n.x * a, s * c, -s * n.x);
    b = float3(c, s + n.y * n.y * a, -n.y);
}

float distribution(float n_dot_h, float a)
{
    float a2 = a * a;
    float f = (n_dot_h * a2 - n_dot_h) * n_dot_h + 1.0;
    return a2 / (PI * f * f);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void prefilter(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    float3 n = face_direction(id.xy, id.z, push.size);
    TextureCube source = g_cubes[push.source];
    SamplerState s = g_samplers[push.sampler];
    if (push.level == 0)
    {
        g_cube_faces[push.target][id] = float4(source.SampleLevel(s, n, 0.0).rgb, 1.0);
        return;
    }
    float perceptual = float(push.level) / float(push.levels - 1);
    float a = perceptual * perceptual;
    float3 t, b;
    basis(n, t, b);
    // Solid angle of a texel of the source's mip 0.
    float texel = 4.0 * PI / (6.0 * float(push.source_size * push.source_size));
    float3 sum = 0.0;
    float weight = 0.0;
    for (uint i = 0; i < push.samples; i++)
    {
        float3 h = ggx_half(hammersley(i, push.samples), a);
        h = h.x * t + h.y * b + h.z * n;
        // n = v = r (the split sum's assumption).
        float n_dot_h = dot(n, h);
        float3 l = 2.0 * n_dot_h * h - n;
        float n_dot_l = dot(n, l);
        if (n_dot_l <= 0.0)
            continue;
        // pdf of l: D · n·h / (4 v·h), with v·h = n·h.
        float pdf = distribution(n_dot_h, a) * 0.25;
        float sample_angle = 1.0 / (float(push.samples) * pdf + 1e-6);
        float lod = max(0.5 * log2(sample_angle / texel) + 1.0, 0.0);
        sum += source.SampleLevel(s, l, lod).rgb * n_dot_l;
        weight += n_dot_l;
    }
    g_cube_faces[push.target][id] = float4(sum / max(weight, 1e-6), 1.0);
}

// The 9 real spherical harmonics (bands 0–2) at unit direction d.
void sh_basis(float3 d, out float y[9])
{
    y[0] = 0.282095;
    y[1] = 0.488603 * d.y;
    y[2] = 0.488603 * d.z;
    y[3] = 0.488603 * d.x;
    y[4] = 1.092548 * d.x * d.y;
    y[5] = 1.092548 * d.y * d.z;
    y[6] = 0.315392 * (3.0 * d.z * d.z - 1.0);
    y[7] = 1.092548 * d.x * d.z;
    y[8] = 0.546274 * (d.x * d.x - d.y * d.y);
}

groupshared float3 g_partial[256];
// The clamped cosine's convolution per band.
static const float BAND[9] = { PI, 2.0 * PI / 3.0, 2.0 * PI / 3.0, 2.0 * PI / 3.0, PI / 4.0, PI / 4.0, PI / 4.0, PI / 4.0, PI / 4.0 };

// One group of 256 threads: radiance × basis × solid angle over every texel of the
// source's mip `level` (face size `size`), reduced per coefficient, then convolved with the
// clamped cosine (π, 2π/3, π/4 per band): irradiance coefficients.
[shader("compute")]
[numthreads(256, 1, 1)]
void irradiance(uint local : SV_GroupIndex)
{
    TextureCube source = g_cubes[push.source];
    SamplerState s = g_samplers[push.sampler];
    uint size = push.size;
    float3 acc[9];
    [unroll]
    for (uint k = 0; k < 9; k++)
        acc[k] = 0.0;
    for (uint i = local; i < 6 * size * size; i += 256)
    {
        uint face = i / (size * size);
        uint r = i - face * size * size;
        uint2 xy = uint2(r % size, r / size);
        float2 uv = (float2(xy) + 0.5) / float(size) * 2.0 - 1.0;
        float3 d = face_direction(xy, face, size);
        // Solid angle of the texel: (2 / size)² / (1 + u² + v²)^(3/2).
        float solid = 4.0 / (float(size * size) * pow(1.0 + dot(uv, uv), 1.5));
        float3 radiance = source.SampleLevel(s, d, float(push.level)).rgb * solid;
        float y[9];
        sh_basis(d, y);
        [unroll]
        for (uint k = 0; k < 9; k++)
            acc[k] += radiance * y[k];
    }
    for (uint k = 0; k < 9; k++)
    {
        g_partial[local] = acc[k];
        GroupMemoryBarrierWithGroupSync();
        for (uint stride = 128; stride > 0; stride >>= 1)
        {
            if (local < stride)
                g_partial[local] += g_partial[local + stride];
            GroupMemoryBarrierWithGroupSync();
        }
        if (local == 0)
            push.sh[k] = float4(g_partial[0] * BAND[k], 0.0);
        GroupMemoryBarrierWithGroupSync();
    }
}

// Height-correlated Smith visibility, V = G / (4 n·l n·v).
float visibility(float n_dot_v, float n_dot_l, float a)
{
    float a2 = a * a;
    float gv = n_dot_l * sqrt(n_dot_v * n_dot_v * (1.0 - a2) + a2);
    float gl = n_dot_v * sqrt(n_dot_l * n_dot_l * (1.0 - a2) + a2);
    return 0.5 / (gv + gl);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void dfg(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    float n_dot_v = (float(id.x) + 0.5) / float(push.size);
    float perceptual = (float(id.y) + 0.5) / float(push.size);
    float a = perceptual * perceptual;
    float3 v = float3(sqrt(1.0 - n_dot_v * n_dot_v), 0.0, n_dot_v);
    float2 r = 0.0;
    for (uint i = 0; i < push.samples; i++)
    {
        float3 h = ggx_half(hammersley(i, push.samples), a);
        float v_dot_h = dot(v, h);
        float3 l = 2.0 * v_dot_h * h - v;
        float n_dot_l = saturate(l.z);
        float n_dot_h = saturate(h.z);
        if (n_dot_l <= 0.0)
            continue;
        // The sample's weight: V · 4 v·h n·l / n·h (the GGX pdf divided out).
        float gv = visibility(n_dot_v, n_dot_l, a) * 4.0 * saturate(v_dot_h) * n_dot_l / n_dot_h;
        float fc = pow(1.0 - saturate(v_dot_h), 5.0);
        r += float2(gv * fc, gv);
    }
    g_luts[push.target][id.xy] = r / float(push.samples);
}
