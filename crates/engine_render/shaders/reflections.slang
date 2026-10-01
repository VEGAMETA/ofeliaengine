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
// - `probeTiles` sets, per 16 × 16 tile, one bit per visible probe whose screen
//   rectangle covers it; the composite walks the bits of its tile, wave by wave.
// - Screen-space reflections: `ssrDepth` reduces the depth into a pyramid of nearest
//   depths; `ssrTrace` (at full or half resolution) samples a reflected direction from
//   the visible normals of the GGX lobe (Heitz 2018; mirror rays below a roughness) and
//   walks the pyramid (the hierarchical traversal of AMD's FidelityFX SSSR, MIT, see
//   THIRD_PARTY_NOTICES.md), reading the frame's lit color at the hit; `ssrSpatial`
//   averages neighbouring rays with depth and normal weights; `ssrTemporal` blends with
//   last frame's result reprojected through the reflected point, clamped to the current
//   neighbourhood. Values are premultiplied by their confidence (fading at the screen's
//   edges, behind thin objects, at the roughness cutoff), so the composite adds the hit
//   over the probes and the sky by it.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(0, 0)]] TextureCubeArray g_cube_arrays[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2DArray<float4> g_layers[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(1, 0)]] [vk::image_format("r16f")] RWTexture2D<float> g_scalars[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const float PI = 3.14159265;
static const uint NO_TEXTURE = 0xFFFFFFFFu;
// `PROBE_TILE` in reflections.rs.
static const uint PROBE_TILE = 16;

// `ProbeData` in reflections.rs (144 bytes).
struct Probe
{
    float4 influence[3];     // rows: world → influence box, scaled to [-1, 1]
    float4 extents;          // xyz: influence half extents; w: 1 / blend distance
    float4 parallax[3];      // rows: world → parallax box (rotation and translation)
    float4 parallax_extents; // xyz: parallax half extents; w: cube index in the array
    float4 position;         // xyz: capture point; w: intensity
};

// `ReflectFrame` in reflections.rs.
struct ReflectFrame
{
    float4x4 clip_from_world;          // as the depth was drawn (with any jitter)
    float4x4 previous_clip_from_world; // the SSR history's
    float4 camera;                     // xyz: position
    float4 forward;                    // xyz: view direction; w: near plane distance
    float4 ray_base;                   // xyz: ray to view depth 1 at ndc 0; w: p22
    float4 ray_x;                      // xyz: per ndc.x; w: p32 (view depth = p32 / (depth + p22))
    float4 ray_y;                      // xyz: per ndc.y
    uint4 size;                        // xy: target; zw: SSR trace
    uint4 env;                         // x: prefiltered sky cube, y: its mips, z: linear sampler, w: probe array
    float4 params;                     // x: sky intensity; y: SSR max roughness; z: SSR thickness; w: SSR temporal blend
    uint4 probe_info;                  // x: visible probes, y: words per tile, z: tiles per row, w: frame
    Probe* probes;
    uint* _unused;
    uint* probe_rects;                 // per probe: min x | min y << 16, max x | max y << 16 (tiles)
    uint _pad0;
    uint _pad1;
    uint4 ssr;                         // x: steps, y: trace scale, z: history valid, w: pyramid levels
    uint4 pyramid_offsets[4];          // offset of level k (1-16) in floats, at [k - 1]
};

