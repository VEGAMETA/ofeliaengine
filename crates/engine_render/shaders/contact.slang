// Contact shadows of the sun (ADR 0071): what the depth buffer hides from the light at
// small scales the shadow maps miss (a texel of a cascade is centimetres to metres wide).
// Two ways, measured against each other (docs/research/local-shadows.md):
//
// - contactBend: Bend Studio's screen-space shadows (Days Gone; Graham Aldridge,
//   SIGGRAPH 2023; `bend_sss_gpu.h`, Copyright 2023 Sony Interactive Entertainment,
//   Apache-2.0, adapted here; see THIRD_PARTY_NOTICES.md). Every wave walks 64 pixels of a
//   line towards the light's point on screen; each thread reads a few depths along it into
//   shared memory, perspective-corrected so all rays to the light are parallel there, and
//   compares its pixel's depth with the next SAMPLE_COUNT depths of the line: one long
//   shadow for a few texture reads per pixel. The CPU splits the screen into up to eight
//   dispatches around the light's point (`contact.rs`, after `bend_sss_cpu.h`).
// - contactMarch: a ray per pixel towards the sun in world space, MARCH_STEPS samples over
//   a fixed length (jittered by interleaved gradient noise), each projected and compared
//   with the depth buffer within a thickness.
//
// Both write the sun's visibility (1 lit, 0 shadowed) into an R8 target the size of the
// depth, which the forward pass's `*Contact` entry points multiply into the sun's light.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("r8")] RWTexture2D<float> g_images[];

// `March` in contact.rs: what contactMarch needs of the view.
struct March
{
    float4x4 clip_from_world;  // jittered, as the depth was drawn
    float4x4 world_from_clip;
    float4 to_light;           // xyz unit vector towards the sun; w: the ray's length (world units)
    float4 eye;                // xyz camera position; w: thickness of a depth sample (world units)
    float4 forward;            // xyz unit view direction
    float4 depth_terms;        // x p22, y p32: view depth = p32 / (depth + p22)
};

// `ContactPush` in contact.rs (72 bytes).
struct ContactPush
{
    float4 light;              // Bend: the light's pixel xy, its depth, sign of its w
    int2 wave_offset;          // Bend: this dispatch's first wave, in pixels from the light
    uint2 size;                // the depth's size in pixels
    float surface_thickness;   // Bend: a pixel's thickness, part of the depth to the far plane
    float bilinear_threshold;  // Bend: depth step treated as an edge
    float shadow_contrast;     // Bend: contrast of the transition
    float far_depth;           // depth value of the far plane
    float near_depth;          // depth value of the near plane
    uint depth;                // bindless sampled: the depth target
    uint target;               // bindless storage: the visibility
    uint frame;                // contactMarch: noise index
    March* march;
};

[[vk::push_constant]]
ConstantBuffer<ContactPush> push;

// The depth at a pixel; the far plane outside the target (Bend's clamp-to-border sampler).
float load_depth(int2 xy)
{
    if (any(xy < int2(0, 0)) || any(xy >= int2(push.size)))
        return push.far_depth;
    return g_textures[push.depth].Load(int3(xy, 0)).r;
}

// Bend's configuration (bend_sss_gpu.h, its recommended starting values).
static const int WAVE_SIZE = 64;
static const int SAMPLE_COUNT = 60;
static const int HARD_SHADOW_SAMPLES = 4;
static const int FADE_OUT_SAMPLES = 8;
static const int READ_COUNT = SAMPLE_COUNT / WAVE_SIZE + 2;

groupshared float g_depth_data[READ_COUNT * WAVE_SIZE];
groupshared bool g_early_out;

// The start pixel of this thread on its wave's line, and the step to the pixel WAVE_SIZE
// further along it (Bend's ComputeWavefrontExtents).
void wavefront_extents(int3 group, uint thread, out float2 delta_xy, out float2 pixel_xy, out float pixel_distance, out bool x_axis_major)
{
    int2 xy = group.yz * WAVE_SIZE + push.wave_offset;
    float2 light_xy = floor(push.light.xy) + 0.5;
    float2 light_xy_fraction = push.light.xy - light_xy;
    bool reverse_direction = push.light.w > 0.0;
    int2 sign_xy = int2(sign(float2(xy)));
    bool horizontal = abs(xy.x + sign_xy.y) < abs(xy.y - sign_xy.x);
    int2 axis;
    axis.x = horizontal ? sign_xy.y : 0;
    axis.y = horizontal ? 0 : -sign_xy.x;
    xy = axis * group.x + xy;
    float2 xy_f = float2(xy);
    x_axis_major = abs(xy_f.x) > abs(xy_f.y);
    float major_axis = x_axis_major ? xy_f.x : xy_f.y;
    float major_axis_start = abs(major_axis);
    float major_axis_end = abs(major_axis) - float(WAVE_SIZE);
    float ma_light_frac = x_axis_major ? light_xy_fraction.x : light_xy_fraction.y;
    ma_light_frac = major_axis > 0 ? -ma_light_frac : ma_light_frac;
    float2 start_xy = xy_f + light_xy;
    // The innermost ring interpolates to a pixel-centered uv, so rounding skips no pixel.
    float2 end_xy = lerp(push.light.xy, start_xy, (major_axis_end + ma_light_frac) / (major_axis_start + ma_light_frac));
    float2 xy_delta = start_xy - end_xy;
    float thread_step = float(thread ^ (reverse_direction ? 0u : uint(WAVE_SIZE - 1)));
    pixel_xy = lerp(start_xy, end_xy, thread_step / float(WAVE_SIZE));
    pixel_distance = major_axis_start - thread_step + ma_light_frac;
    delta_xy = xy_delta;
}

