// Local reflections (reflections.rs, ADR 0073): reflection probes and screen-space
// reflections, applied after the opaque surfaces by one full-screen composite.
//
// Views with reflections shade their opaque surfaces with the `fragmentReflect*` entry
// points of forward.slang, which also write each pixel's normal and roughness and the
// weight of its prefiltered radiance (split sum × specular occlusion). The forward pass
// lit them with the sky's prefiltered cube as everywhere else; the composite adds
// weight × (reflection − sky), which swaps the sky's specular light for the chain
// screen-space hit → probes → sky:
//
// - Probes (Lagarde and Zanuttini, SIGGRAPH 2012): each is a cube captured from a point
//   by the forward renderer itself (`probeFace` copies a rendered face into a capture
//   cube, filling what nothing covered with the sky; `probeDownsample` makes its mips;
//   `probePrefilter` convolves it with GGX per roughness into the probe's layers of one
//   cube array, as environment.slang does for the sky). A pixel blends the probes whose
//   influence box holds it, smallest first, each fading in over its blend distance; the
//   reflected ray is intersected with the probe's parallax box and the cube read towards
//   that point (parallax correction). The rest of the weight goes to the sky.
//   The probes a pixel tests are listed by screen cell and view-depth z-bin in the view's
//   constants (reflections_common.slang).
// - Screen-space reflections: `ssrDepth` reduces the depth into a pyramid of nearest
//   depths; `ssrTrace` (at full or half resolution) samples a reflected direction from
//   the visible normals of the GGX lobe (Heitz 2018; mirror rays below a roughness) and
//   walks the pyramid (the hierarchical traversal of AMD's FidelityFX SSSR, MIT, see
//   THIRD_PARTY_NOTICES.md), reading the frame's lit color at the hit; `ssrSpatial`
//   averages neighbouring rays with depth, normal and roughness weights at the trace's
//   resolution; `ssrTemporal` brings it to full resolution and blends it with last frame's
//   result reprojected through the reflected point, clipped to the current neighbourhood. Values are premultiplied by their confidence (fading at the screen's
//   edges, behind thin objects, at the roughness cutoff), so the composite adds the hit
//   over the probes and the sky by it.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(0, 0)]] TextureCubeArray g_cube_arrays[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2DArray<float4> g_layers[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(1, 0)]] [vk::image_format("r16f")] RWTexture2D<float> g_scalars[];
[[vk::binding(1, 0)]] [vk::image_format("rg16f")] RWTexture2D<float2> g_scalars2[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];
// Multisampled depth (MSAA, ADR 0075), the same bindless slots.
[[vk::binding(0, 0)]] Texture2DMS<float> g_depth_ms[];

static const float PI = 3.14159265;

#include "reflections_common.slang"

struct ReflectPush
{
    ReflectFrame* frame;
    uint* buffer;            // SSR: the nearest depths (float bits, levels 1.. one after the other)
    uint t0;                 // bindless indices, by pass (see each entry point)
    uint t1;
    uint t2;
    uint t3;
    uint t4;
    uint t5;
    uint target;             // storage image written
    uint read;               // storage image read
    uint size;
    uint level;
    uint levels;
    uint layer;
    uint face;
    uint mode;
    uint sampler;
    float value;
};

[[vk::push_constant]]
ConstantBuffer<ReflectPush> push;

// --- Shared --------------------------------------------------------------------------

// The direction through the centre of texel `xy` of cube face `face` (environment.slang's
// convention: +X, −X, +Y, −Y, +Z, −Z; v grows downwards).
float3 face_direction(uint2 xy, uint face, uint size)
{
    float2 uv = (float2(xy) + 0.5) / float(size) * 2.0 - 1.0;
    float3 d;
    switch (face)
    {
    case 0: d = float3(1.0, -uv.y, -uv.x); break;
    case 1: d = float3(-1.0, -uv.y, uv.x); break;
    case 2: d = float3(uv.x, 1.0, uv.y); break;
    case 3: d = float3(uv.x, -1.0, -uv.y); break;
    case 4: d = float3(uv.x, -uv.y, 1.0); break;
    default: d = float3(-uv.x, -uv.y, -1.0); break;
    }
    return normalize(d);
}

// The camera each face is rendered with (`FACES` in reflections.rs): forward and up.
void face_camera(uint face, out float3 forward, out float3 up)
{
    switch (face)
    {
    case 0: forward = float3(1.0, 0.0, 0.0); up = float3(0.0, 1.0, 0.0); break;
    case 1: forward = float3(-1.0, 0.0, 0.0); up = float3(0.0, 1.0, 0.0); break;
    case 2: forward = float3(0.0, 1.0, 0.0); up = float3(0.0, 0.0, -1.0); break;
    case 3: forward = float3(0.0, -1.0, 0.0); up = float3(0.0, 0.0, 1.0); break;
    case 4: forward = float3(0.0, 0.0, 1.0); up = float3(0.0, 1.0, 0.0); break;
    default: forward = float3(0.0, 0.0, -1.0); up = float3(0.0, 1.0, 0.0); break;
    }
}

