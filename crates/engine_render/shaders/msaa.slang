// The multisampled scene's resolve (MSAA, ADR 0075): one full-screen pass from the
// samples to one value per pixel, before the hook for scene effects and step 4.
//
// - Color: a weighted average, each sample by 1 / (1 + its luma) (Karis 2014, "High
//   Quality Temporal Supersampling"; the tonemapped resolve of "MSAA Resolve + Temporal
//   AA", Pettineo 2015): what averaging the samples after tone mapping gives, so a very
//   bright sample does not take over an edge pixel (the box filter's aliasing of HDR
//   edges). Exactly the box average where the samples are dark or equal.
// - Motion vectors: those of the nearest sample (the smallest depth), so an edge pixel
//   moves with the surface in front, as temporal antialiasing expects.
// - SMAA S2x: the plain average, and half the difference of the two samples, from which
//   each sample's image is rebuilt after the hook (`postaa.slang`'s `splitFragment`).
//
// Separate entry points per sample count (literal loop bounds): a view pays for its own.

[[vk::binding(0, 0)]] Texture2DMS<float4> g_color_ms[];
[[vk::binding(0, 0)]] Texture2DMS<float2> g_motion_ms[];
[[vk::binding(0, 0)]] Texture2DMS<float> g_depth_ms[];

// `Push` in msaa.rs (16 bytes).
struct Push
{
    uint color;
    uint motion;
    uint depth;
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
};

[shader("vertex")]
VertexOut resolveVertex(uint vertex : SV_VulkanVertexID)
{
    VertexOut o;
    float2 uv = float2((vertex << 1) & 2, vertex & 2);
    o.position = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

float luma(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

float4 resolve_color(int2 pixel, uint samples)
{
    float3 sum = 0.0;
    float weights = 0.0;
    for (uint s = 0; s < samples; s++)
    {
        float3 c = max(g_color_ms[push.color].Load(pixel, s).rgb, 0.0);
        float w = 1.0 / (1.0 + luma(c));
        sum += c * w;
        weights += w;
    }
    return float4(sum / weights, 1.0);
}

float2 nearest_motion(int2 pixel, uint samples)
{
    float nearest = 2.0;
    float2 motion = 0.0;
    for (uint s = 0; s < samples; s++)
    {
        float d = g_depth_ms[push.depth].Load(pixel, s);
        if (d < nearest)
        {
            nearest = d;
            motion = g_motion_ms[push.motion].Load(pixel, s);
        }
    }
    return motion;
}

struct WithMotion
{
    float4 color : SV_Target0;
    float2 motion : SV_Target1;
};

[shader("fragment")]
float4 resolve2(VertexOut input) : SV_Target0
{
    return resolve_color(int2(input.position.xy), 2);
}

[shader("fragment")]
float4 resolve4(VertexOut input) : SV_Target0
{
    return resolve_color(int2(input.position.xy), 4);
}

[shader("fragment")]
float4 resolve8(VertexOut input) : SV_Target0
{
    return resolve_color(int2(input.position.xy), 8);
}

[shader("fragment")]
WithMotion resolveMotion2(VertexOut input)
{
    int2 pixel = int2(input.position.xy);
    WithMotion o;
    o.color = resolve_color(pixel, 2);
    o.motion = nearest_motion(pixel, 2);
    return o;
}

[shader("fragment")]
WithMotion resolveMotion4(VertexOut input)
{
    int2 pixel = int2(input.position.xy);
    WithMotion o;
    o.color = resolve_color(pixel, 4);
    o.motion = nearest_motion(pixel, 4);
    return o;
}

[shader("fragment")]
WithMotion resolveMotion8(VertexOut input)
{
    int2 pixel = int2(input.position.xy);
    WithMotion o;
    o.color = resolve_color(pixel, 8);
    o.motion = nearest_motion(pixel, 8);
    return o;
}

// SMAA S2x: the average and half the difference of the two samples.
struct Split
{
    float4 color : SV_Target0;
    float4 difference : SV_Target1;
};

Split split(int2 pixel)
{
    float3 a = max(g_color_ms[push.color].Load(pixel, 0).rgb, 0.0);
    float3 b = max(g_color_ms[push.color].Load(pixel, 1).rgb, 0.0);
    Split o;
    o.color = float4(0.5 * (a + b), 1.0);
    o.difference = float4(0.5 * (a - b), 0.0);
    return o;
}

[shader("fragment")]
Split resolveSplit(VertexOut input)
{
    return split(int2(input.position.xy));
}

struct SplitMotion
{
    float4 color : SV_Target0;
    float4 difference : SV_Target1;
    float2 motion : SV_Target2;
};

[shader("fragment")]
SplitMotion resolveSplitMotion(VertexOut input)
{
    int2 pixel = int2(input.position.xy);
    Split s = split(pixel);
    SplitMotion o;
    o.color = s.color;
    o.difference = s.difference;
    o.motion = nearest_motion(pixel, 2);
    return o;
}
