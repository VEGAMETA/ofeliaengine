// Global illumination from a volume of probes traced with ray queries (ADR 0088, phase 1
// of ADR 0084): a grid of probes, each holding its irradiance over all directions in a
// small octahedral map (6 × 6 texels inside a border of one) and the mean and mean square
// of its rays' hit distances in another (14 × 14), as in Majercik et al., "Dynamic Diffuse
// Global Illumination with Ray-Traced Irradiance Fields" (JCGT 2019) and "Scaling
// Probe-Based Real-Time Dynamic Global Illumination for Production" (JCGT 2021).
//
// - `giTrace`: a ray per thread from a probe (spherical Fibonacci directions turned by
//   each update's rotation) against the scene's top level (`rays.slang`); a front-face
//   hit is shaded with its material's albedo and emission (at a coarse mip), the sun with
//   a shadow ray and, from the second update on, the volume's own irradiance at the hit
//   (the next bounce); a miss sees the sky; a back-face hit (the probe is inside
//   something) keeps its distance, negated, unless the material is double-sided (a
//   leaf, a sheet: shaded as a front face).
// - `giIrradiance` and `giMoments`: one workgroup per probe blends its rays into its two
//   maps (cosine-weighted radiance; distances under a sharp lobe), then fills the border
//   from the opposite edges so bilinear filtering wraps across the octahedron's seams.
// - `giRelocate`: moves a probe out of geometry, or away from a surface it nearly touches,
//   within its cell, and turns off one that stays inside.
// - `giResolve`: a pixel's diffuse irradiance from the eight probes around its surface
//   (the depth's position and normal), weighted trilinearly, by facing and by a Chebyshev
//   test against the distances (no light through walls); the sky's harmonics outside the
//   volume, blended over its last cell.
// - `giSample`: the same at given points (tests).
//
// No code was copied from any implementation; see docs/research/global-illumination.md.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_irradiance_images[];
[[vk::binding(1, 0)]] [vk::image_format("rg16f")] RWTexture2D<float2> g_moment_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const float PI = 3.14159265;
static const uint NO_TEXTURE = 0xFFFFFFFFu;
// Texels per probe in the two atlases, border included, and inside it.
static const uint IRRADIANCE_TILE = 8;
static const uint IRRADIANCE_INNER = 6;
static const uint MOMENTS_TILE = 16;
static const uint MOMENTS_INNER = 14;
// The most rays a probe casts in one update (the blend passes stage them in shared memory).
static const uint MAX_RAYS = 256;
// A miss's distance: beyond any probe's reach.
static const float MISS_DISTANCE = 1e4;
// Probe states (`w` of a probe's record; a zeroed record is a probe on, at rest).
static const float PROBE_ON = 0.0;
static const float PROBE_OFF = 1.0;

// `GiVolumeData` in gi.rs (96 bytes).
struct GiVolume
{
    float4 origin;           // xyz: the rest position of probe (0, 0, 0); w: the distance probes' moments reach
    float4 spacing;          // xyz: between neighbouring probes; w: the smallest of them
    float4 inv_spacing;      // xyz: 1 / spacing; w: the normal bias (world units)
    uint4 counts;            // xyz: probes along each axis; w: all of them
    float4 atlas;            // xy: 1 / the irradiance atlas's size; zw: 1 / the moments atlas's
    uint irradiance;         // bindless: the irradiance atlas (sampled)
    uint moments;            // bindless: the moments atlas (sampled)
    uint sampler;            // bilinear, clamp
    float view_bias;         // world units
};

// `Instance` in forward.slang (64 bytes).
struct Instance
{
    float4 rows[3];          // world_from_object: xyz linear part, w translation
    uint material;
    uint batch;
    uint2 _pad;
};

// `BatchRay` in rays.rs (32 bytes): the batch's bottom level and, for shading hits, where
// its mesh's vertices start, the first index of the level the structure holds and
// whether its material is double-sided.
struct Batch
{
    uint2 blas;
    uint mask;
    uint flags;
    uint first_vertex;
    uint first_index;
    uint double_sided;
    uint _pad;
};

// `Material` in forward.slang (80 bytes); texture fields: bindless index in the low 24
// bits, sampler index in the top 8.
struct Material
{
    float4 base_color;
    float3 emissive;
    float normal_scale;
    float metallic;
    float roughness;
    float occlusion_strength;
    float alpha_cutoff;
    uint base_color_texture;
    uint metallic_roughness_texture;
    uint normal_texture;
    uint occlusion_texture;
    uint emissive_texture;
    uint subsurface;
    uint thickness_texture;
    float thickness;
};

