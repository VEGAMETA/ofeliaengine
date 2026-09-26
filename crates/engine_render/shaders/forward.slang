// Forward PBR shading with one directional light and a shadow map, drawn GPU-driven
// (ADR 0034): a compute pass culls instances against the view frustum and compacts the
// visible ones per batch (a run of instances sharing a mesh); the forward pass then draws
// every batch with one indirect, instanced draw call.
//
// With occlusion culling (ADR 0035) the forward pass runs twice. The early phase draws
// what was visible in the view's previous frame; a depth pyramid (the farthest depth of
// every 2^k × 2^k pixel block) is built from that depth; the late phase tests every
// instance against it, draws the visible ones the early phase missed, and records which
// instances were visible for the next frame.
//
// BRDF after Filament's "Physically Based Rendering in Filament" (Apache-2.0 document):
// GGX distribution, height-correlated Smith-GGX visibility, Schlick Fresnel, Lambertian
// diffuse; metallic-roughness parameterization. All data is read through buffer device
// addresses; the shadow map comes from the bindless arrays.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerComparisonState g_compare_samplers[];

struct Vertex
{
    float4 position;
    float4 normal;
};

struct Frame
{
    float4x4 clip_from_world;
    float4x4 light_clip_from_world;
    float4 camera_position;  // xyz
    float4 to_light;         // xyz: unit vector towards the light
    float4 light_color;      // rgb: color × intensity
    float4 ambient;          // rgb
    float4 planes[6];        // view frustum: xyz inward unit normal, w offset
    uint shadow_sampler;
    uint cull;               // 0: every instance counts as visible
    uint visibility_capacity; // instances with visibility flags (occlusion culling)
    uint hiz_levels;         // depth pyramid levels 1..hiz_levels; the last is 1×1
    uint2 depth_size;        // size of the depth target in pixels
    uint2 _pad;
    uint4 hiz_offsets[4];    // offset of level k + 1 in the pyramid, in floats
};

struct Instance
{
    float4 row0;             // world_from_object rows: xyz linear part, w translation
    float4 row1;
    float4 row2;
    float4 base_color_metallic; // rgb linear base color, a metallic
    float roughness;         // perceptual
    uint batch;
    uint2 _pad;
};

struct Batch
{
    Vertex* vertices;
    uint vertex_count;
    uint first;              // first instance of the batch; also its first slot in `visible`
    float4 center;           // object-space bounding box: center (xyz)
    float4 extents;          // and half extents (xyz)
};

// VkDrawIndirectCommand.
struct DrawCommand
{
    uint vertex_count;
    uint instance_count;
    uint first_vertex;
    uint first_instance;
};

struct Push
{
    Frame* frame;
    Instance* instances;
    Batch* batches;
    uint* visible;           // per batch, from `first`: indices of the visible instances
    DrawCommand* commands;   // one per batch; culling counts the visible instances
    uint* visibility;        // per instance: visible in the previous frame (occlusion)
    float* pyramid;          // depth pyramid levels, one after the other
    uint instance_count;
    uint texture;            // bindless index: the shadow map, or the depth to reduce
    uint phase;              // PHASE_*
    uint level;              // pyramid level written by hizDown
};

static const uint PHASE_ALL = 0;    // frustum culling only
static const uint PHASE_EARLY = 1;  // visible last frame and in the frustum
static const uint PHASE_LATE = 2;   // visible now and not drawn by the early phase
// Depth (NDC) a box's nearest point must lie beyond the pyramid to count as hidden.
static const float OCCLUSION_MARGIN = 1e-5;

[[vk::push_constant]]
ConstantBuffer<Push> push;

float3 transform_point(Instance i, float3 p)
{
    return float3(dot(i.row0.xyz, p) + i.row0.w, dot(i.row1.xyz, p) + i.row1.w, dot(i.row2.xyz, p) + i.row2.w);
}