struct ReflectPush
{
    ReflectFrame* frame;
    uint* buffer;            // the probes' tile masks (`probeTiles`, the composite), or the SSR's
                             // nearest depths (float bits, levels 1.. one after the other)
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

// A unit vector from octahedral encoding in [0, 1]² (forward.slang's `encode_octahedral`).
float3 decode_normal(float2 e)
{
    e = e * 2.0 - 1.0;
    float3 v = float3(e, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-v.z, 0.0);
    v.x += v.x >= 0.0 ? -t : t;
    v.y += v.y >= 0.0 ? -t : t;
    return normalize(v);
}

// The world position at pixel `pixel` (centre) with depth-buffer value `depth`, and the
// view depth.
float3 world_position(ReflectFrame* f, float2 pixel, float depth, out float view_depth)
{
    float2 ndc = pixel / float2(f.size.xy) * 2.0 - 1.0;
    float3 ray = f.ray_base.xyz + ndc.x * f.ray_x.xyz + ndc.y * f.ray_y.xyz;
    view_depth = f.ray_x.w / (depth + f.ray_base.w);
    return f.camera.xyz + ray * view_depth;
}

float view_depth_of(ReflectFrame* f, float depth)
{
    return f.ray_x.w / (depth + f.ray_base.w);
}

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

// --- Probe selection ---------------------------------------------------------------

// One thread per tile: one bit per visible probe whose screen rectangle covers it.
[shader("compute")]
[numthreads(8, 8, 1)]
void probeTiles(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    uint tiles_x = f.probe_info.z;
    uint tiles_y = push.size;
    if (id.x >= tiles_x || id.y >= tiles_y)
        return;
    uint count = f.probe_info.x;
    uint words = f.probe_info.y;
    uint base = (id.y * tiles_x + id.x) * words;
    for (uint w = 0; w < words; w++)
    {
        uint mask = 0;
        for (uint bit = 0; bit < 32; bit++)
        {
            uint p = w * 32 + bit;
            if (p >= count)
                break;
            uint lo = f.probe_rects[p * 2];
            uint hi = f.probe_rects[p * 2 + 1];
            bool inside = id.x >= (lo & 0xFFFFu) && id.y >= (lo >> 16) && id.x <= (hi & 0xFFFFu) && id.y <= (hi >> 16);
            mask |= inside ? 1u << bit : 0u;
        }
        push.buffer[base + w] = mask;
    }
}

// How much probe `p` covers `world`: 1 inside its influence box less the blend
// distance, falling to 0 at the box.
float probe_influence(Probe* p, float3 world)
{
    float3 local = float3(dot(p.influence[0].xyz, world) + p.influence[0].w, dot(p.influence[1].xyz, world) + p.influence[1].w, dot(p.influence[2].xyz, world) + p.influence[2].w);
    float3 inside = (1.0 - abs(local)) * p.extents.xyz;
    return saturate(min(inside.x, min(inside.y, inside.z)) * p.extents.w);
}

// Where the ray from `world` along `r` leaves probe `p`'s parallax box, as a direction from
// its capture point (Lagarde and Zanuttini 2012). From outside the box, `r` itself.
float3 probe_direction(Probe* p, float3 world, float3 r)
{
    float3 o = float3(dot(p.parallax[0].xyz, world) + p.parallax[0].w, dot(p.parallax[1].xyz, world) + p.parallax[1].w, dot(p.parallax[2].xyz, world) + p.parallax[2].w);
    float3 d = float3(dot(p.parallax[0].xyz, r), dot(p.parallax[1].xyz, r), dot(p.parallax[2].xyz, r));
    float3 e = p.parallax_extents.xyz;
    if (any(abs(o) > e))
        return r;
    float3 inv = 1.0 / (abs(d) > 1e-8 ? d : 1e-8);
    float3 t = max((e - o) * inv, (-e - o) * inv);
    float distance = min(t.x, min(t.y, t.z));
    return world + r * distance - p.position.xyz;
}

// The probes' reflection along `r` at `world` (pixel `pixel`), the rest of the weight from
// `sky`: front to back in priority order (smallest probes first).
float3 probe_radiance(ReflectFrame* f, float3 world, float3 r, float lod, uint2 pixel, float3 sky)
{
    uint count = f.probe_info.x;
    if (count == 0)
        return sky;
    uint words = f.probe_info.y;
    uint2 tile = pixel / PROBE_TILE;
    uint base = (tile.y * f.probe_info.z + tile.x) * words;
    TextureCubeArray cubes = g_cube_arrays[f.env.w];
    SamplerState s = g_samplers[f.env.z];
    float3 sum = 0.0;
    float covered = 0.0;
    for (uint w = 0; w < words; w++)
    {
        uint mask = push.buffer[base + w];
        uint merged = WaveActiveBitOr(mask);
        while (merged != 0)
        {
            uint bit = firstbitlow(merged);
            merged &= merged - 1u;
            if (((mask >> bit) & 1u) != 0 && covered < 0.999)
            {
                Probe* p = f.probes + (w * 32 + bit);
                float influence = probe_influence(p, world);
                if (influence > 0.0)
                {
                    float3 d = probe_direction(p, world, r);
                    float3 c = cubes.SampleLevel(s, float4(d, p.parallax_extents.w), lod).rgb * p.position.w;
                    float share = (1.0 - covered) * influence;
                    sum += share * c;
                    covered += share;
                }
            }
        }
    }
    return sum + (1.0 - covered) * sky;
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

// Added to the opaque image (additive blending): weight × (reflection − sky), the
// reflection being the screen-space hit over the probes over the sky. `t0`: depth, `t1`:
// normal and roughness, `t2`: weight, `t3`: SSR (premultiplied by confidence) or none.
[shader("fragment")]
float4 compositeFragment(Composite input) : SV_Target0
{
    ReflectFrame* f = push.frame;
    int2 pixel = int2(input.position.xy);
    float3 weight = g_textures[push.t2].Load(int3(pixel, 0)).rgb;
    // Sky, subsurface and transparent-free pixels: nothing written there.
    if (max(weight.r, max(weight.g, weight.b)) <= 0.0)
        discard;
    float depth = g_textures[push.t0].Load(int3(pixel, 0)).r;
    float4 nr = g_textures[push.t1].Load(int3(pixel, 0));
    float3 n = decode_normal(nr.xy);
    float roughness = nr.z;
    float view_depth;
    float3 world = world_position(f, input.position.xy, depth, view_depth);
    float3 v = normalize(f.camera.xyz - world);
    float3 r = reflect(-v, n);
    float lod = roughness * float(f.env.y - 1);
    SamplerState s = g_samplers[f.env.z];
    float3 sky = g_cubes[f.env.x].SampleLevel(s, r, lod).rgb * f.params.x;
    float3 reflection = probe_radiance(f, world, r, lod, uint2(pixel), sky);
    if (push.t3 != NO_TEXTURE)
    {
        float4 hit = g_textures[push.t3].Load(int3(pixel, 0));
        reflection = reflection * (1.0 - hit.a) + hit.rgb;
    }
    float3 delta = weight * (reflection - sky);
    if (max(abs(delta.r), max(abs(delta.g), abs(delta.b))) <= 1e-7)
        discard;
    return float4(delta, 0.0);
}

// --- Screen-space reflections -------------------------------------------------------

uint2 level_size(uint level)
{
    return ((push.frame.size.xy - 1) >> level) + 1;
}

uint pyramid_offset(uint level)
{
    return push.frame.pyramid_offsets[(level - 1) >> 2][(level - 1) & 3];
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
        uint o = pyramid_offset(level - 1);
        d = min(min(asfloat(push.buffer[o + p.y * below.x + p.x]), asfloat(push.buffer[o + p.y * below.x + q.x])),
                min(asfloat(push.buffer[o + q.y * below.x + p.x]), asfloat(push.buffer[o + q.y * below.x + q.x])));
    }
    push.buffer[pyramid_offset(level) + id.y * size.x + id.x] = asuint(d);
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

float3 hierarchical_march(float3 origin, float3 direction, float2 screen, uint steps, uint max_level, out bool valid)
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
    while (i < steps && level >= 0)
    {
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
    // Below level 0: the ray met the surface (otherwise it ran out of steps).
    valid = level < 0;
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
    uint scale = f.ssr.y;
    if (scale == 1)
        return int2(t);
    uint k = f.probe_info.w & 3u;
    uint2 offset = uint2(k & 1u, k >> 1);
    return int2(min(t * 2 + offset, f.size.xy - 1));
}

// One ray per trace texel. `t0`: depth, `t1`: normal and roughness, `t2`: weight, `t3`:
// the lit color; writes the hit's color × confidence and the confidence (`target`), and
// the hit's distance (`read`).
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrTrace(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    if (any(id.xy >= f.size.zw))
        return;
    int2 pixel = trace_pixel(f, id.xy);
    float max_roughness = f.params.y;
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
    float3 v = normalize(f.camera.xyz - world);
    float3 r = reflect(-v, n);
    if (roughness >= MIRROR_ROUGHNESS)
    {
        uint seed = pcg(uint(pixel.x) + pcg(uint(pixel.y) + pcg(f.probe_info.w)));
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
    float toward = dot(r, f.forward.xyz);
    float reach = view_depth;
    if (toward < 0.0)
        reach = min(reach, (view_depth - f.forward.w * 1.01) / -toward);
    float3 far_point = world + r * max(reach, 1e-4);
    float4 clip = mul(f.clip_from_world, float4(far_point, 1.0));
    float2 screen = float2(f.size.xy);
    float3 origin = float3(center / screen, depth);
    float3 end = float3(clip.xy / clip.w * 0.5 + 0.5, clip.z / clip.w);
    float3 direction = end - origin;
    bool valid;
    float3 hit = hierarchical_march(origin, direction, screen, f.ssr.x, f.ssr.w, valid);
    float confidence = 0.0;
    float3 color = 0.0;
    float distance = 0.0;
    if (valid && all(hit.xy >= 0.0) && all(hit.xy <= 1.0))
    {
        int2 hit_pixel = clamp(int2(hit.xy * screen), int2(0, 0), int2(f.size.xy) - 1);
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
            float thickness = 1.0 - smoothstep(0.0, f.params.z, behind);
            float2 edge = 0.05 * float2(screen.y / screen.x, 1.0);
            float2 border = smoothstep(0.0, edge, hit.xy) * (1.0 - smoothstep(1.0 - edge, 1.0, hit.xy));
            float fade = saturate((max_roughness - roughness) / (0.2 * max_roughness));
            confidence = thickness * thickness * border.x * border.y * fade;
            color = g_textures[push.t3].SampleLevel(g_samplers[f.env.z], hit.xy, 0.0).rgb;
            distance = length(hit_world - world);
        }
    }
    g_images[push.target][id.xy] = float4(color * confidence, confidence);
    g_scalars[push.read][id.xy] = distance;
}

// The rays around each pixel, averaged with depth, normal and roughness weights (`t0`:
// depth, `t1`: normal and roughness, `t2`: weight, `t3`: the trace, `t4`: its distances);
// writes premultiplied color and confidence (`target`) and the hits' mean distance
// (`read`). Mirror-like pixels use their nearest ray only.
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrSpatial(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    if (any(id.xy >= f.size.xy))
        return;
    int2 pixel = int2(id.xy);
    float3 weight = g_textures[push.t2].Load(int3(pixel, 0)).rgb;
    float4 nr = g_textures[push.t1].Load(int3(pixel, 0));
    float roughness = nr.z;
    if (max(weight.r, max(weight.g, weight.b)) <= 0.0 || roughness >= f.params.y)
    {
        g_images[push.target][id.xy] = 0.0;
        g_scalars[push.read][id.xy] = 0.0;
        return;
    }
    float3 n = decode_normal(nr.xy);
    float depth = view_depth_of(f, g_textures[push.t0].Load(int3(pixel, 0)).r);
    uint scale = f.ssr.y;
    int2 center = pixel / int(scale);
    int2 last = int2(f.size.zw) - 1;
    // Wider for rougher surfaces: up to 2 trace texels away.
    int radius = roughness < MIRROR_ROUGHNESS ? (scale == 1 ? 0 : 1) : (roughness < 0.3 ? 1 : 2);
    float4 sum = 0.0;
    float total = 0.0;
    float distance_sum = 0.0;
    float hits = 0.0;
    for (int y = -radius; y <= radius; y++)
    {
        for (int x = -radius; x <= radius; x++)
        {
            int2 t = clamp(center + int2(x, y), int2(0, 0), last);
            int2 q = trace_pixel(f, uint2(t));
            float4 qnr = g_textures[push.t1].Load(int3(q, 0));
            float qdepth = view_depth_of(f, g_textures[push.t0].Load(int3(q, 0)).r);
            float w = pow(saturate(dot(decode_normal(qnr.xy), n)), 16.0)
                * exp(-abs(qdepth - depth) / (0.02 * depth))
                * exp(-abs(qnr.z - roughness) * 8.0)
                * exp(-0.5 * float(x * x + y * y) / max(float(radius * radius), 1.0));
            float4 value = g_textures[push.t3].Load(int3(t, 0));
            sum += value * w;
            total += w;
            float hit_weight = value.a * w;
            distance_sum += g_textures[push.t4].Load(int3(t, 0)).r * hit_weight;
            hits += hit_weight;
        }
    }
    g_images[push.target][id.xy] = total > 1e-6 ? sum / total : 0.0;
    g_scalars[push.read][id.xy] = hits > 1e-6 ? distance_sum / hits : 0.0;
}

// Blends the spatial result (`t3`, distances `t4`) with last frame's (`t5`, sampled),
// reprojected through the reflected point (the surface pushed back along the view ray by
// the hit's distance) and clamped to the 3 × 3 neighbourhood's range; writes `target`.
// `t0`: depth, `t1`: normal and roughness.
[shader("compute")]
[numthreads(8, 8, 1)]
void ssrTemporal(uint3 id : SV_DispatchThreadID)
{
    ReflectFrame* f = push.frame;
    if (any(id.xy >= f.size.xy))
        return;
    int2 pixel = int2(id.xy);
    Texture2D current = g_textures[push.t3];
    float4 value = current.Load(int3(pixel, 0));
    float4 lo = value;
    float4 hi = value;
    int2 last = int2(f.size.xy) - 1;
    for (int y = -1; y <= 1; y++)
    {
        for (int x = -1; x <= 1; x++)
        {
            float4 s = current.Load(int3(clamp(pixel + int2(x, y), int2(0, 0), last), 0));
            lo = min(lo, s);
            hi = max(hi, s);
        }
    }
    float4 result = value;
    if (f.ssr.z != 0)
    {
        float depth = g_textures[push.t0].Load(int3(pixel, 0)).r;
        float view_depth;
        float3 world = world_position(f, float2(pixel) + 0.5, depth, view_depth);
        float distance = g_textures[push.t4].Load(int3(pixel, 0)).r;
        float3 virtual_point = world + normalize(world - f.camera.xyz) * distance;
        float4 previous = mul(f.previous_clip_from_world, float4(virtual_point, 1.0));
        float2 uv = previous.xy / previous.w * 0.5 + 0.5;
        if (depth < 1.0 && previous.w > 1e-6 && all(uv >= 0.0) && all(uv <= 1.0))
        {
            float4 history = g_textures[push.t5].SampleLevel(g_samplers[f.env.z], uv, 0.0);
            history = clamp(history, lo, hi);
            float roughness = g_textures[push.t1].Load(int3(pixel, 0)).z;
            // Mirror rays carry no noise: follow the current frame more closely.
            float blend = lerp(0.5, f.params.w, saturate(roughness / (2.0 * MIRROR_ROUGHNESS)));
            result = lerp(history, value, blend);
        }
    }
    g_images[push.target][id.xy] = result;
}
