// Tonemapping: HDR scene radiance → display. A fullscreen triangle samples the HDR target
// through the bindless arrays and applies exposure and the Khronos PBR Neutral tone
// mapper (implemented from its published description: linear below the knee, a smooth
// highlight roll-off, desaturation of very bright colors).

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint source;    // bindless index of the HDR texture
    uint sampler;   // bindless sampler index
    float exposure; // linear multiplier
    uint _pad;
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

float3 pbr_neutral(float3 color)
{
    const float start_compression = 0.8 - 0.04;
    const float desaturation = 0.15;
    float x = min(color.r, min(color.g, color.b));
    float offset = x < 0.08 ? x - 6.25 * x * x : 0.04;
    color -= offset;
    float peak = max(color.r, max(color.g, color.b));
    if (peak < start_compression)
        return color;
    float d = 1.0 - start_compression;
    float new_peak = 1.0 - d * d / (peak + d - start_compression);
    color *= new_peak / peak;
    float g = 1.0 - 1.0 / (desaturation * (peak - new_peak) + 1.0);
    return lerp(color, float3(new_peak, new_peak, new_peak), g);
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    float3 hdr = g_textures[NonUniformResourceIndex(push.source)]
        .SampleLevel(g_samplers[push.sampler], input.uv, 0).rgb;
    return float4(pbr_neutral(hdr * push.exposure), 1.0);
}