// The inverse transpose of the linear part up to a positive factor: its cofactor matrix,
// with the sign of the determinant so mirrored instances keep outward normals. The
// fragment shader normalizes.
float3 transform_normal(Instance i, float3 n)
{
    float3 bc = cross(i.row1.xyz, i.row2.xyz);
    float3 ca = cross(i.row2.xyz, i.row0.xyz);
    float3 ab = cross(i.row0.xyz, i.row1.xyz);
    float s = dot(i.row0.xyz, bc) < 0.0 ? -1.0 : 1.0;
    return s * float3(dot(bc, n), dot(ca, n), dot(ab, n));
}

uint hiz_offset(uint level)
{
    return push.frame.hiz_offsets[(level - 1) >> 2][(level - 1) & 3];
}

// Size of pyramid level `level`: every texel covers 2^level × 2^level pixels.
uint2 hiz_size(uint level)
{
    return ((push.frame.depth_size - 1) >> level) + 1;
}

// Whether a world-space box is hidden: its nearest point is farther than the farthest
// depth of the pyramid over the screen rectangle it covers (at most 2×2 texels of the
// level where that rectangle fits).
bool occluded(float3 center, float3 extents)
{
    float4x4 m = push.frame.clip_from_world;
    float4 c = mul(m, float4(center, 1.0));
    float4 x = mul(m, float4(extents.x, 0.0, 0.0, 0.0));
    float4 y = mul(m, float4(0.0, extents.y, 0.0, 0.0));
    float4 z = mul(m, float4(0.0, 0.0, extents.z, 0.0));
    float2 lo = float2(1e30, 1e30);
    float2 hi = float2(-1e30, -1e30);
    float nearest = 1.0;
    [unroll]
    for (uint i = 0; i < 8; i++)
    {
        float4 p = c + ((i & 1) != 0 ? x : -x) + ((i & 2) != 0 ? y : -y) + ((i & 4) != 0 ? z : -z);
        // A corner behind the camera: the rectangle is unbounded.
        if (p.w <= 1e-5)
            return false;
        float3 ndc = p.xyz / p.w;
        lo = min(lo, ndc.xy);
        hi = max(hi, ndc.xy);
        nearest = min(nearest, ndc.z);
    }
    if (nearest <= 0.0)
        return false;
    float2 size = float2(push.frame.depth_size);
    uint2 lo_px = uint2(clamp((lo * 0.5 + 0.5) * size, 0.0, size - 1.0));
    uint2 hi_px = uint2(clamp((hi * 0.5 + 0.5) * size, 0.0, size - 1.0));
    uint span = max(hi_px.x - lo_px.x, hi_px.y - lo_px.y) + 1;
    // 2^level >= span: the rectangle touches at most two texels per axis.
    uint level = span <= 2 ? 1 : firstbithigh(span - 1) + 1;
    level = min(level, push.frame.hiz_levels);
    uint base = hiz_offset(level);
    uint width = hiz_size(level).x;
    uint2 a = lo_px >> level;
    uint2 b = hi_px >> level;
    float farthest = max(
        max(push.pyramid[base + a.y * width + a.x], push.pyramid[base + a.y * width + b.x]),
        max(push.pyramid[base + b.y * width + a.x], push.pyramid[base + b.y * width + b.x]));
    // A surface lying on the box's face (a wall filling the view is its own occluder's
    // depth) compares equal up to rounding: the rasterizer and this projection round
    // differently, and the AMD iGPU of ref-1 came out on the hidden side. The margin keeps
    // such boxes visible; it costs 1 mm of culling at 7 m and ~20 cm at 100 m (near 0.5).
    return nearest > farthest + OCCLUSION_MARGIN;
}