// --- Probe capture -------------------------------------------------------------------

// One rendered face into mip 0 of the capture cube (`target`, layer `face`): texel by
// texel through the face camera's projection (90°, square). Where nothing was drawn
// (depth 1), the sky's radiance cube (`t2`, × `value`) without the sun's disk: the
// sun's specular light comes from the directional light. `t0`: the face's color, `t1`:
// its depth.
[shader("compute")]
[numthreads(8, 8, 1)]
void probeFace(uint3 id : SV_DispatchThreadID)
{
    uint size = push.size;
    if (any(id.xy >= size))
        return;
    float3 d = face_direction(id.xy, push.face, size);
    float3 forward, up;
    face_camera(push.face, forward, up);
    float3 right = cross(forward, up);
    float a = dot(d, forward);
    float2 s = float2(dot(d, right), dot(d, up)) / a;
    int2 pixel = clamp(int2(floor(float2(s.x + 1.0, 1.0 - s.y) * 0.5 * float(size))), int2(0, 0), int2(size - 1, size - 1));
    float depth = g_textures[push.t1].Load(int3(pixel, 0)).r;
    float3 color;
    if (depth >= 1.0)
        color = g_cubes[push.t2].SampleLevel(g_samplers[push.sampler], d, 0.0).rgb * push.value;
    else
        color = g_textures[push.t0].Load(int3(pixel, 0)).rgb;
    g_layers[push.target][uint3(id.xy, push.face)] = float4(color, 1.0);
}

// Mip `size` of the capture cube from the mip above (`read`): the average of each 2 × 2.
[shader("compute")]
[numthreads(8, 8, 1)]
void probeDownsample(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    RWTexture2DArray<float4> above = g_layers[push.read];
    uint3 p = uint3(id.xy * 2, id.z);
    float4 sum = above[p] + above[p + uint3(1, 0, 0)] + above[p + uint3(0, 1, 0)] + above[p + uint3(1, 1, 0)];
    g_layers[push.target][id] = sum * 0.25;
}

float2 hammersley(uint i, uint n)
{
    return float2((float(i) + 0.5) / float(n), float(reversebits(i)) * 2.3283064365386963e-10);
}

float3 ggx_half(float2 xi, float a)
{
    float phi = 2.0 * PI * xi.x;
    float cos_theta = sqrt((1.0 - xi.y) / (1.0 + (a * a - 1.0) * xi.y));
    float sin_theta = sqrt(1.0 - cos_theta * cos_theta);
    return float3(sin_theta * cos(phi), sin_theta * sin(phi), cos_theta);
}

// An orthonormal basis around `n` (Duff et al. 2017).
void basis(float3 n, out float3 t, out float3 b)
{
    float s = n.z >= 0.0 ? 1.0 : -1.0;
    float a = -1.0 / (s + n.z);
    float c = n.x * n.y * a;
    t = float3(1.0 + s * n.x * n.x * a, s * c, -s * n.x);
    b = float3(c, s + n.y * n.y * a, -n.y);
}

float distribution(float n_dot_h, float a)
{
    float a2 = a * a;
    float f = (n_dot_h * a2 - n_dot_h) * n_dot_h + 1.0;
    return a2 / (PI * f * f);
}

// Mip `level` of probe `layer` in the cube array (`target`: that mip's storage view of
// every layer), from the capture cube (`t0`, all mips, face size `read` at mip 0):
// GGX-prefiltered for perceptual roughness level / (levels − 1), with filtered importance
// sampling (environment.slang's `prefilter`; `mode` samples).
[shader("compute")]
[numthreads(8, 8, 1)]
void probePrefilter(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return;
    float3 n = face_direction(id.xy, id.z, push.size);
    TextureCube source = g_cubes[push.t0];
    SamplerState s = g_samplers[push.sampler];
    uint3 out_texel = uint3(id.xy, push.layer * 6 + id.z);
    if (push.level == 0)
    {
        g_layers[push.target][out_texel] = float4(source.SampleLevel(s, n, 0.0).rgb, 1.0);
        return;
    }
    float perceptual = float(push.level) / float(push.levels - 1);
    float a = perceptual * perceptual;
    float3 t, b;
    basis(n, t, b);
    float texel = 4.0 * PI / (6.0 * float(push.read * push.read));
    float3 sum = 0.0;
    float weight = 0.0;
    uint samples = push.mode;
    for (uint i = 0; i < samples; i++)
    {
        float3 h = ggx_half(hammersley(i, samples), a);
        h = h.x * t + h.y * b + h.z * n;
        float n_dot_h = dot(n, h);
        float3 l = 2.0 * n_dot_h * h - n;
        float n_dot_l = dot(n, l);
        if (n_dot_l <= 0.0)
            continue;
        float pdf = distribution(n_dot_h, a) * 0.25;
        float sample_angle = 1.0 / (float(samples) * pdf + 1e-6);
        float lod = max(0.5 * log2(sample_angle / texel) + 1.0, 0.0);
        sum += source.SampleLevel(s, l, lod).rgb * n_dot_l;
        weight += n_dot_l;
    }
    g_layers[push.target][out_texel] = float4(sum / max(weight, 1e-6), 1.0);
}

