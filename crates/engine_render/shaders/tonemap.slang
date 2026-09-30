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
        exposure *= push.auto_exposure[0].x;
    color *= exposure;
    if ((push.flags & FLAG_PASS_THROUGH) == 0)
        color = (push.flags & FLAG_PBR_NEUTRAL) != 0 ? pbr_neutral(color, push.peak) : gt3(color, push.peak);
    color *= push.white;
    if ((push.flags & FLAG_ENCODE_PQ) != 0)
        color = pq_encode(mul(BT2020_FROM_BT709, color));
    return float4(color, 1.0);
}