// One thread per instance: frustum (and occlusion) test of the instance's world-space
// bounding box, then append to the batch's visible list.
[shader("compute")]
[numthreads(64, 1, 1)]
void cullMain(uint3 id : SV_DispatchThreadID)
{
    uint index = id.x;
    if (index >= push.instance_count)
        return;
    Instance inst = push.instances[index];
    Batch batch = push.batches[inst.batch];
    // The world-space box around the transformed object box (Arvo): the center goes
    // through the transform, the extents through the absolute linear part.
    float3 center = transform_point(inst, batch.center.xyz);
    float3 e = batch.extents.xyz;
    float3 extents = float3(dot(abs(inst.row0.xyz), e), dot(abs(inst.row1.xyz), e), dot(abs(inst.row2.xyz), e));
    bool cull = push.frame.cull != 0;
    bool visible = true;
    if (cull)
    {
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            float4 plane = push.frame.planes[i];
            // Outside when the box's most inward corner is behind the plane.
            visible = visible && dot(plane.xyz, center) + dot(abs(plane.xyz), extents) + plane.w >= 0.0;
        }
    }
    if (push.phase != PHASE_ALL)
    {
        // Instances beyond the flags' capacity count as not visible last frame: the late
        // phase always tests them.
        bool tracked = index < push.frame.visibility_capacity;
        bool was_visible = tracked && push.visibility[index] != 0;
        if (push.phase == PHASE_EARLY)
        {
            visible = visible && was_visible;
        }
        else
        {
            if (visible && cull)
                visible = !occluded(center, extents);
            if (tracked)
                push.visibility[index] = visible ? 1 : 0;
            visible = visible && !was_visible;
        }
    }
    uint b = inst.batch;
    if (WaveActiveAllEqual(b))
    {
        // A batch's instances are contiguous, so a wave almost always belongs to one
        // batch: one atomic per wave instead of one per visible instance.
        uint count = WaveActiveCountBits(visible);
        uint base = 0;
        if (WaveIsFirstLane() && count > 0)
            InterlockedAdd(push.commands[b].instance_count, count, base);
        base = WaveReadLaneFirst(base);
        if (visible)
            push.visible[batch.first + base + WavePrefixCountBits(visible)] = index;
    }
    else if (visible)
    {
        uint slot;
        InterlockedAdd(push.commands[b].instance_count, 1, slot);
        push.visible[batch.first + slot] = index;
    }
}

// Depth pyramid level 1 from the depth target: the farthest of every 2×2 pixels (the
// last row and column repeat on odd sizes).
[shader("compute")]
[numthreads(8, 8, 1)]
void hizFirst(uint3 id : SV_DispatchThreadID)
{
    uint2 size = push.frame.depth_size;
    uint2 out_size = hiz_size(1);
    if (any(id.xy >= out_size))
        return;
    Texture2D depth = g_textures[NonUniformResourceIndex(push.texture)];
    uint2 p = id.xy * 2;
    uint2 q = min(p + 1, size - 1);
    float d = max(
        max(depth.Load(int3(p.x, p.y, 0)).r, depth.Load(int3(q.x, p.y, 0)).r),
        max(depth.Load(int3(p.x, q.y, 0)).r, depth.Load(int3(q.x, q.y, 0)).r));
    push.pyramid[hiz_offset(1) + id.y * out_size.x + id.x] = d;
}

// Depth pyramid level `push.level` (2 or more) from the level below.
[shader("compute")]
[numthreads(8, 8, 1)]
void hizDown(uint3 id : SV_DispatchThreadID)
{
    uint level = push.level;
    uint2 in_size = hiz_size(level - 1);
    uint2 out_size = hiz_size(level);
    if (any(id.xy >= out_size))
        return;
    uint src = hiz_offset(level - 1);
    uint2 p = id.xy * 2;
    uint2 q = min(p + 1, in_size - 1);
    float d = max(
        max(push.pyramid[src + p.y * in_size.x + p.x], push.pyramid[src + p.y * in_size.x + q.x]),
        max(push.pyramid[src + q.y * in_size.x + p.x], push.pyramid[src + q.y * in_size.x + q.x]));
    push.pyramid[hiz_offset(level) + id.y * out_size.x + id.x] = d;
}

struct VertexOut
{
    float4 position : SV_Position;
    float3 world_position : POSITION;
    float3 normal : NORMAL;
    float4 light_position : TEXCOORD0;
    nointerpolation float4 base_color_metallic : TEXCOORD1;
    nointerpolation float roughness : TEXCOORD2;
};

