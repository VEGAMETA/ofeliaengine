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
// Local lights (lights.rs, ADR 0045): point and spot lights sorted by depth along the view.
// The CPU fills z-bins (each slice of view depth: the lowest and highest index of the
// lights overlapping it) and each light's rectangle of screen tiles; `lightTiles` sets one
// bit per light in every tile its rectangle covers. A pixel shades the lights in both its
// tile's bits and its bin's index range; the wave walks the union of its lanes' bits, one
// light at a time, so light data loads are shared.
//
// Ambient light (environment.rs, ADR 0046): with an environment, diffuse light comes from
// its 9 irradiance harmonics and specular light from its prefiltered cube at the pixel's
// roughness times the DFG table (split sum), with Filament's multiple-scattering
// compensation and specular occlusion from the material's occlusion (Lagarde and de
// Rousiers 2014); `skyFragment` draws the sky and the sun's disk where no geometry was.
//
// Materials (material.rs, ADR 0040) are glTF 2.0 metallic-roughness: base color, metallic
// and roughness, normal map, occlusion and emission, each a factor times a texture, read
// per pixel from the material table by the instance's material slot. Masked materials
// discard below their alpha cutoff (`fragmentMasked`, and `shadowFragmentMasked` for
// their shadows); opaque ones never discard, so their depth test stays early.
//
// Subsurface scattering (subsurface.rs, ADR 0059): materials with a profile are drawn by
// `fragmentSubsurface` after the others, depth tested for equality with the prepass. It
// writes the specular light and emission as usual, and keeps the diffuse light apart, per
// unit albedo, with the light passing through from behind (transmission, from the
// profile's slab integral). `scatterFragment`, drawn with the same geometry and depth
// test, spreads that light with the profile and multiplies it by the exit albedo (see
// there).

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] TextureCube g_cubes[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];
[[vk::binding(2, 0)]] SamplerComparisonState g_compare_samplers[];

// The atmosphere and height fog (ADR 0069): only the transparent surfaces' atmosphere
// variants read them (the opaque ones get theirs from atmosphere.slang's full-screen pass).
#include "atmosphere_common.slang"

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

// `LightData` in lights.rs.
struct Light
{
    float4 position_range;   // xyz position, w range (the falloff ends there)
    float4 color_scale;      // rgb color × intensity; w spot cone scale (0: point light)
    float4 direction_offset; // xyz direction of the cone, w cone offset (1: point light)
};

// `GpuProfile` in subsurface.rs: a sum of Gaussians in world units (ADR 0059).
struct SubsurfaceProfile
{
    float4 gaussians[6];     // rgb: weight / (2π v); w: −log2(e) / 2v
    float3 falloff;          // per channel −log2(e) / 2v̄ (v̄ its mean variance): the scatter weights' depth falloff
    uint count;
    float4 radii;            // the scatter pass's largest sample radius per budget
    uint4 kernel[148];       // per budget: offset on the golden-angle spiral (xy, float bits), the profile there over n · density (zw, three halves)
};