// --- Probes and their maps ----------------------------------------------------------

// A unit vector in octahedral coordinates, [-1, 1]² (Cigolle et al. 2014).
float2 oct_encode(float3 d)
{
    d /= abs(d.x) + abs(d.y) + abs(d.z);
    float2 o = d.xy;
    if (d.z < 0.0)
        o = (1.0 - abs(o.yx)) * float2(o.x >= 0.0 ? 1.0 : -1.0, o.y >= 0.0 ? 1.0 : -1.0);
    return o;
}

float3 oct_decode(float2 o)
{
    float3 d = float3(o, 1.0 - abs(o.x) - abs(o.y));
    float t = max(-d.z, 0.0);
    d.x += d.x >= 0.0 ? -t : t;
    d.y += d.y >= 0.0 ? -t : t;
    return normalize(d);
}

uint probe_index(GiVolume v, uint3 c)
{
    return c.x + v.counts.x * (c.y + v.counts.y * c.z);
}

uint3 probe_coords(GiVolume v, uint index)
{
    uint plane = v.counts.x * v.counts.y;
    return uint3(index % v.counts.x, (index % plane) / v.counts.x, index / plane);
}

// The texel of probe `c`'s tile corner in an atlas whose tiles are `tile` texels wide:
// tiles go along x by the probes' x then y, along y by their z.
uint2 tile_corner(GiVolume v, uint3 c, uint tile)
{
    return uint2(c.x + v.counts.x * c.y, c.z) * tile;
}

float3 rest_position(GiVolume v, uint3 c)
{
    return v.origin.xyz + float3(c) * v.spacing.xyz;
}

// Where a direction falls in a probe's tile (texels from the atlas's corner).
float2 tile_texel(uint2 corner, uint inner, float3 d)
{
    return float2(corner) + 1.0 + (oct_encode(d) * 0.5 + 0.5) * float(inner);
}

// Spherical Fibonacci point `i` of `n`: directions spread evenly over the sphere.
float3 fibonacci(uint i, uint n)
{
    float phi = 2.0 * PI * frac(float(i) * 0.618034);
    float cos_theta = 1.0 - (2.0 * float(i) + 1.0) / float(n);
    float sin_theta = sqrt(saturate(1.0 - cos_theta * cos_theta));
    return float3(cos(phi) * sin_theta, sin(phi) * sin_theta, cos_theta);
}

// The diffuse irradiance at surface point `p` with normal `n` seen along `to_eye` (unit,
// from the point; or the normal for points nobody looks at) from the eight probes around
// it. `covered` is 1 inside the volume, falling to 0 over one cell beyond its last
// probes, and 0 where no probe sees the point.
float3 volume_irradiance(GiVolume v, float4* probes, float3 p, float3 n, float3 to_eye, out float covered)
{
    // The point the probes are looked up from: off the surface, so its own distance
    // to a probe is not the surface's (DDGI's normal and view bias).
    float3 q = p + n * v.inv_spacing.w + to_eye * v.view_bias;
    float3 grid = (q - v.origin.xyz) * v.inv_spacing.xyz;
    float3 last = float3(v.counts.xyz - 1);
    float3 outside = max(-grid, 0.0) + max(grid - last, 0.0);
    covered = saturate(1.0 - max(outside.x, max(outside.y, outside.z)));
    int3 base = clamp(int3(floor(grid)), int3(0, 0, 0), int3(v.counts.xyz) - 2);
    float3 t = saturate(grid - float3(base));
    float3 sum = 0.0;
    float total = 0.0;
    SamplerState s = g_samplers[v.sampler];
    for (uint i = 0; i < 8; i++)
    {
        uint3 offset = uint3(i, i >> 1, i >> 2) & 1u;
        uint3 c = uint3(base) + offset;
        uint index = probe_index(v, c);
        float4 probe = loadAligned<16>(probes + index);
        if (probe.w == PROBE_OFF)
            continue;
        float3 at = rest_position(v, c) + probe.xyz;
        // Facing: probes behind the surface count less, never nothing (a wrap, so small
        // details whose normals see no probe still get light).
        float3 to_probe = normalize(at - p);
        float facing = (dot(to_probe, n) + 1.0) * 0.5;
        float weight = facing * facing + 0.2;
        // Visibility: Chebyshev's bound on the chance the probe's rays reach this far.
        float3 from_probe = q - at;
        float distance = length(from_probe);
        float2 m = g_textures[v.moments].SampleLevel(s, tile_texel(tile_corner(v, c, MOMENTS_TILE), MOMENTS_INNER, from_probe / max(distance, 1e-6)) * v.atlas.zw, 0.0).xy;
        if (distance > m.x)
        {
            float variance = abs(m.y - m.x * m.x);
            float d = distance - m.x;
            float chebyshev = variance / (variance + d * d);
            weight *= max(chebyshev * chebyshev * chebyshev, 0.0);
        }
        weight = max(weight, 1e-6);
        // Faint weights fall faster: a probe barely visible lends almost no light.
        if (weight < 0.2)
            weight *= weight * weight * 25.0;
        float3 tri = lerp(1.0 - t, t, float3(offset));
        weight *= tri.x * tri.y * tri.z;
        float3 e = g_textures[v.irradiance].SampleLevel(s, tile_texel(tile_corner(v, c, IRRADIANCE_TILE), IRRADIANCE_INNER, n) * v.atlas.xy, 0.0).rgb;
        sum += e * weight;
        total += weight;
    }
    if (total <= 0.0)
    {
        covered = 0.0;
        return 0.0;
    }
    return sum / total;
}

