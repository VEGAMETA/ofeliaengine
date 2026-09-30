// Post-processing before tone mapping (ADR 0047): bloom and automatic exposure.
//
// Bloom (bloom.rs, after Jimenez, "Next Generation Post Processing in Call of Duty:
// Advanced Warfare", SIGGRAPH 2014): the HDR image is halved several times with a 13-tap
// filter (the first time weighting each 2 × 2 group by 1 / (1 + luma), Karis' average, so
// single bright pixels do not flicker), then enlarged back with a 3 × 3 tent, each level
// mixing its own downsample with the enlarged smaller levels. Every filter's weights sum
// to 1: a uniform image blooms into itself, and the tone mapper mixes the result in
// (energy is neither added nor lost).
//
// Automatic exposure (exposure.rs): a histogram of log2 luminance (256 bins), the average
// over the pixels between two percentiles (the darkest and brightest are ignored), an
// exposure that brings that average to middle grey, adapted over time in EV. It never
// leaves the GPU.
//
// No code was copied; see docs/research/post-processing.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint* histogram;         // 256 bins of log2 luminance
    float4* exposure;        // x: exposure, y: log2 exposure (adapted), z: its target
    uint source;             // sampled: the image read
    uint target;             // storage: the level written
    uint sampler;            // bilinear, clamp
    uint karis;              // bloom: 1 on the first downsample
    uint2 size;              // size written (bloom), or of the image (histogram)
    float scatter;           // bloom: how much of the smaller levels each level takes
    uint lower;              // bloom upsample: the enlarged smaller level (sampled)
    float min_log;           // histogram: log2 luminance of bin 1
    float log_range;         // histogram: log2 luminance spanned by bins 1..255
    float low_percent;       // average over the pixels between these fractions
    float high_percent;
    float rate_up;           // adaptation per frame towards a higher exposure (0..1)
    float rate_down;         // and towards a lower one
    float key;               // the average's target luminance (middle grey)
    float min_exposure;      // log2
    float max_exposure;      // log2
    uint reset;              // 1: jump to the target (first frame, cut)
    uint step;               // histogram: every step-th pixel along each axis
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