// The scatter pass's budgets: samples and first kernel entry (`BUDGETS` and
// `KERNEL_OFFSETS` in subsurface.rs).
static const uint BUDGET_SAMPLES[4] = { 8, 20, 40, 80 };
static const uint BUDGET_FIRST[4] = { 0, 8, 28, 68 };
// The scatter pass takes at most one sample per this many pixels of its disk.
static const float PIXELS_PER_SAMPLE = 4.0;

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
    float lod_scale;         // pixels per object unit at distance 1 over the LOD threshold (0: full meshes)
    uint slot_count;         // instance slots per command list
    uint4 hiz_offsets[4];    // offset of level k + 1 in the pyramid, in floats
    float4x4 previous_clip_from_world;  // last frame's camera, unjittered
    float4x4 previous_clip_from_clip;   // for the background: this frame's clip space
                                        // (unjittered) to last frame's
    float4 jitter;           // xy: this frame's jitter in NDC (clip.xy += jitter × w)
    uint previous_count;     // instances with a previous transform
    uint scatter_budget;     // the subsurface budget, BUDGET_* (ADR 0059)
    uint2 _pad2;
    Cascade cascades[4];
    uint cascade_count;      // 0: everything is lit
    uint command_count;      // draw commands per list (one per detail level of each batch)
    float cascade_blend;     // NDC band at a cascade's edge that blends into the next
    float normal_bias;       // receiver offset along its normal, in texels of its cascade
    uint fade_last;          // 1: shadows fade out at the last cascade's edge
    float scatter_pixels_per_unit; // pixels per world unit at clip w = 1
    float scatter_w_scale;   // clip w = w_scale · depth from the near plane + w_offset
    float scatter_w_offset;
    float4 view_axis;        // xyz view direction; w: −dot(direction, near plane center)
    Light* lights;           // local lights, sorted by depth along the view
    uint* zbins;             // Z_BINS entries: lowest light index | highest << 16
    uint light_count;
    uint light_words;        // 32-bit words of a tile's light bitmask
    uint tiles_x;            // light tiles per row
    float bin_scale;         // z-bins per unit of depth along the view
    uint light_culling;      // 0: every light for every pixel (the reference)
    uint scatter_frame;      // noise index of the scatter pass
    uint2 _pad4;
    float4x4 world_from_clip; // inverse of clip_from_world (jittered): the sky's rays
    uint environment;        // 1: ambient light and background from the environment
    uint sky_cube;           // bindless: the environment's radiance cube
    uint prefiltered;        // bindless: its prefiltered cube
    uint dfg_lut;            // bindless: the DFG table
    uint env_levels;         // mips of the prefiltered cube
    float env_intensity;     // the environment's radiance multiplier
    uint linear_sampler;     // trilinear, clamp
    uint subsurface_count;   // profiles at `subsurface` (0: no subsurface draws)
    float4* sh;              // 9 irradiance harmonics
    SubsurfaceProfile* subsurface;
    AtmosphereFrame* atmosphere; // aerial perspective and fog (ADR 0069), for `fragmentBlendedAtmosphere*`
    uint2 _pad5;
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
    uint command;            // the finest level's draw command; the coarser ones follow
    uint slot;               // its first slot in `visible`; each level has `count` more
    uint count;              // instances
    uint lods;               // detail levels of the mesh (ADR 0053)
    float4 center;           // object-space bounding box: center (xyz)
    float4 extents;          // and half extents (xyz)
    float4 errors;           // each level's error, object-space units
};

// Bits of `Batch.lods`: the level count, and whether the batch is blended (ADR 0054).
static const uint LOD_COUNT_MASK = 0xFFu;
static const uint BATCH_BLENDED = 1u << 8;

// The coarsest detail level whose error, projected at the box's nearest possible
// distance, stays within the threshold (ADR 0053): error × scale × lod_scale ≤ distance.
uint choose_lod(Instance inst, Batch batch, float3 center, float3 extents)
{
    uint lods = batch.lods & LOD_COUNT_MASK;
    if (lods <= 1 || push.frame.lod_scale <= 0.0)
        return 0;
    // The largest scale of the instance's linear part (its columns' lengths).
    float3 c0 = float3(inst.row0.x, inst.row1.x, inst.row2.x);
    float3 c1 = float3(inst.row0.y, inst.row1.y, inst.row2.y);
    float3 c2 = float3(inst.row0.z, inst.row1.z, inst.row2.z);
    float scale = sqrt(max(dot(c0, c0), max(dot(c1, c1), dot(c2, c2))));
    float distance = max(length(center - push.frame.camera_position.xyz) - length(extents), 1e-3);
    float allowed = distance / (scale * push.frame.lod_scale);
    uint lod = 0;
    for (uint k = 1; k < lods; k++)
    {
        if (batch.errors[k] <= allowed)
            lod = k;
    }
    return lod;
}

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
    uint subsurface;         // bits 0-7 profile + 1 (0: none); 8-15 mask; 16-17 transmission; 18 pre- and post-scatter texturing
    uint thickness_texture;  // G, times `thickness`
    float thickness;         // world units
};

// Bits of `Material.subsurface` (`GpuMaterial` in material.rs).
static const uint TRANSMISSION_THIN = 1;
static const uint SUBSURFACE_SPLIT_TEXTURING = 1u << 18;

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
    uint* light_tiles;       // per tile, `light_words` bitmask words of its lights
    uint instance_count;
    uint texture;            // bindless index: the shadow map, or the depth to reduce
    uint phase;              // PHASE_*; the cascade in the shadow pass
    uint ao;                 // bindless index: the view's ambient occlusion (NO_TEXTURE: none; ADR 0058)
};