// --- Tracing ------------------------------------------------------------------------

// `TraceFrame` in gi.rs (224 bytes), in the upload ring.
struct TraceFrame
{
    GiVolume volume;
    float4 rotation[3];      // rows of this update's turn of the ray directions
    float4 sun_direction;    // xyz: towards the sun; w: 1 with a sun, 0 without
    float4 sun_color;        // rgb: irradiance on a surface facing it
    float4 sky;              // rgb: the flat sky's radiance (no cube); w: the cube's intensity
    uint2 accel;             // the top level's address
    uint sky_cube;           // bindless radiance cube, or NO_TEXTURE for the flat sky
    uint rays;               // per probe
    uint mask;               // the rays' instance mask
    uint bounce;             // 1: hits add the volume's irradiance (another bounce)
    uint linear_sampler;     // for the sky cube
    uint _pad0;
};

// `TracePush` in gi.rs (72 bytes).
struct TracePush
{
    TraceFrame* frame;
    float4* probes;          // per probe: xyz its offset from its rest position, w PROBE_ON or PROBE_OFF
    uint2* results;          // per ray: radiance (rgb) and distance as four halves
    Instance* instances;     // the GPU scene's, by slot
    Batch* batches;          // the ray scene's batch table
    float* positions;        // the geometry pool's positions (3 floats a vertex; unused)
    uint4* attributes;       // its other attributes (16 bytes a vertex)
    uint* indices;           // its indices (per mesh, from its first vertex)
    Material* materials;     // the material table
};

[[vk::push_constant]]
ConstantBuffer<TracePush> trace_push;

RaytracingAccelerationStructure accel_at(uint2 address)
{
    return spirv_asm {
        OpExtension "SPV_KHR_ray_query";
        OpCapability RayQueryKHR;
        result: $$RaytracingAccelerationStructure = OpConvertUToAccelerationStructureKHR $address;
    };
}

// A unit vector from two snorm16 in octahedral encoding (`decode_octahedral` in
// forward.slang).
float3 decode_octahedral(uint bits)
{
    float2 e = max(float2(int2(bits << 16, bits) >> 16) / 32767.0, -1.0);
    float3 v = float3(e, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-v.z, 0.0);
    v.x += v.x >= 0.0 ? -t : t;
    v.y += v.y >= 0.0 ? -t : t;
    return normalize(v);
}

// The inverse transpose of an instance's linear part up to a positive factor
// (`transform_normal` in forward.slang).
float3 transform_normal(Instance i, float3 n)
{
    float3 bc = cross(i.rows[1].xyz, i.rows[2].xyz);
    float3 ca = cross(i.rows[2].xyz, i.rows[0].xyz);
    float3 ab = cross(i.rows[0].xyz, i.rows[1].xyz);
    float mirror = dot(i.rows[0].xyz, bc) < 0.0 ? -1.0 : 1.0;
    return mirror * float3(dot(bc, n), dot(ca, n), dot(ab, n));
}

