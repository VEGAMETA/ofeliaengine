// Forward PBR shading with one directional light and a shadow map, drawn GPU-driven
// (ADR 0034): a compute pass culls instances against the view frustum and compacts the
// visible ones per batch (a run of instances sharing a mesh and a pipeline state); the
// forward pass then draws the batches with one indirect draw call per pipeline state.
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
// addresses; the shadow map and material textures come from the bindless arrays.
//
// Vertices come from the geometry pool (mesh.rs, ADR 0039): positions as three floats in
// their own buffer, the other attributes packed in 16 bytes, drawn with indexed indirect
// draws whose vertex offset selects the mesh.
//
// Materials (material.rs, ADR 0040) are glTF 2.0 metallic-roughness: base color, metallic
// and roughness, normal map, occlusion and emission, each a factor times a texture, read
// per pixel from the material table by the instance's material slot. Masked materials
// discard below their alpha cutoff (`fragmentMasked`, and `shadowFragmentMasked` for
// their shadows); opaque ones never discard, so their depth test stays early.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];
[[vk::binding(2, 0)]] SamplerComparisonState g_compare_samplers[];

// `VertexAttributes` in mesh.rs.
struct Attributes
{
    uint normal;   // octahedral, 2 × snorm16
    uint tangent;  // octahedral, 2 × snorm16; bit 16: bitangent sign (1: negative)
    float2 uv;
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
    uint material;           // slot in the material table
    uint batch;
    uint _pad0;
    uint _pad1;
};

struct Batch
{
    uint first;              // first instance of the batch; also its first slot in `visible`
    uint3 _pad;
    float4 center;           // object-space bounding box: center (xyz)
    float4 extents;          // and half extents (xyz)
};

// `GpuMaterial` in material.rs. Textures: bindless index in the low 24 bits, sampler
// index in the top 8.
struct Material
{
    float4 base_color;       // linear RGBA factor
    float3 emissive;         // linear
    float normal_scale;
    float metallic;
    float roughness;         // perceptual
    float occlusion_strength;
    float alpha_cutoff;      // masked materials
    uint base_color_texture; // sRGB; NO_TEXTURE (every texture): white
    uint metallic_roughness_texture; // G roughness, B metallic
    uint normal_texture;     // NO_TEXTURE: the interpolated normal
    uint occlusion_texture;  // R
    uint emissive_texture;   // sRGB
    uint _pad0;
    uint _pad1;
    uint _pad2;
};

static const uint NO_TEXTURE = 0xFFFFFFFFu;

// VkDrawIndexedIndirectCommand.
struct DrawCommand
{
    uint index_count;
    uint instance_count;
    uint first_index;
    int vertex_offset;
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
    float* positions;        // geometry pool: three floats per vertex
    Attributes* attributes;  // geometry pool: the other attributes
    Material* materials;     // the material table
    uint instance_count;
    uint texture;            // bindless index: the shadow map, or the depth to reduce
    uint phase;              // PHASE_*
    uint _pad;
};

static const uint PHASE_ALL = 0;    // frustum culling only
static const uint PHASE_EARLY = 1;  // visible last frame and in the frustum
static const uint PHASE_LATE = 2;   // visible now and not drawn by the early phase
// Depth (NDC) a box's nearest point must lie beyond the pyramid to count as hidden.
static const float OCCLUSION_MARGIN = 1e-5;

[[vk::push_constant]]
ConstantBuffer<Push> push;

float3 load_position(uint vertex)
{
    uint i = vertex * 3;
    return float3(push.positions[i], push.positions[i + 1], push.positions[i + 2]);
}

// A unit vector from two snorm16 in octahedral encoding (Cigolle et al. 2014).
float3 decode_octahedral(uint bits)
{
    float2 e = max(float2(int2(bits << 16, bits) >> 16) / 32767.0, -1.0);
    float3 v = float3(e, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-v.z, 0.0);
    v.x += v.x >= 0.0 ? -t : t;
    v.y += v.y >= 0.0 ? -t : t;
    return normalize(v);
}

float3 transform_point(Instance i, float3 p)
{
    return float3(dot(i.row0.xyz, p) + i.row0.w, dot(i.row1.xyz, p) + i.row1.w, dot(i.row2.xyz, p) + i.row2.w);
}

