// Temporal antialiasing and upsampling (ADR 0043): one compute pass at output resolution.
//
// Every frame the projection is jittered by a sub-pixel offset, so over a few frames the
// render-resolution samples cover the output pixels densely. For each output pixel:
//
// 1. The current frame's contribution is reconstructed from the 3 × 3 render-resolution
//    samples around it, each weighted by its (jittered) distance with a Blackman-Harris
//    approximation, exp(−2.29 d²) (Karis 2014).
// 2. Their mean and standard deviation in YCoCg bound what the pixel can be now
//    (variance clipping, Salvi 2016).
// 3. The history is fetched where the surface was last frame, following the motion of
//    the nearest surface in the neighbourhood (edges of moving objects keep their motion),
//    bilinearly by default, or with a 5-tap Catmull-Rom filter (sharper in motion;
//    Jimenez et al.) when specialized so, and clipped towards the box's center.
// 4. Current and history blend by their weights; the accumulated weight lives in history
//    alpha, capped lower when the pixel moves (resampling blurs).
//
// Colors are blended reversibly tonemapped (c / (1 + max c)) so HDR outliers do not
// flicker. No code was copied; see docs/research/temporal-aa.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint color;          // render resolution, HDR (sampled)
    uint depth;          // render resolution (sampled)
    uint velocity;       // render resolution, motion vectors (sampled)
    uint history;        // output resolution, last frame's result (sampled)
    uint output;         // output resolution (storage)
    uint linear_sampler; // clamp to edge
    uint2 input_size;
    uint2 output_size;
    float2 jitter;       // render pixels
    uint reset;          // 1: ignore the history
    uint reactive;       // render resolution: transparent coverage (sampled), or NO_REACTIVE
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

// 1: the history is fetched with the 5-tap Catmull-Rom filter; 0: one bilinear fetch
// (`HistoryFilter` in lib.rs). A specialization constant, so each pipeline only has the
// registers its filter needs.
[vk::constant_id(0)] const uint CATMULL_ROM = 0;

// Accumulated weight of a still pixel (1/16 of each new frame), and of a pixel that moves
// by 2 output pixels or more.
static const float MAX_WEIGHT = 16.0;
// No reactive mask this frame.
static const uint NO_REACTIVE = 0xFFFFFFFFu;
static const float MOVING_WEIGHT = 4.0;

float max3(float3 c)
{
    return max(c.r, max(c.g, c.b));
}

float3 tonemap(float3 c)
{
    return c / (1.0 + max3(c));
}

float3 untonemap(float3 c)
{
    return c / max(1.0 - max3(c), 1e-4);
}

float3 to_ycocg(float3 c)
{
    return float3(0.25 * c.r + 0.5 * c.g + 0.25 * c.b, 0.5 * c.r - 0.5 * c.b, -0.25 * c.r + 0.5 * c.g - 0.25 * c.b);
}

float3 from_ycocg(float3 c)
{
    return float3(c.x + c.y - c.z, c.x + c.z, c.x - c.y - c.z);
}

// History at `uv` (output texture coordinates) with a 5-tap Catmull-Rom filter: the 4 × 4
// kernel with its corners dropped, bilinear fetches combining pairs of taps. The
// accumulated weight in alpha comes from the same taps.
float4 sample_history(float2 uv)
{
    Texture2D history = g_textures[push.history];
    SamplerState s = g_samplers[push.linear_sampler];
    if (CATMULL_ROM == 0)
        return history.SampleLevel(s, uv, 0);
    float2 size = float2(push.output_size);
    float2 position = uv * size;
    float2 center = floor(position - 0.5) + 0.5;
    float2 f = position - center;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 p0 = (center - 1.0) / size;
    float2 p3 = (center + 2.0) / size;
    float2 p12 = (center + w2 / w12) / size;
    float4 sum = history.SampleLevel(s, float2(p12.x, p0.y), 0) * (w12.x * w0.y)
        + history.SampleLevel(s, float2(p0.x, p12.y), 0) * (w0.x * w12.y)
        + history.SampleLevel(s, p12, 0) * (w12.x * w12.y)
        + history.SampleLevel(s, float2(p3.x, p12.y), 0) * (w3.x * w12.y)
        + history.SampleLevel(s, float2(p12.x, p3.y), 0) * (w12.x * w3.y);
    float total = w12.x * w0.y + w0.x * w12.y + w12.x * w12.y + w3.x * w12.y + w12.x * w3.y;
    // Catmull-Rom overshoots; negative colors would survive clipping in YCoCg, and the
    // weight must stay within what was stored.
    return clamp(sum / total, 0.0, float4(65504.0, 65504.0, 65504.0, MAX_WEIGHT));
}