// --- Composite -----------------------------------------------------------------------

struct Composite
{
    float4 position : SV_Position;
};

[shader("vertex")]
Composite compositeVertex(uint vertex : SV_VulkanVertexID)
{
    float2 ndc = float2((vertex << 1) & 2, vertex & 2) * 2.0 - 1.0;
    Composite o;
    o.position = float4(ndc, 0.0, 1.0);
    return o;
}

// Added to the opaque image (additive blending): weight × (reflection − sky). The forward
// pass lit each surface with the sky's prefiltered light; the reflection is the
// screen-space hit (premultiplied by its confidence a) over the probes (front to back by
// priority) over the sky: a·hit + (1 − a)·probes. `t0`: depth, `t1`: normal and roughness,
// `t2`: weight, `t3`: the screen-space reflections. `probes` and `ssr` are literals: each
// combination is an entry point of its own, with only its code (registers set the iGPU's
// occupancy, and with it how well texture latency hides).
float4 composite(Composite input, bool probes, bool ssr)
{
    ReflectFrame* f = push.frame;
    int2 pixel = int2(input.position.xy);
    float4 hit = 0.0;
    if (ssr)
    {
        hit = g_textures[push.t3].Load(int3(pixel, 0));
        // Without probes, only the pixels whose rays hit something change.
        if (!probes && hit.a <= 0.0)
            discard;
    }
    float3 weight = g_textures[push.t2].Load(int3(pixel, 0)).rgb;
    // The sky and subsurface materials: nothing written there.
    if (max(weight.r, max(weight.g, weight.b)) <= 0.0)
        discard;
    float depth = g_textures[push.t0].Load(int3(pixel, 0)).r;
    float4 nr = g_textures[push.t1].Load(int3(pixel, 0));
    float3 n = decode_normal(nr.xy);
    float roughness = nr.z;
    float view_depth;
    float3 world = world_position(f, input.position.xy, depth, view_depth);
    float3 v = normalize(read4(&f.camera).xyz - world);
    float3 r = reflect(-v, n);
    float lod = roughness * float(read4u(&f.env).y - 1);
    float3 sky = g_cubes[read4u(&f.env).x].SampleLevel(g_samplers[read4u(&f.env).z], r, lod).rgb * read4(&f.params).x;
    float3 reflection = sky;
    if (probes)
        reflection = probe_radiance(f, world, r, lod, input.position.xy, view_depth, sky);
    if (ssr)
        reflection = reflection * (1.0 - hit.a) + hit.rgb;
    float3 delta = weight * (reflection - sky);
    if (max(abs(delta.r), max(abs(delta.g), abs(delta.b))) <= 1e-7)
        discard;
    return float4(delta, 0.0);
}

[shader("fragment")]
float4 compositeProbes(Composite input) : SV_Target0
{
    return composite(input, true, false);
}

[shader("fragment")]
float4 compositeSsr(Composite input) : SV_Target0
{
    return composite(input, false, true);
}

[shader("fragment")]
float4 compositeProbesSsr(Composite input) : SV_Target0
{
    return composite(input, true, true);
}

