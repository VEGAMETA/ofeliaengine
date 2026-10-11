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
    g_images[push.target][id.xy] = float4(downsampled(a, b, c, d, e, f, g, h, i, j, k, l, m), 1.0);
}

// The 13 taps (a b c / d e / f g h / i j / k l m, in rows) of `bloomDownsample`, combined.
float3 downsampled(
    float3 a, float3 b, float3 c, float3 d, float3 e, float3 f, float3 g, float3 h, float3 i,
    float3 j, float3 k, float3 l, float3 m)
{
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
    return sum / total;
}

// Pixels per side of a group of the shared-memory bloom passes.
static const uint BLOOM_TILE = 16;
static const uint BLOOM_THREADS = BLOOM_TILE * BLOOM_TILE;
// Texel corners of the downsample's taps for a group: on its pixels' 2 × 2 blocks (the
// taps at 0 and ±2 texels, ALIGNED a side), then between the blocks (the taps at ±1).
static const uint ALIGNED = BLOOM_TILE + 2;
static const uint SHIFTED = BLOOM_TILE + 1;
static const uint CORNERS = ALIGNED * ALIGNED + SHIFTED * SHIFTED;
groupshared float3 g_corners[CORNERS];

// `bloomDownsample` of a level exactly half its source's size, through shared memory:
// neighbouring pixels' taps fall on the same texel corners, so a group fetches each corner
// once (613 fetches for 256 pixels instead of 3,328). The same fetches and sums.
[shader("compute")]
[numthreads(BLOOM_TILE, BLOOM_TILE, 1)]
void bloomDownsampleShared(
    uint3 group : SV_GroupID,
    uint3 local : SV_GroupThreadID,
    uint index : SV_GroupIndex)
{
    Texture2D source = g_textures[push.source];
    SamplerState s = g_samplers[push.sampler];
    float2 t = 1.0 / float2(push.size * 2);
    int2 origin = int2(group.xy * BLOOM_TILE);
    // Pixel x's taps at 0 and ±2 texels are the corners between texels 2k and 2k + 1 for
    // k = x - 1 .. x + 1 (at 2k + 1); at ±1, those between 2k - 1 and 2k for k = x, x + 1.
    // Every fetch first, then every store: the fetches are in flight together.
    static const uint ROUNDS = (CORNERS + BLOOM_THREADS - 1) / BLOOM_THREADS;
    float3 fetched[ROUNDS];
    [unroll]
    for (uint r = 0; r < ROUNDS; r++)
    {
        uint n = min(index + r * BLOOM_THREADS, CORNERS - 1);
        bool aligned = n < ALIGNED * ALIGNED;
        uint m = aligned ? n : n - ALIGNED * ALIGNED;
        uint side = aligned ? ALIGNED : SHIFTED;
        int2 k = origin + int2(m % side, m / side) - (aligned ? 1 : 0);
        fetched[r] = fetch(source, s, float2(k * 2 + (aligned ? 1 : 0)) * t);
    }
    [unroll]
    for (uint r = 0; r < ROUNDS; r++)
    {
        uint n = index + r * BLOOM_THREADS;
        if (n < CORNERS)
            g_corners[n] = fetched[r];
    }
    GroupMemoryBarrierWithGroupSync();
    uint2 id = group.xy * BLOOM_TILE + local.xy;
    if (any(id >= push.size))
        return;
    // The pixel's corners: on blocks from (x - 1, y - 1), between them from (x, y).
    uint on = local.y * ALIGNED + local.x;
    uint between = ALIGNED * ALIGNED + local.y * SHIFTED + local.x;
    g_images[push.target][id] = float4(
        downsampled(
            g_corners[on], g_corners[on + 1], g_corners[on + 2],
            g_corners[between], g_corners[between + 1],
            g_corners[on + ALIGNED], g_corners[on + ALIGNED + 1], g_corners[on + ALIGNED + 2],
            g_corners[between + SHIFTED], g_corners[between + SHIFTED + 1],
            g_corners[on + 2 * ALIGNED], g_corners[on + 2 * ALIGNED + 1],
            g_corners[on + 2 * ALIGNED + 2]),
        1.0);
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

// The tent's taps of a group of `bloomUpsampleShared`: pixel 2X + p (p its parity) has
// them at X + 0.25 + 0.5 p + j lower texels (j = -1, 0, 1), so the pixels of one parity
// share them; per parity of the column and of the row, BLOOM_TILE / 2 + 2 a side.
static const uint TENT = BLOOM_TILE / 2 + 2;
static const uint TENT_TAPS = 4 * TENT * TENT;
groupshared float3 g_tent[TENT_TAPS];

// `bloomUpsample` of a level exactly twice its lower level's size, through shared memory
// (400 fetches of the lower level for 256 pixels instead of 2,304). The same fetches and
// sums.
[shader("compute")]
[numthreads(BLOOM_TILE, BLOOM_TILE, 1)]
void bloomUpsampleShared(
    uint3 group : SV_GroupID,
    uint3 local : SV_GroupThreadID,
    uint index : SV_GroupIndex)
{
    SamplerState s = g_samplers[push.sampler];
    Texture2D lower = g_textures[push.lower];
    float2 t = 1.0 / float2(push.size / 2);
    // The group's X are group * BLOOM_TILE / 2 + 0 .. BLOOM_TILE / 2 - 1: their taps' X + j
    // from one before the first to one after the last. Taps by parity (row, column), then
    // row, then column.
    int2 first = int2(group.xy * (BLOOM_TILE / 2)) - 1;
    static const uint ROUNDS = (TENT_TAPS + BLOOM_THREADS - 1) / BLOOM_THREADS;
    float3 fetched[ROUNDS];
    [unroll]
    for (uint r = 0; r < ROUNDS; r++)
    {
        uint n = min(index + r * BLOOM_THREADS, TENT_TAPS - 1);
        uint2 cell = uint2(n % TENT, (n / TENT) % TENT);
        uint2 parity = uint2((n / (TENT * TENT)) % 2, n / (2 * TENT * TENT));
        float2 c = float2(first + int2(cell)) + 0.25 + 0.5 * float2(parity);
        fetched[r] = fetch(lower, s, c * t);
    }
    [unroll]
    for (uint r = 0; r < ROUNDS; r++)
    {
        uint n = index + r * BLOOM_THREADS;
        if (n < TENT_TAPS)
            g_tent[n] = fetched[r];
    }
    GroupMemoryBarrierWithGroupSync();
    uint2 id = group.xy * BLOOM_TILE + local.xy;
    if (any(id >= push.size))
        return;
    uint2 p = local.xy & 1;
    // The taps' cells: q, q + 1 and q + 2 along each axis.
    uint2 q = local.xy >> 1;
    uint o = (p.y * 2 + p.x) * TENT * TENT + q.y * TENT + q.x;
    float3 tent = g_tent[o + TENT + 1] * 4.0
        + (g_tent[o + TENT] + g_tent[o + TENT + 2] + g_tent[o + 1] + g_tent[o + 2 * TENT + 1]) * 2.0
        + g_tent[o] + g_tent[o + 2] + g_tent[o + 2 * TENT] + g_tent[o + 2 * TENT + 2];
    float2 uv = (float2(id) + 0.5) / float2(push.size);
    float3 own = fetch(g_textures[push.source], s, uv);
    g_images[push.target][id] = float4(lerp(own, tent / 16.0, push.scatter), 1.0);
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
    float current = push.reset != 0 ? target : loadAligned<16>(push.exposure).y;
    float rate = target > current ? push.rate_up : push.rate_down;
    current += (target - current) * rate;
    storeAligned<16>(push.exposure, float4(exp2(current), current, target, 1.0));
}
