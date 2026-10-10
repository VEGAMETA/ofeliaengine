// Tonemapping: HDR scene radiance → display. A fullscreen triangle (over the viewport the
// pass sets, so it can fill a sub-rectangle when compositing) samples the source
// through the bindless arrays and applies exposure and a tone curve (ADR 0048):
//
// - by default Uchimura's (Gran Turismo Sport, CEDEC 2017, from its published formula),
//   per channel: toe, linear mid-tones, an exponential shoulder towards the peak;
// - or the Khronos PBR Neutral tone mapper (implemented from its published description:
//   linear below the knee, a smooth highlight roll-off, desaturation of very bright
//   colors).
//
// HDR output (ADR 0038): values are in units of reference white (SDR white), and the
// tone mapper rolls highlights off towards `peak`, the display's peak luminance over
// reference white, instead of towards 1. The same pass encodes HDR10 (BT.2020 primaries,
// PQ) for swapchains that need it.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const uint FLAG_PASS_THROUGH = 1; // exposure only, no tone mapping (LDR sources)
static const uint FLAG_ENCODE_PQ = 2;    // HDR10 output: BT.2020 primaries, PQ encoding
static const uint FLAG_BLOOM = 4;        // mix in `bloom` (postfx.slang) by its intensity
static const uint FLAG_AUTO_EXPOSURE = 8; // multiply by the measured exposure
static const uint FLAG_PBR_NEUTRAL = 16; // PBR Neutral instead of Uchimura's curve
static const uint FLAG_AGX = 32;         // AgX instead of Uchimura's curve
static const uint FLAG_ACES = 64;        // Hill's ACES fit instead of Uchimura's curve