static const uint PHASE_ALL = 0;    // frustum culling only
static const uint PHASE_EARLY = 1;  // visible last frame and in the frustum
static const uint PHASE_LATE = 2;   // visible now and not drawn by the early phase
static const uint PHASE_SHADOW = 3; // casters of each cascade
// `LIGHT_TILE` and `Z_BINS` in lights.rs.
static const uint LIGHT_TILE = 16;
static const uint Z_BINS = 1024;
// 0 in the pipelines drawn without local lights: the light loop compiles away.
[vk::constant_id(0)] const uint LOCAL_LIGHTS = 1;
// 0 in the pipelines drawn without an environment: image-based light compiles away.
[vk::constant_id(1)] const uint ENVIRONMENT = 1;
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
    // Blended batches cast no shadows and are drawn once, after the opaque surfaces: in the
    // last phase, tested against the pyramid of what the first phase drew.
    bool blended = (batch.lods & BATCH_BLENDED) != 0;
    uint lod = choose_lod(inst, batch, center, extents);
    uint command = batch.command + lod;
    uint first = batch.slot + lod * batch.count;
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
            bool caster = !blended && (!cull || (all(abs(c.xy - b.xy) <= b.z + e.xy) && -(c.z + e.z) <= b.w));
            append(push.commands + k * push.frame.command_count, push.visible + k * push.frame.slot_count, command, first, index, caster);
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
    if (push.phase != PHASE_ALL && blended)
    {
        if (push.phase == PHASE_EARLY)
            visible = false;
        else if (visible && cull)
            visible = !occluded(center, extents);
    }
    else if (push.phase != PHASE_ALL)
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
    append(push.commands, push.visible, command, first, index, visible);
}

// One group per light: its bit is set in the (cleared) bitmasks of every tile of its
// rectangle. `visible` holds the rectangles (min x | min y << 16, max x | max y << 16).
// The work follows what the lights cover, not tiles × lights.
[shader("compute")]
[numthreads(64, 1, 1)]
void lightTiles(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    uint light = group.x;
    uint lo = push.visible[light * 2];
    uint hi = push.visible[light * 2 + 1];
    uint2 first = uint2(lo & 0xFFFFu, lo >> 16);
    uint2 size = uint2(hi & 0xFFFFu, hi >> 16) - first + 1;
    uint words = push.frame.light_words;
    uint tiles_x = push.frame.tiles_x;
    uint word = light >> 5;
    uint bit = 1u << (light & 31u);
    for (uint i = local; i < size.x * size.y; i += 64)
    {
        uint2 t = first + uint2(i % size.x, i / size.x);
        InterlockedOr(push.light_tiles[(t.y * tiles_x + t.x) * words + word], bit);
    }
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
// A LOD bias (temporal upscaling) lives in the samplers (`MaterialTable::set_texture_lod_bias`):
// here it would cost registers in every shading pipeline (ADR 0060).
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

// Light reflected towards `v` by a surface with normal `n`, for unit irradiance from `l`
// (the BRDF times n·l).
float3 surface_light(float3 n, float3 v, float3 l, float3 diffuse, float3 f0, float a)
{
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_l = saturate(dot(n, l));
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));
    float3 f = fresnel_schlick(v_dot_h, f0);
    float3 specular = distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
    return (diffuse + specular) * n_dot_l;
}

// One local light at `p`: inverse-square falloff windowed to zero at its range (Karis 2013),
// times the spot cone. Adds the irradiance it brings to `irradiance` (subsurface
// materials; unused and compiled away elsewhere).
float3 local_light(Light light, float3 p, float3 n, float3 v, float3 diffuse, float3 f0, float a, inout float3 irradiance)
{
    float3 to = light.position_range.xyz - p;
    float d2 = dot(to, to);
    float r = light.position_range.w;
    float x = d2 / (r * r);
    float window = saturate(1.0 - x * x);
    if (window <= 0.0)
        return 0.0;
    float3 l = to * rsqrt(max(d2, 1e-8));
    float cone = saturate(dot(-l, light.direction_offset.xyz) * light.color_scale.w + light.direction_offset.w);
    float attenuation = window * window / max(d2, 1e-4) * cone * cone;
    irradiance += light.color_scale.rgb * attenuation * saturate(dot(n, l));
    return surface_light(n, v, l, diffuse, f0, a) * light.color_scale.rgb * attenuation;
}

// Bits `lo`..`hi` (light indices) of word `w`.
uint range_bits(uint w, uint lo, uint hi)
{
    int first = clamp(int(lo) - int(w * 32), 0, 32);
    int end = clamp(int(hi) - int(w * 32) + 1, 0, 32);
    uint below_end = end >= 32 ? 0xFFFFFFFFu : (1u << uint(end)) - 1u;
    uint from_first = first >= 32 ? 0u : 0xFFFFFFFFu << uint(first);
    return below_end & from_first;
}

