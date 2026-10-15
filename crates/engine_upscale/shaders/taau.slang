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
//    bilinearly (`resolve`), or with Catmull-Rom over the 4 × 4 texels around it, corners
//    left out (`resolveCatmullRom`: sharper in motion), and clipped towards the box's
//    center.
// 4. Current and history blend by their weights; the accumulated weight lives in history
//    alpha, capped lower when the pixel moves (resampling blurs).
//
// Colors are blended reversibly tonemapped (c / (1 + max c)) so HDR outliers do not
// flicker. No code was copied; see docs/research/temporal-aa.md.
//
// Where the render size is at most the output's (every quality preset), each group of
// 8 × 8 output pixels first tone maps the render pixels its taps read, once, into shared
// memory (`resolveShared`): the arithmetic done for every tap was most of the resolve's
// cost on the Radeon iGPU (ADR 0043's addendum of 2026-10-15).

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

// Catmull-Rom's weights (Keys's cubic convolution, a = -1/2) of the four texels around a
// sample at fraction `f` past the second one.
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

// History at `uv` (output texture coordinates): one bilinear fetch, or Catmull-Rom over
// the 4 × 4 texels around it with the corners left out (their weights are the smallest,
// as in Jimenez et al.'s 5-tap filter), each texel read on its own: merging pairs of them
// into bilinear fetches is what ATI's US 8,675,007 B2 claims
// (docs/research/antialiasing.md). The accumulated weight in alpha comes from the same
// taps. `catmull_rom` is a literal: each entry point has only its own filter's code.
float4 sample_history(float2 uv, bool catmull_rom)
{
    Texture2D history = g_textures[push.history];
    if (!catmull_rom)
        return history.SampleLevel(g_samplers[push.linear_sampler], uv, 0);
    float2 position = uv * float2(push.output_size) - 0.5;
    float2 base = floor(position);
    float2 f = position - base;
    float4 wx = catmull_rom_weights(f.x);
    float4 wy = catmull_rom_weights(f.y);
    int2 origin = int2(base) - 1;
    int2 last = int2(push.output_size) - 1;
    float4 sum = 0.0;
    float total = 0.0;
    [unroll]
    for (int y = 0; y < 4; y++)
    {
        [unroll]
        for (int x = 0; x < 4; x++)
        {
            if ((x == 0 || x == 3) && (y == 0 || y == 3))
                continue;
            float w = wx[x] * wy[y];
            sum += history.Load(int3(clamp(origin + int2(x, y), int2(0, 0), last), 0)) * w;
            total += w;
        }
    }
    // Catmull-Rom overshoots; negative colors would survive clipping in YCoCg, and the
    // weight must stay within what was stored.
    return clamp(sum / total, 0.0, float4(65504.0, 65504.0, 65504.0, MAX_WEIGHT));
}

// The resolve of one output pixel, the history fetched by `sample_history`.
void resolve_pixel(uint3 id, bool catmull_rom)
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
    float4 previous = valid ? sample_history(history_uv, catmull_rom) : float4(0.0, 0.0, 0.0, 0.0);
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

// The bilinear history (`HistoryFilter::Bilinear`, the default), for a render size larger
// than the output (the shared variants below cover the rest).
[shader("compute")]
[numthreads(8, 8, 1)]
void resolve(uint3 id : SV_DispatchThreadID)
{
    resolve_pixel(id, false);
}

// The Catmull-Rom history (`HistoryFilter::CatmullRom`), for a render size larger than the
// output.
[shader("compute")]
[numthreads(8, 8, 1)]
void resolveCatmullRom(uint3 id : SV_DispatchThreadID)
{
    resolve_pixel(id, true);
}

// A group's output pixels, and the render pixels around them when the render size is at
// most the output's: the nearest ones of its first and last pixels differ by at most
// TILE − 1 per axis (TILE with rounding), and each pixel reads one more on either side.
// 8 × 8 measured best in Sponza's frames on both GPUs; 16 × 16 read less of each
// footprint twice but cost the Radeon 8–11 % at a native 1080p.
static const uint TILE_X = 8;
static const uint TILE_Y = 8;
static const uint SPAN_X = TILE_X + 3;
static const uint SPAN_Y = TILE_Y + 3;
// Each render pixel of the group's footprint, tone mapped and in YCoCg, with its depth:
// computed once for the group instead of once per tap (nine taps, each render pixel seen by
// up to nine output pixels: on the Radeon iGPU the resolve's arithmetic was its cost).
groupshared float4 g_footprint[SPAN_X * SPAN_Y];

// The render pixel whose (jittered) sample is nearest to output pixel `id`'s center.
int2 nearest_render_pixel(uint2 id)
{
    float2 uv = (float2(id) + 0.5) / float2(push.output_size);
    return int2(floor(uv * float2(push.input_size) + push.jitter));
}