float3 transform_vector(Instance i, float3 v)
{
    return float3(dot(i.row0.xyz, v), dot(i.row1.xyz, v), dot(i.row2.xyz, v));
}

// 1, or -1 when the instance is mirrored (negative determinant).
float mirror_sign(Instance i)
{
    return dot(i.row0.xyz, cross(i.row1.xyz, i.row2.xyz)) < 0.0 ? -1.0 : 1.0;
}

// The inverse transpose of the linear part up to a positive factor: its cofactor matrix,
// with the sign of the determinant so mirrored instances keep outward normals.
float3 transform_normal(Instance i, float3 n)
{
    float3 bc = cross(i.row1.xyz, i.row2.xyz);
    float3 ca = cross(i.row2.xyz, i.row0.xyz);
    float3 ab = cross(i.row0.xyz, i.row1.xyz);
    return mirror_sign(i) * float3(dot(bc, n), dot(ca, n), dot(ab, n));
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
    // Level 1 is not stored when there are more levels (hiz.slang): start at 2.
    uint levels = push.frame.hiz_levels;
    level = clamp(level, min(2u, levels), levels);
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

struct VertexOut
{
    float4 position : SV_Position;
    float3 world_position : POSITION;
    float3 normal : NORMAL;           // unit at the vertices
    float4 tangent : TANGENT;         // xyz unit at the vertices; w bitangent sign
    float2 uv : TEXCOORD0;
    float4 light_position : TEXCOORD1;
    nointerpolation uint material : MATERIAL;
};

// Drawn with the culled commands: instance `slot` (first_instance + instance) is a slot
// of the visible list; the vertex index includes the draw's vertex offset (Vulkan
// semantics), so it indexes the pool directly.
[shader("vertex")]
VertexOut vertexMain(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = push.instances[push.visible[slot]];
    Attributes a = push.attributes[vertex];
    float4 world = float4(transform_point(inst, load_position(vertex)), 1.0);
    VertexOut o;
    o.position = mul(push.frame.clip_from_world, world);
    o.world_position = world.xyz;
    // Unit vectors at the vertices, interpolated without renormalizing: the tangent
    // frame MikkTSpace bakes normal maps against.
    o.normal = normalize(transform_normal(inst, decode_octahedral(a.normal)));
    // A mirrored instance flips its bitangent relative to the (outward) normal.
    float sign = (a.tangent & 0x10000u) != 0 ? -1.0 : 1.0;
    o.tangent = float4(normalize(transform_vector(inst, decode_octahedral(a.tangent))), sign * mirror_sign(inst));
    o.uv = a.uv;
    o.light_position = mul(push.frame.light_clip_from_world, world);
    o.material = inst.material;
    return o;
}

// Drawn with every instance of every batch: the instance index is the instance. Only
// positions are fetched.
[shader("vertex")]
float4 shadowVertex(uint vertex : SV_VulkanVertexID, uint index : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = push.instances[index];
    return mul(push.frame.light_clip_from_world, float4(transform_point(inst, load_position(vertex)), 1.0));
}

struct ShadowOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    nointerpolation uint material : MATERIAL;
};

// Shadows of masked materials: texture coordinates for the alpha test.
[shader("vertex")]
ShadowOut shadowVertexMasked(uint vertex : SV_VulkanVertexID, uint index : SV_VulkanInstanceID)
{
    Instance inst = push.instances[index];
    ShadowOut o;
    o.position = mul(push.frame.light_clip_from_world, float4(transform_point(inst, load_position(vertex)), 1.0));
    o.uv = push.attributes[vertex].uv;
    o.material = inst.material;
    return o;
}

// A material texture: bindless index and sampler from the packed field; white without
// one. Indices differ between instances of one draw, hence non-uniform; the branch is
// uniform across a quad (a quad belongs to one primitive), so derivatives stay defined.
float4 sample_material(uint packed, float2 uv)
{
    if (packed == NO_TEXTURE)
        return float4(1.0, 1.0, 1.0, 1.0);
    Texture2D t = g_textures[NonUniformResourceIndex(packed & 0xFFFFFFu)];
    SamplerState s = g_samplers[NonUniformResourceIndex(packed >> 24)];
    return t.Sample(s, uv);
}