// The local lights reaching `p` (at `pixel`): those in both its tile's bits and its
// z-bin's index range, or every light without culling. Their irradiance adds to
// `irradiance`.
float3 local_lights(float3 p, float2 pixel, float3 n, float3 v, float3 diffuse, float3 f0, float a, inout float3 irradiance)
{
    uint count = push.frame.light_count;
    float3 sum = 0.0;
    if (LOCAL_LIGHTS == 0 || count == 0)
        return sum;
    if (push.frame.light_culling == 0)
    {
        for (uint i = 0; i < count; i++)
            sum += local_light(push.frame.lights[i], p, n, v, diffuse, f0, a, irradiance);
        return sum;
    }
    float depth = dot(p, push.frame.view_axis.xyz) + push.frame.view_axis.w;
    uint bin = uint(clamp(depth * push.frame.bin_scale, 0.0, float(Z_BINS - 1)));
    uint range = push.frame.zbins[bin];
    uint lo = range & 0xFFFFu;
    uint hi = range >> 16;
    bool any = lo <= hi;
    // The wave walks the union of its lanes' words.
    uint first_word = WaveActiveMin(any ? lo >> 5 : 0xFFFFFFFFu);
    uint last_word = WaveActiveMax(any ? hi >> 5 : 0u);
    uint2 tile = uint2(pixel) / LIGHT_TILE;
    uint base = (tile.y * push.frame.tiles_x + tile.x) * push.frame.light_words;
    for (uint w = first_word; w <= last_word && first_word != 0xFFFFFFFFu; w++)
    {
        uint mask = any ? push.light_tiles[base + w] & range_bits(w, lo, hi) : 0u;
        uint merged = WaveActiveBitOr(mask);
        while (merged != 0)
        {
            uint bit = firstbitlow(merged);
            merged &= merged - 1u;
            if ((mask >> bit) & 1u)
                sum += local_light(push.frame.lights[w * 32 + bit], p, n, v, diffuse, f0, a, irradiance);
        }
    }
    return sum;
}

// Jimenez et al.'s fit of the light bouncing between occluders (GTAO, 2016): ambient
// occlusion darkens light-colored surfaces less.
float multibounce(float visibility, float albedo)
{
    float a = 2.0404 * albedo - 0.3324;
    float b = -4.7951 * albedo + 0.6417;
    float c = 2.7552 * albedo + 0.6903;
    return max(visibility, ((visibility * a + b) * visibility + c) * visibility);
}

// Light from the whole sky: the environment's harmonics (diffuse) and prefiltered cube
// (specular), or the flat ambient color without an environment. `occlusion` is the
// material's, times the view's ambient occlusion when `ao` (ADR 0058).
float3 ambient_light(float3 n, float3 v, float3 diffuse, float3 f0, float roughness, float occlusion, float3 albedo, bool ao)
{
    // With the view's ambient occlusion, the fit of its bounces (for the albedo's
    // luminance: one value instead of three, fewer registers); without it, the material's
    // occlusion as it was.
    float diffuse_occlusion = ao && occlusion < 1.0
        ? multibounce(occlusion, dot(albedo, float3(0.2126, 0.7152, 0.0722)))
        : occlusion;
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return push.frame.ambient.rgb * albedo * diffuse_occlusion;
    float n_dot_v = max(dot(n, v), 1e-4);
    float4* sh = push.frame.sh;
    float3 irradiance = sh[0].rgb * 0.282095
        + sh[1].rgb * (0.488603 * n.y) + sh[2].rgb * (0.488603 * n.z) + sh[3].rgb * (0.488603 * n.x)
        + sh[4].rgb * (1.092548 * n.x * n.y) + sh[5].rgb * (1.092548 * n.y * n.z)
        + sh[6].rgb * (0.315392 * (3.0 * n.z * n.z - 1.0)) + sh[7].rgb * (1.092548 * n.x * n.z)
        + sh[8].rgb * (0.546274 * (n.x * n.x - n.y * n.y));
    irradiance = max(irradiance, 0.0);
    SamplerState s = g_samplers[push.frame.linear_sampler];
    float3 r = reflect(-v, n);
    float lod = roughness * float(push.frame.env_levels - 1);
    float3 prefiltered = g_cubes[push.frame.prefiltered].SampleLevel(s, r, lod).rgb;
    float2 dfg = g_textures[push.frame.dfg_lut].SampleLevel(s, float2(n_dot_v, roughness), 0.0).xy;
    float3 specular = prefiltered * lerp(dfg.xxx, dfg.yyy, f0) * (1.0 + f0 * (1.0 / max(dfg.y, 1e-4) - 1.0));
    float a = roughness * roughness;
    // Lagarde and de Rousiers' specular occlusion; 1 without occlusion (most materials),
    // where skipping it frees registers (72 → 64 VGPRs on the Radeon).
    float specular_occlusion = occlusion < 1.0 ? saturate(pow(n_dot_v + occlusion, exp2(-16.0 * a - 1.0)) - 1.0 + occlusion) : 1.0;
    return (diffuse * irradiance * diffuse_occlusion + specular * specular_occlusion) * push.frame.env_intensity;
}