// Bend Studio's screen-space shadow (WriteScreenSpaceShadow), with
// BilinearSamplingOffsetMode, IgnoreEdgePixels and UsePrecisionOffset off, and the early
// out for pixels without geometry (the far plane) on.
[shader("compute")]
[numthreads(64, 1, 1)]
void contactBend(uint3 group_id : SV_GroupID, uint thread : SV_GroupIndex)
{
    int3 group = int3(group_id);
    float2 xy_delta;
    float2 pixel_xy;
    float pixel_distance;
    bool x_axis_major;
    wavefront_extents(group, thread, xy_delta, pixel_xy, pixel_distance, x_axis_major);

    float sampling_depth[READ_COUNT];
    float shadowing_depth[READ_COUNT];
    float depth_thickness_scale[READ_COUNT];
    float sample_distance[READ_COUNT];
    float direction = -push.light.w;
    float z_sign = push.near_depth > push.far_depth ? -1.0 : 1.0;
    float2 write_xy = floor(pixel_xy);

    [unroll]
    for (int i = 0; i < READ_COUNT; i++)
    {
        // Two depths per sample, interpolated on the minor axis unless they straddle an edge.
        float2 read_xy = floor(pixel_xy);
        float minor_axis = x_axis_major ? pixel_xy.y : pixel_xy.x;
        float bilinear = frac(minor_axis) - 0.5;
        int bias = bilinear > 0.0 ? 1 : -1;
        int2 offset_xy = int2(x_axis_major ? 0 : bias, x_axis_major ? bias : 0);
        float2 depths;
        depths.x = load_depth(int2(read_xy));
        depths.y = load_depth(int2(read_xy) + offset_xy);
        // Thresholds are parts of the depth between the sample and the far plane.
        depth_thickness_scale[i] = abs(push.far_depth - depths.x);
        bool use_point_filter = abs(depths.x - depths.y) > depth_thickness_scale[i] * push.bilinear_threshold;
        sampling_depth[i] = depths.x;
        // Any sample in this wave may be interpolated towards the second depth: cast from
        // the farther of the two.
        float shadow_depth = depths.x + abs(depths.x - depths.y) * z_sign;
        shadowing_depth[i] = use_point_filter ? depths.x : shadow_depth;
        sample_distance[i] = pixel_distance + float(WAVE_SIZE * i) * direction;
        pixel_xy += xy_delta * direction;
    }

    // Pixels without geometry need no shadow (they are written lit); a wave of them stops
    // here.
    float start = sampling_depth[0];
    bool skip_pixel = start >= max(push.far_depth, push.near_depth) || start <= min(push.far_depth, push.near_depth);
    int2 out_xy = int2(write_xy);
    bool inside = all(out_xy >= int2(0, 0)) && all(out_xy < int2(push.size));
    if (skip_pixel && inside)
        g_images[push.target][out_xy] = 1.0;
    bool early_out = !WaveActiveAnyTrue(!skip_pixel);
    if (WaveGetLaneCount() == WAVE_SIZE)
    {
        if (early_out)
            return;
    }
    else
    {
        // Several smaller waves make the group: they stop together.
        g_early_out = true;
        GroupMemoryBarrierWithGroupSync();
        if (!early_out)
            g_early_out = false;
        GroupMemoryBarrierWithGroupSync();
        if (g_early_out)
            return;
    }

    [unroll]
    for (int i = 0; i < READ_COUNT; i++)
    {
        // Perspective-corrected: in this space all rays to the light are parallel.
        float stored_depth = (shadowing_depth[i] - push.light.z) / sample_distance[i];
        // Reads past the light's point are ignored.
        if (i != 0)
            stored_depth = sample_distance[i] > 0.0 ? stored_depth : 1e10;
        g_depth_data[i * WAVE_SIZE + int(thread)] = stored_depth;
    }
    GroupMemoryBarrierWithGroupSync();
    if (skip_pixel)
        return;

    float start_depth = (start - push.light.z) / sample_distance[0];
    int sample_index = int(thread) + 1;
    float4 shadow_value = 1.0;
    float hard_shadow = 1.0;
    // The window of the projected samples is 1 wide: scale by the thickness, and keep a
    // minimum width close to the light.
    float depth_scale = min(sample_distance[0] + direction, 1.0 / push.surface_thickness) * sample_distance[0] / depth_thickness_scale[0];
    start_depth = start_depth * depth_scale - z_sign;

    // The first samples alone may shadow the pixel fully (grounding contact).
    [unroll]
    for (int i = 0; i < HARD_SHADOW_SAMPLES; i++)
    {
        float depth_delta = abs(start_depth - g_depth_data[sample_index + i] * depth_scale);
        hard_shadow = min(hard_shadow, depth_delta);
    }
    // The rest average in four groups, which softens single-pixel shadows.
    [unroll]
    for (int i = HARD_SHADOW_SAMPLES; i < SAMPLE_COUNT - FADE_OUT_SAMPLES; i++)
    {
        float depth_delta = abs(start_depth - g_depth_data[sample_index + i] * depth_scale);
        shadow_value[i & 3] = min(shadow_value[i & 3], depth_delta);
    }
    // The last ones fade out, so the shadow does not end abruptly.
    [unroll]
    for (int i = SAMPLE_COUNT - FADE_OUT_SAMPLES; i < SAMPLE_COUNT; i++)
    {
        float depth_delta = abs(start_depth - g_depth_data[sample_index + i] * depth_scale);
        float fade_out = float(i + 1 - (SAMPLE_COUNT - FADE_OUT_SAMPLES)) / float(FADE_OUT_SAMPLES + 1) * 0.75;
        shadow_value[i & 3] = min(shadow_value[i & 3], depth_delta + fade_out);
    }
    shadow_value = saturate(shadow_value * push.shadow_contrast + (1.0 - push.shadow_contrast));
    hard_shadow = saturate(hard_shadow * push.shadow_contrast + (1.0 - push.shadow_contrast));
    float result = min(hard_shadow, dot(shadow_value, 0.25));
    if (inside)
        g_images[push.target][out_xy] = result;
}

