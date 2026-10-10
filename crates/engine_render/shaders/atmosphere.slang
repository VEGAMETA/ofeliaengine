// A physically based sky and atmosphere (atmosphere.rs, ADR 0069), after Hillaire, "A
// Scalable and Production Ready Sky and Atmosphere Rendering Technique" (EGSR 2020), and
// adapted from its MIT-licensed reference implementation (github.com/sebh/
// UnrealEngineSkyAtmosphere, Copyright (c) 2020 Epic Games, Inc.; THIRD_PARTY_NOTICES.md):
// the tables' parameterizations, the ray march and the multiple scattering approximation.
//
// A planet of radius `planet.x` (km) under an atmosphere up to `planet.y`: Rayleigh and Mie
// scattering with exponential density, ozone absorption in a tent around its peak. Light
// is the sun's, of unit illuminance: the tables are transfer factors, scaled by the light's
// color where they are read.
//
// - `transmittanceLut` (256 × 64): transmittance from a point to the top of the
//   atmosphere, by altitude and zenith angle (Bruneton's parameterization), 40 steps
//   (midpoints).
// - `multipleScatteringLut` (32 × 32): light scattered twice or more, by altitude and sun
//   zenith angle: 64 directions per texel (one thread each) gather second-order light
//   L₂ and the fraction f_ms re-scattered, summed as the series L₂ / (1 − f_ms).
// - `skyViewLut` (192 × 108): the sky seen from the camera's altitude by zenith angle
//   (finer near the horizon) and azimuth from the sun; the ground reflects with its albedo.
// - `aerialPerspective` (32 × 32 × 32): light scattered towards the camera and
//   transmittance along each froxel column, every frame: one thread per froxel integrates
//   its slice's segment, and a parallel scan sums each column front to back (Hillaire's
//   version marches each froxel's whole ray from the camera).
// - `atmosphereCube`: the sky into the environment's radiance cube, for image-based light.
// - `compositeFragment`: one full-screen pass after the opaque surfaces: the sky where
//   nothing was drawn, aerial perspective and height fog over the rest (premultiplied:
//   color = light + behind × transmittance).
//
// Distances in km, with the planet's center at the origin and +Y up.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2DArray<float4> g_cube_faces[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture3D<float4> g_volume_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];
// Multisampled depth targets (MSAA, ADR 0075), the same bindless slots.
[[vk::binding(0, 0)]] Texture2DMS<float> g_depth_ms[];

#include "atmosphere_common.slang"

