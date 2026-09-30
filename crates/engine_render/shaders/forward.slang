// Forward PBR shading with one directional light and cascaded shadow maps, drawn GPU-driven
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
// With a depth prepass (ADR 0042), `depthVertex` (and `depthVertexMasked`, which tests
// alpha) first writes the depth of everything visible; the forward pass then shades each
// pixel once, testing depth for equality. Both compute clip positions with
// `clip_position`: `precise`, and matrix products written out as sums, so every
// operation carries NoContraction and the two passes produce the same bits.
//
// Motion vectors (ADR 0043): with a temporal state, the projection is jittered by a
// sub-pixel offset every frame, and the `*Motion` fragment entry points also write each
// pixel's motion, the texture-coordinate delta current − previous without jitter, into a
// second target. The previous position comes from last frame's instance transforms
// (`previous_instances`, by draw index) and camera. `backgroundMotion` fills the pixels
// no geometry covered (depth still 1) with the camera's motion.
//
// Cascaded shadows (shadow.rs, ADR 0044): up to four cascades share one light view (a
// rotation) and differ by a scale and an offset; they are tiles of one atlas. The culling
// shader's shadow phase tests every instance's box, in light view space, against each
// cascade's square and far depth and appends the casters to that cascade's commands; the
// shadow pass draws each cascade into its tile. Receivers pick the finest cascade whose
// square holds them with room for the filter, push themselves along their normal by a
// multiple of its texel size, filter 3 × 3 comparisons, and blend into the next cascade
// over a band at the edge (the last one fades out).
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

// `CascadeData` in forward.rs.
struct Cascade
{
    float4 scale;            // xyz: light view → cascade NDC scale; w: world size of a texel
    float4 offset;           // xyz: offset; w: NDC edge within which the filter stays inside
    float4 atlas;            // cascade NDC → atlas uv: xy scale, zw offset
    float4 bounds;           // light view: xy center, z half size, w far depth (−z)
};

struct Frame
{
    float4x4 clip_from_world;
    float4x4 light_from_world; // the light's view: a rotation shared by the cascades
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
    float4x4 previous_clip_from_world;  // last frame's camera, unjittered
    float4x4 previous_clip_from_clip;   // for the background: this frame's clip space
                                        // (unjittered) to last frame's
    float4 jitter;           // xy: this frame's jitter in NDC (clip.xy += jitter × w)
    uint previous_count;     // instances with a previous transform
    uint3 _pad2;
    Cascade cascades[4];
    uint cascade_count;      // 0: everything is lit
    uint batch_count;        // batches per command list (the shadow phase's lists)
    float cascade_blend;     // NDC band at a cascade's edge that blends into the next
    float normal_bias;       // receiver offset along its normal, in texels of its cascade
    uint fade_last;          // 1: shadows fade out at the last cascade's edge
    uint3 _pad3;
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
    Instance* previous_instances; // last frame's instances, by draw index (motion)
    uint instance_count;
    uint texture;            // bindless index: the shadow map, or the depth to reduce
    uint phase;              // PHASE_*; the cascade in the shadow pass
    uint _pad;
};

static const uint PHASE_ALL = 0;    // frustum culling only
static const uint PHASE_EARLY = 1;  // visible last frame and in the frustum
static const uint PHASE_LATE = 2;   // visible now and not drawn by the early phase
static const uint PHASE_SHADOW = 3; // casters of each cascade
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

// World and clip position of an object-space point, bit-identical in every pass that
// tests depth for equality (see the file comment).
struct Placed
{
    float3 world;
    float4 clip;
};

Placed clip_position(Instance i, float3 p)
{
    precise float3 world = float3(
        i.row0.x * p.x + i.row0.y * p.y + i.row0.z * p.z + i.row0.w,
        i.row1.x * p.x + i.row1.y * p.y + i.row1.z * p.z + i.row1.w,
        i.row2.x * p.x + i.row2.y * p.y + i.row2.z * p.z + i.row2.w);
    // Columns of clip_from_world, weighted: mul() stays one instruction that cannot be
    // marked NoContraction.
    float4x4 m = transpose(push.frame.clip_from_world);
    precise float4 clip = m[0] * world.x + m[1] * world.y + m[2] * world.z + m[3];
    Placed placed;
    placed.world = world;
    placed.clip = clip;
    return placed;
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

// Appends instance `index` (of batch `b`, whose slots start at `first`) to `visible` and
// counts it in `commands[b]` when `keep`. Called with the whole wave.
void append(DrawCommand* commands, uint* visible, uint b, uint first, uint index, bool keep)
{
    if (WaveActiveAllEqual(b))
    {
        // A batch's instances are contiguous, so a wave almost always belongs to one
        // batch: one atomic per wave instead of one per visible instance.
        uint count = WaveActiveCountBits(keep);
        uint base = 0;
        if (WaveIsFirstLane() && count > 0)
            InterlockedAdd(commands[b].instance_count, count, base);
        base = WaveReadLaneFirst(base);
        if (keep)
            visible[first + base + WavePrefixCountBits(keep)] = index;
    }
    else if (keep)
    {
        uint slot;
        InterlockedAdd(commands[b].instance_count, 1, slot);
        visible[first + slot] = index;
    }
}

// One thread per instance: frustum (and occlusion) test of the instance's world-space
// bounding box, then append to the batch's visible list. The shadow phase instead tests
// the box against every cascade and appends to each cascade's lists.
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
    if (push.phase == PHASE_SHADOW)
    {
        // The box in light view space (Arvo again: the light view is a rotation).
        float4x4 light = push.frame.light_from_world;
        float3 c = mul(light, float4(center, 1.0)).xyz;
        float3 e = float3(dot(abs(light[0].xyz), extents), dot(abs(light[1].xyz), extents), dot(abs(light[2].xyz), extents));
        uint cascades = push.frame.cascade_count;
        for (uint k = 0; k < cascades; k++)
        {
            float4 b = push.frame.cascades[k].bounds;
            // Inside the cascade's square, and not wholly beyond its far depth: anything
            // between the light and the slice casts into it.
            bool caster = !cull || (all(abs(c.xy - b.xy) <= b.z + e.xy) && -(c.z + e.z) <= b.w);
            append(push.commands + k * push.frame.batch_count, push.visible + k * push.instance_count, inst.batch, batch.first, index, caster);
        }
        return;
    }
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
    append(push.commands, push.visible, inst.batch, batch.first, index, visible);
}

