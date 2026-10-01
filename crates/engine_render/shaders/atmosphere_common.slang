// What the atmosphere's full-screen pass (atmosphere.slang) and the transparent surfaces'
// atmosphere variants (forward.slang) share: the frame's atmosphere and fog constants, the
// aerial perspective lookup and the exponential height fog (ADR 0069, atmosphere.rs).
//
// No entry points: `#include`d after the bindless arrays (`g_samplers`) are declared.

[[vk::binding(0, 0)]] Texture3D g_volumes[];

// `AtmosphereConstants` in atmosphere.rs (464 bytes). Distances of the atmosphere in km,
// with the planet's center at the origin; the scene's in world units.
struct AtmosphereFrame
{
    float4x4 world_from_clip;        // jittered: the composite's rays, as the scene was drawn
    float4x4 aerial_world_from_clip; // unjittered: the froxels' rays
    float4 rayleigh;                 // rgb scattering (1/km), w: −1 / scale height (1/km)
    float4 mie_scattering;           // rgb (1/km), w: −1 / scale height
    float4 mie_extinction;           // rgb (1/km), w: phase anisotropy g
    float4 ozone;                    // rgb absorption (1/km), w: altitude of the layer's peak (km)
    float4 ground_albedo;            // rgb, w: half width of the ozone layer (km)
    float4 planet;                   // x ground radius, y top radius (km), z multiple scattering factor, w cos(sun angular radius)
    float4 sun_direction;            // xyz towards the sun (world); w: cos(sun zenith) the sky-view table was made for
    float4 sun_illuminance;          // rgb: the light's color outside the atmosphere (sky and aerial perspective scale)
    float4 camera_world;             // xyz camera (world units); w: km per world unit
    float4 camera_planet;            // xyz camera (km, planet's center at 0); w: the sky-view table's radius (km)
    float4 view;                     // x aerial perspective distance scale, y environment intensity, z distance (world units) within which the aerial perspective is invisible
    float4 fog_albedo;               // rgb single-scattering albedo, w: extinction per world unit at the base height
    float4 fog;                      // x base height, y height falloff (per world unit), z start distance, w max opacity
    float4 fog_phase;                // x anisotropy g of the sun's in-scattering, y extinction per world unit at the camera's height
    float4 sun_color;                // rgb: the directional light at the camera (after the atmosphere)
    float4 ray_base;                 // perspective: world direction to view depth 1 at ndc 0 (xyz); w: p22
    float4 ray_x;                    // its change per ndc x (xyz); w: p32 (view depth = p32 / (depth + p22))
    float4 ray_y;                    // its change per ndc y (xyz); w: 1 for a perspective camera, else 0
    float4* sh;                      // the environment's 9 irradiance harmonics (fog ambient light)
    uint transmittance;              // bindless: transmittance table
    uint multiple_scattering;        // bindless: multiple scattering table
    uint sky_view;                   // bindless: sky-view table
    uint aerial;                     // bindless: aerial perspective froxels
    uint sky_cube;                   // bindless: the environment's radiance cube (gradient sky)
    uint sampler;                    // bilinear, clamp
    uint flags;                      // ATMOSPHERE_* bits
    uint _pad;
    uint2 size;                      // the color target's size in pixels
};

static const uint ATMOSPHERE_SKY = 1u;   // the physical atmosphere (else the gradient's cube)
static const uint ATMOSPHERE_FOG = 2u;   // exponential height fog

static const float ATMOSPHERE_PI = 3.14159265;
// Aerial perspective froxels (Hillaire 2020): 32 slices, squared distribution over
// 32 × 4 km.
static const float AERIAL_SLICES = 32.0;
static const float AERIAL_KM_PER_SLICE = 4.0;

// Henyey-Greenstein; `c`: cos of the angle between the view ray and the light.
float henyey_greenstein(float g, float c)
{
    float d = 1.0 + g * g - 2.0 * g * c;
    return (1.0 - g * g) / (4.0 * ATMOSPHERE_PI * d * sqrt(d));
}

// The aerial perspective in front of a surface `distance` world units away along the
// pixel at `uv`: in-scattered light (rgb) and opacity (a, 1 − mean transmittance).
float4 aerial_perspective(AtmosphereFrame* a, float2 uv, float distance)
{
    if (distance <= a.view.z)
        return float4(0.0, 0.0, 0.0, 0.0);
    float slice = distance * a.camera_world.w * a.view.x / AERIAL_KM_PER_SLICE;
    float weight = 1.0;
    if (slice < 0.5)
    {
        // Fades to nothing at the camera, through the first slice.
        weight = saturate(slice * 2.0);
        slice = 0.5;
    }
    float w = sqrt(slice / AERIAL_SLICES);
    float4 ap = g_volumes[a.aerial].SampleLevel(g_samplers[a.sampler], float3(uv, w), 0.0) * weight;
    return float4(ap.rgb * a.sun_illuminance.rgb * a.view.y, ap.a);
}

// Optical depth of the height fog from the camera along unit `d` over `distance` world
// units: ∫ density · exp(−falloff · (y − base)) along the ray, analytically. The density at
// the camera's height comes with the constants.
float fog_optical_depth(AtmosphereFrame* a, float3 d, float distance)
{
    float start = a.fog.z;
    float len = distance - start;
    float k = a.fog.y;
    float density = a.fog_phase.y;
    if (start > 0.0)
        density *= exp(clamp(-k * d.y * start, -80.0, 80.0));
    float x = max(k * d.y * len, -80.0);
    // (1 − e^−x) / x, and its series near 0 (horizontal rays, no falloff).
    float g = abs(x) > 1e-4 ? (1.0 - exp(-x)) / x : 1.0 - 0.5 * x;
    return density * max(len, 0.0) * g;
}

// The fog in front of a surface `distance` world units away along unit `d`: in-scattered
// light (rgb) and transmittance (a). Light reaching the fog: the sky's average radiance
// (isotropic) and the sun (Henyey-Greenstein), not shadowed.
float4 height_fog(AtmosphereFrame* a, float3 d, float distance)
{
    float t = exp(-fog_optical_depth(a, d, distance));
    t = max(t, 1.0 - a.fog.w);
    // Average radiance of the sky: the irradiance harmonics' band 0 (π · Y00 · ∫L Y00) ×
    // Y00 / π.
    float3 ambient = a.sh[0].rgb * (0.282095 / ATMOSPHERE_PI) * a.view.y;
    float3 sun = a.sun_color.rgb * henyey_greenstein(a.fog_phase.x, dot(d, a.sun_direction.xyz));
    return float4((1.0 - t) * a.fog_albedo.rgb * (ambient + sun), t);
}

// A surface's color seen through the aerial perspective and the fog: `world` is its
// position, `uv` its pixel's place on the target.
float3 atmosphere_apply(AtmosphereFrame* a, float3 color, float3 world, float2 uv)
{
    float3 to = world - a.camera_world.xyz;
    float distance = length(to);
    if ((a.flags & ATMOSPHERE_SKY) != 0)
    {
        float4 ap = aerial_perspective(a, uv, distance);
        color = color * (1.0 - ap.a) + ap.rgb;
    }
    if ((a.flags & ATMOSPHERE_FOG) != 0)
    {
        float4 fog = height_fog(a, to / max(distance, 1e-6), distance);
        color = color * fog.a + fog.rgb;
    }
    return color;
}