// Interleaved gradient noise (Jimenez 2014) in [0, 1) at a pixel.
float gradient_noise(float2 pixel)
{
    return frac(52.9829189 * frac(dot(pixel, float2(0.06711056, 0.00583715))));
}

static const int MARCH_STEPS = 16;

// A ray per pixel towards the sun (see the file comment).
[shader("compute")]
[numthreads(8, 8, 1)]
void contactMarch(uint3 id : SV_DispatchThreadID)
{
    int2 pixel = int2(id.xy);
    if (any(pixel >= int2(push.size)))
        return;
    float depth = load_depth(pixel);
    if (depth >= push.far_depth)
    {
        g_images[push.target][pixel] = 1.0;
        return;
    }
    March* m = push.march;
    float2 size = float2(push.size);
    float2 ndc = (float2(pixel) + 0.5) / size * 2.0 - 1.0;
    float4 h = mul(m.world_from_clip, float4(ndc, depth, 1.0));
    float3 world = h.xyz / h.w;
    float3 eye = m.eye.xyz;
    float3 forward = m.forward.xyz;
    float reach = m.to_light.w;
    float thickness = m.eye.w;
    float step = reach / float(MARCH_STEPS);
    float jitter = gradient_noise(float2(pixel) + 5.588238 * float(push.frame & 63u));
    float lit = 1.0;
    for (int i = 0; i < MARCH_STEPS; i++)
    {
        float t = (float(i) + jitter) * step;
        float3 p = world + m.to_light.xyz * t;
        float4 c = mul(m.clip_from_world, float4(p, 1.0));
        if (c.w <= 0.0)
            break;
        int2 at = int2((c.xy / c.w * 0.5 + 0.5) * size);
        if (any(at < int2(0, 0)) || any(at >= int2(push.size)))
            break;
        float scene = m.depth_terms.y / (load_depth(at) + m.depth_terms.x);
        float ray = dot(p - eye, forward);
        // Behind the depth by more than its own surface's rounding and less than the
        // thickness: occluded; fading with the distance along the ray.
        float delta = ray - scene;
        if (delta > ray * 0.002 && delta < thickness)
        {
            float f = t / reach;
            lit = f * f;
            break;
        }
    }
    g_images[push.target][pixel] = lit;
}