float4 sample_material(uint texture, float2 uv, float lod)
{
    if (texture == NO_TEXTURE)
        return 1.0;
    return g_textures[NonUniformResourceIndex(texture & 0xFFFFFFu)].SampleLevel(g_samplers[NonUniformResourceIndex(texture >> 24)], uv, lod);
}

float3 sky_radiance(float3 d)
{
    TraceFrame* f = trace_push.frame;
    uint cube = f.sky_cube;
    if (cube == NO_TEXTURE)
        return loadAligned<16>(&f.sky).rgb;
    return g_cubes[cube].SampleLevel(g_samplers[f.linear_sampler], d, 0.0).rgb * f.sky.w;
}

// The coarse mip materials are read at: probes see surfaces from afar, and a mip of a few
// dozen texels keeps the reads cheap.
static const float HIT_LOD = 5.0;

// Radiance leaving a front-face hit towards the probe.
float3 shade_hit(float3 p, float3 d, uint slot, uint primitive, float2 bary)
{
    TraceFrame* f = trace_push.frame;
    Instance inst = loadAligned<16>(trace_push.instances + slot);
    Batch b = loadAligned<16>(trace_push.batches + inst.batch);
    uint first = b.first_index + primitive * 3;
    uint3 tri = uint3(trace_push.indices[first], trace_push.indices[first + 1], trace_push.indices[first + 2]) + b.first_vertex;
    uint4 a0 = loadAligned<16>(trace_push.attributes + tri.x);
    uint4 a1 = loadAligned<16>(trace_push.attributes + tri.y);
    uint4 a2 = loadAligned<16>(trace_push.attributes + tri.z);
    float3 w = float3(1.0 - bary.x - bary.y, bary.x, bary.y);
    float3 object_normal = decode_octahedral(a0.x) * w.x + decode_octahedral(a1.x) * w.y + decode_octahedral(a2.x) * w.z;
    float3 n = normalize(transform_normal(inst, object_normal));
    // The side the ray came from.
    if (dot(n, d) > 0.0)
        n = -n;
    float2 uv = asfloat(a0.zw) * w.x + asfloat(a1.zw) * w.y + asfloat(a2.zw) * w.z;
    Material m = loadAligned<16>(trace_push.materials + inst.material);
    float3 albedo = m.base_color.rgb * sample_material(m.base_color_texture, uv, HIT_LOD).rgb;
    float3 emissive = m.emissive * sample_material(m.emissive_texture, uv, HIT_LOD).rgb;
    // glTF's metalness: the factor (1 unless set) times the texture's blue channel.
    float metallic = saturate(m.metallic * sample_material(m.metallic_roughness_texture, uv, HIT_LOD).b);
    float3 diffuse = albedo * (1.0 - metallic) / PI;
    float3 irradiance = 0.0;
    float4 sun = loadAligned<16>(&f.sun_direction);
    float n_dot_l = dot(n, sun.xyz);
    if (sun.w > 0.0 && n_dot_l > 0.0)
    {
        RayDesc shadow;
        shadow.Origin = p + n * 1e-3;
        shadow.TMin = 1e-3;
        shadow.Direction = sun.xyz;
        shadow.TMax = 1e30;
        RayQuery<RAY_FLAG_FORCE_OPAQUE | RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH> q;
        q.TraceRayInline(accel_at(f.accel), 0, f.mask, shadow);
        while (q.Proceed())
        {
        }
        if (q.CommittedStatus() != COMMITTED_TRIANGLE_HIT)
            irradiance += loadAligned<16>(&f.sun_color).rgb * n_dot_l;
    }
    if (f.bounce != 0)
    {
        GiVolume v = loadAligned<16>(&f.volume);
        float covered;
        float3 e = volume_irradiance(v, trace_push.probes, p, n, -d, covered);
        irradiance += e * covered;
    }
    return diffuse * irradiance + emissive;
}

uint pack_half2(float a, float b)
{
    return f32tof16(a) | (f32tof16(b) << 16);
}

// Whether the instance in GPU-scene slot `slot` draws a double-sided material.
bool double_sided(uint slot)
{
    Instance inst = loadAligned<16>(trace_push.instances + slot);
    return trace_push.batches[inst.batch].double_sided != 0;
}