// With MSAA (ADR 0075): the inputs are the nearest sample's (`t0` its depth), and the
// change is added only to the samples of that surface (`t4`: the multisampled depth,
// `t5`: the sample count), so it does not spill over the other side of a silhouette. A
// sample belongs to the surface when its view depth is within the surface's own change
// across a pixel (the smaller of each axis' one-sided differences: at a silhouette the
// side on the surface), plus 0.2 %. One invocation per pixel, as without MSAA.
uint nearest_samples(Composite input, uint nearest_depth)
{
    ReflectFrame* f = push.frame;
    int2 pixel = int2(input.position.xy);
    int2 hi = int2(read4u(&f.size).xy) - 1;
    Texture2D depths = g_textures[nearest_depth];
    float nearest = view_depth_of(f, depths.Load(int3(pixel, 0)).r);
    float left = view_depth_of(f, depths.Load(int3(max(pixel - int2(1, 0), 0), 0)).r);
    float right = view_depth_of(f, depths.Load(int3(min(pixel + int2(1, 0), hi), 0)).r);
    float up = view_depth_of(f, depths.Load(int3(max(pixel - int2(0, 1), 0), 0)).r);
    float down = view_depth_of(f, depths.Load(int3(min(pixel + int2(0, 1), hi), 0)).r);
    float slope = min(abs(left - nearest), abs(right - nearest)) + min(abs(up - nearest), abs(down - nearest));
    float tolerance = slope + 0.002 * nearest;
    uint mask = 0;
    for (uint s = 0; s < push.t5; s++)
    {
        float d = view_depth_of(f, g_depth_ms[push.t4].Load(pixel, s));
        if (d - nearest <= tolerance)
            mask |= 1u << s;
    }
    return mask;
}

[shader("fragment")]
float4 compositeProbesMs(Composite input, out uint coverage : SV_Coverage) : SV_Target0
{
    coverage = nearest_samples(input, push.t0);
    return composite(input, true, false);
}

[shader("fragment")]
float4 compositeSsrMs(Composite input, out uint coverage : SV_Coverage) : SV_Target0
{
    coverage = nearest_samples(input, push.t0);
    return composite(input, false, true);
}

[shader("fragment")]
float4 compositeProbesSsrMs(Composite input, out uint coverage : SV_Coverage) : SV_Target0
{
    coverage = nearest_samples(input, push.t0);
    return composite(input, true, true);
}

// The other side of a silhouette (MSAA): the samples behind the nearest surface get the
// probes' reflection of the farthest sample's surface (`t0`: its depth, `t1`, `t2`: its
// inputs; `read`: the nearest depth). Inside surfaces every sample is the nearest's: the
// pass stops there.
[shader("fragment")]
float4 compositeProbesFarMs(Composite input, out uint coverage : SV_Coverage) : SV_Target0
{
    uint all = (1u << push.t5) - 1u;
    coverage = all & ~nearest_samples(input, push.read);
    if (coverage == 0)
        discard;
    return composite(input, true, false);
}

// --- Screen-space reflections -------------------------------------------------------

uint2 level_size(uint level)
{
    return ((read4u(&push.frame.size).xy - 1) >> level) + 1;
}

uint pyramid_offset(uint level)
{
    return push.frame.pyramid_offsets[(level - 1) >> 2][(level - 1) & 3];
}

// `pyramid_offset` where every thread reads the same level (the depth pass): one 16-byte
// load of the row, measured faster than the 4-byte one there on the Radeon (the trace's
// levels vary per thread and per step, where the 4-byte load is faster).
uint pyramid_offset_uniform(uint level)
{
    return loadAligned<16>(&push.frame.pyramid_offsets[(level - 1) >> 2])[(level - 1) & 3];
}

// Level `level` of the nearest-depth pyramid (≥ 1) from the level below (`t0`: the depth
// target for level 1): the nearest of each 2 × 2 (sizes round up, reads clamp).
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrDepth(uint3 id : SV_DispatchThreadID)
{
    uint level = push.level;
    uint2 size = level_size(level);
    if (any(id.xy >= size))
        return;
    uint2 below = level_size(level - 1);
    uint2 p = id.xy * 2;
    uint2 q = min(p + 1, below - 1);
    float d;
    if (level == 1)
    {
        Texture2D depth = g_textures[push.t0];
        d = min(min(depth.Load(int3(p, 0)).r, depth.Load(int3(int2(q.x, p.y), 0)).r),
                min(depth.Load(int3(int2(p.x, q.y), 0)).r, depth.Load(int3(q, 0)).r));
    }
    else
    {
        uint o = pyramid_offset_uniform(level - 1);
        d = min(min(asfloat(push.buffer[o + p.y * below.x + p.x]), asfloat(push.buffer[o + p.y * below.x + q.x])),
                min(asfloat(push.buffer[o + q.y * below.x + p.x]), asfloat(push.buffer[o + q.y * below.x + q.x])));
    }
    push.buffer[pyramid_offset_uniform(level) + id.y * size.x + id.x] = asuint(d);
}

// The nearest depth over texel `texel` of `level` (0: the depth target, `t0`).
float load_depth(int2 texel, int level)
{
    uint2 size = level_size(uint(level));
    uint2 t = uint2(clamp(texel, int2(0, 0), int2(size) - 1));
    if (level == 0)
        return g_textures[push.t0].Load(int3(t, 0)).r;
    return asfloat(push.buffer[pyramid_offset(uint(level)) + t.y * size.x + t.x]);
}

