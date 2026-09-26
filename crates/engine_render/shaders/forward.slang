// Forward PBR shading with one directional light and a shadow map, drawn GPU-driven
// (ADR 0034): a compute pass culls instances against the view frustum and compacts the
// visible ones per batch (a run of instances sharing a mesh); the forward pass then draws
// every batch with one indirect, instanced draw call.
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
    uint2 _pad;
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
    uint instance_count;
    uint shadow_map;         // bindless index of the shadow map
};

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

// One thread per instance: frustum test of the instance's world-space bounding box, then
// append to the batch's visible list.
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
    bool visible = true;
    if (push.frame.cull != 0)
    {
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            float4 plane = push.frame.planes[i];
            // Outside when the box's most inward corner is behind the plane.
            visible = visible && dot(plane.xyz, center) + dot(abs(plane.xyz), extents) + plane.w >= 0.0;
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
    Texture2D map = g_textures[NonUniformResourceIndex(push.shadow_map)];
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