float luma(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

float3 fetch(Texture2D t, SamplerState s, float2 uv)
{
    // HDR sources may hold infinities or NaN after a bad frame: never let them spread.
    float3 c = t.SampleLevel(s, uv, 0.0).rgb;
    return min(max(c, 0.0), 65000.0);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void bloomDownsample(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    Texture2D source = g_textures[push.source];
    SamplerState s = g_samplers[push.sampler];
    uint width, height;
    source.GetDimensions(width, height);
    float2 t = 1.0 / float2(width, height);
    float2 uv = (float2(id.xy) + 0.5) / float2(push.size);
    float3 a = fetch(source, s, uv + t * float2(-2, -2));
    float3 b = fetch(source, s, uv + t * float2(0, -2));
    float3 c = fetch(source, s, uv + t * float2(2, -2));
    float3 d = fetch(source, s, uv + t * float2(-1, -1));
    float3 e = fetch(source, s, uv + t * float2(1, -1));
    float3 f = fetch(source, s, uv + t * float2(-2, 0));
    float3 g = fetch(source, s, uv);
    float3 h = fetch(source, s, uv + t * float2(2, 0));
    float3 i = fetch(source, s, uv + t * float2(-1, 1));
    float3 j = fetch(source, s, uv + t * float2(1, 1));
    float3 k = fetch(source, s, uv + t * float2(-2, 2));
    float3 l = fetch(source, s, uv + t * float2(0, 2));
    float3 m = fetch(source, s, uv + t * float2(2, 2));
    // Five overlapping 2 × 2 groups: the center one weighs 1/2, the corners 1/8 each.
    float3 groups[5] = {
        (d + e + i + j) * 0.25,
        (a + b + f + g) * 0.25,
        (b + c + g + h) * 0.25,
        (f + g + k + l) * 0.25,
        (g + h + l + m) * 0.25,
    };
    float weights[5] = { 0.5, 0.125, 0.125, 0.125, 0.125 };
    float3 sum = 0.0;
    float total = 0.0;
    [unroll]
    for (uint n = 0; n < 5; n++)
    {
        float w = weights[n];
        if (push.karis != 0)
            w /= 1.0 + luma(groups[n]);
        sum += groups[n] * w;
        total += w;
    }
    g_images[push.target][id.xy] = float4(sum / total, 1.0);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void bloomUpsample(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    SamplerState s = g_samplers[push.sampler];
    Texture2D lower = g_textures[push.lower];
    uint width, height;
    lower.GetDimensions(width, height);
    float2 t = 1.0 / float2(width, height);
    float2 uv = (float2(id.xy) + 0.5) / float2(push.size);
    // A 3 × 3 tent (1 2 1 / 2 4 2 / 1 2 1) over the smaller level.
    float3 tent = fetch(lower, s, uv) * 4.0
        + (fetch(lower, s, uv + t * float2(-1, 0)) + fetch(lower, s, uv + t * float2(1, 0))
            + fetch(lower, s, uv + t * float2(0, -1)) + fetch(lower, s, uv + t * float2(0, 1))) * 2.0
        + fetch(lower, s, uv + t * float2(-1, -1)) + fetch(lower, s, uv + t * float2(1, -1))
        + fetch(lower, s, uv + t * float2(-1, 1)) + fetch(lower, s, uv + t * float2(1, 1));
    float3 own = fetch(g_textures[push.source], s, uv);
    g_images[push.target][id.xy] = float4(lerp(own, tent / 16.0, push.scatter), 1.0);
}

groupshared uint g_bins[256];

// One thread per sampled pixel (every `step`-th along each axis: the average does not need
// them all): its luminance's bin, counted in the group, then added to the histogram once
// per bin and group.
[shader("compute")]
[numthreads(16, 16, 1)]
void exposureHistogram(uint3 id : SV_DispatchThreadID, uint local : SV_GroupIndex)
{
    g_bins[local] = 0;
    GroupMemoryBarrierWithGroupSync();
    uint2 pixel = id.xy * push.step;
    if (all(pixel < push.size))
    {
        float l = luma(g_textures[push.source].Load(int3(pixel, 0)).rgb);
        uint bin = 0;
        if (l > 1e-6)
            bin = uint(saturate((log2(l) - push.min_log) / push.log_range) * 254.0) + 1;
        InterlockedAdd(g_bins[bin], 1u);
    }
    GroupMemoryBarrierWithGroupSync();
    if (g_bins[local] != 0)
        InterlockedAdd(push.histogram[local], g_bins[local]);
}

groupshared float g_prefix[256];
groupshared float2 g_sum[256];

// One group of 256 threads, one per bin: prefix sums of the counts, each bin's share
// between the two percentiles, reduced into the average log2 luminance; then the exposure
// that maps it to the key, adapted towards.
[shader("compute")]
[numthreads(256, 1, 1)]
void exposureAverage(uint b : SV_GroupIndex)
{
    uint2 sampled = (push.size + push.step - 1) / push.step;
    float total = float(sampled.x * sampled.y);
    float count = float(push.histogram[b]);
    g_prefix[b] = count;
    GroupMemoryBarrierWithGroupSync();
    // Inclusive prefix sum (Hillis-Steele).
    for (uint offset = 1; offset < 256; offset <<= 1)
    {
        // Read only in range: a ternary would evaluate both sides.
        float add = 0.0;
        if (b >= offset)
            add = g_prefix[b - offset];
        GroupMemoryBarrierWithGroupSync();
        g_prefix[b] += add;
        GroupMemoryBarrierWithGroupSync();
    }
    float end = g_prefix[b];
    float start = end - count;
    float lo = total * push.low_percent;
    float hi = total * push.high_percent;
    float take = max(min(end, hi) - max(start, lo), 0.0);
    float log_l = b == 0 ? push.min_log : push.min_log + (float(b) - 0.5) / 254.0 * push.log_range;
    g_sum[b] = float2(take * log_l, take);
    GroupMemoryBarrierWithGroupSync();
    for (uint stride = 128; stride > 0; stride >>= 1)
    {
        if (b < stride)
            g_sum[b] += g_sum[b + stride];
        GroupMemoryBarrierWithGroupSync();
    }
    if (b != 0)
        return;
    float average = g_sum[0].y > 0.0 ? g_sum[0].x / g_sum[0].y : push.min_log;
    float target = clamp(log2(push.key) - average, push.min_exposure, push.max_exposure);
    float current = push.reset != 0 ? target : push.exposure[0].y;
    float rate = target > current ? push.rate_up : push.rate_down;
    current += (target - current) * rate;
    push.exposure[0] = float4(exp2(current), current, target, 1.0);
}