struct VertexOut
{
    float4 position : SV_Position;
    float3 world_position : POSITION;
    float3 normal : NORMAL;           // unit at the vertices
    float4 tangent : TANGENT;         // xyz unit at the vertices; w bitangent sign
    float2 uv : TEXCOORD0;
    float3 light_position : TEXCOORD1; // light view position (shadows)
    float4 previous_clip : TEXCOORD2; // last frame's clip position (motion vectors)
    float3 light_normal : TEXCOORD3;  // the normal in light view space (unit at the vertices)
    nointerpolation uint material : MATERIAL;
};

// Drawn with the culled commands: instance `slot` (first_instance + instance) is a slot
// of the visible list; the vertex index includes the draw's vertex offset (Vulkan
// semantics), so it indexes the pool directly.
[shader("vertex")]
VertexOut vertexMain(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    uint index = push.visible[slot];
    Instance inst = push.instances[index];
    Attributes a = push.attributes[vertex];
    float3 p = load_position(vertex);
    Placed placed = clip_position(inst, p);
    float4 world = float4(placed.world, 1.0);
    VertexOut o;
    o.position = placed.clip;
    o.world_position = world.xyz;
    // Unit vectors at the vertices, interpolated without renormalizing: the tangent
    // frame MikkTSpace bakes normal maps against.
    o.normal = normalize(transform_normal(inst, decode_octahedral(a.normal)));
    // A mirrored instance flips its bitangent relative to the (outward) normal.
    float sign = (a.tangent & 0x10000u) != 0 ? -1.0 : 1.0;
    o.tangent = float4(normalize(transform_vector(inst, decode_octahedral(a.tangent))), sign * mirror_sign(inst));
    o.uv = a.uv;
    // Shadows look up in light view space; transforming here keeps it off every pixel.
    float4x4 light = push.frame.light_from_world;
    o.light_position = mul(light, world).xyz;
    o.light_normal = mul((float3x3)light, o.normal);
    // Without a previous transform the instance counts as still.
    Instance previous = index < push.frame.previous_count ? push.previous_instances[index] : inst;
    o.previous_clip = mul(push.frame.previous_clip_from_world, float4(transform_point(previous, p), 1.0));
    o.material = inst.material;
    return o;
}

// A world position in the clip space of cascade `push.phase`.
float4 shadow_clip(float3 world)
{
    Cascade k = push.frame.cascades[push.phase];
    float3 light = mul(push.frame.light_from_world, float4(world, 1.0)).xyz;
    return float4(light * k.scale.xyz + k.offset.xyz, 1.0);
}

// Drawn with a cascade's culled commands: instance `slot` is a slot of its caster list.
// Only positions are fetched.
[shader("vertex")]
float4 shadowVertex(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = push.instances[push.visible[slot]];
    return shadow_clip(transform_point(inst, load_position(vertex)));
}

// The depth prepass: the visible instances' depth, positions only.
[shader("vertex")]
float4 depthVertex(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = push.instances[push.visible[slot]];
    return clip_position(inst, load_position(vertex)).clip;
}

struct ShadowOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    nointerpolation uint material : MATERIAL;
};

// Shadows of masked materials: texture coordinates for the alpha test.
[shader("vertex")]
ShadowOut shadowVertexMasked(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = push.instances[push.visible[slot]];
    ShadowOut o;
    o.position = shadow_clip(transform_point(inst, load_position(vertex)));
    o.uv = push.attributes[vertex].uv;
    o.material = inst.material;
    return o;
}