[shader("fragment")]
void shadowFragmentMasked(ShadowOut input)
{
    Material m = push.materials[input.material];
    if (sample_material(m.base_color_texture, input.uv).a * m.base_color.a < m.alpha_cutoff)
        discard;
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

// Geometric specular antialiasing (Kaplanyan et al., HPG 2016; Tokuyoshi and Kaplanyan,
// I3D 2019), with Filament's defaults: the variance of the normal across the pixel widens
// the GGX lobe, so highlights on curved and small geometry stop flickering.
static const float SPECULAR_AA_VARIANCE = 0.15;
static const float SPECULAR_AA_THRESHOLD = 0.2;

// The widening, to add to the squared GGX roughness (α²).
float specular_aa_kernel(float3 normal)
{
    float3 du = ddx(normal);
    float3 dv = ddy(normal);
    float variance = SPECULAR_AA_VARIANCE * (dot(du, du) + dot(dv, dv));
    return min(2.0 * variance, SPECULAR_AA_THRESHOLD);
}

// Perceptual roughness widened by a kernel of `specular_aa_kernel`.
float widen_roughness(float perceptual, float kernel)
{
    float a = perceptual * perceptual;
    return sqrt(sqrt(saturate(a * a + kernel)));
}

// 3×3 percentage-closer filtering with hardware comparison.
float shadow(float4 light_position)
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

// The shading of both forward entry points; `masked` discards below the alpha cutoff.
float4 shade(VertexOut input, bool front, bool masked)
{
    Frame frame = push.frame[0];
    Material m = push.materials[input.material];
    // The back of a double-sided surface is lit from its own side: the whole tangent
    // frame flips (as in Khronos' glTF sample viewer).
    float side = front ? 1.0 : -1.0;
    float3 geometric = input.normal * side;
    float3 ng = normalize(geometric);
    // Derivatives first, while every lane of the quad is alive.
    float kernel = specular_aa_kernel(ng);
    float4 base = sample_material(m.base_color_texture, input.uv) * m.base_color;
    if (masked && base.a < m.alpha_cutoff)
        discard;

    float3 n = ng;
    if (m.normal_texture != NO_TEXTURE)
    {
        // MikkTSpace: the per-pixel bitangent from the interpolated (not renormalized)
        // normal and tangent, then the normal map's vector in that frame.
        float3 t = input.tangent.xyz * side;
        float3 b = input.tangent.w * cross(input.normal, input.tangent.xyz) * side;
        float2 xy = sample_material(m.normal_texture, input.uv).xy * 2.0 - 1.0;
        float z = sqrt(saturate(1.0 - dot(xy, xy)));
        xy *= m.normal_scale;
        n = normalize(xy.x * t + xy.y * b + z * geometric);
    }
    float4 mr = sample_material(m.metallic_roughness_texture, input.uv);
    float metallic = saturate(mr.b * m.metallic);
    float perceptual = widen_roughness(saturate(mr.g * m.roughness), kernel);
    float occlusion = 1.0 + m.occlusion_strength * (sample_material(m.occlusion_texture, input.uv).r - 1.0);
    float3 emissive = sample_material(m.emissive_texture, input.uv).rgb * m.emissive;

    float3 v = normalize(frame.camera_position.xyz - input.world_position);
    float3 l = frame.to_light.xyz;
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_l = saturate(dot(n, l));
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));

    float roughness = max(perceptual, 0.045);
    float a = roughness * roughness;
    float3 f0 = lerp(float3(0.04, 0.04, 0.04), base.rgb, metallic);
    float3 f = fresnel_schlick(v_dot_h, f0);
    float3 specular = distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
    float3 diffuse = (1.0 - metallic) * base.rgb / PI;
    // A surface facing away from the light is in its own shadow, whatever its normal map
    // says.
    float visible = n_dot_l > 0.0 && dot(ng, l) > 0.0 ? shadow(input.light_position) : 0.0;
    float3 color = (diffuse + specular) * frame.light_color.rgb * n_dot_l * visible
        + frame.ambient.rgb * base.rgb * occlusion
        + emissive;
    return float4(color, 1.0);
}

[shader("fragment")]
float4 fragmentMain(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return shade(input, front, false);
}

[shader("fragment")]
float4 fragmentMasked(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return shade(input, front, true);
}