// Drawn with the culled commands: instance `slot` (first_instance + instance) is a slot
// of the visible list.
[shader("vertex")]
VertexOut vertexMain(uint vertex_id : SV_VertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = push.instances[push.visible[slot]];
    Vertex v = push.batches[inst.batch].vertices[vertex_id];
    float4 world = float4(transform_point(inst, v.position.xyz), 1.0);
    VertexOut o;
    o.position = mul(push.frame.clip_from_world, world);
    o.world_position = world.xyz;
    o.normal = transform_normal(inst, v.normal.xyz);
    o.light_position = mul(push.frame.light_clip_from_world, world);
    o.base_color_metallic = inst.base_color_metallic;
    o.roughness = inst.roughness;
    return o;
}

// Drawn with every instance of every batch: the instance index is the instance.
[shader("vertex")]
float4 shadowVertex(uint vertex_id : SV_VertexID, uint index : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = push.instances[index];
    Vertex v = push.batches[inst.batch].vertices[vertex_id];
    return mul(push.frame.light_clip_from_world, float4(transform_point(inst, v.position.xyz), 1.0));
}

static const float PI = 3.14159265;

float distribution_ggx(float n_dot_h, float a)
{
    float a2 = a * a;
    float f = (n_dot_h * a2 - n_dot_h) * n_dot_h + 1.0;
    return a2 / (PI * f * f);
}

float visibility_smith_ggx_correlated(float n_dot_v, float n_dot_l, float a)
{
    float a2 = a * a;
    float ggx_l = n_dot_v * sqrt((-n_dot_l * a2 + n_dot_l) * n_dot_l + a2);
    float ggx_v = n_dot_l * sqrt((-n_dot_v * a2 + n_dot_v) * n_dot_v + a2);
    return 0.5 / (ggx_v + ggx_l);
}

float3 fresnel_schlick(float v_dot_h, float3 f0)
{
    float f = pow(1.0 - v_dot_h, 5.0);
    return f0 + (1.0 - f0) * f;
}

// 3×3 percentage-closer filtering with hardware comparison.
float shadow(float4 light_position, float n_dot_l)
{
    float3 ndc = light_position.xyz / light_position.w;
    float2 uv = ndc.xy * 0.5 + 0.5;
    if (any(uv < 0.0) || any(uv > 1.0) || ndc.z > 1.0)
        return 1.0;
    Texture2D map = g_textures[NonUniformResourceIndex(push.texture)];
    SamplerComparisonState s = g_compare_samplers[push.frame.shadow_sampler];
    uint width, height;
    map.GetDimensions(width, height);
    float2 texel = 1.0 / float2(width, height);
    float lit = 0.0;
    [unroll]
    for (int y = -1; y <= 1; y++)
        [unroll]
        for (int x = -1; x <= 1; x++)
            lit += map.SampleCmpLevelZero(s, uv + float2(x, y) * texel, ndc.z);
    return lit / 9.0;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    Frame frame = push.frame[0];
    float3 n = normalize(input.normal);
    float3 v = normalize(frame.camera_position.xyz - input.world_position);
    float3 l = frame.to_light.xyz;
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_l = saturate(dot(n, l));
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));

    float3 base = input.base_color_metallic.rgb;
    float metallic = input.base_color_metallic.a;
    float roughness = max(input.roughness, 0.045);
    float a = roughness * roughness;
    float3 f0 = lerp(float3(0.04, 0.04, 0.04), base, metallic);

    float3 f = fresnel_schlick(v_dot_h, f0);
    float3 specular = distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
    float3 diffuse = (1.0 - metallic) * base / PI;
    float visible = n_dot_l > 0.0 ? shadow(input.light_position, n_dot_l) : 0.0;
    float3 color = (diffuse + specular) * frame.light_color.rgb * n_dot_l * visible
        + frame.ambient.rgb * base;
    return float4(color, 1.0);
}