// The depth prepass of masked materials: texture coordinates for the alpha test
// (`shadowFragmentMasked`).
[shader("vertex")]
ShadowOut depthVertexMasked(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = push.instances[push.visible[slot]];
    ShadowOut o;
    o.position = clip_position(inst, load_position(vertex)).clip;
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

// 3×3 percentage-closer filtering with hardware comparison in cascade `c`, at light view
// position `light` of a receiver with light-view normal `normal` (pushed along it first).
float cascade_shadow(uint c, float3 light, float3 normal)
{
    Cascade k = push.frame.cascades[c];
    float3 ndc = (light + normal * (push.frame.normal_bias * k.scale.w)) * k.scale.xyz + k.offset.xyz;
    if (ndc.z > 1.0)
        return 1.0;
    float2 uv = ndc.xy * k.atlas.xy + k.atlas.zw;
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

// How far towards the edge of cascade `c` a light view position lies (0 center, 1 edge),
// or 2 when it is outside the cascade's depth range. A cascade holds the point with room
// for the filter below its limit (`Cascade::holds` in shadow.rs).
float cascade_edge(uint c, float3 light)
{
    float4 scale = push.frame.cascades[c].scale;
    float4 offset = push.frame.cascades[c].offset;
    float3 ndc = light * scale.xyz + offset.xyz;
    return ndc.z < 0.0 || ndc.z > 1.0 ? 2.0 : max(abs(ndc.x), abs(ndc.y));
}

// Interleaved gradient noise (Jimenez 2014) in [0, 1) at a pixel.
float gradient_noise(float2 pixel)
{
    return frac(52.9829189 * frac(dot(pixel, float2(0.06711056, 0.00583715))));
}

// How lit a receiver at light view position `light`, with light-view geometric normal `n`,
// at `pixel` is: the finest cascade that holds it. Over the band at a cascade's edge the
// pixel dithers into the next one that holds it (the last cascade dithers out, when
// shadows end at the distance setting), with a probability rising across the band: one
// lookup per pixel, and TAA's jitter moves the noise so the transition resolves smooth.
// The frame is read through its pointer: a local copy indexed by the cascade would sit in
// scratch memory on some GPUs.
float shadow(float3 light, float3 n, float2 pixel)
{
    uint count = push.frame.cascade_count;
    float band = push.frame.cascade_blend;
    for (uint c = 0; c < count; c++)
    {
        float edge = cascade_edge(c, light);
        float limit = push.frame.cascades[c].offset.w;
        if (edge >= limit)
            continue;
        uint chosen = c;
        float t = saturate((edge - (limit - band)) / band);
        // The jitter (in NDC, a few thousandths) shifts the pattern every TAA frame.
        if (t > 0.0 && gradient_noise(pixel + push.frame.jitter.xy * 4096.0) < t)
        {
            if (c + 1 < count && cascade_edge(c + 1, light) < push.frame.cascades[c + 1].offset.w)
                chosen = c + 1;
            else if (c + 1 == count && push.frame.fade_last != 0)
                return 1.0;
        }
        return cascade_shadow(chosen, light, n);
    }
    return 1.0;
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
    float visible = n_dot_l > 0.0 && dot(ng, l) > 0.0 ? shadow(input.light_position, normalize(input.light_normal) * side, input.position.xy) : 0.0;
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

// The motion of the surface at this pixel: where it is now (the pixel center, jitter
// removed) minus where it was last frame, in texture coordinates.
float2 motion(float4 pixel, float4 previous_clip)
{
    Frame frame = push.frame[0];
    float2 uv = pixel.xy / float2(frame.depth_size) - frame.jitter.xy * 0.5;
    if (previous_clip.w <= 1e-6)
        return float2(0.0, 0.0);
    float2 previous = previous_clip.xy / previous_clip.w * 0.5 + 0.5;
    return uv - previous;
}

struct ShadedMotion
{
    float4 color : SV_Target0;
    float2 motion : SV_Target1;
};

[shader("fragment")]
ShadedMotion fragmentMainMotion(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = shade(input, front, false);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

[shader("fragment")]
ShadedMotion fragmentMaskedMotion(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = shade(input, front, true);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

struct Background
{
    float4 position : SV_Position;
    float2 ndc : TEXCOORD0;
};

// A triangle covering the target at the far plane (depth exactly 1): with an equal depth
// test it reaches only the pixels no geometry covered.
[shader("vertex")]
Background backgroundVertex(uint vertex : SV_VulkanVertexID)
{
    float2 ndc = float2((vertex << 1) & 2, vertex & 2) * 2.0 - 1.0;
    Background o;
    o.position = float4(ndc, 1.0, 1.0);
    o.ndc = ndc;
    return o;
}

// The camera's motion at infinity along this pixel's ray.
[shader("fragment")]
float2 backgroundMotion(Background input) : SV_Target0
{
    Frame frame = push.frame[0];
    float4 previous = mul(frame.previous_clip_from_clip, float4(input.position.xy / float2(frame.depth_size) * 2.0 - 1.0 - frame.jitter.xy, 1.0, 1.0));
    float2 uv = input.position.xy / float2(frame.depth_size) - frame.jitter.xy * 0.5;
    if (abs(previous.w) <= 1e-6)
        return float2(0.0, 0.0);
    return uv - (previous.xy / previous.w * 0.5 + 0.5);
}