// One ray of one probe: its radiance and distance (negative for a back face).
[shader("compute")]
[numthreads(64, 1, 1)]
void giTrace(uint3 id : SV_DispatchThreadID)
{
    TraceFrame* f = trace_push.frame;
    GiVolume v = loadAligned<16>(&f.volume);
    uint rays = f.rays;
    uint probe = id.x / rays;
    uint ray = id.x % rays;
    if (probe >= v.counts.w)
        return;
    float4 record = loadAligned<16>(trace_push.probes + probe);
    float3 origin = rest_position(v, probe_coords(v, probe)) + record.xyz;
    float3 local = fibonacci(ray, rays);
    float3 d = normalize(float3(
        dot(loadAligned<16>(&f.rotation[0]).xyz, local),
        dot(loadAligned<16>(&f.rotation[1]).xyz, local),
        dot(loadAligned<16>(&f.rotation[2]).xyz, local)));
    float3 radiance = 0.0;
    float distance = MISS_DISTANCE;
    if (record.w != PROBE_OFF)
    {
        RayDesc r;
        r.Origin = origin;
        r.TMin = 0.0;
        r.Direction = d;
        r.TMax = MISS_DISTANCE;
        RayQuery<RAY_FLAG_FORCE_OPAQUE> q;
        q.TraceRayInline(accel_at(f.accel), 0, f.mask, r);
        while (q.Proceed())
        {
        }
        if (q.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
        {
            float t = q.CommittedRayT();
            if (q.CommittedTriangleFrontFace() || double_sided(q.CommittedInstanceID()))
            {
                radiance = shade_hit(origin + d * t, d, q.CommittedInstanceID(), q.CommittedPrimitiveIndex(), q.CommittedTriangleBarycentrics());
                distance = t;
            }
            else
            {
                // Inside something: no light, and the distance marked.
                distance = -t;
            }
        }
        else
        {
            radiance = sky_radiance(d);
        }
    }
    radiance = min(radiance, 60000.0);
    trace_push.results[id.x] = uint2(pack_half2(radiance.r, radiance.g), pack_half2(radiance.b, distance));
}

// --- Blending -----------------------------------------------------------------------

// `BlendPush` in gi.rs (40 bytes).
struct BlendPush
{
    TraceFrame* frame;
    uint2* results;
    float4* probes;
    float alpha;             // this update's share of the result (1: it replaces it)
    uint target;             // bindless storage image of the atlas written
    uint classify;           // `giRelocate`: 1 turns off the probes still inside, moving none
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<BlendPush> blend_push;

groupshared float4 g_rays[MAX_RAYS];      // xyz: direction; w: distance
groupshared float3 g_radiance[MAX_RAYS];
// A tile's texels once blended, for its border.
groupshared float4 g_tile[MOMENTS_TILE * MOMENTS_TILE];

// Loads probe `probe`'s rays into shared memory (every thread of the group helps).
void stage_rays(TraceFrame* f, uint probe, uint rays, uint thread, uint threads)
{
    for (uint r = thread; r < rays; r += threads)
    {
        uint2 packed = blend_push.results[probe * rays + r];
        float3 local = fibonacci(r, rays);
        float3 d = normalize(float3(
            dot(loadAligned<16>(&f.rotation[0]).xyz, local),
            dot(loadAligned<16>(&f.rotation[1]).xyz, local),
            dot(loadAligned<16>(&f.rotation[2]).xyz, local)));
        g_rays[r] = float4(d, f16tof32(packed.y >> 16));
        g_radiance[r] = float3(f16tof32(packed.x), f16tof32(packed.x >> 16), f16tof32(packed.y));
    }
    GroupMemoryBarrierWithGroupSync();
}

// The border texel of a tile (`inner` texels inside a border of one) that `texel` is, as
// the inside texel it repeats: an edge repeats its mirror across the octahedron's seam, a
// corner the opposite corner.
uint2 border_source(uint2 texel, uint inner)
{
    uint last = inner + 1;
    uint2 s = texel;
    bool left = texel.x == 0, right = texel.x == last, top = texel.y == 0, bottom = texel.y == last;
    if ((left || right) && (top || bottom))
        return uint2(left ? inner : 1, top ? inner : 1);
    if (left || right)
        return uint2(left ? 1 : inner, last - texel.y);
    return uint2(last - texel.x, top ? 1 : inner);
}

// One probe per group: the cosine-weighted mean radiance of its rays in each texel's
// direction, times π (irradiance), blended into the atlas; then the border.
[shader("compute")]
[numthreads(8, 8, 1)]
void giIrradiance(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID, uint flat : SV_GroupIndex)
{
    TraceFrame* f = blend_push.frame;
    GiVolume v = loadAligned<16>(&f.volume);
    uint probe = group.x;
    uint rays = f.rays;
    stage_rays(f, probe, rays, flat, 64);
    uint2 corner = tile_corner(v, probe_coords(v, probe), IRRADIANCE_TILE);
    RWTexture2D<float4> target = g_irradiance_images[blend_push.target];
    bool inside = all(local.xy >= 1u) && all(local.xy <= IRRADIANCE_INNER);
    if (inside)
    {
        float3 n = oct_decode((float2(local.xy - 1u) + 0.5) / float(IRRADIANCE_INNER) * 2.0 - 1.0);
        float3 sum = 0.0;
        float total = 0.0;
        for (uint r = 0; r < rays; r++)
        {
            float4 ray = g_rays[r];
            // Back faces bring no light but still count: the probe sees darkness there.
            float w = max(dot(n, ray.xyz), 0.0);
            sum += g_radiance[r] * w;
            total += w;
        }
        float3 e = total > 0.0 ? sum / total * PI : 0.0;
        uint2 at = corner + local.xy;
        // A replacing update does not read the atlas (never written before the first).
        float4 blended = float4(blend_push.alpha >= 1.0 ? e : lerp(target[at].rgb, e, blend_push.alpha), 1.0);
        target[at] = blended;
        g_tile[local.y * IRRADIANCE_TILE + local.x] = blended;
    }
    GroupMemoryBarrierWithGroupSync();
    if (!inside)
    {
        uint2 source = border_source(local.xy, IRRADIANCE_INNER);
        target[corner + local.xy] = g_tile[source.y * IRRADIANCE_TILE + source.x];
    }
}

// One probe per group: the mean and mean square of its rays' distances under a sharp
// lobe around each texel's direction, blended into the atlas; then the border.
[shader("compute")]
[numthreads(16, 16, 1)]
void giMoments(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID, uint flat : SV_GroupIndex)
{
    TraceFrame* f = blend_push.frame;
    GiVolume v = loadAligned<16>(&f.volume);
    uint probe = group.x;
    uint rays = f.rays;
    stage_rays(f, probe, rays, flat, 256);
    uint2 corner = tile_corner(v, probe_coords(v, probe), MOMENTS_TILE);
    RWTexture2D<float2> target = g_moment_images[blend_push.target];
    bool inside = all(local.xy >= 1u) && all(local.xy <= MOMENTS_INNER);
    if (inside)
    {
        float3 n = oct_decode((float2(local.xy - 1u) + 0.5) / float(MOMENTS_INNER) * 2.0 - 1.0);
        float2 sum = 0.0;
        float total = 0.0;
        float reach = v.origin.w;
        for (uint r = 0; r < rays; r++)
        {
            float4 ray = g_rays[r];
            float c = max(dot(n, ray.xyz), 0.0);
            // cos⁵⁰: the lobe of a texel's own direction.
            float c2 = c * c;
            float c4 = c2 * c2;
            float c8 = c4 * c4;
            float c16 = c8 * c8;
            float c32 = c16 * c16;
            float w = c32 * c16 * c2;
            // A back face counts as near: a probe inside something is hidden from the
            // other side.
            float d = ray.w < 0.0 ? -ray.w * 0.2 : min(ray.w, reach);
            sum += float2(d, d * d) * w;
            total += w;
        }
        uint2 at = corner + local.xy;
        // No ray in the lobe: nothing near, as far as these rays know.
        float2 value = total > 0.0 ? sum / total : float2(reach, reach * reach);
        float2 blended = blend_push.alpha >= 1.0 ? value : lerp(target[at], value, total > 0.0 ? blend_push.alpha : 0.0);
        target[at] = blended;
        g_tile[local.y * MOMENTS_TILE + local.x] = float4(blended, 0.0, 0.0);
    }
    GroupMemoryBarrierWithGroupSync();
    if (!inside)
    {
        uint2 source = border_source(local.xy, MOMENTS_INNER);
        target[corner + local.xy] = g_tile[source.y * MOMENTS_TILE + source.x].xy;
    }
}

// --- Relocation -----------------------------------------------------------------------

// One probe per thread: from this update's rays, move it out of the geometry it is in
// (through the nearest back face), or away from a surface it nearly touches, staying
// within 45 % of a cell from its rest position. With `classify`, nothing moves: a probe
// that still sees mostly back faces (inside something too thick to leave) turns off.
[shader("compute")]
[numthreads(64, 1, 1)]
void giRelocate(uint3 id : SV_DispatchThreadID)
{
    TraceFrame* f = blend_push.frame;
    GiVolume v = loadAligned<16>(&f.volume);
    uint probe = id.x;
    if (probe >= v.counts.w)
        return;
    uint rays = f.rays;
    float4 record = loadAligned<16>(blend_push.probes + probe);
    uint backs = 0;
    float near_back = MISS_DISTANCE;
    float3 back_dir = 0.0;
    float near_front = MISS_DISTANCE;
    float3 front_dir = 0.0;
    for (uint r = 0; r < rays; r++)
    {
        float distance = f16tof32(blend_push.results[probe * rays + r].y >> 16);
        float3 local = fibonacci(r, rays);
        float3 d = float3(
            dot(loadAligned<16>(&f.rotation[0]).xyz, local),
            dot(loadAligned<16>(&f.rotation[1]).xyz, local),
            dot(loadAligned<16>(&f.rotation[2]).xyz, local));
        if (distance < 0.0)
        {
            backs++;
            if (-distance < near_back)
            {
                near_back = -distance;
                back_dir = d;
            }
        }
        else if (distance < near_front)
        {
            near_front = distance;
            front_dir = d;
        }
    }
    bool inside = backs * 4 > rays;
    if (blend_push.classify != 0)
    {
        storeAligned<16>(blend_push.probes + probe, float4(record.xyz, inside ? PROBE_OFF : PROBE_ON));
        return;
    }
    float keep = 0.1 * v.spacing.w;
    float3 offset = record.xyz;
    if (inside)
        offset += back_dir * (near_back + keep);
    else if (near_front < keep)
        offset -= front_dir * (keep - near_front);
    float3 limit = 0.45 * v.spacing.xyz;
    storeAligned<16>(blend_push.probes + probe, float4(clamp(offset, -limit, limit), PROBE_ON));
}

// --- Shading ------------------------------------------------------------------------

// `ResolveFrame` in gi.rs (208 bytes), in the upload ring.
struct ResolveFrame
{
    GiVolume volume;
    float4x4 world_from_clip;  // of the depth as drawn (jittered)
    float4 eye;                // xyz: the camera's position
    float2 ndc_per_pixel;      // 2 / size
    uint2 size;                // the depth's
    float intensity;           // the sky harmonics' multiplier
    uint scale;                // pixels per resolved sample along each axis (1, 2 or 4)
    uint2 low_size;            // the resolved samples' grid: size / scale, rounded up
};

// `ResolvePush` in gi.rs (32 bytes).
struct ResolvePush
{
    ResolveFrame* frame;
    float4* probes;
    float4* sh;                // the sky's 9 irradiance harmonics (outside the volume)
    uint depth;                // bindless: the depth (sampled)
    uint target;               // bindless storage: rgba16f irradiance (a: the volume's share)
};

[[vk::push_constant]]
ConstantBuffer<ResolvePush> resolve_push;

float3 sky_irradiance(float4* sh, float3 n)
{
    float3 e = loadAligned<16>(sh + 0).rgb * 0.282095
        + loadAligned<16>(sh + 1).rgb * (0.488603 * n.y) + loadAligned<16>(sh + 2).rgb * (0.488603 * n.z) + loadAligned<16>(sh + 3).rgb * (0.488603 * n.x)
        + loadAligned<16>(sh + 4).rgb * (1.092548 * n.x * n.y) + loadAligned<16>(sh + 5).rgb * (1.092548 * n.y * n.z)
        + loadAligned<16>(sh + 6).rgb * (0.315392 * (3.0 * n.z * n.z - 1.0)) + loadAligned<16>(sh + 7).rgb * (1.092548 * n.x * n.z)
        + loadAligned<16>(sh + 8).rgb * (0.546274 * (n.x * n.x - n.y * n.y));
    return max(e, 0.0);
}

float4 column(float4x4 m, uint i)
{
    return float4(m[0][i], m[1][i], m[2][i], m[3][i]);
}

float load_depth(ResolveFrame* f, int2 xy)
{
    xy = clamp(xy, int2(0, 0), int2(f.size) - 1);
    return g_textures[resolve_push.depth].Load(int3(xy, 0)).r;
}

// The diffuse irradiance (rgb) of one sample per `scale` × `scale` pixels, and (a) its
// surface's distance from the eye (0: the sky), which the GI shading reads four at a
// time, weighted by distance (`gi_irradiance` in forward.slang): the block's nearest
// surface on one sample and its farthest on the next, in a checkerboard, so both sides of
// an edge have samples near them.
[shader("compute")]
[numthreads(8, 8, 1)]
void giResolve(uint3 id : SV_DispatchThreadID)
{
    ResolveFrame* f = resolve_push.frame;
    if (any(id.xy >= f.low_size))
        return;
    uint scale = f.scale;
    int2 xy = int2(id.xy * scale);
    float depth = load_depth(f, xy);
    bool nearest = ((id.x + id.y) & 1) == 0;
    for (uint k = 1; k < scale * scale; k++)
    {
        int2 at = int2(id.xy * scale + uint2(k % scale, k / scale));
        float d = load_depth(f, at);
        bool closer = nearest ? d < depth : (d > depth && d < 1.0) || depth >= 1.0;
        if (closer && all(at < int2(f.size)))
        {
            depth = d;
            xy = at;
        }
    }
    if (depth >= 1.0)
    {
        g_irradiance_images[resolve_push.target][id.xy] = 0.0;
        return;
    }
    float4x4 m = loadAligned<16>(&f.world_from_clip);
    float2 ndc = (float2(xy) + 0.5) * f.ndc_per_pixel + float2(-1.0, -1.0);
    float4 h = mul(m, float4(ndc, depth, 1.0));
    float3 p = h.xyz / h.w;
    // The surface's normal from the depth: on each axis the side whose depth is closer, so
    // edges do not bend it (as the decal pass).
    float dl = load_depth(f, xy - int2(1, 0));
    float dr = load_depth(f, xy + int2(1, 0));
    float du = load_depth(f, xy - int2(0, 1));
    float dd = load_depth(f, xy + int2(0, 1));
    float sx = abs(dr - depth) < abs(dl - depth) ? 1.0 : -1.0;
    float sy = abs(dd - depth) < abs(du - depth) ? 1.0 : -1.0;
    float4 along_x = column(m, 0) * f.ndc_per_pixel.x;
    float4 along_y = column(m, 1) * f.ndc_per_pixel.y;
    float4 along_z = column(m, 2);
    float4 hx = h + along_x * sx + along_z * ((sx > 0.0 ? dr : dl) - depth);
    float4 hy = h + along_y * sy + along_z * ((sy > 0.0 ? dd : du) - depth);
    float3 dx = (hx.xyz / hx.w - p) * sx;
    float3 dy = (hy.xyz / hy.w - p) * sy;
    float3 n = cross(dy, dx);
    n = n / max(length(n), 1e-20);
    float3 eye = loadAligned<16>(&f.eye).xyz;
    float3 to_eye = normalize(eye - p);
    if (dot(n, to_eye) < 0.0)
        n = -n;
    GiVolume v = loadAligned<16>(&f.volume);
    float covered;
    float3 e = volume_irradiance(v, resolve_push.probes, p, n, to_eye, covered);
    float3 sky = sky_irradiance(resolve_push.sh, n) * f.intensity;
    g_irradiance_images[resolve_push.target][id.xy] = float4(lerp(sky, e, covered), length(eye - p));
}

// `SamplePush` in gi.rs (40 bytes): irradiance at given points and normals (tests).
struct SamplePush
{
    TraceFrame* frame;
    float4* probes;
    float4* points;          // per sample: xyz point, then xyz normal (two float4)
    float4* out;             // per sample: rgb irradiance, a coverage
    uint count;
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<SamplePush> sample_push;

[shader("compute")]
[numthreads(64, 1, 1)]
void giSample(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= sample_push.count)
        return;
    TraceFrame* f = sample_push.frame;
    GiVolume v = loadAligned<16>(&f.volume);
    float3 p = loadAligned<16>(sample_push.points + id.x * 2).xyz;
    float3 n = normalize(loadAligned<16>(sample_push.points + id.x * 2 + 1).xyz);
    float covered;
    float3 e = volume_irradiance(v, sample_push.probes, p, n, n, covered);
    storeAligned<16>(sample_push.out + id.x, float4(e, covered));
}