struct Push
{
    AtmosphereFrame* frame;
    uint target;             // storage image written
    uint depth;              // composite: bindless depth target
    uint size;               // cube: face size
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

static const float PI = 3.14159265;
static const uint TRANSMITTANCE_WIDTH = 256;
static const uint TRANSMITTANCE_HEIGHT = 64;
static const float MULTIPLE_SCATTERING_SIZE = 32.0;
static const float SKY_VIEW_WIDTH = 192.0;
static const float SKY_VIEW_HEIGHT = 108.0;
// Samples of the sky-view rays: from the shortest to rays of 100 km and more.
static const float SKY_VIEW_MIN_SAMPLES = 8.0;
static const float SKY_VIEW_MAX_SAMPLES = 32.0;
// Steps of each froxel slice in `aerialPerspective`.
static const uint AERIAL_STEPS = 2;
// Points start this far (km) above the ground, out of its way.
static const float PLANET_OFFSET = 0.01;

float from_unit_to_sub_uv(float u, float resolution) { return (u + 0.5 / resolution) * (resolution / (resolution + 1.0)); }
float from_sub_uv_to_unit(float u, float resolution) { return (u - 0.5 / resolution) * (resolution / (resolution - 1.0)); }

// Distance from `o` along unit `d` to the sphere of radius `r` around the origin: the
// nearest intersection ahead, or −1.
float ray_sphere(float3 o, float3 d, float r)
{
    float b = dot(d, o);
    float c = dot(o, o) - r * r;
    float delta = b * b - c;
    if (delta < 0.0)
        return -1.0;
    float s = sqrt(delta);
    float t0 = -b - s;
    float t1 = -b + s;
    if (t0 < 0.0 && t1 < 0.0)
        return -1.0;
    if (t0 < 0.0)
        return max(0.0, t1);
    if (t1 < 0.0)
        return max(0.0, t0);
    return max(0.0, min(t0, t1));
}

// The transmittance table's texel → altitude (as a radius) and cos zenith (Bruneton 2017).
void transmittance_params(AtmosphereFrame* a, float2 uv, out float radius, out float mu)
{
    float bottom = atmosphere_read4(&a.planet).x;
    float top = atmosphere_read4(&a.planet).y;
    float h = sqrt(top * top - bottom * bottom);
    float rho = h * uv.y;
    radius = sqrt(rho * rho + bottom * bottom);
    float d_min = top - radius;
    float d_max = rho + h;
    float d = d_min + uv.x * (d_max - d_min);
    mu = d == 0.0 ? 1.0 : (h * h - rho * rho - d * d) / (2.0 * radius * d);
    mu = clamp(mu, -1.0, 1.0);
}

float2 transmittance_uv(AtmosphereFrame* a, float radius, float mu)
{
    float bottom = atmosphere_read4(&a.planet).x;
    float top = atmosphere_read4(&a.planet).y;
    float h = sqrt(max(0.0, top * top - bottom * bottom));
    float rho = sqrt(max(0.0, radius * radius - bottom * bottom));
    float discriminant = radius * radius * (mu * mu - 1.0) + top * top;
    float d = max(0.0, -radius * mu + sqrt(max(discriminant, 0.0)));
    float d_min = top - radius;
    float d_max = rho + h;
    return float2((d - d_min) / (d_max - d_min), rho / h);
}

float3 transmittance_to_top(AtmosphereFrame* a, float radius, float mu)
{
    return g_textures[a.transmittance].SampleLevel(g_samplers[a.sampler], transmittance_uv(a, radius, mu), 0.0).rgb;
}

float3 multiple_scattering(AtmosphereFrame* a, float radius, float mu_sun)
{
    float2 uv = saturate(float2(mu_sun * 0.5 + 0.5, (radius - atmosphere_read4(&a.planet).x) / (atmosphere_read4(&a.planet).y - atmosphere_read4(&a.planet).x)));
    uv = float2(from_unit_to_sub_uv(uv.x, MULTIPLE_SCATTERING_SIZE), from_unit_to_sub_uv(uv.y, MULTIPLE_SCATTERING_SIZE));
    return g_textures[a.multiple_scattering].SampleLevel(g_samplers[a.sampler], uv, 0.0).rgb;
}

// The sky-view table's texel → cos zenith and cos azimuth from the sun, for a camera at
// `radius`: half the rows above the horizon, half below, squared towards it.
void sky_view_params(AtmosphereFrame* a, float2 uv, float radius, out float mu, out float cos_azimuth)
{
    uv = float2(from_sub_uv_to_unit(uv.x, SKY_VIEW_WIDTH), from_sub_uv_to_unit(uv.y, SKY_VIEW_HEIGHT));
    float horizon = sqrt(max(radius * radius - atmosphere_read4(&a.planet).x * atmosphere_read4(&a.planet).x, 0.0));
    float beta = acos(horizon / radius);
    float zenith_horizon = PI - beta;
    if (uv.y < 0.5)
    {
        float coord = 1.0 - 2.0 * uv.y;
        coord = 1.0 - coord * coord;
        mu = cos(zenith_horizon * coord);
    }
    else
    {
        float coord = uv.y * 2.0 - 1.0;
        mu = cos(zenith_horizon + beta * coord * coord);
    }
    float coord = uv.x * uv.x;
    cos_azimuth = -(coord * 2.0 - 1.0);
}

float2 sky_view_uv(AtmosphereFrame* a, bool ground, float mu, float cos_azimuth, float radius)
{
    float horizon = sqrt(max(radius * radius - atmosphere_read4(&a.planet).x * atmosphere_read4(&a.planet).x, 0.0));
    float beta = acos(horizon / radius);
    float zenith_horizon = PI - beta;
    float2 uv;
    if (!ground)
    {
        float coord = 1.0 - acos(mu) / zenith_horizon;
        uv.y = (1.0 - sqrt(max(coord, 0.0))) * 0.5;
    }
    else
    {
        float coord = (acos(mu) - zenith_horizon) / beta;
        uv.y = sqrt(max(coord, 0.0)) * 0.5 + 0.5;
    }
    uv.x = sqrt(-cos_azimuth * 0.5 + 0.5);
    return float2(from_unit_to_sub_uv(uv.x, SKY_VIEW_WIDTH), from_unit_to_sub_uv(uv.y, SKY_VIEW_HEIGHT));
}

struct Medium
{
    float3 scattering;
    float3 extinction;
    float3 rayleigh;
    float3 mie;
};

Medium medium(AtmosphereFrame* a, float3 p)
{
    float altitude = max(length(p) - atmosphere_read4(&a.planet).x, 0.0);
    float rayleigh = exp(atmosphere_read4(&a.rayleigh).w * altitude);
    float mie = exp(atmosphere_read4(&a.mie_scattering).w * altitude);
    float ozone = max(0.0, 1.0 - abs(altitude - atmosphere_read4(&a.ozone).w) / atmosphere_read4(&a.ground_albedo).w);
    Medium m;
    m.rayleigh = rayleigh * atmosphere_read4(&a.rayleigh).rgb;
    m.mie = mie * atmosphere_read4(&a.mie_scattering).rgb;
    m.scattering = m.rayleigh + m.mie;
    m.extinction = m.rayleigh + mie * atmosphere_read4(&a.mie_extinction).rgb + ozone * atmosphere_read4(&a.ozone).rgb;
    return m;
}

float rayleigh_phase(float c)
{
    return 3.0 / (16.0 * PI) * (1.0 + c * c);
}

// Cornette-Shanks, as the reference; `c`: cos between the view ray and the sun.
float mie_phase(float g, float c)
{
    float k = 3.0 / (8.0 * PI) * (1.0 - g * g) / (2.0 + g * g);
    return k * (1.0 + c * c) / pow(1.0 + g * g - 2.0 * g * c, 1.5);
}

struct Scattering
{
    float3 luminance;        // in-scattered, per unit sun illuminance
    float3 transmittance;    // along the march
    float3 multiple_as_one;  // f_ms: light re-scattered if what reaches each point were 1
};

// In-scattering along the ray from `o` (km, planet frame) in unit direction `d` to the
// ground, the top of the atmosphere or `t_limit`, lit by a sun in unit direction `sun`.
// `variable`: the sample count grows with the ray (the sky-view table), distributed
// quadratically; otherwise `samples` even steps. `phases`: Rayleigh and Mie phase
// functions (else isotropic, for multiple scattering); `ground`: add the light the ground
// reflects at the end; `multiple`: add the multiple scattering table's light. All four are
// literals at every call.
Scattering integrate(AtmosphereFrame* a, float3 o, float3 d, float3 sun, float samples, bool variable, bool phases, bool ground, bool multiple, float t_limit)
{
    Scattering result = (Scattering)0;
    result.transmittance = 1.0;
    float t_bottom = ray_sphere(o, d, atmosphere_read4(&a.planet).x);
    float t_top = ray_sphere(o, d, atmosphere_read4(&a.planet).y);
    float t_max;
    if (t_bottom < 0.0)
    {
        if (t_top < 0.0)
            return result;
        t_max = t_top;
    }
    else
        t_max = t_top > 0.0 ? min(t_top, t_bottom) : t_bottom;
    t_max = min(t_max, t_limit);

    float count = samples;
    float count_floor = samples;
    float t_max_floor = t_max;
    if (variable)
    {
        count = lerp(SKY_VIEW_MIN_SAMPLES, SKY_VIEW_MAX_SAMPLES, saturate(t_max * 0.01));
        count_floor = floor(count);
        t_max_floor = t_max * count_floor / count;
    }
    float c = dot(sun, d);
    float mie_value = mie_phase(atmosphere_read4(&a.mie_extinction).w, c);
    float rayleigh_value = rayleigh_phase(c);
    const float uniform_phase = 1.0 / (4.0 * PI);

    float3 throughput = 1.0;
    float t = 0.0;
    float dt = t_max / count;
    for (float s = 0.0; s < count; s += 1.0)
    {
        if (variable)
        {
            float t0 = s / count_floor;
            float t1 = (s + 1.0) / count_floor;
            t0 = t0 * t0 * t_max_floor;
            t1 = t1 > 1.0 ? t_max : t1 * t1 * t_max_floor;
            t = t0 + (t1 - t0) * 0.5;
            dt = t1 - t0;
        }
        else
        {
            // Midpoints of even steps (the reference puts samples 0.3 into each step and
            // leaves the last 0.7 of the ray out).
            t = t_max * (s + 0.5) / count;
        }
        float3 p = o + t * d;
        Medium m = medium(a, p);
        float3 step_transmittance = exp(-m.extinction * dt);

        float radius = length(p);
        float3 up = p / radius;
        float mu_sun = dot(sun, up);
        float3 to_sun = transmittance_to_top(a, radius, mu_sun);
        float3 phase_scattering = phases ? m.mie * mie_value + m.rayleigh * rayleigh_value : m.scattering * uniform_phase;
        // The planet's shadow.
        float shadow = ray_sphere(p, sun, atmosphere_read4(&a.planet).x + PLANET_OFFSET) >= 0.0 ? 0.0 : 1.0;
        float3 ms = multiple ? multiple_scattering(a, radius, mu_sun) * atmosphere_read4(&a.planet).z : 0.0;
        float3 source = shadow * to_sun * phase_scattering + ms * m.scattering;

        // Each term integrated over the step against its transmittance (Hillaire 2015,
        // slide 28): (S − S · T) / σt.
        float3 inv_extinction = 1.0 / max(m.extinction, 1e-8);
        result.multiple_as_one += throughput * (m.scattering - m.scattering * step_transmittance) * inv_extinction;
        result.luminance += throughput * (source - source * step_transmittance) * inv_extinction;
        throughput *= step_transmittance;
    }
    if (ground && t_max == t_bottom && t_bottom > 0.0)
    {
        // Sunlight the ground reflects (Lambertian).
        float3 p = o + t_bottom * d;
        float radius = length(p);
        float3 up = p / radius;
        float mu_sun = dot(sun, up);
        result.luminance += transmittance_to_top(a, radius, mu_sun) * throughput * saturate(mu_sun) * atmosphere_read4(&a.ground_albedo).rgb / PI;
    }
    result.transmittance = throughput;
    return result;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void transmittanceLut(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= TRANSMITTANCE_WIDTH || id.y >= TRANSMITTANCE_HEIGHT)
        return;
    AtmosphereFrame* a = push.frame;
    float2 uv = (float2(id.xy) + 0.5) / float2(TRANSMITTANCE_WIDTH, TRANSMITTANCE_HEIGHT);
    float radius, mu;
    transmittance_params(a, uv, radius, mu);
    float3 o = float3(0.0, radius, 0.0);
    float3 d = float3(sqrt(1.0 - mu * mu), mu, 0.0);
    // Optical depth only: 40 even steps to the top (or the ground).
    float t_bottom = ray_sphere(o, d, atmosphere_read4(&a.planet).x);
    float t_top = ray_sphere(o, d, atmosphere_read4(&a.planet).y);
    float t_max = t_bottom >= 0.0 && t_top >= 0.0 ? min(t_top, t_bottom) : max(t_top, 0.0);
    float3 depth = 0.0;
    const float steps = 40.0;
    for (float s = 0.0; s < steps; s += 1.0)
        depth += medium(a, o + t_max * (s + 0.5) / steps * d).extinction;
    depth *= t_max / steps;
    g_images[push.target][id.xy] = float4(exp(-depth), 1.0);
}

groupshared float3 g_multiple[64];
groupshared float3 g_second[64];

// One workgroup per texel, one thread per direction of an 8 × 8 grid on the sphere.
[shader("compute")]
[numthreads(1, 1, 64)]
void multipleScatteringLut(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    AtmosphereFrame* a = push.frame;
    float2 uv = (float2(group.xy) + 0.5) / MULTIPLE_SCATTERING_SIZE;
    uv = float2(from_sub_uv_to_unit(uv.x, MULTIPLE_SCATTERING_SIZE), from_sub_uv_to_unit(uv.y, MULTIPLE_SCATTERING_SIZE));
    float mu_sun = uv.x * 2.0 - 1.0;
    float3 sun = float3(0.0, mu_sun, sqrt(saturate(1.0 - mu_sun * mu_sun)));
    float radius = atmosphere_read4(&a.planet).x + saturate(uv.y + PLANET_OFFSET) * (atmosphere_read4(&a.planet).y - atmosphere_read4(&a.planet).x - PLANET_OFFSET);
    float3 o = float3(0.0, radius, 0.0);

    float i = 0.5 + float(local / 8);
    float j = 0.5 + float(local % 8);
    float theta = 2.0 * PI * i / 8.0;
    float phi = acos(1.0 - 2.0 * j / 8.0);
    float3 d = float3(cos(theta) * sin(phi), cos(phi), sin(theta) * sin(phi));
    Scattering s = integrate(a, o, d, sun, 20.0, false, false, true, false, 1e9);
    const float sphere = 4.0 * PI / 64.0;
    g_multiple[local] = s.multiple_as_one * sphere;
    g_second[local] = s.luminance * sphere;
    GroupMemoryBarrierWithGroupSync();
    for (uint stride = 32; stride > 0; stride >>= 1)
    {
        if (local < stride)
        {
            g_multiple[local] += g_multiple[local + stride];
            g_second[local] += g_second[local + stride];
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (local != 0)
        return;
    const float isotropic = 1.0 / (4.0 * PI);
    // f_ms (Hillaire's equation 7) and L₂ (equation 5); all orders: L₂ / (1 − f_ms).
    float3 f_ms = g_multiple[0] * isotropic;
    float3 second = g_second[0] * isotropic;
    g_images[push.target][group.xy] = float4(second / (1.0 - f_ms), 1.0);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void skyViewLut(uint3 id : SV_DispatchThreadID)
{
    if (float(id.x) >= SKY_VIEW_WIDTH || float(id.y) >= SKY_VIEW_HEIGHT)
        return;
    AtmosphereFrame* a = push.frame;
    float2 uv = (float2(id.xy) + 0.5) / float2(SKY_VIEW_WIDTH, SKY_VIEW_HEIGHT);
    float radius = atmosphere_read4(&a.camera_planet).w;
    float mu, cos_azimuth;
    sky_view_params(a, uv, radius, mu, cos_azimuth);
    float mu_sun = atmosphere_read4(&a.sun_direction).w;
    float3 sun = float3(sqrt(saturate(1.0 - mu_sun * mu_sun)), mu_sun, 0.0);
    float sin_zenith = sqrt(saturate(1.0 - mu * mu));
    float3 d = float3(sin_zenith * cos_azimuth, mu, sin_zenith * sqrt(saturate(1.0 - cos_azimuth * cos_azimuth)));
    float3 o = float3(0.0, radius, 0.0);
    Scattering s = integrate(a, o, d, sun, 0.0, true, true, true, true, 1e9);
    g_images[push.target][id.xy] = float4(s.luminance, 1.0);
}

// The sky along world direction `d`, per unit sun illuminance, from the sky-view table.
float3 sky_luminance(AtmosphereFrame* a, float3 d)
{
    float radius = atmosphere_read4(&a.camera_planet).w;
    float mu = d.y;
    // Azimuth from the sun: both projected on the horizontal plane.
    float2 view_h = d.xz;
    float2 sun_h = atmosphere_read4(&a.sun_direction).xz;
    float lv = dot(view_h, view_h);
    float ls = dot(sun_h, sun_h);
    float cos_azimuth = lv > 1e-12 && ls > 1e-12 ? dot(view_h, sun_h) * rsqrt(lv * ls) : 1.0;
    bool ground = ray_sphere(float3(0.0, radius, 0.0), d, atmosphere_read4(&a.planet).x) >= 0.0;
    float2 uv = sky_view_uv(a, ground, mu, clamp(cos_azimuth, -1.0, 1.0), radius);
    return g_textures[a.sky_view].SampleLevel(g_samplers[a.sampler], uv, 0.0).rgb;
}

// The sky (without the sun's disk) into mip 0 of the environment's radiance cube, in the
// light's units.
[shader("compute")]
[numthreads(8, 8, 1)]
void atmosphereCube(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    AtmosphereFrame* a = push.frame;
    float3 d = face_direction(id.xy, id.z, push.size);
    g_cube_faces[push.target][id] = float4(sky_luminance(a, d) * atmosphere_read4(&a.sun_illuminance).rgb, 1.0);
}

groupshared float3 g_light[64];
groupshared float3 g_through[64];

// Aerial perspective: one thread per froxel, two columns of 32 slices per workgroup. Each
// thread integrates its own slice's segment of the column's ray (AERIAL_STEPS steps):
// in-scattered light L and transmittance T; then the column is summed front to back by a
// parallel scan (Hillis-Steele) of (L, T) under "a then b" = (La + Ta Lb, Ta Tb). Each
// froxel holds the light (per unit sun illuminance) from the camera to its slice's depth
// and the opacity 1 − mean transmittance.
[shader("compute")]
[numthreads(32, 2, 1)]
void aerialPerspective(uint3 id : SV_DispatchThreadID, uint3 local : SV_GroupThreadID)
{
    AtmosphereFrame* a = push.frame;
    uint slice = local.x;
    uint lane = local.y * 32 + slice;
    uint2 column = uint2(id.y % uint(AERIAL_SLICES), id.y / uint(AERIAL_SLICES));
    float2 ndc = (float2(column) + 0.5) / AERIAL_SLICES * 2.0 - 1.0;
    float4 near = mul(atmosphere_read44(&a.aerial_world_from_clip), float4(ndc, 0.0, 1.0));
    float4 mid = mul(atmosphere_read44(&a.aerial_world_from_clip), float4(ndc, 0.5, 1.0));
    float3 d = normalize(mid.xyz / mid.w - near.xyz / near.w);
    float3 o = atmosphere_read4(&a.camera_planet).xyz;
    float3 sun = atmosphere_read4(&a.sun_direction).xyz;
    float c = dot(sun, d);
    float mie_value = mie_phase(atmosphere_read4(&a.mie_extinction).w, c);
    float rayleigh_value = rayleigh_phase(c);

    // This slice's segment: from the previous slice's depth to its own.
    float w0 = float(slice) - 0.5;
    float w1 = float(slice) + 0.5;
    float start = slice == 0 ? 0.0 : w0 * w0 / AERIAL_SLICES * AERIAL_KM_PER_SLICE;
    float end = w1 * w1 / AERIAL_SLICES * AERIAL_KM_PER_SLICE;
    float dt = (end - start) / float(AERIAL_STEPS);
    float3 light = 0.0;
    float3 through = 1.0;
    for (uint k = 0; k < AERIAL_STEPS; k++)
    {
        float3 p = o + (start + (float(k) + 0.5) * dt) * d;
        // Below the ground (a camera under the world's origin), the air at the ground.
        float radius = max(length(p), atmosphere_read4(&a.planet).x + PLANET_OFFSET);
        float3 up = normalize(p);
        p = up * radius;
        Medium m = medium(a, p);
        float3 step_transmittance = exp(-m.extinction * dt);
        float mu_sun = dot(sun, up);
        float3 to_sun = transmittance_to_top(a, radius, mu_sun);
        float shadow = ray_sphere(p, sun, atmosphere_read4(&a.planet).x + PLANET_OFFSET) >= 0.0 ? 0.0 : 1.0;
        float3 ms = multiple_scattering(a, radius, mu_sun) * atmosphere_read4(&a.planet).z;
        float3 source = shadow * to_sun * (m.mie * mie_value + m.rayleigh * rayleigh_value) + ms * m.scattering;
        light += through * (source - source * step_transmittance) / max(m.extinction, 1e-8);
        through *= step_transmittance;
    }
    // Front to back along the column: each slice takes the segments before it.
    for (uint stride = 1; stride < uint(AERIAL_SLICES); stride <<= 1)
    {
        g_light[lane] = light;
        g_through[lane] = through;
        GroupMemoryBarrierWithGroupSync();
        if (slice >= stride)
        {
            float3 before_light = g_light[lane - stride];
            float3 before_through = g_through[lane - stride];
            light = before_light + before_through * light;
            through = before_through * through;
        }
        GroupMemoryBarrierWithGroupSync();
    }
    g_volume_images[push.target][uint3(column, slice)] = float4(light, 1.0 - dot(through, 1.0 / 3.0));
}

struct Composite
{
    float4 position : SV_Position;
};

// A triangle covering the target.
[shader("vertex")]
Composite compositeVertex(uint vertex : SV_VulkanVertexID)
{
    float2 ndc = float2((vertex << 1) & 2, vertex & 2) * 2.0 - 1.0;
    Composite o;
    o.position = float4(ndc, 0.0, 1.0);
    return o;
}

// The sky behind the scene along `d` (world): the atmosphere's or the environment's cube,
// and the sun's disk.
float3 background(AtmosphereFrame* a, float3 d)
{
    float3 color;
    if ((a.flags & ATMOSPHERE_SKY) != 0)
        color = sky_luminance(a, d) * atmosphere_read4(&a.sun_illuminance).rgb * atmosphere_read4(&a.view).y;
    else
        color = g_cubes[a.sky_cube].SampleLevel(g_samplers[a.sampler], turned_back(d, atmosphere_read4(&a.sky_turn).xy), 0.0).rgb * atmosphere_read4(&a.view).y;
    float cos_sun = atmosphere_read4(&a.planet).w;
    if (dot(d, atmosphere_read4(&a.sun_direction).xyz) > cos_sun)
        color += atmosphere_read4(&a.sun_color).rgb / (2.0 * PI * (1.0 - cos_sun));
    return color;
}

// After the opaque surfaces, before the transparent ones: where depth is 1 the sky (fog
// over it), elsewhere the aerial perspective and fog in front of the surface. Blended
// premultiplied: target = light + target × transmittance.
[shader("fragment")]
float4 compositeFragment(Composite input) : SV_Target0
{
    AtmosphereFrame* a = push.frame;
    float2 uv = input.position.xy / float2(loadAligned<8>(&a.size));
    float2 ndc = uv * 2.0 - 1.0;
    float depth = g_textures[push.depth].Load(int3(int2(input.position.xy), 0)).r;
    float3 d;
    float distance;
    if (atmosphere_read4(&a.ray_y).w != 0.0)
    {
        // Perspective: the ray to view depth 1, scaled by the pixel's view depth.
        float3 ray = atmosphere_read4(&a.ray_base).xyz + ndc.x * atmosphere_read4(&a.ray_x).xyz + ndc.y * atmosphere_read4(&a.ray_y).xyz;
        float inv_length = rsqrt(dot(ray, ray));
        d = ray * inv_length;
        distance = atmosphere_read4(&a.ray_x).w / (depth + atmosphere_read4(&a.ray_base).w) / inv_length;
    }
    else
    {
        float4 near = mul(atmosphere_read44(&a.world_from_clip), float4(ndc, 0.0, 1.0));
        float3 near_point = near.xyz / near.w;
        float4 far = mul(atmosphere_read44(&a.world_from_clip), float4(ndc, depth >= 1.0 ? 0.5 : depth, 1.0));
        float3 far_point = far.xyz / far.w;
        d = normalize(far_point - near_point);
        distance = length(far_point - atmosphere_read4(&a.camera_world).xyz);
    }
    float3 light = 0.0;
    float transmittance = 1.0;
    if (depth >= 1.0)
    {
        light = background(a, d);
        transmittance = 0.0;
        distance = 1e9;
    }
    else if ((a.flags & ATMOSPHERE_SKY) != 0)
    {
        float4 ap = aerial_perspective(a, uv, distance);
        light = ap.rgb;
        transmittance = 1.0 - ap.a;
    }
    if ((a.flags & ATMOSPHERE_FOG) != 0)
    {
        float4 fog = height_fog(a, d, distance);
        light = light * fog.a + fog.rgb;
        transmittance *= fog.a;
    }
    // Nothing visible to add (the near surfaces of small scenes): no blending, no color
    // traffic. Below these limits the change is under 0.1% of any pixel brighter than
    // 0.01, under half a code of an 8-bit sRGB output.
    if (transmittance >= 0.999 && max(light.r, max(light.g, light.b)) <= 1e-5)
        discard;
    return float4(light, 1.0 - transmittance);
}

// `compositeFragment` with MSAA (ADR 0075): per sample, from the sample's own depth, so
// fog and aerial perspective follow each sample's surface at edges (no halo of the far
// side's fog over the near side). The sample index makes it run at sample rate.
[shader("fragment")]
float4 compositeFragmentMs(Composite input, uint sample : SV_SampleIndex) : SV_Target0
{
    AtmosphereFrame* a = push.frame;
    float2 uv = input.position.xy / float2(loadAligned<8>(&a.size));
    float2 ndc = uv * 2.0 - 1.0;
    float depth = g_depth_ms[push.depth].Load(int2(input.position.xy), sample).r;
    float3 d;
    float distance;
    if (atmosphere_read4(&a.ray_y).w != 0.0)
    {
        // Perspective: the ray to view depth 1, scaled by the pixel's view depth.
        float3 ray = atmosphere_read4(&a.ray_base).xyz + ndc.x * atmosphere_read4(&a.ray_x).xyz + ndc.y * atmosphere_read4(&a.ray_y).xyz;
        float inv_length = rsqrt(dot(ray, ray));
        d = ray * inv_length;
        distance = atmosphere_read4(&a.ray_x).w / (depth + atmosphere_read4(&a.ray_base).w) / inv_length;
    }
    else
    {
        float4 near = mul(atmosphere_read44(&a.world_from_clip), float4(ndc, 0.0, 1.0));
        float3 near_point = near.xyz / near.w;
        float4 far = mul(atmosphere_read44(&a.world_from_clip), float4(ndc, depth >= 1.0 ? 0.5 : depth, 1.0));
        float3 far_point = far.xyz / far.w;
        d = normalize(far_point - near_point);
        distance = length(far_point - atmosphere_read4(&a.camera_world).xyz);
    }
    float3 light = 0.0;
    float transmittance = 1.0;
    if (depth >= 1.0)
    {
        light = background(a, d);
        transmittance = 0.0;
        distance = 1e9;
    }
    else if ((a.flags & ATMOSPHERE_SKY) != 0)
    {
        float4 ap = aerial_perspective(a, uv, distance);
        light = ap.rgb;
        transmittance = 1.0 - ap.a;
    }
    if ((a.flags & ATMOSPHERE_FOG) != 0)
    {
        float4 fog = height_fog(a, d, distance);
        light = light * fog.a + fog.rgb;
        transmittance *= fog.a;
    }
    // Nothing visible to add (the near surfaces of small scenes): no blending, no color
    // traffic. Below these limits the change is under 0.1% of any pixel brighter than
    // 0.01, under half a code of an 8-bit sRGB output.
    if (transmittance >= 0.999 && max(light.r, max(light.g, light.b)) <= 1e-5)
        discard;
    return float4(light, 1.0 - transmittance);
}