struct Push
{
    uint source;    // bindless index of the source texture
    uint sampler;   // bindless sampler index
    float exposure; // linear multiplier before tone mapping
    uint flags;
    float peak;     // brightest tone-mapped value, in units of reference white (1: SDR)
    float white;    // output value of reference white: linear, or nits / 10000 for PQ
    uint bloom;     // bindless index of the bloom (half size), with FLAG_BLOOM
    float bloom_intensity; // how much of the image the bloom replaces
    float4* auto_exposure; // x: the measured exposure, with FLAG_AUTO_EXPOSURE
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

[shader("vertex")]
VertexOut vertexMain(uint vertex_id : SV_VertexID)
{
    // One triangle covering the screen: (0,0), (2,0), (0,2) in UV.
    float2 uv = float2((vertex_id << 1) & 2, vertex_id & 2);
    VertexOut o;
    o.position = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

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

// BT.709 → BT.2020 primaries, both linear (ITU-R BT.2087).
static const float3x3 BT2020_FROM_BT709 = float3x3(
    0.6274040, 0.3292820, 0.0433136,
    0.0690970, 0.9195400, 0.0113612,
    0.0163916, 0.0880132, 0.8955950);

// SMPTE ST 2084 (PQ) inverse EOTF: luminance / 10000 nits → signal.
float3 pq_encode(float3 y)
{
    const float m1 = 2610.0 / 16384.0;
    const float m2 = 2523.0 / 4096.0 * 128.0;
    const float c1 = 3424.0 / 4096.0;
    const float c2 = 2413.0 / 4096.0 * 32.0;
    const float c3 = 2392.0 / 4096.0 * 32.0;
    float3 p = pow(saturate(y), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    SamplerState s = g_samplers[push.sampler];
    float3 color = g_textures[NonUniformResourceIndex(push.source)].SampleLevel(s, input.uv, 0).rgb;
    // Bloom replaces its share of the image: energy is kept.
    if ((push.flags & FLAG_BLOOM) != 0)
    {
        float3 bloom = g_textures[NonUniformResourceIndex(push.bloom)].SampleLevel(s, input.uv, 0).rgb;
        color = lerp(color, bloom, push.bloom_intensity);
    }
    float exposure = push.exposure;
    if ((push.flags & FLAG_AUTO_EXPOSURE) != 0)
        exposure *= loadAligned<16>(push.auto_exposure).x;
    color *= exposure;
    if ((push.flags & FLAG_PASS_THROUGH) == 0)
    {
        if ((push.flags & FLAG_PBR_NEUTRAL) != 0)
            color = pbr_neutral(color, push.peak);
        else if ((push.flags & FLAG_AGX) != 0)
            color = agx(color);
        else if ((push.flags & FLAG_ACES) != 0)
            color = aces(color);
        else
            color = gt3(color, push.peak);
    }
    color *= push.white;
    if ((push.flags & FLAG_ENCODE_PQ) != 0)
        color = pq_encode(mul(BT2020_FROM_BT709, color));
    return float4(color, 1.0);
}

// `fragmentMain`'s color, for `fragmentPerceptual` (`fragmentMain` keeps its own copy:
// its compiled code stays as it was).
float3 tonemapped(VertexOut input)
{
    SamplerState s = g_samplers[push.sampler];
    float3 color = g_textures[NonUniformResourceIndex(push.source)].SampleLevel(s, input.uv, 0).rgb;
    // Bloom replaces its share of the image: energy is kept.
    if ((push.flags & FLAG_BLOOM) != 0)
    {
        float3 bloom = g_textures[NonUniformResourceIndex(push.bloom)].SampleLevel(s, input.uv, 0).rgb;
        color = lerp(color, bloom, push.bloom_intensity);
    }
    float exposure = push.exposure;
    if ((push.flags & FLAG_AUTO_EXPOSURE) != 0)
        exposure *= loadAligned<16>(push.auto_exposure).x;
    color *= exposure;
    if ((push.flags & FLAG_PASS_THROUGH) == 0)
    {
        if ((push.flags & FLAG_PBR_NEUTRAL) != 0)
            color = pbr_neutral(color, push.peak);
        else if ((push.flags & FLAG_AGX) != 0)
            color = agx(color);
        else if ((push.flags & FLAG_ACES) != 0)
            color = aces(color);
        else
            color = gt3(color, push.peak);
    }
    color *= push.white;
    if ((push.flags & FLAG_ENCODE_PQ) != 0)
        color = pq_encode(mul(BT2020_FROM_BT709, color));
    return color;
}

// sRGB's encoding (IEC 61966-2-1).
float3 srgb_oetf(float3 c)
{
    return select(c <= 0.0031308, c * 12.92, 1.055 * pow(c, 1.0 / 2.4) - 0.055);
}

// For the antialiasing filters after tone mapping (ADR 0075, `postaa_common.hlsli`): a
// perceptual image. PQ outputs are perceptual already; linear ones are sRGB-encoded over
// their brightest value (`peak` × `white`).
[shader("fragment")]
float4 fragmentPerceptual(VertexOut input) : SV_Target
{
    float3 color = tonemapped(input);
    if ((push.flags & FLAG_ENCODE_PQ) == 0)
        color = srgb_oetf(saturate(color / (push.peak * push.white)));
    return float4(color, 1.0);
}

// Resampling a smaller source to the output size (ADR 0075's addendum): Catmull-Rom and
// Lanczos-3 entry points, so the tone mapper above keeps its code. Each texel of the
// kernel is read on its own (`Load`, clamped to the image's edge) and weighted; no
// bilinear fetches are combined (ATI's US 8,675,007 B2 claims that,
// docs/research/antialiasing.md). The result is limited to the range of the four texels
// around the sample (the anti-ringing clamp): a negative lobe's undershoot grows with the
// contrast, and before tone mapping a highlight of 1000 next to 1 would ring far below 0.

// `tonemapped`'s steps after the source is read: bloom, exposure, the curve, the output's
// encoding.
float3 tonemap_color(float3 color, float2 uv)
{
    SamplerState s = g_samplers[push.sampler];
    // Bloom replaces its share of the image: energy is kept.
    if ((push.flags & FLAG_BLOOM) != 0)
    {
        float3 bloom = g_textures[NonUniformResourceIndex(push.bloom)].SampleLevel(s, uv, 0).rgb;
        color = lerp(color, bloom, push.bloom_intensity);
    }
    float exposure = push.exposure;
    if ((push.flags & FLAG_AUTO_EXPOSURE) != 0)
        exposure *= loadAligned<16>(push.auto_exposure).x;
    color *= exposure;
    if ((push.flags & FLAG_PASS_THROUGH) == 0)
    {
        if ((push.flags & FLAG_PBR_NEUTRAL) != 0)
            color = pbr_neutral(color, push.peak);
        else if ((push.flags & FLAG_AGX) != 0)
            color = agx(color);
        else if ((push.flags & FLAG_ACES) != 0)
            color = aces(color);
        else
            color = gt3(color, push.peak);
    }
    color *= push.white;
    if ((push.flags & FLAG_ENCODE_PQ) != 0)
        color = pq_encode(mul(BT2020_FROM_BT709, color));
    return color;
}

// `fragmentPerceptual`'s encoding of a tone-mapped color.
float3 perceptual(float3 color)
{
    if ((push.flags & FLAG_ENCODE_PQ) == 0)
        color = srgb_oetf(saturate(color / (push.peak * push.white)));
    return color;
}

// The source's texel at `p`, clamped to its edge.
float3 texel(Texture2D source, int2 p, int2 last)
{
    return source.Load(int3(clamp(p, int2(0, 0), last), 0)).rgb;
}

// Catmull-Rom's weights (Keys's cubic convolution with a = -0.5) of the four texels around
// a sample at fraction `f` past the second one. They sum to 1.
float4 catmull_rom_weights(float f)
{
    float f2 = f * f;
    float f3 = f2 * f;
    return float4(
        -0.5 * f + f2 - 0.5 * f3,
        1.0 - 2.5 * f2 + 1.5 * f3,
        0.5 * f + 2.0 * f2 - 1.5 * f3,
        -0.5 * f2 + 0.5 * f3);
}

// The source at `uv` through Catmull-Rom: 4 × 4 texels, clamped to the middle 2 × 2.
float3 catmull_rom(float2 uv)
{
    Texture2D source = g_textures[NonUniformResourceIndex(push.source)];
    uint width, height;
    source.GetDimensions(width, height);
    float2 position = uv * float2(width, height) - 0.5;
    float2 base = floor(position);
    float2 f = position - base;
    float4 wx = catmull_rom_weights(f.x);
    float4 wy = catmull_rom_weights(f.y);
    int2 origin = int2(base) - 1;
    int2 last = int2(width, height) - 1;
    float3 sum = 0.0;
    float3 lo = 65504.0;
    float3 hi = -65504.0;
    [unroll]
    for (int y = 0; y < 4; y++)
    {
        float3 row = 0.0;
        [unroll]
        for (int x = 0; x < 4; x++)
        {
            float3 c = texel(source, origin + int2(x, y), last);
            row += c * wx[x];
            if ((x == 1 || x == 2) && (y == 1 || y == 2))
            {
                lo = min(lo, c);
                hi = max(hi, c);
            }
        }
        sum += row * wy[y];
    }
    return clamp(sum, lo, hi);
}

// Lanczos-3's weights, sinc(d) sinc(d / 3), of the six texels around a sample at fraction
// `f` past the third one, normalized (between texels they do not sum to 1).
void lanczos3_weights(float f, out float w[6])
{
    const float pi = 3.14159265;
    float total = 0.0;
    [unroll]
    for (int i = 0; i < 6; i++)
    {
        // The texel's distance from the sample, kept off 0 (where the kernel is 1).
        float x = pi * max(abs(f - float(i - 2)), 1e-5);
        w[i] = 3.0 * sin(x) * sin(x / 3.0) / (x * x);
        total += w[i];
    }
    [unroll]
    for (int i = 0; i < 6; i++)
        w[i] /= total;
}

// Lanczos-3 is separable and runs in two passes (6 + 6 reads for each output pixel instead
// of 36): `lanczos3Across` resamples the source's rows to the output's width, into an
// image of the output's width and the source's height; the tone mapper's
// `fragmentLanczos3` resamples its columns to the output's height. Each pass clamps to the
// two texels nearest its sample, so the result stays within the four nearest texels'
// range.
[shader("fragment")]
float4 lanczos3Across(VertexOut input) : SV_Target
{
    Texture2D source = g_textures[NonUniformResourceIndex(push.source)];
    uint width, height;
    source.GetDimensions(width, height);
    float position = input.uv.x * float(width) - 0.5;
    float base = floor(position);
    float w[6];
    lanczos3_weights(position - base, w);
    int row = min(int(input.uv.y * float(height)), int(height) - 1);
    int origin = int(base) - 2;
    int last = int(width) - 1;
    float3 sum = 0.0;
    float3 lo = 65504.0;
    float3 hi = -65504.0;
    [unroll]
    for (int i = 0; i < 6; i++)
    {
        float3 c = source.Load(int3(clamp(origin + i, 0, last), row, 0)).rgb;
        sum += c * w[i];
        if (i == 2 || i == 3)
        {
            lo = min(lo, c);
            hi = max(hi, c);
        }
    }
    return float4(clamp(sum, lo, hi), 1.0);
}

// The source (`lanczos3Across`'s image, the output's width) at `uv` through Lanczos-3 down
// its columns.
float3 lanczos3_down(float2 uv)
{
    Texture2D source = g_textures[NonUniformResourceIndex(push.source)];
    uint width, height;
    source.GetDimensions(width, height);
    int column = min(int(uv.x * float(width)), int(width) - 1);
    float position = uv.y * float(height) - 0.5;
    float base = floor(position);
    float w[6];
    lanczos3_weights(position - base, w);
    int origin = int(base) - 2;
    int last = int(height) - 1;
    float3 sum = 0.0;
    float3 lo = 65504.0;
    float3 hi = -65504.0;
    [unroll]
    for (int i = 0; i < 6; i++)
    {
        float3 c = source.Load(int3(column, clamp(origin + i, 0, last), 0)).rgb;
        sum += c * w[i];
        if (i == 2 || i == 3)
        {
            lo = min(lo, c);
            hi = max(hi, c);
        }
    }
    return clamp(sum, lo, hi);
}

[shader("fragment")]
float4 fragmentCatmullRom(VertexOut input) : SV_Target
{
    return float4(tonemap_color(catmull_rom(input.uv), input.uv), 1.0);
}

[shader("fragment")]
float4 fragmentLanczos3(VertexOut input) : SV_Target
{
    return float4(tonemap_color(lanczos3_down(input.uv), input.uv), 1.0);
}

[shader("fragment")]
float4 fragmentPerceptualCatmullRom(VertexOut input) : SV_Target
{
    return float4(perceptual(tonemap_color(catmull_rom(input.uv), input.uv)), 1.0);
}

[shader("fragment")]
float4 fragmentPerceptualLanczos3(VertexOut input) : SV_Target
{
    return float4(perceptual(tonemap_color(lanczos3_down(input.uv), input.uv)), 1.0);
}