// The irradiance of the environment's harmonics at normal `n` (as in `ambient_light`).
float3 sky_irradiance(float3 n)
{
    float4* sh = push.frame.sh;
    float3 irradiance = sh[0].rgb * 0.282095
        + sh[1].rgb * (0.488603 * n.y) + sh[2].rgb * (0.488603 * n.z) + sh[3].rgb * (0.488603 * n.x)
        + sh[4].rgb * (1.092548 * n.x * n.y) + sh[5].rgb * (1.092548 * n.y * n.z)
        + sh[6].rgb * (0.315392 * (3.0 * n.z * n.z - 1.0)) + sh[7].rgb * (1.092548 * n.x * n.z)
        + sh[8].rgb * (0.546274 * (n.x * n.x - n.y * n.y));
    return max(irradiance, 0.0);
}

// The diffuse part of `ambient_light` per unit base color, for subsurface materials;
// `kd` is (1 − metallic) / π.
float3 ambient_diffuse(float3 n, float occlusion, float kd)
{
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return push.frame.ambient.rgb * occlusion;
    return kd * sky_irradiance(n) * occlusion * push.frame.env_intensity;
}

// π log2(e): a profile Gaussian's weight from its stored amplitude and exponent.
static const float PI_LOG2E = 4.53236014;

// Transmission through a slab `d` world units thick: the profile integrated over a plane
// at distance `d` (`SubsurfaceProfile::transmission` in subsurface.rs).
float3 profile_transmission(SubsurfaceProfile* p, float d)
{
    float3 t = 0.0;
    uint count = p.count;
    for (uint i = 0; i < count; i++)
    {
        float4 g = p.gaussians[i];
        t += g.rgb * (PI_LOG2E / -g.w) * exp2(g.w * d * d);
    }
    return t;
}

// How far the sun's light travelled from the nearest surface towards it to reach light
// view position `light` of a receiver with light-view normal `normal`, in world units:
// the shadow map's depth there (in the finest cascade holding it) against the receiver's,
// pushed into the surface along its normal first. 0 outside the cascades.
float shadow_distance(float3 light, float3 normal)
{
    uint count = push.frame.cascade_count;
    for (uint c = 0; c < count; c++)
    {
        if (cascade_edge(c, light) >= push.frame.cascades[c].offset.w)
            continue;
        Cascade k = push.frame.cascades[c];
        float3 ndc = (light - normal * (push.frame.normal_bias * k.scale.w)) * k.scale.xyz + k.offset.xyz;
        float2 uv = ndc.xy * k.atlas.xy + k.atlas.zw;
        Texture2D map = g_textures[NonUniformResourceIndex(push.texture)];
        uint width, height;
        map.GetDimensions(width, height);
        int2 texel = clamp(int2(uv * float2(width, height)), int2(0, 0), int2(width, height) - 1);
        float occluder = map.Load(int3(texel, 0)).r;
        return max(ndc.z - occluder, 0.0) / abs(k.scale.z);
    }
    return 0.0;
}

// What the forward subsurface pass writes besides the color (ADR 0059).
struct SubsurfaceOut
{
    float4 irradiance;       // rgb: mask × diffuse light per unit albedo × entry albedo; a: depth from the near plane
    float4 albedo;           // rgb: exit albedo; a: profile index / 255
};

// The shading of the forward entry points, with the material's alpha; `masked` discards
// below the alpha cutoff; `ao` reads the view's ambient occlusion (ADR 0058). Both are
// literals at every call: the compiler removes the code of the other case, so shading
// without ambient occlusion spends no registers on it (on RDNA2 even dead code under a
// specialization constant took the shading from 64 to 72 VGPRs, 9 to 13 % slower).
float4 shade(VertexOut input, bool front, bool masked, bool ao)
{
    SubsurfaceOut unused;
    return shade_surface(input, front, masked, ao, false, unused);
}

