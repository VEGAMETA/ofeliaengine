// Color grading (ADR 0094, grading.rs): the view's grading and its tone curve as one 3D
// LUT, which the tone mapper's graded entry points sample once per pixel. The LUT is
// indexed by the ACEScct encoding (Academy S-2016-001) of the exposed scene-linear color,
// from black, and holds the tone-mapped color in units of reference white. PBR Neutral
// couples the channels (its offset follows the darkest, its roll-off the brightest), which
// a LUT interpolates badly (up to 6 / 255): with it the LUT holds the graded color in
// ACEScct, and the tone mapper applies the curve after it.
//
// A view's color vision transform (ADR 0097, color_vision.rs) follows the curve, on the
// displayed color. With PBR Neutral it comes before the curve (which follows the LUT): the
// same below the curve's roll-off (0.76 of white), where PBR Neutral leaves colors as they
// are but for its toe's offset.
//
// The grading, in Filament's order (Apache-2.0; the operations are published standards),
// in linear BT.709 unless stated: white balance and the channel mixer (one matrix, built
// on the CPU), shadows / mid-tones / highlights (multipliers over Resolve's tonal ranges),
// in ACEScct the ASC CDL and contrast around mid-grey, then vibrance and saturation.
// `ColorGrading::apply` in grading.rs is the CPU description, and must match.

#include "tone_curves.slang"

[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture3D<float4> g_volume_images[];

struct GradingParams
{
    float4 matrix[3];        // rows (xyz): white balance, then the channel mixer
    float4 shadows;          // xyz: multipliers
    float4 midtones;
    float4 highlights;
    float4 ranges;           // the four tonal range points, linear luminance
    float4 slope;            // xyz: the ASC CDL, in ACEScct
    float4 offset;
    float4 power;
    float4 misc;             // x: contrast, y: vibrance, z: saturation
    // The color vision transform (`color_vision::Transform`): keep · c + take · sim(c).
    float4 sim_a[3];         // rows: the simulation on the plane's side (dot ≥ 0)
    float4 sim_b[3];         // rows: on its other side
    float4 plane;            // xyz: the separation plane's normal, w: the simulation's mix
    float4 keep[3];
    float4 take[3];
};

struct Push
{
    uint lut;                // storage index of the LUT
    uint size;               // texels per side
    uint curve;              // CURVE_*: the tone mapper's flags for the same curves
    float peak;              // the curve's peak, in units of reference white
    GradingParams* params;
};

static const uint CURVE_PBR_NEUTRAL = 16;
static const uint CURVE_AGX = 32;
static const uint CURVE_ACES = 64;

[[vk::push_constant]]
ConstantBuffer<Push> push;

static const float3 LUMINANCE = float3(0.2126, 0.7152, 0.0722);
static const float MID_GREY_ACESCCT = 0.4135884;

float4 read4(float4* v)
{
    return loadAligned<16>(v);
}

// `ColorGrading::apply`.
float3 grade(float3 v, GradingParams* p)
{
    v = max(float3(dot(read4(&p.matrix[0]).xyz, v), dot(read4(&p.matrix[1]).xyz, v), dot(read4(&p.matrix[2]).xyz, v)), 0.0);
    // Shadows, mid-tones, highlights.
    float4 ranges = read4(&p.ranges);
    float y = dot(v, LUMINANCE);
    float s = 1.0 - smoothstep(ranges.x, max(ranges.y, ranges.x + 1e-6), y);
    float h = smoothstep(ranges.z, max(ranges.w, ranges.z + 1e-6), y);
    float m = 1.0 - s - h;
    v *= s * read4(&p.shadows).xyz + m * read4(&p.midtones).xyz + h * read4(&p.highlights).xyz;
    // In ACEScct: the CDL, then contrast around mid-grey.
    float4 misc = read4(&p.misc);
    float3 c = acescct_encode(v) * read4(&p.slope).xyz + read4(&p.offset).xyz;
    c = select(c > 0.0, pow(max(c, 1e-10), read4(&p.power).xyz), c);
    c = (c - MID_GREY_ACESCCT) * misc.x + MID_GREY_ACESCCT;
    v = acescct_decode(c);
    // Vibrance, then saturation, around luminance.
    y = dot(v, LUMINANCE);
    float high = max(v.r, max(v.g, v.b));
    float low = min(v.r, min(v.g, v.b));
    float saturated = high > 1e-6 ? saturate((high - low) / high) : 0.0;
    float amount = 1.0 + (misc.y - 1.0) * (1.0 - saturated);
    v = y + amount * (v - y);
    y = dot(v, LUMINANCE);
    return max(y + misc.z * (v - y), 0.0);
}

// `SEPARATION_BAND` in color_vision.rs.
static const float SEPARATION_BAND = 0.02;

float3 rows(float4* m, float3 v)
{
    return float3(dot(read4(&m[0]).xyz, v), dot(read4(&m[1]).xyz, v), dot(read4(&m[2]).xyz, v));
}

// `ColorVision::apply`: the displayed color as a viewer with a deficiency sees it, or
// corrected for them (never below 0; a correction may pass the curve's peak).
float3 color_vision(float3 c, GradingParams* p)
{
    float4 plane = read4(&p.plane);
    // The two half-planes meet on the grey axis: blended over a narrow band around their
    // separation (`ColorVision::apply`), so that the LUT interpolating across it keeps
    // greys grey.
    float side = smoothstep(-SEPARATION_BAND, SEPARATION_BAND, dot(plane.xyz, c) / max(c.r + c.g + c.b, 1e-6));
    float3 projected = lerp(rows(&p.sim_b[0], c), rows(&p.sim_a[0], c), side);
    float3 sim = c + plane.w * (projected - c);
    return max(rows(&p.keep[0], c) + rows(&p.take[0], sim), 0.0);
}

// One texel of the LUT: the color at its coordinate (from black, `lut_linear`), graded,
// then tone-mapped, then the color vision transform (with PBR Neutral: graded, the color
// vision transform, in ACEScct).
[shader("compute")]
[numthreads(4, 4, 4)]
void gradeLut(uint3 id : SV_DispatchThreadID)
{
    if (any(id >= push.size))
        return;
    float3 v = lut_linear(float3(id) / float(push.size - 1));
    v = grade(max(v, 0.0), push.params);
    if ((push.curve & CURVE_PBR_NEUTRAL) != 0)
    {
        g_volume_images[push.lut][id] = float4(acescct_encode(color_vision(v, push.params)), 1.0);
        return;
    }
    if ((push.curve & CURVE_AGX) != 0)
        v = agx(v);
    else if ((push.curve & CURVE_ACES) != 0)
        v = aces(v);
    else
        v = gt3(v, push.peak);
    g_volume_images[push.lut][id] = float4(color_vision(v, push.params), 1.0);
}