// FidelityFX SSSR's hierarchical traversal (MIT; ffx_sssr.h, AMD), adapted: the ray in
// screen space (uv, depth), its origin and direction; steps up a level when it crossed a
// texel without meeting the surface, down when it met it. Our pyramid levels round up in
// size, so a level's texel boundaries are at uv multiples of 2^level / size.
void initial_advance(float3 origin, float3 direction, float3 inv_direction, float2 resolution, float2 resolution_inv, float2 floor_offset, float2 uv_offset, out float3 position, out float t)
{
    float2 current = resolution * origin.xy;
    float2 xy_plane = floor(current) + floor_offset;
    xy_plane = xy_plane * resolution_inv + uv_offset;
    float2 ts = xy_plane * inv_direction.xy - origin.xy * inv_direction.xy;
    t = min(ts.x, ts.y);
    position = origin + t * direction;
}

bool advance(float3 origin, float3 direction, float3 inv_direction, float2 current, float2 resolution_inv, float2 floor_offset, float2 uv_offset, float surface_z, inout float3 position, inout float t)
{
    float2 xy_plane = floor(current) + floor_offset;
    xy_plane = xy_plane * resolution_inv + uv_offset;
    float3 planes = float3(xy_plane, surface_z);
    float3 ts = planes * inv_direction - origin * inv_direction;
    // The surface plane only when the ray goes away from the camera.
    ts.z = direction.z > 0.0 ? ts.z : 3.4e38;
    float t_min = min(min(ts.x, ts.y), ts.z);
    bool above = surface_z > position.z;
    bool skipped = asuint(t_min) != asuint(ts.z) && above;
    t = above ? t_min : t;
    position = origin + t * direction;
    return skipped;
}

// Glossy rays stop once at most this many lanes of their wave still march (FidelityFX
// SSSR's low-occupancy exit): the last few would hold the whole wave; where they stand is
// checked like any hit.
static const uint MIN_OCCUPANCY = 4;

float3 hierarchical_march(float3 origin, float3 direction, float2 screen, uint steps, uint max_level, bool glossy, out bool valid)
{
    float3 inv_direction = float3(
        direction.x != 0.0 ? 1.0 / direction.x : 3.4e38,
        direction.y != 0.0 ? 1.0 / direction.y : 3.4e38,
        direction.z != 0.0 ? 1.0 / direction.z : 3.4e38);
    int level = 0;
    float2 resolution = screen;
    float2 resolution_inv = 1.0 / resolution;
    float2 uv_offset = 0.005 / screen;
    uv_offset = float2(direction.x < 0.0 ? -uv_offset.x : uv_offset.x, direction.y < 0.0 ? -uv_offset.y : uv_offset.y);
    float2 floor_offset = float2(direction.x < 0.0 ? 0.0 : 1.0, direction.y < 0.0 ? 0.0 : 1.0);
    float t;
    float3 position;
    initial_advance(origin, direction, inv_direction, resolution, resolution_inv, floor_offset, uv_offset, position, t);
    uint i = 0;
    bool low_occupancy = false;
    while (i < steps && level >= 0 && !low_occupancy)
    {
        low_occupancy = glossy && WaveActiveCountBits(true) <= MIN_OCCUPANCY;
        float2 current = resolution * position.xy;
        float surface = load_depth(int2(floor(current)), level);
        bool skipped = advance(origin, direction, inv_direction, current, resolution_inv, floor_offset, uv_offset, surface, position, t);
        if (!(skipped && uint(level) >= max_level))
        {
            level += skipped ? 1 : -1;
            resolution *= skipped ? 0.5 : 2.0;
            resolution_inv *= skipped ? 2.0 : 0.5;
        }
        i++;
    }
    // Below level 0: the ray met the surface (otherwise it ran out of steps, or stopped
    // with the wave nearly done).
    valid = level < 0 || low_occupancy;
    return position;
}

// A GGX visible normal around +Z for view `ve` (tangent space) and roughness `a` (Heitz
// 2018, "Sampling the GGX Distribution of Visible Normals").
float3 sample_vndf(float3 ve, float a, float2 u)
{
    float3 vh = normalize(float3(a * ve.x, a * ve.y, ve.z));
    float lensq = vh.x * vh.x + vh.y * vh.y;
    float3 t1 = lensq > 0.0 ? float3(-vh.y, vh.x, 0.0) * rsqrt(lensq) : float3(1.0, 0.0, 0.0);
    float3 t2 = cross(vh, t1);
    float r = sqrt(u.x);
    float phi = 2.0 * PI * u.y;
    float p1 = r * cos(phi);
    float p2 = r * sin(phi);
    float s = 0.5 * (1.0 + vh.z);
    p2 = (1.0 - s) * sqrt(max(1.0 - p1 * p1, 0.0)) + s * p2;
    float3 nh = p1 * t1 + p2 * t2 + sqrt(max(0.0, 1.0 - p1 * p1 - p2 * p2)) * vh;
    return normalize(float3(a * nh.x, a * nh.y, max(0.0, nh.z)));
}