// `shade`; with `subsurface`, the scattered part of the diffuse light goes to `sss`
// instead of the color. Subsurface materials' pipelines always take `ao` (they are their
// own; without ambient occlusion `push.ao` skips the read).
float4 shade_surface(VertexOut input, bool front, bool masked, bool ao, bool subsurface, out SubsurfaceOut sss)
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
    // The view's ambient occlusion (ADR 0058) joins the material's.
    if (ao && push.ao != NO_TEXTURE)
        occlusion *= g_textures[push.ao].Load(int3(int2(input.position.xy), 0)).r;
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
    // Subsurface materials keep their diffuse light apart: the lighting below then returns
    // the specular light only.
    float3 surface_diffuse = subsurface ? float3(0.0, 0.0, 0.0) : diffuse;
    float3 ambient_albedo = subsurface ? float3(0.0, 0.0, 0.0) : base.rgb;
    // A surface facing away from the light is in its own shadow, whatever its normal map
    // says.
    float visible = n_dot_l > 0.0 && dot(ng, l) > 0.0 ? shadow(input.light_position, normalize(input.light_normal) * side, input.position.xy) : 0.0;
    float3 local_irradiance = 0.0;
    float3 color = (surface_diffuse + specular) * frame.light_color.rgb * n_dot_l * visible
        + ambient_light(n, v, surface_diffuse, f0, roughness, occlusion, ambient_albedo, ao)
        + emissive
        + local_lights(input.world_position, input.position.xy, n, v, surface_diffuse, f0, a, local_irradiance);
    sss = (SubsurfaceOut)0;
    if (subsurface)
    {
        uint bits = m.subsurface;
        uint index = min(max(bits & 0xFFu, 1u) - 1u, max(frame.subsurface_count, 1u) - 1u);
        SubsurfaceProfile* profile = frame.subsurface + index;
        // Diffuse light per unit base color: sun, local lights, sky.
        float kd = (1.0 - metallic) / PI;
        float3 lit = kd * (frame.light_color.rgb * n_dot_l * visible + local_irradiance) + ambient_diffuse(n, occlusion, kd);
        // Light from behind, through the material (Jimenez et al. 2010): the profile's slab
        // integral over its thickness, or over the object's in the shadow map.
        uint transmission = (bits >> 16) & 3u;
        float back = saturate(-dot(n, l));
        if (transmission != 0 && back > 0.0)
        {
            float thickness = m.thickness * sample_material(m.thickness_texture, input.uv).g;
            float3 light_normal = normalize(input.light_normal) * side;
            float d = thickness;
            float lit_back = 1.0;
            if (transmission == TRANSMISSION_THIN)
                // Shadowed by other objects: the receiver pushed through its thickness to
                // the lit side.
                lit_back = shadow(input.light_position - light_normal * thickness, -light_normal, input.position.xy);
            else
                d = max(thickness, shadow_distance(input.light_position, light_normal));
            lit += kd * frame.light_color.rgb * back * lit_back * profile_transmission(profile, d);
        }
        float mask = float((bits >> 8) & 0xFFu) / 255.0;
        bool split = (bits & SUBSURFACE_SPLIT_TEXTURING) != 0;
        float3 entry = split ? sqrt(base.rgb) : float3(1.0, 1.0, 1.0);
        float3 exit = split ? sqrt(base.rgb) : base.rgb;
        color += (1.0 - mask) * base.rgb * lit;
        float depth = max(dot(input.world_position, frame.view_axis.xyz) + frame.view_axis.w, 1e-6);
        sss.irradiance = float4(mask * lit * entry, depth);
        sss.albedo = float4(exit, float(index) / 255.0);
    }
    return float4(color, base.a);
}

[shader("fragment")]
float4 fragmentMain(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false).rgb, 1.0);
}

// After the depth prepass of a view with ambient occlusion (ADR 0058).
[shader("fragment")]
float4 fragmentMainAo(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMasked(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, true, false).rgb, 1.0);
}

// Transparent surfaces (ADR 0054): premultiplied color over what is behind. Alpha is
// coverage: the whole shading scales with it, as glTF's BLEND mode says.
[shader("fragment")]
float4 fragmentBlended(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    float4 c = shade(input, front, false, false);
    float alpha = saturate(c.a);
    return float4(c.rgb * alpha, alpha);
}

// Transparent surfaces of a view with an atmosphere or height fog (ADR 0069): the light
// scattered between them and the camera, and its absorption, before blending. Entry points
// of their own, so the other shading carries none of it.
[shader("fragment")]
float4 fragmentBlendedAtmosphere(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    float4 c = shade(input, front, false, false);
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(push.frame.depth_size));
    float alpha = saturate(c.a);
    return float4(c.rgb * alpha, alpha);
}