// `resolve_pixel` with the group's footprint in shared memory, for a render size at most
// the output's. The taps' weights are separable (exp(a) exp(b)), from the samples' positions
// unclamped: at the image's edge the edge pixel stands in for the one beyond it, weighted
// where that one would be.
void resolve_pixel_shared(uint2 group, uint local, bool catmull_rom)
{
    uint2 first = group * uint2(TILE_X, TILE_Y);
    int2 origin = nearest_render_pixel(first) - 1;
    int2 last = int2(push.input_size) - 1;
    Texture2D color = g_textures[push.color];
    Texture2D depth = g_textures[push.depth];
    for (uint i = local; i < SPAN_X * SPAN_Y; i += TILE_X * TILE_Y)
    {
        int2 pixel = clamp(origin + int2(i % SPAN_X, i / SPAN_X), int2(0, 0), last);
        float3 c = tonemap(max(color.Load(int3(pixel, 0)).rgb, 0.0));
        g_footprint[i] = float4(to_ycocg(c), depth.Load(int3(pixel, 0)).r);
    }
    GroupMemoryBarrierWithGroupSync();
    uint2 id = first + uint2(local % TILE_X, local / TILE_X);
    if (any(id >= push.output_size))
        return;
    float2 output_size = float2(push.output_size);
    float2 input_size = float2(push.input_size);
    float2 uv = (float2(id) + 0.5) / output_size;
    float2 position = uv * input_size;
    int2 nearest = nearest_render_pixel(id);
    int2 at = clamp(nearest - origin, int2(1, 1), int2(SPAN_X - 2, SPAN_Y - 2));
    // Where the nearest pixel's sample lies, jitter removed, relative to the output pixel's
    // center, in output pixels (as `resolve_pixel`); its neighbours lie a render pixel on.
    float2 step = output_size / input_size;
    float2 d = (float2(nearest) + 0.5 - push.jitter - position) * step;
    float3 wx = float3(exp(-2.29 * (d.x - step.x) * (d.x - step.x)), exp(-2.29 * d.x * d.x), exp(-2.29 * (d.x + step.x) * (d.x + step.x)));
    float3 wy = float3(exp(-2.29 * (d.y - step.y) * (d.y - step.y)), exp(-2.29 * d.y * d.y), exp(-2.29 * (d.y + step.y) * (d.y + step.y)));
    float3 sum = 0.0;
    float weights = 0.0;
    float best_weight = 0.0;
    float3 m1 = 0.0;
    float3 m2 = 0.0;
    float closest = 2.0;
    int2 closest_offset = int2(0, 0);
    [unroll]
    for (int y = -1; y <= 1; y++)
    {
        [unroll]
        for (int x = -1; x <= 1; x++)
        {
            float4 tap = g_footprint[(at.y + y) * SPAN_X + at.x + x];
            float w = wx[x + 1] * wy[y + 1];
            sum += tap.xyz * w;
            weights += w;
            best_weight = max(best_weight, w);
            m1 += tap.xyz;
            m2 += tap.xyz * tap.xyz;
            if (tap.w < closest)
            {
                closest = tap.w;
                closest_offset = int2(x, y);
            }
        }
    }
    float3 current = from_ycocg(sum / max(weights, 1e-6));
    float3 mean = m1 / 9.0;
    float3 deviation = sqrt(max(m2 / 9.0 - mean * mean, 0.0));
    int2 closest_pixel = clamp(nearest + closest_offset, int2(0, 0), last);

    float2 motion = g_textures[push.velocity].Load(int3(closest_pixel, 0)).xy;
    float2 history_uv = uv - motion;
    bool valid = push.reset == 0 && all(history_uv >= 0.0) && all(history_uv <= 1.0);
    float4 previous = valid ? sample_history(history_uv, catmull_rom) : float4(0.0, 0.0, 0.0, 0.0);
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
        // Transparent surfaces do not move with the motion vectors (ADR 0054).
        if (push.reactive != NO_REACTIVE)
        {
            float coverage = g_textures[push.reactive].Load(int3(closest_pixel, 0)).r;
            cap = lerp(cap, 1.0, coverage);
        }
        history_weight = min(history_weight, cap);
        blended = (history * history_weight + current * best_weight) / (history_weight + best_weight);
        new_weight = min(history_weight + best_weight, cap);
    }
    g_images[push.output][id] = float4(untonemap(saturate(blended)), new_weight);
}

// The bilinear history with the group's footprint shared (render size at most the output's).
[shader("compute")]
[numthreads(TILE_X, TILE_Y, 1)]
void resolveShared(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    resolve_pixel_shared(group.xy, local, false);
}

// The Catmull-Rom history with the group's footprint shared.
[shader("compute")]
[numthreads(TILE_X, TILE_Y, 1)]
void resolveSharedCatmullRom(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    resolve_pixel_shared(group.xy, local, true);
}
