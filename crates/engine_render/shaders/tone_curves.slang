// The tone curves (ADR 0048), shared by the tone mapper (`tonemap.slang`) and the color
// grading LUT that holds them (`grading.slang`, ADR 0094): plain functions, no bindings.

// PBR Neutral with its roll-off scaled to `peak`: the toe is unchanged, compression
// starts at 0.76 · peak and approaches peak. With peak = 1 it is the published curve.
float3 pbr_neutral(float3 color, float peak)
{
    const float start_compression = 0.8 - 0.04;
    const float desaturation = 0.15;
    float x = min(color.r, min(color.g, color.b));
    float offset = x < 0.08 ? x - 6.25 * x * x : 0.04;
    color -= offset;
    float brightest = max(color.r, max(color.g, color.b));
    float start = start_compression * peak;
    if (brightest < start)
        return color;
    float d = peak - start;
    float new_peak = peak - d * d / (brightest + d - start);
    color *= new_peak / brightest;
    float g = 1.0 - 1.0 / (desaturation * (brightest - new_peak) / peak + 1.0);
    return lerp(color, float3(new_peak, new_peak, new_peak), g);
}

// Uchimura's curve (`gt` in post.rs), the published defaults, rolling off to `peak`. Only
// the section a value falls in is evaluated (neighbouring pixels share it): the toe's power
// and the shoulder's exponential are not both paid for.
float gt(float x, float peak)
{
    const float a = 1.0;
    const float m = 0.22;
    const float l = 0.4;
    const float c = 1.33;
    const float b = 0.0;
    float p = max(peak, m + 1e-3);
    float l0 = (p - m) * l / a;
    float s0 = m + l0;
    float linear = m + a * (x - m);
    if (x < m)
    {
        float t = x / m;
        float w0 = 1.0 - t * t * (3.0 - 2.0 * t);
        float toe = m * pow(max(t, 0.0), c) + b;
        return toe * w0 + linear * (1.0 - w0);
    }
    if (x < s0)
        return linear;
    float s1 = m + a * l0;
    float cp = -a / (p - s1);
    return p - (p - s1) * exp(cp * (x - s0));
}

float3 gt3(float3 color, float peak)
{
    color = max(color, 0.0);
    return float3(gt(color.r, peak), gt(color.g, peak), gt(color.b, peak));
}

// AgX (`agx` in post.rs): Blender's inset matrix for Rec.2020 folded with BT.709 → BT.2020,
// a log encoding over [MIN_EV, MAX_EV] around mid-grey, a polynomial sigmoid (Wrensch's
// minimal AgX, Rec.2020 coefficients as in Filament), the outset matrix, a 2.2 power back
// to linear, and back to BT.709. The constants' provenance is in post.rs and ADR 0048.
static const float3x3 AGX_IN = float3x3(
    0.5448148, 0.3737865, 0.0813982,
    0.14041676, 0.7541371, 0.1054447,
    0.08881054, 0.17887152, 0.7323176);
static const float3x3 AGX_OUT = float3x3(
    1.1271006, -0.11060664, -0.016493939,
    -0.14132976, 1.1578237, -0.016493939,
    -0.14132976, -0.11060664, 1.2519364);
static const float3x3 BT709_FROM_BT2020 = float3x3(
    1.6604903, -0.5876391, -0.0728516,
    -0.12454997, 1.1328999, -0.008347964,
    -0.01815112, -0.10057872, 1.1187299);

float3 agx(float3 color)
{
    const float min_ev = -12.47393;
    const float max_ev = 4.026069;
    float3 v = mul(AGX_IN, max(color, 0.0));
    v = saturate((log2(max(v, 1e-10)) - min_ev) / (max_ev - min_ev));
    float3 x2 = v * v;
    float3 x4 = x2 * x2;
    float3 x6 = x4 * x2;
    v = -17.86 * x6 * v + 78.01 * x6 - 126.7 * x4 * v + 92.06 * x4 - 28.72 * x2 * v + 4.361 * x2
        - 0.1718 * v + 0.002857;
    v = pow(max(mul(AGX_OUT, v), 0.0), 2.2);
    // Colors AgX leaves outside BT.709 (it works in Rec.2020) are clipped to the display.
    return saturate(mul(BT709_FROM_BT2020, v));
}

// Stephen Hill's fit of ACES 1.0 RRT + sRGB ODT (`aces` in post.rs; MJP's Baking Lab, MIT;
// the transforms it fits are the Academy's, see THIRD_PARTY_NOTICES.md).
static const float3x3 ACES_IN = float3x3(
    0.59719, 0.35458, 0.04823,
    0.07600, 0.90834, 0.01566,
    0.02840, 0.13383, 0.83777);
static const float3x3 ACES_OUT = float3x3(
    1.60475, -0.53108, -0.07367,
    -0.10208, 1.10813, -0.00605,
    -0.00327, -0.07276, 1.07602);

float3 aces(float3 color)
{
    float3 v = mul(ACES_IN, max(color, 0.0));
    float3 a = v * (v + 0.0245786) - 0.000090537;
    float3 b = v * (0.983729 * v + 0.4329510) + 0.238081;
    return saturate(mul(ACES_OUT, a / b));
}

// ACEScct (Academy S-2016-001): the color grading LUT's coordinate (ADR 0094). A linear
// toe below 2^-7, then (log2(x) + 9.72) / 17.52; 0..1 covers linear 0..222.86.
float3 acescct_encode(float3 x)
{
    return select(x <= 0.0078125, 10.540238 * x + 0.07290553, (log2(max(x, 1e-10)) + 9.72) / 17.52);
}

float3 acescct_decode(float3 c)
{
    return select(c <= 0.15525114, (c - 0.07290553) / 10.540238, exp2(c * 17.52 - 9.72));
}

// ACEScct's black: the grading LUT's first texel, so that black falls on a texel and not
// between one and a clamped neighbour below it.
static const float ACESCCT_BLACK = 0.07290553;

// The grading LUT's coordinate (0..1 along each axis) of a scene-linear color, and back.
float3 lut_coordinate(float3 x)
{
    return (acescct_encode(max(x, 0.0)) - ACESCCT_BLACK) / (1.0 - ACESCCT_BLACK);
}

float3 lut_linear(float3 u)
{
    return acescct_decode(ACESCCT_BLACK + u * (1.0 - ACESCCT_BLACK));
}
