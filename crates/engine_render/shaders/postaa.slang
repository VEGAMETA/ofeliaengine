// Antialiasing filters after tone mapping (ADR 0075), on the tone mapper's perceptual
// image (`postaa_common.hlsli`):
//
// - `fxaaFragment`: FXAA, the algorithm of Lottes's FXAA 3.11 "quality" path (NVIDIA
//   2011), written here from its description (no code carried over): luma contrast
//   against the 3 × 3 neighbourhood, the edge's direction from second differences, a
//   search along the edge in both directions until the luma leaves the edge's average,
//   a blend across the edge by the pixel's position on the span, and subpixel smoothing
//   of single-pixel features.
// - `copyFragment`: the image into the output, decoded (the last step when the filter
//   wrote an intermediate: SMAA T2x's history).
// - `splitFragment`: SMAA S2x: the scene's resolved color plus and minus half the
//   samples' difference (both samples, each with what the hook added to the resolved
//   color), in two targets the tone mapper then maps one by one.

#include "postaa_common.hlsli"

struct VertexOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

[shader("vertex")]
VertexOut postVertex(uint vertex : SV_VulkanVertexID)
{
    VertexOut o;
    fullscreen(vertex, o.position, o.uv);
    return o;
}

float luma(float3 c)
{
    return dot(c, float3(0.299, 0.587, 0.114));
}

float4 fetch(float2 uv)
{
    return g_textures[push.color].SampleLevel(g_samplers[push.linear_sampler], uv, 0);
}

float luma_at(float2 uv)
{
    return luma(fetch(uv).rgb);
}

float luma_offset(int2 pixel, int2 offset)
{
    int2 p = clamp(pixel + offset, int2(0, 0), int2(push.metrics.zw) - 1);
    return luma(g_textures[push.color].Load(int3(p, 0)).rgb);
}

// FXAA 3.11's defaults for its quality path.
static const float EDGE_THRESHOLD = 0.166;
static const float EDGE_THRESHOLD_MIN = 0.0833;
static const float SUBPIXEL = 0.75;
// Search steps along the edge, in pixels (the quality presets' shape).
static const uint STEPS = 10;
static const float STEP[STEPS] = { 1.0, 1.0, 1.0, 1.0, 1.5, 2.0, 2.0, 2.0, 4.0, 8.0 };

