// Shared by the antialiasing filters after tone mapping (ADR 0075): `postaa.slang` (FXAA,
// the output copy, SMAA S2x's sample split) and the SMAA files (`smaa*.slang`).
//
// The tone mapper writes them a perceptual image (`tonemap.slang`'s `fragmentPerceptual`):
// sRGB-encoded values of the output over its brightest value for linear outputs, PQ as
// it is for HDR10. Edges are found and blended in that space, as FXAA and SMAA expect
// ("gamma space"); the last pass decodes into the output's encoding.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

// `Push` in postaa.rs (80 bytes).
struct Push
{
    float4 metrics;            // 1 / width, 1 / height, width, height
    float4 subsample_indices;  // SMAA's area table offsets (S2x, T2x, 4x)
    uint color;                // the perceptual image
    uint edges;                // SMAA: edges
    uint blend;                // SMAA: blending weights
    uint area;                 // SMAA: area table
    uint search;               // SMAA: search table
    uint velocity;             // SMAA T2x: motion vectors
    uint previous;             // SMAA T2x: last frame's antialiased image
    uint linear_sampler;
    uint point_sampler;
    uint decode;               // DECODE_*: how the last pass writes the output
    float decode_scale;        // the output's brightest value (linear outputs)
    uint second;               // S2x split: half the samples' difference
};

static const uint DECODE_NONE = 0; // perceptual out (an intermediate, or PQ outputs)
static const uint DECODE_SRGB = 1; // linear out: sRGB-decoded × decode_scale

[[vk::push_constant]]
ConstantBuffer<Push> push;

// One triangle over the viewport; uv from the top left.
void fullscreen(uint vertex, out float4 position, out float2 uv)
{
    uv = float2((vertex << 1) & 2, vertex & 2);
    position = float4(uv * 2.0 - 1.0, 0.0, 1.0);
}

float3 srgb_eotf(float3 p)
{
    return select(p <= 0.04045, p / 12.92, pow((p + 0.055) / 1.055, 2.4));
}

// The perceptual color `c` as the output stores it.
float4 output(float3 c)
{
    if (push.decode == DECODE_SRGB)
        c = srgb_eotf(saturate(c)) * push.decode_scale;
    return float4(c, 1.0);
}
