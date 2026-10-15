// Color grading (ADR 0094, grading.rs): the view's grading and its tone curve as one 3D
// LUT, which the tone mapper's graded entry points sample once per pixel. The LUT is
// indexed by the ACEScct encoding (Academy S-2016-001) of the exposed scene-linear color,
// from black, and holds the tone-mapped color in units of reference white. PBR Neutral
// couples the channels (its offset follows the darkest, its roll-off the brightest), which
// a LUT interpolates badly (up to 6 / 255): with it the LUT holds the graded color in
// ACEScct, and the tone mapper applies the curve after it.
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

// One texel of the LUT: the color at its coordinate (from black, `lut_linear`), graded,
// then tone-mapped (in ACEScct with PBR Neutral).
[shader("compute")]
[numthreads(4, 4, 4)]
void gradeLut(uint3 id : SV_DispatchThreadID)
{
    if (any(id >= push.size))
        return;
    float3 v = lut_linear(float3(id) / float(push.size - 1));
    v = grade(max(v, 0.0), push.params);
    if ((push.curve & CURVE_PBR_NEUTRAL) != 0)
        v = acescct_encode(v);
    else if ((push.curve & CURVE_AGX) != 0)
        v = agx(v);
    else if ((push.curve & CURVE_ACES) != 0)
        v = aces(v);
    else
        v = gt3(v, push.peak);
    g_volume_images[push.lut][id] = float4(v, 1.0);
}
