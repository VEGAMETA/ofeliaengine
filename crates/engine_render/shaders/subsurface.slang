// Screen-space subsurface scattering (subsurface.rs, ADR 0059): the scatter pass.
//
// The forward subsurface pass left, at every pixel of a subsurface material, its diffuse
// light per unit albedo (rgb) with its depth from the near plane (a; 0 elsewhere), and
// its exit albedo with its profile's index. A full-screen triangle adds, at each such
// pixel, the albedo times the profile-weighted average of that light around it:
//
// - the share of the profile that falls inside the pixel's own footprint (its CDF at half
//   a pixel) weights the pixel itself;
// - the samples beyond it lie on a disk in the screen plane: radii importance-sampled
//   from the profile, stratified in its CDF (precomputed per profile and budget on the
//   CPU), angles on a golden-angle spiral rotated per pixel by interleaved gradient noise
//   (new every frame only under a temporal resolve);
// - each sample is weighted by the profile at its 3D distance (so light does not cross
//   depth discontinuities) over the density it was drawn with; pixels of other materials
//   are skipped;
// - each channel is renormalized by the weights it used (Golubev 2018; Jimenez et al.
//   2015), which keeps a surface lit evenly exactly as it was.
//
// A pixel whose whole kernel is smaller than half a pixel skips the samples: its albedo
// times its own light, as without scattering.

[[vk::binding(0, 0)]] Texture2D g_textures[];

// `GpuProfile` in subsurface.rs (as in forward.slang).
struct SubsurfaceProfile
{
    float4 gaussians[6];     // rgb: weight / (2π v); w: −log2(e) / 2v
    uint count;
    uint3 _pad;
    float4 radii;            // largest sample radius per budget
    float2 kernel[148];      // per budget: radius, 1 / (n · density)
};

// `ScatterPush` in subsurface.rs.
struct ScatterPush
{
    SubsurfaceProfile* profiles;
    uint irradiance;         // bindless: rgb diffuse light, a depth from the near plane
    uint albedo;             // bindless: rgb exit albedo, a profile index / 255
    uint profile_count;
    uint kernel;             // the budget's first kernel entry
    uint samples;
    uint budget;             // index into `radii`
    uint2 size;              // target size in pixels
    float pixels_per_unit;   // pixels per world unit at w = 1
    float w_scale;           // clip w = w_scale · depth + w_offset
    float w_offset;
    uint frame;              // noise index
    uint2 _pad;
};

[[vk::push_constant]]
ConstantBuffer<ScatterPush> push;

static const float GOLDEN_ANGLE = 2.39996323;
static const float TWO_PI = 6.28318531;
// π log2(e): a Gaussian's weight from its stored amplitude and exponent.
static const float PI_LOG2E = 4.53236014;

// The profile's reflectance per channel at squared distance `r2` (world units²):
// `SubsurfaceProfile::evaluate` in subsurface.rs.
float3 profile_evaluate(SubsurfaceProfile* p, uint count, float r2)
{
    float3 r = 0.0;
    for (uint i = 0; i < count; i++)
    {
        float4 g = p.gaussians[i];
        r += g.rgb * exp2(g.w * r2);
    }
    return r;
}

// The share of each channel's light within radius `r`: `SubsurfaceProfile::cdf`.
float3 profile_cdf(SubsurfaceProfile* p, uint count, float r)
{
    float3 c = 0.0;
    for (uint i = 0; i < count; i++)
    {
        float4 g = p.gaussians[i];
        c += g.rgb * (PI_LOG2E / -g.w) * (1.0 - exp2(g.w * r * r));
    }
    return c;
}

// Interleaved gradient noise (Jimenez 2014) in [0, 1), moved per frame.
float gradient_noise(float2 pixel, uint frame)
{
    pixel += 5.588238 * float(frame & 63u);
    return frac(52.9829189 * frac(dot(pixel, float2(0.06711056, 0.00583715))));
}

// A triangle covering the target.
[shader("vertex")]
float4 scatterVertex(uint vertex : SV_VulkanVertexID) : SV_Position
{
    float2 ndc = float2((vertex << 1) & 2, vertex & 2) * 2.0 - 1.0;
    return float4(ndc, 0.0, 1.0);
}

[shader("fragment")]
float4 scatterFragment(float4 position : SV_Position) : SV_Target0
{
    int2 pixel = int2(position.xy);
    Texture2D irradiance = g_textures[push.irradiance];
    float4 center = irradiance.Load(int3(pixel, 0));
    if (center.a <= 0.0)
        discard;
    float4 albedo = g_textures[push.albedo].Load(int3(pixel, 0));
    uint index = min(uint(albedo.a * 255.0 + 0.5), max(push.profile_count, 1u) - 1u);
    SubsurfaceProfile* profile = push.profiles + index;
    uint count = profile.count;
    float depth = center.a;
    // World units per pixel here, and half of it: the pixel's own footprint.
    float pixels_per_unit = push.pixels_per_unit / max(push.w_scale * depth + push.w_offset, 1e-6);
    float inside = 0.5 / pixels_per_unit;
    if (profile.radii[push.budget] <= inside)
        return float4(albedo.rgb * center.rgb, 0.0);

    float3 weights = profile_cdf(profile, count, inside);
    float3 sum = weights * center.rgb;
    float rotation = TWO_PI * gradient_noise(position.xy, push.frame);
    int2 last = int2(push.size) - 1;
    for (uint k = 0; k < push.samples; k++)
    {
        float2 entry = profile.kernel[push.kernel + k];
        float r = entry.x;
        if (r <= inside)
            continue;
        float angle = float(k) * GOLDEN_ANGLE + rotation;
        float2 offset = float2(cos(angle), sin(angle)) * (r * pixels_per_unit);
        int2 at = pixel + int2(round(offset));
        if (any(at < int2(0, 0)) || any(at > last))
            continue;
        float4 s = irradiance.Load(int3(at, 0));
        if (s.a <= 0.0)
            continue;
        float dz = s.a - depth;
        float3 w = profile_evaluate(profile, count, r * r + dz * dz) * entry.y;
        sum += w * s.rgb;
        weights += w;
    }
    return float4(albedo.rgb * sum / max(weights, 1e-30), 0.0);
}