[shader("fragment")]
float4 fxaaFragment(VertexOut input) : SV_Target0
{
    // The source's pixel (the pass may draw into a rectangle of a larger output).
    float2 rcp_size = push.metrics.xy;
    int2 pixel = int2(input.uv * push.metrics.zw);
    float2 uv = (float2(pixel) + 0.5) * rcp_size;
    float4 center = g_textures[push.color].Load(int3(pixel, 0));
    float m = luma(center.rgb);
    float n = luma_offset(pixel, int2(0, -1));
    float s = luma_offset(pixel, int2(0, 1));
    float e = luma_offset(pixel, int2(1, 0));
    float w = luma_offset(pixel, int2(-1, 0));
    float highest = max(max(max(n, s), max(e, w)), m);
    float lowest = min(min(min(n, s), min(e, w)), m);
    float range = highest - lowest;
    // Too little contrast: no edge.
    if (range < max(EDGE_THRESHOLD_MIN, highest * EDGE_THRESHOLD))
        return output(center.rgb);

    float nw = luma_offset(pixel, int2(-1, -1));
    float ne = luma_offset(pixel, int2(1, -1));
    float sw = luma_offset(pixel, int2(-1, 1));
    float se = luma_offset(pixel, int2(1, 1));

    // Subpixel aliasing: how much the pixel differs from its neighbourhood's average.
    float average = (2.0 * (n + s + e + w) + (nw + ne + sw + se)) / 12.0;
    float subpixel = saturate(abs(average - m) / range);
    subpixel = smoothstep(0.0, 1.0, subpixel);
    subpixel = subpixel * subpixel * SUBPIXEL;

    // A horizontal edge changes along y (second differences across rows are larger).
    float horizontal = abs(nw + sw - 2.0 * w) + 2.0 * abs(n + s - 2.0 * m) + abs(ne + se - 2.0 * e);
    float vertical = abs(nw + ne - 2.0 * n) + 2.0 * abs(w + e - 2.0 * m) + abs(sw + se - 2.0 * s);
    bool is_horizontal = horizontal >= vertical;

    // The side of the edge: the neighbour across it with the larger gradient.
    float luma_neg = is_horizontal ? n : w;
    float luma_pos = is_horizontal ? s : e;
    float gradient_neg = luma_neg - m;
    float gradient_pos = luma_pos - m;
    bool negative = abs(gradient_neg) >= abs(gradient_pos);
    float gradient = max(abs(gradient_neg), abs(gradient_pos));
    float step_across = is_horizontal ? rcp_size.y : rcp_size.x;
    float edge_luma;
    if (negative)
    {
        step_across = -step_across;
        edge_luma = 0.5 * (luma_neg + m);
    }
    else
    {
        edge_luma = 0.5 * (luma_pos + m);
    }

    // Walk along the edge, half a pixel across, in both directions.
    float2 on_edge = uv;
    if (is_horizontal)
        on_edge.y += 0.5 * step_across;
    else
        on_edge.x += 0.5 * step_across;
    float2 along = is_horizontal ? float2(rcp_size.x, 0.0) : float2(0.0, rcp_size.y);
    float2 p_neg = on_edge - along * STEP[0];
    float2 p_pos = on_edge + along * STEP[0];
    float end_neg = luma_at(p_neg) - edge_luma;
    float end_pos = luma_at(p_pos) - edge_luma;
    float threshold = 0.25 * gradient;
    bool done_neg = abs(end_neg) >= threshold;
    bool done_pos = abs(end_pos) >= threshold;
    [loop]
    for (uint k = 1; k < STEPS && !(done_neg && done_pos); k++)
    {
        if (!done_neg)
        {
            p_neg -= along * STEP[k];
            end_neg = luma_at(p_neg) - edge_luma;
            done_neg = abs(end_neg) >= threshold;
        }
        if (!done_pos)
        {
            p_pos += along * STEP[k];
            end_pos = luma_at(p_pos) - edge_luma;
            done_pos = abs(end_pos) >= threshold;
        }
    }

    // How far the pixel is from each end; blend from the nearer end, if the edge's luma
    // there moves the way the pixel's does (else the pixel is on the other side).
    float dist_neg = is_horizontal ? uv.x - p_neg.x : uv.y - p_neg.y;
    float dist_pos = is_horizontal ? p_pos.x - uv.x : p_pos.y - uv.y;
    bool nearer_neg = dist_neg < dist_pos;
    float nearest = min(dist_neg, dist_pos);
    float span = dist_neg + dist_pos;
    bool center_below = m - edge_luma < 0.0;
    bool good = ((nearer_neg ? end_neg : end_pos) < 0.0) != center_below;
    float offset = good ? 0.5 - nearest / span : 0.0;
    offset = max(offset, subpixel);

    float2 final_uv = uv;
    if (is_horizontal)
        final_uv.y += offset * step_across;
    else
        final_uv.x += offset * step_across;
    return output(fetch(final_uv).rgb);
}

[shader("fragment")]
float4 copyFragment(VertexOut input) : SV_Target0
{
    return output(g_textures[push.color].Load(int3(int2(input.uv * push.metrics.zw), 0)).rgb);
}

// SMAA S2x: the two samples' images from the resolved color (with the hook's additions)
// and half the samples' difference (written by the MSAA resolve).
struct Split
{
    float4 first : SV_Target0;
    float4 second : SV_Target1;
};

[shader("fragment")]
Split splitFragment(VertexOut input)
{
    int3 pixel = int3(int2(input.uv * push.metrics.zw), 0);
    float4 c = g_textures[push.color].Load(pixel);
    float4 d = g_textures[push.second].Load(pixel);
    Split o;
    o.first = float4(max(c.rgb + d.rgb, 0.0), 1.0);
    o.second = float4(max(c.rgb - d.rgb, 0.0), 1.0);
    return o;
}