[shader("compute")]
[numthreads(8, 8, 1)]
void resolve(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.output_size))
        return;
    float2 output_size = float2(push.output_size);
    float2 input_size = float2(push.input_size);
    float2 uv = (float2(id.xy) + 0.5) / output_size;
    // The output pixel's center in render pixels, and the render pixel whose (jittered)
    // sample is nearest to it.
    float2 position = uv * input_size;
    int2 nearest = int2(floor(position + push.jitter));
    int2 last = int2(push.input_size) - 1;

    Texture2D color = g_textures[push.color];
    Texture2D depth = g_textures[push.depth];
    float3 sum = 0.0;
    float weights = 0.0;
    float best_weight = 0.0;
    float3 m1 = 0.0;
    float3 m2 = 0.0;
    float closest = 2.0;
    int2 closest_pixel = clamp(nearest, int2(0, 0), last);
    [unroll]
    for (int y = -1; y <= 1; y++)
    {
        [unroll]
        for (int x = -1; x <= 1; x++)
        {
            int2 pixel = clamp(nearest + int2(x, y), int2(0, 0), last);
            float3 c = tonemap(max(color.Load(int3(pixel, 0)).rgb, 0.0));
            // Where this pixel's sample lies, jitter removed, relative to the output
            // pixel's center, in output pixels: when upsampling, a sample contributes
            // to the output pixels it falls on, not to its render pixel's area.
            float2 d = (float2(pixel) + 0.5 - push.jitter - position) * (output_size / input_size);
            float w = exp(-2.29 * dot(d, d));
            sum += c * w;
            weights += w;
            best_weight = max(best_weight, w);
            float3 ycocg = to_ycocg(c);
            m1 += ycocg;
            m2 += ycocg * ycocg;
            float z = depth.Load(int3(pixel, 0)).r;
            if (z < closest)
            {
                closest = z;
                closest_pixel = pixel;
            }
        }
    }
    float3 current = sum / max(weights, 1e-6);
    float3 mean = m1 / 9.0;
    float3 deviation = sqrt(max(m2 / 9.0 - mean * mean, 0.0));

    float2 motion = g_textures[push.velocity].Load(int3(closest_pixel, 0)).xy;
    float2 history_uv = uv - motion;
    bool valid = push.reset == 0 && all(history_uv >= 0.0) && all(history_uv <= 1.0);
    float4 previous = valid ? sample_history(history_uv) : float4(0.0, 0.0, 0.0, 0.0);
    float history_weight = previous.a;
    float3 blended = current;
    float new_weight = best_weight;
    if (history_weight > 0.0)
    {
        // Clip the history towards the center of the neighbourhood's box.
        float3 history = to_ycocg(tonemap(previous.rgb));
        float3 offset = history - mean;
        float3 units = abs(offset / max(deviation, 1e-5));
        float largest = max3(units);
        if (largest > 1.0)
            history = mean + offset / largest;
        history = from_ycocg(history);
        float moved = length(motion * output_size);
        float cap = lerp(MAX_WEIGHT, MOVING_WEIGHT, saturate(moved * 0.5));
        // Transparent surfaces do not move with the motion vectors (those are the opaque
        // surface's behind them): where they cover the pixel, the history counts at most
        // as much as one new frame (ADR 0054).
        if (push.reactive != NO_REACTIVE)
        {
            float coverage = g_textures[push.reactive].Load(int3(closest_pixel, 0)).r;
            cap = lerp(cap, 1.0, coverage);
        }
        history_weight = min(history_weight, cap);
        blended = (history * history_weight + current * best_weight) / (history_weight + best_weight);
        new_weight = min(history_weight + best_weight, cap);
    }
    g_images[push.output][id.xy] = float4(untonemap(saturate(blended)), new_weight);
}