struct BlendedReactive
{
    float4 color : SV_Target0;
    // The reactive mask for temporal upsampling: blended the same way (premultiplied), so
    // it accumulates the layers' combined coverage 1 − Π(1 − alpha).
    float4 reactive : SV_Target1;
};

[shader("fragment")]
BlendedReactive fragmentBlendedReactive(VertexOut input, bool front : SV_IsFrontFace)
{
    float4 c = shade(input, front, false, false);
    float alpha = saturate(c.a);
    BlendedReactive o;
    o.color = float4(c.rgb * alpha, alpha);
    o.reactive = float4(alpha, 0.0, 0.0, alpha);
    return o;
}

[shader("fragment")]
BlendedReactive fragmentBlendedAtmosphereReactive(VertexOut input, bool front : SV_IsFrontFace)
{
    float4 c = shade(input, front, false, false);
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(push.frame.depth_size));
    float alpha = saturate(c.a);
    BlendedReactive o;
    o.color = float4(c.rgb * alpha, alpha);
    o.reactive = float4(alpha, 0.0, 0.0, alpha);
    return o;
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
    o.color = float4(shade(input, front, false, false).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

// With motion vectors, after the depth prepass of a view with ambient occlusion.
[shader("fragment")]
ShadedMotion fragmentMainMotionAo(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = float4(shade(input, front, false, true).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

[shader("fragment")]
ShadedMotion fragmentMaskedMotion(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = float4(shade(input, front, true, false).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

// The scatter pass (ADR 0059). The forward subsurface pass left, at every pixel of a
// subsurface material, its diffuse light per unit albedo (rgb) with its depth from the
// near plane (a; 0 elsewhere), and its exit albedo with its profile's index. Drawn with
// the subsurface batches again (`depthVertex`, depth tested for equality), it adds at each
// of their pixels the albedo times the profile-weighted average of that light around it:
//
// - the share of the profile that falls inside the pixel's own footprint (its CDF at half
//   a pixel) weights the pixel itself;
// - the samples beyond it lie on a disk in the screen plane: radii importance-sampled
//   from the profile, stratified in its CDF, on a golden-angle spiral (precomputed per
//   profile and budget on the CPU), turned per pixel by interleaved gradient noise (new
//   every frame only under a temporal resolve);
// - each sample is weighted by the profile at its distance over the density it was drawn
//   with (precomputed), times a falloff with the depth between the pixels (each channel's
//   Gaussian of its mean variance: the profile at the 3D distance for one Gaussian per
//   channel), so light does not cross depth discontinuities; pixels of other materials
//   are skipped;
// - where the kernel covers few pixels a smaller budget is used, at most one sample per
//   PIXELS_PER_SAMPLE pixels of its disk;
// - each channel is renormalized by the weights it used (Golubev 2018; Jimenez et al.
//   2015), which keeps a surface lit evenly exactly as it was.
//
// A pixel whose whole kernel lies within half a pixel skips the samples: its albedo times
// its own light, as without scattering. `push.texture` and `push.phase` hold the bindless
// indices of the light and the albedo.

// Two halves (low, high bits) as floats, without 16-bit types: the exponent rebiased by a
// multiplication with 2^112, right for normal and subnormal values (Giesen, "half to float
// done quic").
float2 unpack_halves(uint bits)
{
    uint2 h = uint2(bits & 0xFFFFu, bits >> 16);
    return asfloat(((h & 0x8000u) << 16) | ((h & 0x7FFFu) << 13)) * 5.192297e33;
}

// The share of each channel's light within radius `r`: `SubsurfaceProfile::cdf` in
// subsurface.rs.
float3 profile_cdf(SubsurfaceProfile* p, float r)
{
    float3 c = 0.0;
    uint count = p.count;
    for (uint i = 0; i < count; i++)
    {
        float4 g = p.gaussians[i];
        c += g.rgb * (PI_LOG2E / -g.w) * (1.0 - exp2(g.w * r * r));
    }
    return c;
}

[shader("fragment")]
float4 scatterFragment(float4 position : SV_Position) : SV_Target0
{
    Frame* frame = push.frame;
    int2 pixel = int2(position.xy);
    Texture2D irradiance = g_textures[push.texture];
    float4 center = irradiance.Load(int3(pixel, 0));
    float4 albedo = g_textures[push.phase].Load(int3(pixel, 0));
    uint index = min(uint(albedo.a * 255.0 + 0.5), max(frame.subsurface_count, 1u) - 1u);
    SubsurfaceProfile* stored = frame.subsurface + index;
    float depth = center.a;
    // World units per pixel here, and half of it: the pixel's own footprint.
    float pixels_per_unit = frame.scatter_pixels_per_unit / max(frame.scatter_w_scale * depth + frame.scatter_w_offset, 1e-6);
    float inside = 0.5 / pixels_per_unit;
    uint budget = frame.scatter_budget;
    if (stored.radii[budget] <= inside)
        return float4(albedo.rgb * center.rgb, 0.0);
    // Fewer samples where a smaller budget already has one per PIXELS_PER_SAMPLE pixels.
    while (budget > 0)
    {
        float reach = stored.radii[budget - 1] * pixels_per_unit;
        if (float(BUDGET_SAMPLES[budget - 1]) * PIXELS_PER_SAMPLE < 3.14159265 * reach * reach)
            break;
        budget--;
    }

    float3 weights = profile_cdf(stored, inside);
    float3 sum = weights * center.rgb;
    float3 falloff = stored.falloff;
    // The spiral turned by the pixel's angle, scaled to pixels.
    float angle = 6.28318531 * gradient_noise(position.xy + 5.588238 * float(frame.scatter_frame & 63u));
    float2 turn = float2(cos(angle), sin(angle)) * pixels_per_unit;
    float inside2 = inside * inside;
    int2 last = int2(frame.depth_size) - 1;
    uint first = BUDGET_FIRST[budget];
    uint samples = BUDGET_SAMPLES[budget];
    for (uint k = 0; k < samples; k++)
    {
        uint4 entry = stored.kernel[first + k];
        float2 xy = asfloat(entry.xy);
        if (dot(xy, xy) <= inside2)
            continue;
        float2 offset = float2(xy.x * turn.x - xy.y * turn.y, xy.x * turn.y + xy.y * turn.x);
        int2 at = pixel + int2(round(offset));
        if (any(at < int2(0, 0)) || any(at > last))
            continue;
        float4 s = irradiance.Load(int3(at, 0));
        if (s.a <= 0.0)
            continue;
        float dz = s.a - depth;
        float3 w = float3(unpack_halves(entry.z), unpack_halves(entry.w).x) * exp2(falloff * (dz * dz));
        sum += w * s.rgb;
        weights += w;
    }
    return float4(albedo.rgb * sum / max(weights, 1e-30), 0.0);
}

struct SubsurfaceShaded
{
    float4 color : SV_Target0;
    float4 irradiance : SV_Target1;
    float4 albedo : SV_Target2;
};

// Subsurface materials (ADR 0059), after the prepass: the color without the scattered
// diffuse light, which goes to the other targets.
[shader("fragment")]
SubsurfaceShaded fragmentSubsurface(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    o.color = float4(shade_surface(input, front, false, true, true, sss).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

struct SubsurfaceShadedMotion
{
    float4 color : SV_Target0;
    float2 motion : SV_Target1;
    float4 irradiance : SV_Target2;
    float4 albedo : SV_Target3;
};

[shader("fragment")]
SubsurfaceShadedMotion fragmentSubsurfaceMotion(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShadedMotion o;
    o.color = float4(shade_surface(input, front, false, true, true, sss).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
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

// The environment's sky along this pixel's ray, and the sun's disk (0.27° in radius: the
// light's illuminance spread over its solid angle).
[shader("fragment")]
float4 skyFragment(Background input) : SV_Target0
{
    float2 ndc = input.position.xy / float2(push.frame.depth_size) * 2.0 - 1.0;
    // Two points of the ray: the near plane and halfway in depth (finite with an infinite
    // far plane too).
    float4 near = mul(push.frame.world_from_clip, float4(ndc, 0.0, 1.0));
    float4 mid = mul(push.frame.world_from_clip, float4(ndc, 0.5, 1.0));
    float3 d = normalize(mid.xyz / mid.w - near.xyz / near.w);
    SamplerState s = g_samplers[push.frame.linear_sampler];
    float3 color = g_cubes[push.frame.sky_cube].SampleLevel(s, d, 0.0).rgb * push.frame.env_intensity;
    const float cos_sun = 0.99998889;
    if (dot(d, push.frame.to_light.xyz) > cos_sun)
        color += push.frame.light_color.rgb / (2.0 * PI * (1.0 - cos_sun));
    return float4(color, 1.0);
}