uint pcg(uint v)
{
    uint state = v * 747796405u + 2891336453u;
    uint word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// Below this roughness rays follow the mirror direction (no noise to filter).
static const float MIRROR_ROUGHNESS = 0.1;
// The share of the lobe's tail left out of the samples (fewer stray rays, a little less
// spread): the visible-normal sample's first number is scaled by 1 − this.
static const float LOBE_TRIM = 0.25;

// Where trace texel `t` reads its pixel this frame: one of its 2 × 2 at half resolution,
// turning every frame.
int2 trace_pixel(ReflectFrame* f, uint2 t)
{
    uint scale = read4u(&f.ssr).y;
    if (scale == 1)
        return int2(t);
    uint k = read4u(&f.probe_info).w & 3u;
    uint2 offset = uint2(k & 1u, k >> 1);
    return int2(min(t * 2 + offset, read4u(&f.size).xy - 1));
}

// One ray per trace texel. `t0`: depth, `t1`: normal and roughness, `t2`: weight, `t3`:
// the lit color; writes the hit's color × confidence and the confidence (`target`), and
// the hit's distance (`read`).
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrTrace(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    if (any(id.xy >= read4u(&f.size).zw))
        return;
    int2 pixel = trace_pixel(f, id.xy);
    float max_roughness = read4(&f.params).y;
    float3 weight = g_textures[push.t2].Load(int3(pixel, 0)).rgb;
    float4 nr = g_textures[push.t1].Load(int3(pixel, 0));
    float roughness = nr.z;
    if (max(weight.r, max(weight.g, weight.b)) <= 0.0 || roughness >= max_roughness)
    {
        g_images[push.target][id.xy] = 0.0;
        g_scalars[push.read][id.xy] = 0.0;
        return;
    }
    float depth = g_textures[push.t0].Load(int3(pixel, 0)).r;
    float2 center = float2(pixel) + 0.5;
    float view_depth;
    float3 world = world_position(f, center, depth, view_depth);
    float3 n = decode_normal(nr.xy);
    float3 v = normalize(read4(&f.camera).xyz - world);
    float3 r = reflect(-v, n);
    if (roughness >= MIRROR_ROUGHNESS)
    {
        uint seed = pcg(uint(pixel.x) + pcg(uint(pixel.y) + pcg(read4u(&f.probe_info).w)));
        float2 u = float2(float(seed & 0xFFFFu), float(seed >> 16)) / 65536.0;
        u.x *= 1.0 - LOBE_TRIM;
        float3 t, b;
        basis(n, t, b);
        float3 ve = float3(dot(v, t), dot(v, b), dot(v, n));
        float3 h = sample_vndf(ve, roughness * roughness, u);
        h = h.x * t + h.y * b + h.z * n;
        float3 l = reflect(-v, h);
        if (dot(l, n) > 0.0)
            r = l;
    }
    // The ray in screen space: (uv, depth) is a projective image of the world, so the
    // ray stays straight there. Its second point stays in front of the near plane.
    float toward = dot(r, read4(&f.forward).xyz);
    float reach = view_depth;
    if (toward < 0.0)
        reach = min(reach, (view_depth - read4(&f.forward).w * 1.01) / -toward);
    float3 far_point = world + r * max(reach, 1e-4);
    float4 clip = mul(read44(&f.clip_from_world), float4(far_point, 1.0));
    float2 screen = float2(read4u(&f.size).xy);
    float3 origin = float3(center / screen, depth);
    float3 end = float3(clip.xy / clip.w * 0.5 + 0.5, clip.z / clip.w);
    float3 direction = end - origin;
    bool valid;
    float3 hit = hierarchical_march(origin, direction, screen, read4u(&f.ssr).x, read4u(&f.ssr).w, roughness >= MIRROR_ROUGHNESS, valid);
    float confidence = 0.0;
    float3 color = 0.0;
    float distance = 0.0;
    if (valid && all(hit.xy >= 0.0) && all(hit.xy <= 1.0))
    {
        int2 hit_pixel = clamp(int2(hit.xy * screen), int2(0, 0), int2(read4u(&f.size).xy) - 1);
        float surface = g_textures[push.t0].Load(int3(hit_pixel, 0)).r;
        // Not the sky, not right next to the origin, not the back of a surface.
        float2 moved = abs(hit.xy - origin.xy) * screen;
        float3 hit_normal = decode_normal(g_textures[push.t1].Load(int3(hit_pixel, 0)).xy);
        bool front = dot(hit_normal, r) <= 0.0 || max(g_textures[push.t2].Load(int3(hit_pixel, 0)).r, 0.0) <= 0.0;
        if (surface < 1.0 && max(moved.x, moved.y) >= 2.0 && front)
        {
            float hit_depth;
            float3 hit_world = world_position(f, hit.xy * screen, hit.z, hit_depth);
            float surface_depth = view_depth_of(f, surface);
            // Hits behind the surface by more than the thickness are behind an object.
            float behind = abs(hit_depth - surface_depth);
            float thickness = 1.0 - smoothstep(0.0, read4(&f.params).z, behind);
            float2 edge = 0.05 * float2(screen.y / screen.x, 1.0);
            float2 border = smoothstep(0.0, edge, hit.xy) * (1.0 - smoothstep(1.0 - edge, 1.0, hit.xy));
            float fade = saturate((max_roughness - roughness) / (0.2 * max_roughness));
            confidence = thickness * thickness * border.x * border.y * fade;
            color = g_textures[push.t3].SampleLevel(g_samplers[read4u(&f.env).z], hit.xy, 0.0).rgb;
            distance = length(hit_world - world);
        }
    }
    g_images[push.target][id.xy] = float4(color * confidence, confidence);
    g_scalars[push.read][id.xy] = distance;
}

// The spatial filter, at the trace's resolution: each trace texel averages the rays of
// its neighbours (up to 2 texels away, wider for rougher surfaces) with depth, normal and
// roughness weights, from a tile cached in group shared memory. `t0`: depth, `t1`: normal
// and roughness, `t3`: the trace, `t4`: its distances; writes premultiplied color and
// confidence (`target`) and the hits' mean distance with the texel's view depth (`read`).
static const float LOG2E = 1.44269504;
static const float ROUGHNESS_FALLOFF = 8.0 * LOG2E;
static const int SPATIAL_RADIUS = 2;
static const int SPATIAL_TILE = 8 + 2 * SPATIAL_RADIUS;
groupshared float4 g_value[SPATIAL_TILE * SPATIAL_TILE];
groupshared float4 g_normal_roughness[SPATIAL_TILE * SPATIAL_TILE];
groupshared float2 g_distance_depth[SPATIAL_TILE * SPATIAL_TILE];

[shader("compute")]
[numthreads(8, 8, 1)]
void ssrSpatial(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID, uint index : SV_GroupIndex)
{
    ReflectFrame* f = push.frame;
    int2 last = int2(read4u(&f.size).zw) - 1;
    int2 origin = int2(group.xy * 8) - SPATIAL_RADIUS;
    for (uint i = index; i < uint(SPATIAL_TILE * SPATIAL_TILE); i += 64)
    {
        int2 t = clamp(origin + int2(i % SPATIAL_TILE, i / SPATIAL_TILE), int2(0, 0), last);
        int2 q = trace_pixel(f, uint2(t));
        float4 nr = g_textures[push.t1].Load(int3(q, 0));
        g_value[i] = g_textures[push.t3].Load(int3(t, 0));
        g_normal_roughness[i] = float4(decode_normal(nr.xy), nr.z);
        g_distance_depth[i] = float2(g_textures[push.t4].Load(int3(t, 0)).r, view_depth_of(f, g_textures[push.t0].Load(int3(q, 0)).r));
    }
    GroupMemoryBarrierWithGroupSync();
    uint2 id = group.xy * 8 + local.xy;
    if (any(id >= read4u(&f.size).zw))
        return;
    uint center = (local.y + SPATIAL_RADIUS) * SPATIAL_TILE + local.x + SPATIAL_RADIUS;
    float4 own = g_value[center];
    float4 nr = g_normal_roughness[center];
    float depth = g_distance_depth[center].y;
    float roughness = nr.w;
    if (roughness >= read4(&f.params).y)
    {
        g_images[push.target][id] = 0.0;
        g_scalars2[push.read][id] = float2(0.0, depth);
        return;
    }
    // Mirror-like texels keep their own ray.
    int radius = roughness < MIRROR_ROUGHNESS ? 0 : (roughness < 0.3 ? 1 : SPATIAL_RADIUS);
    float depth_falloff = LOG2E / (0.02 * depth);
    float distance_falloff = 0.5 * LOG2E / max(float(radius * radius), 1.0);
    float4 sum = 0.0;
    float total = 0.0;
    float distance_sum = 0.0;
    float hits = 0.0;
    for (int y = -radius; y <= radius; y++)
    {
        for (int x = -radius; x <= radius; x++)
        {
            uint k = center + y * SPATIAL_TILE + x;
            float4 q = g_normal_roughness[k];
            float2 dd = g_distance_depth[k];
            // normal^16 × e^(−depth difference / 2 %) × e^(−8 roughness difference)
            // × the Gaussian of the distance, in one exponential.
            float c = saturate(dot(q.xyz, nr.xyz));
            c *= c;
            c *= c;
            c *= c;
            c *= c;
            float w = c * exp2(-(abs(dd.y - depth) * depth_falloff + abs(q.w - roughness) * ROUGHNESS_FALLOFF + float(x * x + y * y) * distance_falloff));
            float4 value = g_value[k];
            sum += value * w;
            total += w;
            distance_sum += dd.x * value.a * w;
            hits += value.a * w;
        }
    }
    g_images[push.target][id] = total > 1e-6 ? sum / total : own;
    g_scalars2[push.read][id] = float2(hits > 1e-6 ? distance_sum / hits : 0.0, depth);
}

// Up to the full resolution and through time: each pixel takes the bilinear neighbours
// of the filtered trace (`t3`; distances and view depths `t4`) weighted by their depth's
// likeness, then last frame's result (`t5`, sampled), reprojected through the reflected
// point (the surface pushed back along the view ray by the hit's distance) and clipped to
// the 3 × 3 trace neighbourhood's mean ± 1.25 σ, is blended in; writes `target`. `t0`:
// depth, `t1`: normal and roughness, `t2`: weight.
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrTemporal(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    if (any(id.xy >= read4u(&f.size).xy))
        return;
    int2 pixel = int2(id.xy);
    float3 weight = g_textures[push.t2].Load(int3(pixel, 0)).rgb;
    float4 nr = g_textures[push.t1].Load(int3(pixel, 0));
    float roughness = nr.z;
    if (max(weight.r, max(weight.g, weight.b)) <= 0.0 || roughness >= read4(&f.params).y)
    {
        g_images[push.target][id.xy] = 0.0;
        return;
    }
    float depth = g_textures[push.t0].Load(int3(pixel, 0)).r;
    float view_depth;
    float3 world = world_position(f, float2(pixel) + 0.5, depth, view_depth);
    Texture2D current = g_textures[push.t3];
    Texture2D distances = g_textures[push.t4];
    int2 last = int2(read4u(&f.size).zw) - 1;
    float scale = float(read4u(&f.ssr).y);
    float2 tc = (float2(pixel) + 0.5) / scale - 0.5;
    int2 t0 = int2(floor(tc));
    float2 fraction = tc - float2(t0);
    float4 value = 0.0;
    float distance = 0.0;
    float total = 0.0;
    for (uint k = 0; k < 4; k++)
    {
        int2 o = int2(k & 1u, k >> 1);
        int2 t = clamp(t0 + o, int2(0, 0), last);
        float2 dd = distances.Load(int3(t, 0)).rg;
        float2 b = lerp(1.0 - fraction, fraction, float2(o));
        float w = b.x * b.y * exp(-abs(dd.y - view_depth) / (0.02 * view_depth)) + 1e-5;
        value += current.Load(int3(t, 0)) * w;
        distance += dd.x * w;
        total += w;
    }
    value /= total;
    distance /= total;
    float4 result = value;
    if (read4u(&f.ssr).z != 0)
    {
        float3 virtual_point = world + normalize(world - read4(&f.camera).xyz) * distance;
        float4 previous = mul(read44(&f.previous_clip_from_world), float4(virtual_point, 1.0));
        float2 uv = previous.xy / previous.w * 0.5 + 0.5;
        if (previous.w > 1e-6 && all(uv >= 0.0) && all(uv <= 1.0))
        {
            // The neighbourhood's spread: history outside it is clipped (no ghosts).
            int2 tn = clamp(int2(round(tc)), int2(0, 0), last);
            float4 m1 = 0.0;
            float4 m2 = 0.0;
            for (int y = -1; y <= 1; y++)
            {
                for (int x = -1; x <= 1; x++)
                {
                    float4 s = current.Load(int3(clamp(tn + int2(x, y), int2(0, 0), last), 0));
                    m1 += s;
                    m2 += s * s;
                }
            }
            m1 /= 9.0;
            float4 sigma = sqrt(max(m2 / 9.0 - m1 * m1, 0.0));
            float4 history = g_textures[push.t5].SampleLevel(g_samplers[read4u(&f.env).z], uv, 0.0);
            history = clamp(history, m1 - 1.25 * sigma, m1 + 1.25 * sigma);
            // Mirror rays carry no noise: follow the current frame more closely.
            float blend = lerp(0.5, read4(&f.params).w, saturate(roughness / (2.0 * MIRROR_ROUGHNESS)));
            result = lerp(history, value, blend);
        }
    }
    g_images[push.target][id.xy] = result;
}
