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
// sub-pixel offset every frame, and each pixel's motion, the texture-coordinate delta
// current − previous without jitter, goes into a target of its own: written by the depth
// prepass (`depthVertexMotion`, `prepassMotion` and their masked twins) when the view
// draws one, so the shading after it draws without; else by the shading's `*Motion`
// entry points. The previous position comes from last frame's instance transforms
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
// Shadows of local lights (local_shadows.rs, ADR 0071): lights marked as casting shadows
// get views in a persistent depth atlas (a spot light one perspective view, a point light
// six cube faces), drawn only when the light or a caster in its range changed. Only the
// `*Shadowed` entry points look them up (`shadows` is a literal everywhere, so the other
// shading carries no code for it): per sorted light `light_shadows` holds its first view
// or NO_SHADOW; a point light's face is its major axis; four bilinear comparisons make a
// 3 × 3 tent filter. `cullShadowViews` culls the instances per drawn view and
// `localShadowVertex` draws them into the view's tile.
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
// The sun's shadow map: a layer per cascade (ADR 0093).
[[vk::binding(0, 0)]] Texture2DArray g_texture_arrays[];
// The sun's contact shadow mask: a float's bits (contact.rs, CONTACT_FORMAT).
[[vk::binding(0, 0)]] Texture2D<uint> g_uint_textures[];
[[vk::binding(0, 0)]] Texture2D<uint2> g_uint2_textures[];
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

// `CascadeData` in forward/mod.rs: one cascade of the sun's shadows, in a layer of the
// map: a tile of its only layer drawn whole every frame, or a layer of its own in a
// view's cache, addressed wrap-around (ADR 0093).
struct Cascade
{
    float4 scale;            // xyz: light view → cascade NDC scale; w: world size of a texel
    float4 offset;           // xyz: offset; w: NDC edge within which the filter stays inside
    float4 atlas;            // cascade NDC → uv of its layer: xy scale, zw offset (its tile, or where its window starts)
    float4 drawn[4];         // light view rectangles (x0, y0, x1, y1) drawn this frame (strips, moved casters); an empty one has x0 > x1
    float far;               // light view far depth (−z) of its projection
    float lod_error;         // world-space error its casters' detail levels may have (0: full meshes)
    // Drawn in several pieces at once (a view's cache, ADR 0093; `cullMeshletPieces`): the
    // light view x past which columns are past the layer's seam, the y below which rows
    // are; the pieces drawn (bit r * 4 + q, see `reached_pieces`); and where its meshlet
    // draws' pieces are in `visible`, from their slots.
    float2 seam;
    uint pieces;
    uint masks;
    uint rects;              // rectangles in `drawn` (they come first)
    uint _pad0;
};

// `ShadowViewData` in local_shadows.rs: one view of a light's shadow in the atlas.
struct ShadowView
{
    float4x4 clip_from_world; // the light's view and projection
    float4 atlas;            // NDC → atlas uv: xy scale, zw offset
    float4 eye_bias;         // xyz light position; w normal offset per unit of distance (world)
    float4 tile;             // drawn views: x, y, size of the tile in texels; w pixels per unit at
                             // distance 1 over the LOD threshold. Looked-up point light faces: x
                             // 1 / tan(half angle), y and z the depth's A and B (A + B / major)
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

// The meshlet path (ADR 0085), per batch: each level's meshlets in the geometry pool's
// index buffer (`BatchMeshletData` in forward/mod.rs).
struct BatchMeshlets
{
    uint4 first;             // per level: its first meshlet, as an index of `Frame.meshlets`
    uint4 count;             // per level: its meshlets (0: the level is drawn whole)
    uint state;              // the batch's pipeline state: the list its meshlet draws go to
    uint cone;               // 1: meshlets facing away may be culled (single-sided)
    uint _pad0;
    uint _pad1;
};

// A meshlet (`engine_render::mesh::Meshlet`, 32 bytes).
struct Meshlet
{
    float3 center;           // bounding sphere, object space
    float radius;
    uint cone;               // normal cone: axis x, y, z and cutoff, snorm8 over 127 (x lowest)
    uint first_index;        // relative to its level's
    uint triangles;
    uint reserved;
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
    float lod_scale;         // pixels per object unit at distance 1 over the LOD threshold (0: full meshes)
    uint slot_count;         // instance slots per command list
    uint4 hiz_offsets[4];    // offset of level k + 1 in the pyramid, in floats
    float4x4 previous_clip_from_world;  // last frame's camera, unjittered
    float4x4 previous_clip_from_clip;   // for the background: this frame's clip space
                                        // (unjittered) to last frame's
    float4 jitter;           // xy: this frame's jitter in NDC (clip.xy += jitter × w)
    uint previous_count;     // instances with a previous transform
    uint scatter_budget;     // the subsurface budget, BUDGET_* (ADR 0059)
    uint* decal_tiles;       // per 16-pixel tile a bit: decals reach it (ADR 0082; decal entry points only)
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
    float2 sky_turn;         // the sky pass: sine and cosine of the sky cube's turn about +Y (an HDR panorama's)
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
    uint local_shadow_atlas; // bindless: the local lights' shadow atlas (ADR 0071)
    float sun_disk;          // the sky pass: the cosine of the light's disk (above 1: none, a panorama's sky)
    uint* light_shadows;     // per sorted light: its first view in `shadow_views`, or NO_SHADOW
    ShadowView* shadow_views; // the views of the shadowed lights in view
    ShadowView* shadow_draws; // the views drawn this frame (culling and drawing)
    uint shadow_draw_count;
    float shadow_texel;      // 1 / atlas size
    float gi_scale;          // probe GI (ADR 0088): pixels per resolved sample along each axis
    uint2 gi_samples;        // and the samples' grid
    uint cascade_sampler;    // the cascades' comparison sampler: repeat (their layers wrap around, ADR 0093)
    uint* material_data;     // material types' parameters (ADR 0092): `material_stride` bytes per slot
    uint material_stride;
    float cascade_layers;    // 1: cascade c is layer c of the map (a view's cache, ADR 0093); 0: every cascade is a tile of layer 0
    BatchMeshlets* batch_meshlets; // the meshlet entry points (ADR 0085): per batch, its levels' meshlets
    Meshlet* meshlets;       // the geometry pool's index buffer as meshlets (levels' meshlets follow their indices, 32-byte aligned)
    // Meshlet lists (0: all or early, 1: late, 2 + k: shadow cascade k): bytes from the
    // `commands` of their culling to them, and the first slot of their draws in `visible`.
    uint meshlet_offset[6];
    uint meshlet_slot[6];
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
    float distance = max(length(center - read_float4(&push.frame.camera_position).xyz) - length(extents), 1e-3);
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
// `NO_SHADOW` in local_shadows.rs: a light without a shadow.
static const uint NO_SHADOW = 0xFFFFFFFFu;

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
    uint decal_albedo;       // bindless index: the decal buffer's albedo (decal entry points; ADR 0082)
    uint decal_surface;      // bindless index: its normal change, roughness and metalness
    uint gi;                 // bindless index: the view's diffuse irradiance per pixel (GI entry points; ADR 0088)
    uint _pad;               // `Push` in forward/mod.rs is 128 bytes
};

static const uint PHASE_ALL = 0;    // frustum culling only
static const uint PHASE_EARLY = 1;  // visible last frame and in the frustum
static const uint PHASE_LATE = 2;   // visible now and not drawn by the early phase
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

// Reads of data the engine lays out 16-byte aligned (every element a multiple of 16 bytes
// from a 16-byte aligned base: instances, batches, attributes, materials, lights, shadow
// views, the frame's matrices) declare it. A plain pointer access is declared 4-byte
// aligned (Slang's default for pointers), and a driver may split a 16-byte load in four.
Instance read_instance(Instance* instances, uint index)
{
    return loadAligned<16>(instances + index);
}

Batch read_batch(uint index)
{
    return loadAligned<16>(push.batches + index);
}

Attributes read_attributes(uint vertex)
{
    return loadAligned<16>(push.attributes + vertex);
}

Material read_material(uint index)
{
    return loadAligned<16>(push.materials + index);
}

Light read_light(uint index)
{
    return loadAligned<16>(push.frame.lights + index);
}

ShadowView read_shadow_view(ShadowView* views, uint index)
{
    return loadAligned<16>(views + index);
}

float4x4 read_matrix(float4x4* m)
{
    return loadAligned<16>(m);
}

float4 read_float4(float4* v)
{
    return loadAligned<16>(v);
}


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
    float4x4 m = transpose(read_matrix(&push.frame.clip_from_world));
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
    return ((loadAligned<8>(&push.frame.depth_size) - 1) >> level) + 1;
}

// Whether a world-space box is hidden: its nearest point is farther than the farthest
// depth of the pyramid over the screen rectangle it covers (at most 2×2 texels of the
// level where that rectangle fits).
bool occluded(float3 center, float3 extents)
{
    float4x4 m = read_matrix(&push.frame.clip_from_world);
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
    float2 size = float2(loadAligned<8>(&push.frame.depth_size));
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

// The meshlet lists of a forward phase or shadow cascade (ADR 0085; `MeshletLayout` in
// forward/layout.rs), in the command buffer after the command lists: this header, then
// the work items, a chunk of up to `MESHLET_CHUNK` meshlets of an instance taking the
// meshlet path each (`uint2`: the instance, its level | state << 8 | chunk << 16), then the
// draws, per state from `offset[state]`.
struct MeshletHeader
{
    uint drawn[32];          // per state: meshlet draws written (the draws' count)
    uint reserved[32];       // per state: draws reserved by the instances taking the path
    uint offset[32];         // per state: its first draw among the lists' draws
    uint capacity[32];       // per state: its draws at most
    uint args[3];            // the meshlet cull's dispatch: work items, 1, 1
    uint work;               // room for work items (the draws follow them)
};

static const uint MESHLET_HEADER_BYTES = 528;
// Meshlets per work item of the meshlet cull: one per lane of its group.
static const uint MESHLET_CHUNK = 64;

// `push.phase` of the meshlet cull of shadow cascade k: this plus k.
static const uint PHASE_SHADOW_MESHLETS = 16;
// `SUBSURFACE` in forward/mod.rs: the pipeline state bit of subsurface materials.
static const uint STATE_SUBSURFACE = 16;

// The meshlet lists of this dispatch's phase: 0 (all or early), 1 (late), 2 + k (shadow
// cascade k, in the meshlet cull).
uint meshlet_list()
{
    if (push.phase >= PHASE_SHADOW_MESHLETS)
        return 2 + push.phase - PHASE_SHADOW_MESHLETS;
    return push.phase == PHASE_LATE ? 1 : 0;
}

// Meshlet lists `list`. Addresses are computed as 64-bit integers (`shaderInt64`): AMD's
// compiler (driver 26.8.1) reads zeros through a pointer cast after pointer arithmetic
// (see `material_params`).
uint64_t meshlet_lists(uint list)
{
    return uint64_t(push.commands) + push.frame.meshlet_offset[list];
}

BatchMeshlets read_batch_meshlets(uint batch)
{
    return loadAligned<16>(push.frame.batch_meshlets + batch);
}

// Whether instance `index`, visible in meshlet lists `list` at level `lod`, is drawn by
// meshlet: its level has meshlets and its state's lists have room for all of them. Its
// meshlets are then work items of the meshlet cull (`cullMeshlets`), a chunk of
// `MESHLET_CHUNK` each, instead of an instance of its command. The lists' room for work
// items covers every chunk: the chunks of the instances that fit fill at most one more
// work item each than their meshlets would fill.
bool take_meshlets(uint batch, uint index, uint lod, uint list)
{
    BatchMeshlets bm = read_batch_meshlets(batch);
    uint count = bm.count[lod];
    if (count == 0)
        return false;
    uint64_t lists = meshlet_lists(list);
    MeshletHeader* header = (MeshletHeader*)lists;
    uint base;
    InterlockedAdd(header.reserved[bm.state], count, base);
    if (base + count > header.capacity[bm.state])
        return false;
    uint chunks = (count + MESHLET_CHUNK - 1) / MESHLET_CHUNK;
    uint item;
    InterlockedAdd(header.args[0], chunks, item);
    uint2* work = (uint2*)(lists + MESHLET_HEADER_BYTES);
    for (uint c = 0; c < chunks; c++)
        work[item + c] = uint2(index, lod | (bm.state << 8) | (c << 16));
    return true;
}

// One thread per instance: frustum (and occlusion) test of the instance's world-space
// bounding box, then append to the batch's visible list (or, with `meshlets`, to the
// meshlet cull's work: `take_meshlets`). The shadow cascades cull in `cull_shadow_instance`.
void cull_instance(uint index, bool meshlets)
{
    if (index >= push.instance_count)
        return;
    Instance inst = read_instance(push.instances, index);
    Batch batch = read_batch(inst.batch);
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
    bool visible = true;
    if (cull)
    {
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            float4 plane = read_float4(&push.frame.planes[i]);
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
    // Blended batches have no meshlet lists (their count is 0).
    bool whole = visible;
    if (meshlets && visible && take_meshlets(inst.batch, index, lod, push.phase == PHASE_LATE ? 1 : 0))
        whole = false;
    append(push.commands, push.visible, command, first, index, whole);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void cullMain(uint3 id : SV_DispatchThreadID)
{
    cull_instance(id.x, false);
}

// Whether the light view box `c` ± `e` reaches the light view rectangle `r` (x0, y0, x1,
// y1; never an empty one, x0 > x1).
bool reaches(float4 r, float3 c, float3 e)
{
    return c.x + e.x >= r.x && c.x - e.x <= r.z && c.y + e.y >= r.y && c.y - e.y <= r.w;
}

// The coarsest detail level whose error, for an instance whose largest scale is `scale`,
// stays within `allowed` world units (none: the full mesh).
uint lod_within(Batch batch, float scale, float allowed)
{
    uint lods = batch.lods & LOD_COUNT_MASK;
    if (lods <= 1 || allowed <= 0.0)
        return 0;
    uint lod = 0;
    for (uint k = 1; k < lods; k++)
    {
        if (batch.errors[k] * scale <= allowed)
            lod = k;
    }
    return lod;
}

// The sun's cascades' cull (ADR 0093): one thread per instance. An instance casts into a
// cascade when its world box, in light view space, reaches a rectangle the cascade draws
// this frame and is not wholly beyond its far depth (anything between the light and the
// slice casts into it). It is appended to the cascade's lists (or, with `meshlets`, to its
// meshlet cull's work) at the detail level the cascade's texels allow: what a cascade
// draws never depends on the camera, so the texels it keeps from earlier frames stay what
// it would draw now.
void cull_shadow_instance(uint index, bool meshlets)
{
    if (index >= push.instance_count)
        return;
    Instance inst = read_instance(push.instances, index);
    Batch batch = read_batch(inst.batch);
    float3 center = transform_point(inst, batch.center.xyz);
    float3 e = batch.extents.xyz;
    float3 extents = float3(dot(abs(inst.row0.xyz), e), dot(abs(inst.row1.xyz), e), dot(abs(inst.row2.xyz), e));
    bool cull = push.frame.cull != 0;
    // Blended batches cast no shadows.
    bool blended = (batch.lods & BATCH_BLENDED) != 0;
    // The box in light view space (Arvo again: the light view is a rotation).
    float4x4 light = read_matrix(&push.frame.light_from_world);
    float3 c = mul(light, float4(center, 1.0)).xyz;
    float3 le = float3(dot(abs(light[0].xyz), extents), dot(abs(light[1].xyz), extents), dot(abs(light[2].xyz), extents));
    // The instance's largest scale (its columns' lengths).
    float3 c0 = float3(inst.row0.x, inst.row1.x, inst.row2.x);
    float3 c1 = float3(inst.row0.y, inst.row1.y, inst.row2.y);
    float3 c2 = float3(inst.row0.z, inst.row1.z, inst.row2.z);
    float scale = sqrt(max(dot(c0, c0), max(dot(c1, c1), dot(c2, c2))));
    uint cascades = push.frame.cascade_count;
    for (uint k = 0; k < cascades; k++)
    {
        // A cascade drawing nothing this frame takes nothing, even without culling. Moved
        // casters' clusters after the strips are tested only when there are any: the cull
        // keeps step 1's registers (ADR 0093).
        float4 r0 = read_float4(&push.frame.cascades[k].drawn[0]);
        float4 r1 = read_float4(&push.frame.cascades[k].drawn[1]);
        bool drawn = r0.x <= r0.z || r1.x <= r1.z;
        bool reached = reaches(r0, c, le) || reaches(r1, c, le);
        if (push.frame.cascades[k].rects > 2)
            reached = reached || reaches(read_float4(&push.frame.cascades[k].drawn[2]), c, le) || reaches(read_float4(&push.frame.cascades[k].drawn[3]), c, le);
        reached = reached && -(c.z + le.z) <= push.frame.cascades[k].far;
        bool caster = !blended && drawn && (!cull || reached);
        uint lod = lod_within(batch, scale, push.frame.cascades[k].lod_error);
        uint command = batch.command + lod;
        uint first = batch.slot + lod * batch.count;
        // With meshlets, a caster of the cascade may be its meshlet cull's work item.
        bool whole = caster;
        if (meshlets && caster && take_meshlets(inst.batch, index, lod, 2 + k))
            whole = false;
        append(push.commands + k * push.frame.command_count, push.visible + k * push.frame.slot_count, command, first, index, whole);
    }
}

[shader("compute")]
[numthreads(64, 1, 1)]
void cullShadows(uint3 id : SV_DispatchThreadID)
{
    cull_shadow_instance(id.x, false);
}

// `cullShadows` with the meshlet path (ADR 0085).
[shader("compute")]
[numthreads(64, 1, 1)]
void cullShadowsMeshlets(uint3 id : SV_DispatchThreadID)
{
    cull_shadow_instance(id.x, true);
}

// `cullMain` with the meshlet path (ADR 0085): its own entry point, so views without
// meshlets keep the plain one's registers.
[shader("compute")]
[numthreads(64, 1, 1)]
void cullMainMeshlets(uint3 id : SV_DispatchThreadID)
{
    cull_instance(id.x, true);
}

// A snorm8 over 127 of `bits` at `shift`.
float snorm8(uint bits, uint shift)
{
    return float(int(bits << (24 - shift)) >> 24) / 127.0;
}

// The meshlet cull's chunk: which of its meshlets are drawn, and where each one's indices
// end (relative to its level's).
groupshared uint meshlet_drawn[MESHLET_CHUNK];
groupshared uint meshlet_end[MESHLET_CHUNK];

// The meshlet cull (ADR 0085): one group per work item of `cull_instance`, a lane per
// meshlet of its chunk (a group never loops: a large instance's chunks run side by side).
// A meshlet is drawn unless its bounding sphere is outside the frustum, its normal cone
// faces away from the camera (single-sided batches, uniformly scaled instances), or, in
// the late phase, the depth pyramid hides it. Each drawn meshlet is one indexed draw of
// one instance in its state's list; in a shadow cascade, each run of consecutive drawn
// meshlets of the chunk is (a level's meshlets' indices follow each other): depth-only
// passes pay for every draw, while the view's passes gained nothing from runs on the RTX
// and its shading lost on the Radeon iGPU (ADR 0085). Nothing about a meshlet is kept
// from frame to frame (ADR 0035's patent note).
[shader("compute")]
[numthreads(64, 1, 1)]
void cullMeshlets(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    uint list = meshlet_list();
    uint64_t lists = meshlet_lists(list);
    MeshletHeader* header = (MeshletHeader*)lists;
    uint2* work = (uint2*)(lists + MESHLET_HEADER_BYTES);
    DrawCommand* draws = (DrawCommand*)(lists + MESHLET_HEADER_BYTES + 8 * uint64_t(header.work));
    uint2 item = work[group.x];
    uint index = item.x;
    uint lod = item.y & 0xFFu;
    uint state = (item.y >> 8) & 0xFFu;
    uint m = (item.y >> 16) * MESHLET_CHUNK + local;
    Instance inst = read_instance(push.instances, index);
    Batch batch = read_batch(inst.batch);
    BatchMeshlets bm = read_batch_meshlets(inst.batch);
    // Lanes past the level's last meshlet take part in the chunk's runs as not drawn.
    bool valid = m < bm.count[lod];
    DrawCommand level = push.commands[batch.command + lod];
    // The instance's scale: a sphere grows by the largest; the cone keeps its angle only
    // when all three agree.
    float3 c0 = float3(inst.row0.x, inst.row1.x, inst.row2.x);
    float3 c1 = float3(inst.row0.y, inst.row1.y, inst.row2.y);
    float3 c2 = float3(inst.row0.z, inst.row1.z, inst.row2.z);
    float3 lengths = float3(dot(c0, c0), dot(c1, c1), dot(c2, c2));
    float largest = max(lengths.x, max(lengths.y, lengths.z));
    float smallest = min(lengths.x, min(lengths.y, lengths.z));
    float scale = sqrt(largest);
    bool cone = bm.cone != 0 && smallest >= largest * 0.999;
    float3 eye = read_float4(&push.frame.camera_position).xyz;
    bool cull = push.frame.cull != 0;
    uint first_slot = push.frame.meshlet_slot[list];
    // A shadow cascade's cull: the rectangles it draws this frame in light view space, and
    // the light's direction. The shadow pass draws single-sided geometry's faces away from
    // the light (front faces culled), subsurface materials' faces towards it: a meshlet all
    // of whose faces the rasterizer would cull is not drawn.
    bool shadow = list >= 2;
    uint cascade = shadow ? list - 2 : 0;
    // Read here, not in the test: fewer registers made the RTX run more groups at once,
    // which shuffled the draws they append and cost the depth tests of the passes drawing
    // them (Sponza: prepass +8 %, shadows +9 %; ADR 0093).
    float4 r0 = shadow ? read_float4(&push.frame.cascades[cascade].drawn[0]) : float4(0.0);
    float4 r1 = shadow ? read_float4(&push.frame.cascades[cascade].drawn[1]) : float4(0.0);
    float far = shadow ? push.frame.cascades[cascade].far : 0.0;
    float4x4 light = read_matrix(&push.frame.light_from_world);
    float3 culled_side = read_float4(&push.frame.to_light).xyz * ((state & STATE_SUBSURFACE) != 0 ? -1.0 : 1.0);
    Meshlet meshlet = loadAligned<16>(push.frame.meshlets + bm.first[lod] + min(m, bm.count[lod] - 1));
    float3 center = transform_point(inst, meshlet.center);
    float radius = meshlet.radius * scale;
    bool visible = valid;
    if (cull && shadow)
    {
        // Reaching a rectangle the cascade draws, and not wholly beyond its far depth.
        float3 c = mul(light, float4(center, 1.0)).xyz;
        float3 e = float3(radius, radius, radius);
        visible = reaches(r0, c, e) || reaches(r1, c, e);
        // Moved casters' clusters after the strips, only when there are any (the Radeon's
        // registers for all four rectangles at the top: 53 → 64).
        if (!visible && push.frame.cascades[cascade].rects > 2)
            visible = reaches(read_float4(&push.frame.cascades[cascade].drawn[2]), c, e) || reaches(read_float4(&push.frame.cascades[cascade].drawn[3]), c, e);
        visible = visible && -(c.z + radius) <= far;
        // The light is a direction: the cone test from infinitely far away.
        if (visible && cone && (meshlet.cone >> 24) != 127u)
        {
            float3 axis = normalize(transform_normal(inst, float3(snorm8(meshlet.cone, 0), snorm8(meshlet.cone, 8), snorm8(meshlet.cone, 16))));
            visible = dot(culled_side, axis) < snorm8(meshlet.cone, 24);
        }
    }
    else if (cull)
    {
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            float4 plane = read_float4(&push.frame.planes[i]);
            visible = visible && dot(plane.xyz, center) + plane.w >= -radius;
        }
        if (visible && cone && (meshlet.cone >> 24) != 127u)
        {
            float3 axis = normalize(transform_normal(inst, float3(snorm8(meshlet.cone, 0), snorm8(meshlet.cone, 8), snorm8(meshlet.cone, 16))));
            float cutoff = snorm8(meshlet.cone, 24);
            float3 d = center - eye;
            visible = dot(d, axis) < cutoff * length(d) + radius;
        }
        if (visible && push.phase == PHASE_LATE)
            visible = !occluded(center, float3(radius, radius, radius));
    }
    // A shadow cascade's runs: a drawn meshlet after one that is not starts one, which
    // takes in the drawn meshlets after it (`shadow` is the same in the whole dispatch).
    bool first = visible;
    uint end = meshlet.first_index + meshlet.triangles * 3;
    if (shadow)
    {
        meshlet_drawn[local] = visible ? 1u : 0u;
        meshlet_end[local] = end;
        GroupMemoryBarrierWithGroupSync();
        first = visible && (local == 0 || meshlet_drawn[local - 1] == 0u);
        uint last = local;
        if (first)
        {
            while (last + 1 < MESHLET_CHUNK && meshlet_drawn[last + 1] != 0u)
                last++;
        }
        end = meshlet_end[last];
    }
    uint count = WaveActiveCountBits(first);
    uint base = 0;
    if (WaveIsFirstLane() && count > 0)
        InterlockedAdd(header.drawn[state], count, base);
    base = WaveReadLaneFirst(base);
    if (first)
    {
        uint at = header.offset[state] + base + WavePrefixCountBits(first);
        DrawCommand draw;
        draw.index_count = end - meshlet.first_index;
        draw.instance_count = 1;
        draw.first_index = level.first_index + meshlet.first_index;
        draw.vertex_offset = level.vertex_offset;
        draw.first_instance = first_slot + at;
        draws[at] = draw;
        push.visible[first_slot + at] = index;
    }
}

// The pieces of cascade `cascade` a sphere at `c` in light view space (radius `e`) reaches,
// of those it draws (ADR 0093): bit `r * 4 + q` for the part of rectangle `r` in quadrant
// `q` of its layer (`q & 2`: past the column seam, `q & 1`: past the row seam), each part
// the rectangle cut at the seams' light view lines.
uint reached_pieces(uint cascade, float3 c, float e)
{
    float2 seam = push.frame.cascades[cascade].seam;
    uint reached = 0;
    [unroll]
    for (int r = 0; r < 4; r++)
    {
        float4 rect = read_float4(&push.frame.cascades[cascade].drawn[r]);
        [unroll]
        for (int q = 0; q < 4; q++)
        {
            // Columns past the seam lie at x >= seam.x, rows past it at y <= seam.y (rows
            // grow towards −y).
            float4 part = rect;
            if ((q & 2) != 0)
                part.x = max(part.x, seam.x);
            else
                part.z = min(part.z, seam.x);
            if ((q & 1) != 0)
                part.w = min(part.w, seam.y);
            else
                part.y = max(part.y, seam.y);
            if (part.x <= part.z && part.y <= part.w && reaches(part, c, float3(e, e, e)))
                reached |= 1u << uint(r * 4 + q);
        }
    }
    return reached & push.frame.cascades[cascade].pieces;
}

// The meshlet cull of a shadow cascade drawn in several pieces this frame (ADR 0093): as
// `cullMeshlets` for the cascades, with each run of drawn meshlets reaching the same
// pieces one draw of an instance per piece; the pieces go beside the run's slot, where the
// vertex shader finds each instance's viewport (`shadowVertexPieces`). A piece drawn on its
// own would draw every caster any of the cascade's rectangles reaches. Its own entry point:
// the meshlet cull keeps its registers (and so the order of its draws, see there).
[shader("compute")]
[numthreads(64, 1, 1)]
void cullMeshletPieces(uint3 group : SV_GroupID, uint local : SV_GroupIndex)
{
    uint list = meshlet_list();
    uint64_t lists = meshlet_lists(list);
    MeshletHeader* header = (MeshletHeader*)lists;
    uint2* work = (uint2*)(lists + MESHLET_HEADER_BYTES);
    DrawCommand* draws = (DrawCommand*)(lists + MESHLET_HEADER_BYTES + 8 * uint64_t(header.work));
    uint2 item = work[group.x];
    uint index = item.x;
    uint lod = item.y & 0xFFu;
    uint state = (item.y >> 8) & 0xFFu;
    uint m = (item.y >> 16) * MESHLET_CHUNK + local;
    Instance inst = read_instance(push.instances, index);
    Batch batch = read_batch(inst.batch);
    BatchMeshlets bm = read_batch_meshlets(inst.batch);
    bool valid = m < bm.count[lod];
    DrawCommand level = push.commands[batch.command + lod];
    float3 c0 = float3(inst.row0.x, inst.row1.x, inst.row2.x);
    float3 c1 = float3(inst.row0.y, inst.row1.y, inst.row2.y);
    float3 c2 = float3(inst.row0.z, inst.row1.z, inst.row2.z);
    float3 lengths = float3(dot(c0, c0), dot(c1, c1), dot(c2, c2));
    float largest = max(lengths.x, max(lengths.y, lengths.z));
    float smallest = min(lengths.x, min(lengths.y, lengths.z));
    float scale = sqrt(largest);
    bool cone = bm.cone != 0 && smallest >= largest * 0.999;
    uint first_slot = push.frame.meshlet_slot[list];
    uint cascade = list - 2;
    float3 culled_side = read_float4(&push.frame.to_light).xyz * ((state & STATE_SUBSURFACE) != 0 ? -1.0 : 1.0);
    Meshlet meshlet = loadAligned<16>(push.frame.meshlets + bm.first[lod] + min(m, bm.count[lod] - 1));
    // Without culling every meshlet is drawn into every piece.
    uint pieces = valid ? push.frame.cascades[cascade].pieces : 0u;
    if (valid && push.frame.cull != 0)
    {
        float3 center = transform_point(inst, meshlet.center);
        float radius = meshlet.radius * scale;
        float3 c = mul(read_matrix(&push.frame.light_from_world), float4(center, 1.0)).xyz;
        // Reaching pieces the cascade draws, and not wholly beyond its far depth.
        pieces = -(c.z + radius) <= push.frame.cascades[cascade].far ? reached_pieces(cascade, c, radius) : 0u;
        // The light is a direction: the cone test from infinitely far away.
        if (pieces != 0 && cone && (meshlet.cone >> 24) != 127u)
        {
            float3 axis = normalize(transform_normal(inst, float3(snorm8(meshlet.cone, 0), snorm8(meshlet.cone, 8), snorm8(meshlet.cone, 16))));
            if (!(dot(culled_side, axis) < snorm8(meshlet.cone, 24)))
                pieces = 0;
        }
    }
    // A run: a drawn meshlet after one that is not drawn or reaches other pieces starts
    // one, which takes in the meshlets after it that reach the same pieces.
    meshlet_drawn[local] = pieces;
    meshlet_end[local] = meshlet.first_index + meshlet.triangles * 3;
    GroupMemoryBarrierWithGroupSync();
    bool first = pieces != 0 && (local == 0 || meshlet_drawn[local - 1] != pieces);
    uint last = local;
    if (first)
    {
        while (last + 1 < MESHLET_CHUNK && meshlet_drawn[last + 1] == pieces)
            last++;
    }
    uint end = meshlet_end[last];
    uint count = WaveActiveCountBits(first);
    uint base = 0;
    if (WaveIsFirstLane() && count > 0)
        InterlockedAdd(header.drawn[state], count, base);
    base = WaveReadLaneFirst(base);
    if (first)
    {
        uint at = header.offset[state] + base + WavePrefixCountBits(first);
        DrawCommand draw;
        draw.index_count = end - meshlet.first_index;
        draw.instance_count = countbits(pieces);
        draw.first_index = level.first_index + meshlet.first_index;
        draw.vertex_offset = level.vertex_offset;
        draw.first_instance = first_slot + at;
        draws[at] = draw;
        push.visible[first_slot + at] = index;
        push.visible[first_slot + at + push.frame.cascades[cascade].masks] = pieces;
    }
}

// The coarsest detail level a shadow view seen from `eye` with `lod_scale` pixels per unit
// at distance 1 (over the threshold) may draw: `choose_lod` from the light.
uint choose_lod_from(Instance inst, Batch batch, float3 center, float3 extents, float3 eye, float lod_scale)
{
    uint lods = batch.lods & LOD_COUNT_MASK;
    if (lods <= 1 || lod_scale <= 0.0)
        return 0;
    float3 c0 = float3(inst.row0.x, inst.row1.x, inst.row2.x);
    float3 c1 = float3(inst.row0.y, inst.row1.y, inst.row2.y);
    float3 c2 = float3(inst.row0.z, inst.row1.z, inst.row2.z);
    float scale = sqrt(max(dot(c0, c0), max(dot(c1, c1), dot(c2, c2))));
    float distance = max(length(center - eye) - length(extents), 1e-3);
    float allowed = distance / (scale * lod_scale);
    uint lod = 0;
    for (uint k = 1; k < lods; k++)
    {
        if (batch.errors[k] <= allowed)
            lod = k;
    }
    return lod;
}

// Local light shadows (ADR 0071): one thread per instance and drawn view (y). An instance
// casts into a view when its box is inside the view's frustum (planes from its matrix,
// Gribb and Hartmann); it is appended to the view's command list and caster list.
[shader("compute")]
[numthreads(64, 1, 1)]
void cullShadowViews(uint3 id : SV_DispatchThreadID)
{
    uint index = id.x;
    uint view = id.y;
    if (index >= push.instance_count)
        return;
    Instance inst = read_instance(push.instances, index);
    Batch batch = read_batch(inst.batch);
    float3 center = transform_point(inst, batch.center.xyz);
    float3 e = batch.extents.xyz;
    float3 extents = float3(dot(abs(inst.row0.xyz), e), dot(abs(inst.row1.xyz), e), dot(abs(inst.row2.xyz), e));
    ShadowView v = read_shadow_view(push.frame.shadow_draws, view);
    // Rows of the view's matrix (the planes are sums and differences of them).
    float4x4 m = v.clip_from_world;
    float4 r0 = m[0];
    float4 r1 = m[1];
    float4 r2 = m[2];
    float4 r3 = m[3];
    float4 planes[6] = { r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2 };
    bool caster = (batch.lods & BATCH_BLENDED) == 0;
    if (push.frame.cull != 0)
    {
        [unroll]
        for (int i = 0; i < 6; i++)
        {
            float4 plane = planes[i];
            caster = caster && dot(plane.xyz, center) + dot(abs(plane.xyz), extents) + plane.w >= 0.0;
        }
    }
    uint lod = choose_lod_from(inst, batch, center, extents, v.eye_bias.xyz, v.tile.w);
    uint command = batch.command + lod;
    uint first = batch.slot + lod * batch.count;
    append(push.commands + view * push.frame.command_count, push.visible + view * push.frame.slot_count, command, first, index, caster);
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
    Instance inst = read_instance(push.instances, index);
    Attributes a = read_attributes(vertex);
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
    float4x4 light = read_matrix(&push.frame.light_from_world);
    o.light_position = mul(light, world).xyz;
    o.light_normal = mul((float3x3)light, o.normal);
    // Without a previous transform the instance counts as still.
    Instance previous = index < push.frame.previous_count ? read_instance(push.previous_instances, index) : inst;
    o.previous_clip = mul(read_matrix(&push.frame.previous_clip_from_world), float4(transform_point(previous, p), 1.0));
    o.material = inst.material;
    return o;
}

// A world position in the clip space of cascade `push.phase`. A cascade's fields are read
// one by one, never the whole: the compiler keeps what it loads (ADR 0093).
float4 shadow_clip(float3 world)
{
    float4 scale = read_float4(&push.frame.cascades[push.phase].scale);
    float4 offset = read_float4(&push.frame.cascades[push.phase].offset);
    float3 light = mul(read_matrix(&push.frame.light_from_world), float4(world, 1.0)).xyz;
    return float4(light * scale.xyz + offset.xyz, 1.0);
}

// Drawn with a cascade's culled commands: instance `slot` is a slot of its caster list.
// Only positions are fetched.
[shader("vertex")]
float4 shadowVertex(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    return shadow_clip(transform_point(inst, load_position(vertex)));
}

// A local light's shadow view (`push.phase`: the drawn view): positions only.
[shader("vertex")]
float4 localShadowVertex(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    float3 world = transform_point(inst, load_position(vertex));
    return mul(read_matrix(&push.frame.shadow_draws[push.phase].clip_from_world), float4(world, 1.0));
}

// The depth prepass: the visible instances' depth, positions only.
[shader("vertex")]
float4 depthVertex(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID) : SV_Position
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
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
    Instance inst = read_instance(push.instances, push.visible[slot]);
    ShadowOut o;
    o.position = shadow_clip(transform_point(inst, load_position(vertex)));
    o.uv = read_attributes(vertex).uv;
    o.material = inst.material;
    return o;
}

// A cascade's pieces drawn at once (ADR 0093): instance `i` of a meshlet run's draw (whose
// first instance is its slot) goes to the viewport of the run's `i`-th piece, its pieces
// beside the slot (`cullMeshletPieces`).
uint piece_viewport(uint slot, uint i)
{
    uint pieces = push.visible[slot + push.frame.cascades[push.phase].masks];
    for (uint k = 0; k < i; k++)
        pieces &= pieces - 1;
    return firstbitlow(pieces);
}

struct PieceOut
{
    float4 position : SV_Position;
    uint viewport : SV_ViewportArrayIndex;
};

// `shadowVertex` for a cascade's pieces drawn at once.
[shader("vertex")]
PieceOut shadowVertexPieces(uint vertex : SV_VulkanVertexID, uint instance : SV_VulkanInstanceID, uint slot : SV_StartInstanceLocation)
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    PieceOut o;
    o.position = shadow_clip(transform_point(inst, load_position(vertex)));
    o.viewport = piece_viewport(slot, instance - slot);
    return o;
}

struct ShadowPieceOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    nointerpolation uint material : MATERIAL;
    uint viewport : SV_ViewportArrayIndex;
};

// `shadowVertexMasked` for a cascade's pieces drawn at once.
[shader("vertex")]
ShadowPieceOut shadowVertexMaskedPieces(uint vertex : SV_VulkanVertexID, uint instance : SV_VulkanInstanceID, uint slot : SV_StartInstanceLocation)
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    ShadowPieceOut o;
    o.position = shadow_clip(transform_point(inst, load_position(vertex)));
    o.uv = read_attributes(vertex).uv;
    o.material = inst.material;
    o.viewport = piece_viewport(slot, instance - slot);
    return o;
}

// Local light shadows of masked materials.
[shader("vertex")]
ShadowOut localShadowVertexMasked(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    ShadowOut o;
    float3 world = transform_point(inst, load_position(vertex));
    o.position = mul(read_matrix(&push.frame.shadow_draws[push.phase].clip_from_world), float4(world, 1.0));
    o.uv = read_attributes(vertex).uv;
    o.material = inst.material;
    return o;
}

// The depth prepass of masked materials: texture coordinates for the alpha test
// (`shadowFragmentMasked`).
[shader("vertex")]
ShadowOut depthVertexMasked(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    Instance inst = read_instance(push.instances, push.visible[slot]);
    ShadowOut o;
    o.position = clip_position(inst, load_position(vertex)).clip;
    o.uv = read_attributes(vertex).uv;
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

// Materials (ADR 0092): a material is a type implementing `IMaterial`. It turns what the
// pass interpolated into a `Surface`; the engine lights the surface. The passes are generic
// over the material, so Slang emits one entry point per material type drawn and a material
// pays only for its own code (the built-in glTF material through the interface compiles
// to the same SPIR-V as written out).

// What a material reads: the interpolants of the fragment and its slot in the material
// table. Textures are sampled through `sample`, which uses the fragment's implicit
// derivatives here (a compute material pass would give analytic ones, ADR 0092).
// A texture in a material type's parameters (ADR 0092): the bindless index and sampler bits
// the material table writes for a material file's `Texture(...)`; white when none. Sampled
// with `ctx.sample(p.texture)`.
struct TextureRef
{
    uint packed;
};

struct MaterialContext
{
    float2 uv;
    float3 world_position;
    float3 normal;   // interpolated, not normalized
    float4 tangent;  // xyz interpolated; w the bitangent's sign
    float side;      // 1 for a front face, -1 for a back face (double-sided materials)
    uint material;   // the slot in the material table

    // A bindless texture reference of the material table (index and sampler bits), white
    // when there is none.
    float4 sample(uint packed)
    {
        return sample_material(packed, uv);
    }

    // A material type's texture (`TextureRef`), at the fragment's texture coordinates.
    float4 sample(TextureRef texture)
    {
        return sample_material(texture.packed, uv);
    }
}

MaterialContext material_context(VertexOut input, bool front)
{
    MaterialContext ctx;
    ctx.uv = input.uv;
    ctx.world_position = input.world_position;
    ctx.normal = input.normal;
    ctx.tangent = input.tangent;
    ctx.side = front ? 1.0 : -1.0;
    ctx.material = input.material;
    return ctx;
}

// The masked depth and shadow passes: only the coverage.
MaterialContext coverage_context(float2 uv, uint material)
{
    MaterialContext ctx;
    ctx.uv = uv;
    ctx.world_position = 0.0;
    ctx.normal = float3(0.0, 0.0, 1.0);
    ctx.tangent = float4(1.0, 0.0, 0.0, 1.0);
    ctx.side = 1.0;
    ctx.material = material;
    return ctx;
}

// What a material gives the lighting. Its ambient occlusion and emission come from their
// own functions, which the lighting calls where it needs them: sampled earlier, their values
// stay live through the lighting's start and cost registers (on RDNA2 up to 10 VGPRs in
// some variants, measured).
struct Surface
{
    float4 base;        // linear base color and alpha (coverage)
    float3 normal;      // shading normal, unit, on the fragment's side
    float metallic;
    float roughness;    // perceptual, before the specular antialiasing widens it
    uint subsurface;    // subsurface scattering's bits (ADR 0059): profile, mask, transmission
    // The shading models' own terms (ADR 0092), read only by their model (`IMaterial::model`).
    float clearcoat;            // the coated models: the coat's strength, 0 … 1
    float clearcoat_roughness;  // its perceptual roughness
    float3 clearcoat_normal;    // its own normal (world space, unit; 0: the interpolated normal)
    float3 sheen;               // MODEL_CLOTH and glTF's sheen layer: the sheen's color
    float sheen_roughness;      // its perceptual roughness
    float3 hair_tangent;        // MODEL_HAIR: along the strands (world space, unit)
    float hair_shift;           // the first highlight's shift along the normal (the second's 0.1 lower)
    float toon_steps;           // MODEL_TOON: lit bands above the shadow (1: two tones)
    float toon_softness;        // the bands' edge width in n·l
    float3 toon_highlight;      // the highlight's color (0: none)
    float3 rim;                 // the rim light's color (0: none)
    float rim_width;            // where it shows: n·v below this
};

// Surface terms a model does not read, set to nothing (`Surface` for glTF-like models).
Surface surface_terms(Surface s)
{
    s.clearcoat = 0.0;
    s.clearcoat_roughness = 0.0;
    s.clearcoat_normal = 0.0;
    s.sheen = 0.0;
    s.sheen_roughness = 1.0;
    s.hair_tangent = float3(0.0, 1.0, 0.0);
    s.hair_shift = 0.0;
    s.toon_steps = 1.0;
    s.toon_softness = 0.05;
    s.toon_highlight = 0.0;
    s.rim = 0.0;
    s.rim_width = 0.3;
    return s;
}

// The shading models (ADR 0092): how the engine lights a surface. A material type picks
// one at compile time (`IMaterial::model` returns a literal): each is its own code, never a
// branch the others pay for.
static const uint MODEL_LIT = 0;       // glTF's metallic-roughness (GGX, Lambert)
static const uint MODEL_UNLIT = 1;     // the base color and emission, no light
static const uint MODEL_CLEARCOAT = 2; // lit, under a clear dielectric coat (Filament)
static const uint MODEL_CLOTH = 3;     // sheen: Charlie distribution, Neubelt visibility (Filament)
static const uint MODEL_HAIR = 4;      // Kajiya-Kay with two shifted highlights (Scheuermann 2004)
static const uint MODEL_TOON = 5;      // banded diffuse, a hard highlight and a rim light
static const uint MODEL_SHEEN = 6;     // lit under glTF's sheen layer (KHR_materials_sheen)
static const uint MODEL_SHEEN_CLEARCOAT = 7; // the sheen layer, under a clear coat

// The models with a clear coat, and those with glTF's sheen layer over the lit base: with a
// literal argument each folds to a constant.
bool model_coated(uint model)
{
    return model == MODEL_CLEARCOAT || model == MODEL_SHEEN_CLEARCOAT;
}

bool model_sheen_layer(uint model)
{
    return model == MODEL_SHEEN || model == MODEL_SHEEN_CLEARCOAT;
}

interface IMaterial
{
    // The shading model (`MODEL_*`), a literal.
    static uint model();
    // The material's parameters (its slot of the material table), loaded once per fragment
    // by `load` and handed to the other functions.
    associatedtype Params;
    static Params load(MaterialContext ctx);
    // The surface; with `masked`, fragments below the alpha cutoff are discarded.
    static Surface surface(MaterialContext ctx, Params p, bool masked);
    // The material's own ambient occlusion (1: none).
    static float occlusion(MaterialContext ctx, Params p);
    // Emitted radiance.
    static float3 emission(MaterialContext ctx, Params p);
    // The alpha the masked passes test, and its cutoff.
    static float alpha(MaterialContext ctx, Params p);
    static float alpha_cutoff(Params p);
    // The thickness light passes through (subsurface transmission), sampled only where
    // light comes from behind.
    static float thickness(MaterialContext ctx, Params p);
}

// glTF 2.0's metallic-roughness material (ADR 0040), with subsurface scattering (ADR 0059):
// the importer's and the default.
struct GltfMaterial : IMaterial
{
    typealias Params = Material;

    static uint model()
    {
        return MODEL_LIT;
    }

    static Material load(MaterialContext ctx)
    {
        return read_material(ctx.material);
    }

    static Surface surface(MaterialContext ctx, Material m, bool masked)
    {
        Surface s;
        s.base = ctx.sample(m.base_color_texture) * m.base_color;
        if (masked && s.base.a < m.alpha_cutoff)
            discard;
        float3 geometric = ctx.normal * ctx.side;
        s.normal = normalize(geometric);
        if (m.normal_texture != NO_TEXTURE)
        {
            // MikkTSpace: the per-pixel bitangent from the interpolated (not renormalized)
            // normal and tangent, then the normal map's vector in that frame.
            float3 t = ctx.tangent.xyz * ctx.side;
            float3 b = ctx.tangent.w * cross(ctx.normal, ctx.tangent.xyz) * ctx.side;
            float2 xy = ctx.sample(m.normal_texture).xy * 2.0 - 1.0;
            float z = sqrt(saturate(1.0 - dot(xy, xy)));
            xy *= m.normal_scale;
            s.normal = normalize(xy.x * t + xy.y * b + z * geometric);
        }
        float4 mr = ctx.sample(m.metallic_roughness_texture);
        s.metallic = saturate(mr.b * m.metallic);
        s.roughness = saturate(mr.g * m.roughness);
        s.subsurface = m.subsurface;
        return surface_terms(s);
    }

    static float occlusion(MaterialContext ctx, Material m)
    {
        return 1.0 + m.occlusion_strength * (ctx.sample(m.occlusion_texture).r - 1.0);
    }

    static float3 emission(MaterialContext ctx, Material m)
    {
        return ctx.sample(m.emissive_texture).rgb * m.emissive;
    }

    static float alpha(MaterialContext ctx, Material m)
    {
        return ctx.sample(m.base_color_texture).a * m.base_color.a;
    }

    static float alpha_cutoff(Material m)
    {
        return m.alpha_cutoff;
    }

    static float thickness(MaterialContext ctx, Material m)
    {
        return m.thickness * ctx.sample(m.thickness_texture).g;
    }
}

// A material type's parameters (ADR 0092): its slot's bytes in `Frame.material_data`. A
// type's `load` returns them. The address is computed as a 64-bit integer (`shaderInt64`,
// which material types need): AMD's compiler (driver 26.8.1) reads zeros through a pointer
// cast to `T*` after pointer arithmetic on `uint*`, while the integer path reads right.
T material_params<T>(MaterialContext ctx)
{
    uint64_t address = uint64_t(push.frame.material_data) + uint64_t(ctx.material) * push.frame.material_stride;
    return loadAligned<16>((T*)address);
}

// The material the entry points shade with (ADR 0092): the built-in glTF material, or a
// type compiled in with `-D MATERIAL=<type> -D MATERIAL_FILE="<its module>"`. The
// subsurface entry points stay the glTF material's (types have no subsurface states).
#ifdef MATERIAL_FILE
#include MATERIAL_FILE
#endif
#ifndef MATERIAL
#define MATERIAL GltfMaterial
#endif

[shader("fragment")]
void shadowFragmentMasked(ShadowOut input)
{
    MaterialContext ctx = coverage_context(input.uv, input.material);
    MATERIAL.Params p = MATERIAL::load(ctx);
    if (MATERIAL::alpha(ctx, p) < MATERIAL::alpha_cutoff(p))
        discard;
}

// The depth prepass of masked materials with MSAA (ADR 0075): alpha to coverage instead
// of the alpha test. The alpha is sharpened around the cutoff to about a pixel's width
// (Golus, "Anti-aliased Alpha Test", 2017), so the edge stays where the alpha test puts it
// and only its pixel-wide transition becomes coverage. The pipeline has no color target:
// only alpha at location 0 matters.
[shader("fragment")]
float4 prepassCoverage(ShadowOut input) : SV_Target0
{
    MaterialContext ctx = coverage_context(input.uv, input.material);
    MATERIAL.Params p = MATERIAL::load(ctx);
    float alpha = MATERIAL::alpha(ctx, p);
    alpha = (alpha - MATERIAL::alpha_cutoff(p)) / max(fwidth(alpha), 1e-4) + 0.5;
    return float4(0.0, 0.0, 0.0, alpha);
}

// Texture feedback (feedback.rs, ADR 0076): drawn with `depthVertexMasked` into a small
// depth target (the view's size over a divisor), after the view's culling, with the same
// commands. Each fragment works out the finest level a texture of its material would be
// sampled at, in units of texture coordinates (the level of a texture of size S is this
// plus log2 S), and keeps the minimum per material slot, with a pixel count. No other pass
// has this code: views without streaming pay nothing for it.
//
// `push.light_tiles`: per material slot, the minimum (fixed point, FEEDBACK_SCALE per level,
// FEEDBACK_ZERO at level 0) and the pixel count. `push.texture`: the bits of a float added
// to the level, the sampler's LOD bias minus log2 of the divisor (a pixel here spans
// `divisor` of the view's).
static const float FEEDBACK_SCALE = 256.0;
static const float FEEDBACK_ZERO = 64.0;

[shader("fragment")]
[earlydepthstencil]
void feedbackFragment(ShadowOut input)
{
    float2 dx = ddx(input.uv);
    float2 dy = ddy(input.uv);
    float x2 = dot(dx, dx);
    float y2 = dot(dy, dy);
    // 16× anisotropic filtering samples along the longer axis: the level follows the
    // shorter one, but no more than 16 times finer than the longer (squared: 256).
    float level = 0.5 * log2(max(max(min(x2, y2), max(x2, y2) / 256.0), 1e-30)) + asfloat(push.texture);
    uint q = uint(clamp((level + FEEDBACK_ZERO) * FEEDBACK_SCALE, 0.0, 65535.0 * FEEDBACK_SCALE));
    uint material = input.material;
    uint* slot = push.light_tiles + material * 2;
    // One pair of atomics per wave when the wave shades one material (the usual case),
    // made by a lane that is not a helper (helpers' atomics do nothing).
    bool real = !IsHelperLane();
    if (WaveActiveAllEqual(material))
    {
        uint least = WaveActiveMin(real ? q : 0xFFFFFFFFu);
        uint count = WaveActiveCountBits(real);
        if (real && WavePrefixCountBits(real) == 0)
        {
            InterlockedMin(slot[0], least);
            // Counted down from the cleared 0xFFFFFFFF: the CPU inverts it.
            InterlockedAdd(slot[1], 0u - count);
        }
    }
    else if (real)
    {
        InterlockedMin(slot[0], q);
        InterlockedAdd(slot[1], 0xFFFFFFFFu);
    }
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
    // Field by field (`shadow_clip`): a whole cascade's load cost the Radeon's shading six
    // registers.
    float4 scale = read_float4(&push.frame.cascades[c].scale);
    float4 offset = read_float4(&push.frame.cascades[c].offset);
    float4 atlas = read_float4(&push.frame.cascades[c].atlas);
    float layer = float(c) * push.frame.cascade_layers;
    float3 ndc = (light + normal * (push.frame.normal_bias * scale.w)) * scale.xyz + offset.xyz;
    if (ndc.z > 1.0)
        return 1.0;
    float2 uv = ndc.xy * atlas.xy + atlas.zw;
    // Its layer of the map (a cache's wraps around: the window starts anywhere in it, ADR
    // 0093).
    Texture2DArray map = g_texture_arrays[NonUniformResourceIndex(push.texture)];
    SamplerComparisonState s = g_compare_samplers[push.frame.cascade_sampler];
    uint width, height, layers;
    map.GetDimensions(width, height, layers);
    float2 texel = 1.0 / float2(width, height);
    float lit = 0.0;
    [unroll]
    for (int y = -1; y <= 1; y++)
        [unroll]
        for (int x = -1; x <= 1; x++)
            lit += map.SampleCmpLevelZero(s, float3(uv + float2(x, y) * texel, layer), ndc.z);
    return lit / 9.0;
}

// How far towards the edge of cascade `c` a light view position lies (0 center, 1 edge),
// or 2 when it is outside the cascade's depth range. A cascade holds the point with room
// for the filter below its limit (`Cascade::holds` in shadow.rs).
float cascade_edge(uint c, float3 light)
{
    float4 scale = read_float4(&push.frame.cascades[c].scale);
    float4 offset = read_float4(&push.frame.cascades[c].offset);
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
        float limit = read_float4(&push.frame.cascades[c].offset).w;
        if (edge >= limit)
            continue;
        uint chosen = c;
        float t = saturate((edge - (limit - band)) / band);
        // The jitter (in NDC, a few thousandths) shifts the pattern every TAA frame.
        if (t > 0.0 && gradient_noise(pixel + read_float4(&push.frame.jitter).xy * 4096.0) < t)
        {
            if (c + 1 < count && cascade_edge(c + 1, light) < read_float4(&push.frame.cascades[c + 1].offset).w)
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

// What the light functions need of a shading model (ADR 0092); `model` is a literal at
// every call, so the code of the other models is removed.
struct ModelTerms
{
    uint model;
    float3 base;
    float clearcoat;
    float coat_a;           // the coat's roughness squared
    float3 coat_normal;     // the interpolated normal: the coat is smooth over a normal map
    float3 sheen;
    float sheen_a;          // the sheen's roughness squared
    float sheen_scaling;    // glTF's sheen layer: what the lit base keeps under it
    float3 hair_tangent;
    float hair_shift;
    float toon_steps;
    float toon_softness;
    float3 toon_highlight;
};

// The Charlie sheen distribution (Estevez and Kulla 2017, as in Filament).
float distribution_charlie(float n_dot_h, float a)
{
    float inverse = 1.0 / max(a, 1e-3);
    float sin2 = max(1.0 - n_dot_h * n_dot_h, 0.0078125);
    return (2.0 + inverse) * pow(sin2, inverse * 0.5) / (2.0 * PI);
}

// Neubelt and Pettineo's cloth visibility (2013, as in Filament).
float visibility_neubelt(float n_dot_v, float n_dot_l)
{
    return saturate(1.0 / (4.0 * (n_dot_l + n_dot_v - n_dot_l * n_dot_v)));
}

// Kelemen's visibility for the clear coat (2001, as in Filament).
float visibility_kelemen(float l_dot_h)
{
    return 0.25 / max(l_dot_h * l_dot_h, 1e-4);
}

// A Kajiya-Kay highlight along strands `t` (Scheuermann, "Hair Rendering and Shading", 2004).
float strand_specular(float3 t, float3 h, float exponent)
{
    float t_dot_h = dot(t, h);
    float sin_th = sqrt(max(1.0 - t_dot_h * t_dot_h, 0.0));
    return smoothstep(-1.0, 0.0, t_dot_h) * pow(sin_th, exponent);
}

// Toon light: `x` (n·l) in `steps` lit bands above the shadow (n·l ≤ 0), at 1/steps … 1,
// each rising over `softness` (at most half a band) from its start at n·l = k / steps:
// nothing at n·l ≤ 0, where the shadow test cuts the light anyway.
float toon_band(float x, float steps, float softness)
{
    float count = max(floor(steps), 1.0);
    float w = min(softness, 0.5 / count);
    // The band whose rise is nearest.
    float k = clamp(floor((x - w) * count + 0.5), 0.0, count - 1.0);
    float start = k / count;
    return (k + smoothstep(start, start + 2.0 * w, x)) / count;
}

// `surface_light` for the shading model of `m` (light from `l`, unit irradiance).
float3 model_light(ModelTerms m, float3 n, float3 v, float3 l, float3 diffuse, float3 f0, float a)
{
    if (m.model == MODEL_LIT)
        return surface_light(n, v, l, diffuse, f0, a);
    float n_dot_l_signed = dot(n, l);
    float n_dot_l = saturate(n_dot_l_signed);
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));
    if (model_coated(m.model) || model_sheen_layer(m.model))
    {
        // The lit base; under glTF's sheen layer, what the layer lets through and the
        // layer's lobe (KHR_materials_sheen); under a coat, the coat's lobe (IOR 1.5) on its
        // own normal.
        float3 f = fresnel_schlick(v_dot_h, f0);
        float3 below = diffuse + distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
        if (model_sheen_layer(m.model))
            below = below * m.sheen_scaling + distribution_charlie(n_dot_h, m.sheen_a) * visibility_neubelt(n_dot_v, n_dot_l) * m.sheen;
        if (!model_coated(m.model))
            return below * n_dot_l;
        float fc = fresnel_schlick(v_dot_h, float3(0.04, 0.04, 0.04)).x * m.clearcoat;
        float coat = distribution_ggx(saturate(dot(m.coat_normal, h)), m.coat_a) * visibility_kelemen(v_dot_h) * fc;
        return below * (1.0 - fc) * n_dot_l + coat * saturate(dot(m.coat_normal, l));
    }
    if (m.model == MODEL_CLOTH)
    {
        float3 sheen = distribution_charlie(n_dot_h, m.sheen_a) * visibility_neubelt(n_dot_v, n_dot_l) * m.sheen;
        return (diffuse + sheen) * n_dot_l;
    }
    if (m.model == MODEL_HAIR)
    {
        // Two highlights (Scheuermann): the light reflected off the strands' surface, and
        // a wider one colored by the hair, shifted along the strands. Each lobe normalized
        // as a Blinn-Phong lobe of its exponent; the diffuse is Lambert's (a wrapped one
        // would end at the shadow test's cut).
        float exponent = clamp(2.0 / max(a * a, 1e-4) - 2.0, 4.0, 1024.0);
        float3 t1 = normalize(m.hair_tangent + n * m.hair_shift);
        float3 t2 = normalize(m.hair_tangent + n * (m.hair_shift - 0.1));
        float primary = strand_specular(t1, h, exponent) * (exponent + 2.0);
        float secondary = strand_specular(t2, h, exponent * 0.25) * (exponent * 0.25 + 2.0);
        float3 f = fresnel_schlick(v_dot_h, f0);
        float3 specular = f * (primary + secondary * m.base) * (0.25 / (2.0 * PI));
        return (diffuse + specular) * n_dot_l;
    }
    if (m.model == MODEL_TOON)
    {
        float band = toon_band(n_dot_l_signed, m.toon_steps, m.toon_softness);
        // A flat highlight where GGX is above half its peak (its size from the roughness),
        // on the lit side.
        float peak = distribution_ggx(n_dot_h, a) * (PI * a * a);
        float highlight = smoothstep(0.5 - m.toon_softness, 0.5 + m.toon_softness, peak) * smoothstep(0.0, 2.0 * m.toon_softness, n_dot_l_signed);
        return diffuse * band + m.toon_highlight * highlight;
    }
    return surface_light(n, v, l, diffuse, f0, a);
}

// How lit `p` (normal `n`) is by a local light whose shadow views start at `first` (ADR
// 0071). The receiver moves along its normal by about a texel at its depth in the view
// (the shading normal: the geometric one would cost the light loop three more registers on
// RDNA2). A spot light projects it with its view's matrix (loaded once per wave: the light
// is the same for the whole wave). A point light's face is the major axis from the light
// (faces +x, −x, +y, −y, +z, −z), projected without a matrix and without branches: the
// face's two other axes (signed as `look_at` and the Vulkan projection orient them) over
// the major distance, and the depth A + B / major; only the face's tile differs per pixel.
// Four bilinear comparisons half a texel apart make a 3 × 3 tent filter. Outside the view
// (a spot light's cone beyond its projection) it is lit.
float local_shadow(uint first, Light light, float3 p, float3 n)
{
    float3 position = light.position_range.xyz;
    // World size of a texel per unit of depth, times the normal bias.
    float bias = read_float4(&push.frame.shadow_views[first].eye_bias).w;
    float3 d = p - position;
    float3 ndc;
    float4 atlas;
    if (light.color_scale.w == 0.0)
    {
        float3 a = abs(d);
        bool x_major = a.x >= a.y && a.x >= a.z;
        bool y_major = !x_major && a.y >= a.z;
        float major = x_major ? a.x : (y_major ? a.y : a.z);
        float3 q = d + n * (bias * major);
        // The receiver's signed coordinate along the face's axis, and the other two.
        float along = x_major ? q.x : (y_major ? q.y : q.z);
        float flip = along < 0.0 ? -1.0 : 1.0;
        float u = x_major ? flip * q.z : (y_major ? flip * q.x : -flip * q.x);
        float v = y_major ? -q.z : -q.y;
        uint face = (x_major ? 0u : (y_major ? 2u : 4u)) + (along < 0.0 ? 1u : 0u);
        // x: 1 / tan(half angle); y, z: depth A + B / major.
        float4 terms = read_float4(&push.frame.shadow_views[first].tile);
        float inverse = 1.0 / max(abs(along), 1e-6);
        ndc = float3(float2(u, v) * (terms.x * inverse), terms.y + terms.z * inverse);
        atlas = read_float4(&push.frame.shadow_views[first + face].atlas);
    }
    else
    {
        float3 q = p + n * (bias * max(dot(d, light.direction_offset.xyz), 0.0));
        ShadowView s = read_shadow_view(push.frame.shadow_views, first);
        float4 c = mul(s.clip_from_world, float4(q, 1.0));
        if (c.w <= 0.0)
            return 1.0;
        ndc = c.xyz / c.w;
        atlas = s.atlas;
    }
    if (abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0 || ndc.z > 1.0)
        return 1.0;
    float2 uv = ndc.xy * atlas.xy + atlas.zw;
    Texture2D map = g_textures[push.frame.local_shadow_atlas];
    SamplerComparisonState cmp = g_compare_samplers[push.frame.shadow_sampler];
    float t = 0.5 * push.frame.shadow_texel;
    float lit = map.SampleCmpLevelZero(cmp, uv + float2(-t, -t), ndc.z)
        + map.SampleCmpLevelZero(cmp, uv + float2(t, -t), ndc.z)
        + map.SampleCmpLevelZero(cmp, uv + float2(-t, t), ndc.z)
        + map.SampleCmpLevelZero(cmp, uv + float2(t, t), ndc.z);
    return lit * 0.25;
}

// One local light at `p`: inverse-square falloff windowed to zero at its range (Karis 2013),
// times the spot cone. Adds the irradiance it brings to `irradiance` (subsurface
// materials; unused and compiled away elsewhere). With `shadows` (a literal), light `index`
// is shadowed when it has views (ADR 0071).
float3 local_light(Light light, float3 p, float3 n, float3 v, float3 diffuse, float3 f0, float a, inout float3 irradiance, bool shadows, uint index, ModelTerms m)
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
    if (shadows)
    {
        uint first = push.frame.light_shadows[index];
        // Surfaces facing away get nothing from the light anyway: no lookup.
        if (first != NO_SHADOW && attenuation > 0.0 && dot(n, l) > 0.0)
            attenuation *= local_shadow(first, light, p, n);
    }
    irradiance += light.color_scale.rgb * attenuation * saturate(dot(n, l));
    return model_light(m, n, v, l, diffuse, f0, a) * light.color_scale.rgb * attenuation;
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
// `irradiance`. `shadows` (a literal) looks up the shadowed lights' shadows.
float3 local_lights(float3 p, float2 pixel, float3 n, float3 v, float3 diffuse, float3 f0, float a, inout float3 irradiance, bool shadows, ModelTerms m)
{
    uint count = push.frame.light_count;
    float3 sum = 0.0;
    if (LOCAL_LIGHTS == 0 || count == 0)
        return sum;
    if (push.frame.light_culling == 0)
    {
        for (uint i = 0; i < count; i++)
            sum += local_light(read_light(i), p, n, v, diffuse, f0, a, irradiance, shadows, i, m);
        return sum;
    }
    float depth = dot(p, read_float4(&push.frame.view_axis).xyz) + read_float4(&push.frame.view_axis).w;
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
                sum += local_light(read_light(w * 32 + bit), p, n, v, diffuse, f0, a, irradiance, shadows, w * 32 + bit, m);
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

// Probe GI (ADR 0088): the diffuse irradiance at a pixel (`position` its surface's world
// position) from the four resolved samples around it (`push.gi`: rgb, and a the distance
// of the surface each was resolved at, 0 for the sky), weighted bilinearly and by how
// close their distances are to its own (no light across depth edges); the nearest in
// distance when none is close.
float3 gi_irradiance(float2 pixel, float3 position)
{
    Frame* frame = push.frame;
    float distance = length(read_float4(&frame.camera_position).xyz - position);
    // Mostly the four lie on the pixel's surface: then their bilinear filtering, one read
    // (its filtered distance within 1 % of the pixel's leaves any other surface's sample
    // a weight of 1-2 % at most). The four reads below cost 0.5 ms more on a two-CU
    // Radeon at 1080p.
    float4 filtered = g_textures[push.gi].SampleLevel(g_samplers[frame.linear_sampler], pixel / (frame.gi_scale * float2(frame.gi_samples)), 0.0);
    if (abs(filtered.a - distance) < 0.01 * distance)
        return filtered.rgb;
    float2 low = pixel / frame.gi_scale - 0.5;
    int2 base = int2(floor(low));
    float2 t = low - float2(base);
    int2 last = int2(frame.gi_samples) - 1;
    float3 sum = 0.0;
    float total = 0.0;
    float3 nearest = 0.0;
    float nearest_gap = 1e30;
    for (uint k = 0; k < 4; k++)
    {
        int2 o = int2(k & 1, k >> 1);
        float4 sample = g_textures[push.gi].Load(int3(clamp(base + o, int2(0, 0), last), 0));
        float gap = abs(sample.a - distance);
        if (sample.a <= 0.0)
            continue;
        if (gap < nearest_gap)
        {
            nearest_gap = gap;
            nearest = sample.rgb;
        }
        float2 b = lerp(1.0 - t, t, float2(o));
        float w = b.x * b.y * exp(-gap / (0.05 * distance));
        sum += sample.rgb * w;
        total += w;
    }
    return total > 1e-4 ? sum / total : nearest;
}

// Specular occlusion from the visible cone (GTSO, Jimenez et al. 2016): the share of the
// reflection's cone (aperture from the roughness `a`) inside the cone around the bent
// normal `bent` whose solid angle is the visibility. The cones' overlap goes from all of
// the smaller one (axes closer than the apertures' difference) to none (farther than their
// sum) along a smoothstep, as Oat and Sander's approximation does (Filament's
// `SpecularAO_Cones`), but taken in the cosines of those angles: no arc cosines, fewer
// registers. None for nearly smooth surfaces, whose reflections it would remove entirely.
float cone_specular_occlusion(float3 bent, float3 r, float visibility, float a)
{
    float cos_visible = sqrt(saturate(1.0 - visibility));
    float sin_visible = sqrt(saturate(visibility));
    float cos_specular = exp2(-3.321928 * a * a);
    float sin_specular = sqrt(saturate(1.0 - cos_specular * cos_specular));
    // cos(θv ∓ θs) = cos θv cos θs ± sin θv sin θs.
    float cos_product = cos_visible * cos_specular;
    float sin_product = sin_visible * sin_specular;
    float x = saturate((dot(bent, r) - cos_product + sin_product) / max(2.0 * sin_product, 1e-4));
    float overlap = x * x * (3.0 - 2.0 * x);
    float occlusion = overlap * saturate((1.0 - cos_visible) / max(1.0 - cos_specular, 1e-4));
    return lerp(1.0, occlusion, smoothstep(0.01, 0.09, a));
}

// Light from the whole sky: the environment's harmonics (diffuse) and prefiltered cube
// (specular), or the flat ambient color without an environment. `occlusion` is the
// material's, times the view's ambient occlusion when `ao` (ADR 0058). With `gi` (a
// literal, ADR 0088), the diffuse light is the view's irradiance at `pixel` from the
// probe volume (the sky's harmonics outside it), absolute: the environment's intensity
// is in it. Without `lobe` (a literal), the diffuse light only: a shading model's own
// specular lobe takes the specular light's place (ADR 0092). With `bent` (a literal: the
// view's ambient occlusion gave its bent normal, ADR 0058), the harmonics are read along
// `sky_normal` (the shading normal bent as the geometry's was) and the specular occlusion
// is the visible cone's (GTSO) instead of Lagarde's fit.
float3 ambient_light(float3 n, float3 v, float3 diffuse, float3 f0, float roughness, float occlusion, float3 albedo, bool ao, bool reflective, inout float3 weight, bool gi = false, float2 pixel = 0.0, float3 position = 0.0, bool lobe = true, bool bent = false, float3 sky_normal = 0.0)
{
    // With the view's ambient occlusion, the fit of its bounces (for the albedo's
    // luminance: one value instead of three, fewer registers); without it, the material's
    // occlusion as it was.
    float diffuse_occlusion = ao && occlusion < 1.0
        ? multibounce(occlusion, dot(albedo, float3(0.2126, 0.7152, 0.0722)))
        : occlusion;
    if (gi && (ENVIRONMENT == 0 || push.frame.environment == 0))
        return gi_irradiance(pixel, position) * albedo / PI * diffuse_occlusion;
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return read_float4(&push.frame.ambient).rgb * albedo * diffuse_occlusion;
    float n_dot_v = max(dot(n, v), 1e-4);
    float4* sh = push.frame.sh;
    float3 sn = bent ? sky_normal : n;
    float3 irradiance = gi ? 0.0 : read_float4(sh + 0).rgb * 0.282095
        + read_float4(sh + 1).rgb * (0.488603 * sn.y) + read_float4(sh + 2).rgb * (0.488603 * sn.z) + read_float4(sh + 3).rgb * (0.488603 * sn.x)
        + read_float4(sh + 4).rgb * (1.092548 * sn.x * sn.y) + read_float4(sh + 5).rgb * (1.092548 * sn.y * sn.z)
        + read_float4(sh + 6).rgb * (0.315392 * (3.0 * sn.z * sn.z - 1.0)) + read_float4(sh + 7).rgb * (1.092548 * sn.x * sn.z)
        + read_float4(sh + 8).rgb * (0.546274 * (sn.x * sn.x - sn.y * sn.y));
    irradiance = max(irradiance, 0.0);
    SamplerState s = g_samplers[push.frame.linear_sampler];
    float3 r = reflect(-v, n);
    float lod = roughness * float(push.frame.env_levels - 1);
    float3 prefiltered = g_cubes[push.frame.prefiltered].SampleLevel(s, r, lod).rgb;
    float2 dfg = g_textures[push.frame.dfg_lut].SampleLevel(s, float2(n_dot_v, roughness), 0.0).xy;
    float3 specular = lobe ? prefiltered * lerp(dfg.xxx, dfg.yyy, f0) * (1.0 + f0 * (1.0 / max(dfg.y, 1e-4) - 1.0)) : 0.0;
    float a = roughness * roughness;
    // Lagarde and de Rousiers' specular occlusion; 1 without occlusion (most materials),
    // where skipping it frees registers (72 → 64 VGPRs on the Radeon). With the bent
    // normal, the visible cone's.
    float specular_occlusion = bent
        ? cone_specular_occlusion(sky_normal, r, occlusion, a)
        : (occlusion < 1.0 ? saturate(pow(n_dot_v + occlusion, exp2(-16.0 * a - 1.0)) - 1.0 + occlusion) : 1.0);
    // Reflections (ADR 0073): what multiplies the prefiltered radiance, for the reflection
    // composite to swap the sky's radiance for local probes and screen-space hits.
    if (reflective && lobe)
        weight = lerp(dfg.xxx, dfg.yyy, f0) * (1.0 + f0 * (1.0 / max(dfg.y, 1e-4) - 1.0)) * specular_occlusion;
    if (gi)
        return diffuse * gi_irradiance(pixel, position) * diffuse_occlusion + specular * specular_occlusion * push.frame.env_intensity;
    return (diffuse * irradiance * diffuse_occlusion + specular * specular_occlusion) * push.frame.env_intensity;
}

// The irradiance of the environment's harmonics at normal `n` (as in `ambient_light`).
float3 sky_irradiance(float3 n)
{
    float4* sh = push.frame.sh;
    float3 irradiance = read_float4(sh + 0).rgb * 0.282095
        + read_float4(sh + 1).rgb * (0.488603 * n.y) + read_float4(sh + 2).rgb * (0.488603 * n.z) + read_float4(sh + 3).rgb * (0.488603 * n.x)
        + read_float4(sh + 4).rgb * (1.092548 * n.x * n.y) + read_float4(sh + 5).rgb * (1.092548 * n.y * n.z)
        + read_float4(sh + 6).rgb * (0.315392 * (3.0 * n.z * n.z - 1.0)) + read_float4(sh + 7).rgb * (1.092548 * n.x * n.z)
        + read_float4(sh + 8).rgb * (0.546274 * (n.x * n.x - n.y * n.y));
    return max(irradiance, 0.0);
}

// The clear coat's light from the environment (ADR 0092): its lobe (normal `n`, perceptual
// `roughness`, reflectance 0.04) of the prefiltered sky, `strength` its share; `weight`
// what multiplies the prefiltered radiance, for the reflections' composite (ADR 0073).
// Nothing without an environment.
float3 coat_environment(float3 n, float3 v, float roughness, float strength, float occlusion, out float3 weight)
{
    weight = 0.0;
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return 0.0;
    SamplerState s = g_samplers[push.frame.linear_sampler];
    float n_dot_v = max(dot(n, v), 1e-4);
    float3 r = reflect(-v, n);
    float3 prefiltered = g_cubes[push.frame.prefiltered].SampleLevel(s, r, roughness * float(push.frame.env_levels - 1)).rgb;
    float2 dfg = g_textures[push.frame.dfg_lut].SampleLevel(s, float2(n_dot_v, roughness), 0.0).xy;
    float a = roughness * roughness;
    float specular_occlusion = occlusion < 1.0 ? saturate(pow(n_dot_v + occlusion, exp2(-16.0 * a - 1.0)) - 1.0 + occlusion) : 1.0;
    weight = lerp(dfg.x, dfg.y, 0.04) * strength * specular_occlusion;
    return prefiltered * weight * push.frame.env_intensity;
}

// The directional albedo of cloth's sheen lobe (Charlie distribution, Neubelt visibility,
// ADR 0092) at n·v `mu` and perceptual roughness `r` (0.07 … 1): a fit to its integral,
// within 0.045 (docs/research/custom-materials.md).
float sheen_albedo(float mu, float r)
{
    float a = 1.42617 + r * (-1.68309 + r * 0.78282);
    float b = 0.08726 + (1.09762 + 0.10519 / r) / r;
    float c = 1.78444 + r * (-2.16779 + r * 1.60655);
    return min(a * exp(-b * pow(mu, c)), 1.0);
}

// Cloth's sheen in the environment's light (ADR 0092): the prefiltered sky at the sheen's
// perceptual `roughness` (prefiltered for GGX, standing in for Charlie), times the sheen's
// color and its lobe's albedo; `weight` as `coat_environment`'s. Nothing without an
// environment.
float3 sheen_environment(float3 n, float3 v, float roughness, float3 sheen, float occlusion, out float3 weight)
{
    weight = 0.0;
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return 0.0;
    float3 r = reflect(-v, n);
    float3 prefiltered = g_cubes[push.frame.prefiltered].SampleLevel(g_samplers[push.frame.linear_sampler], r, roughness * float(push.frame.env_levels - 1)).rgb;
    weight = sheen * sheen_albedo(max(dot(n, v), 1e-4), roughness) * occlusion;
    return prefiltered * weight * push.frame.env_intensity;
}

// The reflectance of a surface of reflectance `f0` (from air) under a coat of IOR 1.5:
// its IOR from `f0`, then Fresnel's equation at normal incidence against the coat's.
float3 f0_under_coat(float3 f0)
{
    float3 s = sqrt(min(f0, 0.99));
    float3 ior = (1.0 + s) / (1.0 - s);
    float3 r = (ior - 1.5) / (ior + 1.5);
    return r * r;
}

// The diffuse part of `ambient_light` per unit base color, for subsurface materials;
// `kd` is (1 − metallic) / π; `gi` and `pixel` as there.
float3 ambient_diffuse(float3 n, float occlusion, float kd, bool gi = false, float2 pixel = 0.0, float3 position = 0.0)
{
    if (gi)
        return kd * gi_irradiance(pixel, position) * occlusion;
    if (ENVIRONMENT == 0 || push.frame.environment == 0)
        return read_float4(&push.frame.ambient).rgb * occlusion;
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
        float4 g = read_float4(&p.gaussians[i]);
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
        if (cascade_edge(c, light) >= read_float4(&push.frame.cascades[c].offset).w)
            continue;
        float4 scale = read_float4(&push.frame.cascades[c].scale);
        float4 offset = read_float4(&push.frame.cascades[c].offset);
        float4 atlas = read_float4(&push.frame.cascades[c].atlas);
        float3 ndc = (light - normal * (push.frame.normal_bias * scale.w)) * scale.xyz + offset.xyz;
        float2 uv = ndc.xy * atlas.xy + atlas.zw;
        // Its layer of the map, wrapping around as the lookups' sampler does (ADR 0093).
        Texture2DArray map = g_texture_arrays[NonUniformResourceIndex(push.texture)];
        uint width, height, layers;
        map.GetDimensions(width, height, layers);
        int2 size = int2(width, height);
        int2 texel = int2(floor(uv * float2(size)));
        texel = ((texel % size) + size) % size;
        float occluder = map.Load(int4(texel, int(float(c) * push.frame.cascade_layers), 0)).r;
        return max(ndc.z - occluder, 0.0) / abs(scale.z);
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
// below the alpha cutoff; `ao` reads the view's ambient occlusion (ADR 0058); `shadows`
// looks up local lights' shadows (ADR 0071). All are literals at every call: the compiler
// removes the code of the other case, so shading without ambient occlusion or local
// shadows spends no registers on them (on RDNA2 even dead code under a specialization
// constant took the shading from 64 to 72 VGPRs, 9 to 13 % slower).
float4 shade(VertexOut input, bool front, bool masked, bool ao, bool shadows = false, bool contact = false, bool decals = false, bool gi = false)
{
    SubsurfaceOut unused;
    ReflectOut unused_reflect;
    return shade_surface<MATERIAL>(input, front, masked, ao, false, unused, unused_reflect, shadows, contact, false, decals, gi);
}

// `shade` with ambient occlusion and its bent normal (the `*AoBent*` entry points).
float4 shade_bent(VertexOut input, bool front, bool masked, bool shadows = false, bool contact = false, bool decals = false, bool gi = false)
{
    SubsurfaceOut unused;
    ReflectOut unused_reflect;
    return shade_surface<MATERIAL>(input, front, masked, true, false, unused, unused_reflect, shadows, contact, false, decals, gi, true);
}

// What the reflection entry points write besides the color (ADR 0073).
struct ReflectOut
{
    float3 normal;           // the shading normal (world, unit)
    float roughness;         // perceptual, as the image-based light used it
    float3 weight;           // multiplies the prefiltered radiance (split sum × specular occlusion)
};

// `pack_snorm10` and `pack_unorm10` in decals.slang.
float3 unpack_snorm10(uint p)
{
    int3 q = int3(int(p << 22), int(p << 12), int(p << 2)) >> 22;
    return float3(q) / 511.0;
}

float3 unpack_unorm10(uint p)
{
    return float3(uint3(p, p >> 10, p >> 20) & 1023u) / 1023.0;
}

// The decal buffer (ADR 0082, decals.slang) at `pixel`, blended into the surface before
// it is lit: albedo, the normal's change (laid in the surface's plane by the decal pass),
// roughness and metalness. Only in tiles some decal reaches: the decal pass writes no
// others.
void apply_decals(Frame frame, uint2 pixel, inout float3 albedo, inout float3 n, inout float roughness, inout float metallic)
{
    uint tile = (pixel.y >> 4) * ((frame.depth_size.x + 15) >> 4) + (pixel.x >> 4);
    if ((frame.decal_tiles[tile >> 5] & (1u << (tile & 31u))) == 0)
        return;
    int3 texel = int3(int2(pixel), 0);
    float4 color = g_textures[push.decal_albedo].Load(texel);
    albedo = albedo * (1.0 - color.a) + color.rgb * color.rgb;
    uint2 packed = g_uint2_textures[push.decal_surface].Load(texel);
    float3 bent = n + unpack_snorm10(packed.x);
    n = dot(bent, bent) > 1e-8 ? normalize(bent) : n;
    float3 surface = unpack_unorm10(packed.y);
    roughness = roughness * (1.0 - surface.z) + surface.x;
    metallic = metallic * (1.0 - surface.z) + surface.y;
}

// `shade`; with `subsurface`, the scattered part of the diffuse light goes to `sss`
// instead of the color. Subsurface materials' pipelines always take `ao` (they are their
// own; without ambient occlusion `push.ao` skips the read). `decals` blends the decal
// buffer in (ADR 0082). `bent`: the view's ambient occlusion carries its bent normal
// (`AoSettings::bent_normals`): visibility in R, the bent normal in world space in GBA.
float4 shade_surface<M : IMaterial>(VertexOut input, bool front, bool masked, bool ao, bool subsurface, out SubsurfaceOut sss, out ReflectOut refl, bool shadows = false, bool contact = false, bool reflective = false, bool decals = false, bool gi = false, bool bent = false)
{
    Frame frame = loadAligned<16>(push.frame);
    // The back of a double-sided surface is lit from its own side: the whole tangent
    // frame flips (as in Khronos' glTF sample viewer).
    MaterialContext ctx = material_context(input, front);
    M.Params params = M::load(ctx);
    float side = ctx.side;
    float3 geometric = input.normal * side;
    float3 ng = normalize(geometric);
    // Derivatives first, while every lane of the quad is alive.
    float kernel = specular_aa_kernel(ng);
    Surface surface = M::surface(ctx, params, masked);
    float4 base = surface.base;
    float3 n = surface.normal;
    float metallic = surface.metallic;
    float rough = surface.roughness;
    if (decals)
        apply_decals(frame, uint2(input.position.xy), base.rgb, n, rough, metallic);
    // A coat scatters what the base reflects: the base is at least as rough (Filament).
    if (model_coated(M::model()))
        rough = lerp(rough, max(rough, surface.clearcoat_roughness), saturate(surface.clearcoat));
    float perceptual = widen_roughness(rough, kernel);
    float occlusion = M::occlusion(ctx, params);
    // The view's ambient occlusion (ADR 0058) joins the material's; its bent normal bends
    // the shading normal as it bends the geometry's (`sky_normal`).
    float3 sky_normal = n;
    if (ao && push.ao != NO_TEXTURE)
    {
        float4 view_ao = g_textures[push.ao].Load(int3(int2(input.position.xy), 0));
        occlusion *= view_ao.r;
        if (bent)
            sky_normal = normalize(n + (view_ao.gba * 2.0 - 1.0) - ng);
    }
    float3 emissive = M::emission(ctx, params);
    // Unlit (ADR 0092): the base color and the emission, nothing else.
    if (M::model() == MODEL_UNLIT)
    {
        sss = (SubsurfaceOut)0;
        refl = (ReflectOut)0;
        if (reflective)
        {
            refl.normal = n;
            refl.roughness = 1.0;
        }
        return float4(base.rgb + emissive, base.a);
    }
    ModelTerms terms;
    terms.model = M::model();
    terms.base = base.rgb;
    terms.clearcoat = saturate(surface.clearcoat);
    float coat_roughness = clamp(surface.clearcoat_roughness, 0.045, 1.0);
    float sheen_roughness = clamp(surface.sheen_roughness, 0.07, 1.0);
    terms.coat_a = coat_roughness * coat_roughness;
    // The coat's normal: its own map's, or the interpolated normal (the coat is smooth over
    // the base's normal map).
    terms.coat_normal = model_coated(M::model()) && any(surface.clearcoat_normal != 0.0) ? surface.clearcoat_normal : ng;
    terms.sheen = surface.sheen;
    terms.sheen_a = sheen_roughness * sheen_roughness;
    terms.hair_tangent = surface.hair_tangent;
    terms.hair_shift = surface.hair_shift;
    terms.toon_steps = surface.toon_steps;
    terms.toon_softness = max(surface.toon_softness, 1e-3);
    terms.toon_highlight = surface.toon_highlight;

    float3 v = normalize(frame.camera_position.xyz - input.world_position);
    float3 l = frame.to_light.xyz;
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_l = saturate(dot(n, l));
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));
    // A surface facing away from the light is in its own shadow, whatever its normal map
    // says. The sun's cascades are looked up here, before the lobes' terms are live: on the
    // Radeon, beside them the lookup cost the shading a step of registers (ADR 0093). With
    // decals (a literal), after the terms: the decals' values are live here.
    float visible = 0.0;
    if (!decals)
        visible = n_dot_l > 0.0 && dot(ng, l) > 0.0 ? shadow(input.light_position, normalize(input.light_normal) * side, input.position.xy) : 0.0;
    // glTF's sheen layer lets through what its lobe does not reflect (KHR_materials_sheen's
    // albedo scaling, from n·v; `sheen_albedo` is the lobe's directional albedo).
    terms.sheen_scaling = model_sheen_layer(M::model())
        ? 1.0 - max(terms.sheen.r, max(terms.sheen.g, terms.sheen.b)) * sheen_albedo(n_dot_v, sheen_roughness)
        : 1.0;

    float roughness = max(perceptual, 0.045);
    float a = roughness * roughness;
    float3 f0 = lerp(float3(0.04, 0.04, 0.04), base.rgb, metallic);
    if (model_coated(M::model()))
        f0 = lerp(f0, f0_under_coat(f0), terms.clearcoat);
    float3 f = fresnel_schlick(v_dot_h, f0);
    float3 specular = distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
    float3 diffuse = (1.0 - metallic) * base.rgb / PI;
    // Subsurface materials keep their diffuse light apart: the lighting below then returns
    // the specular light only.
    float3 surface_diffuse = subsurface ? float3(0.0, 0.0, 0.0) : diffuse;
    float3 ambient_albedo = subsurface ? float3(0.0, 0.0, 0.0) : base.rgb;
    if (decals)
        visible = n_dot_l > 0.0 && dot(ng, l) > 0.0 ? shadow(input.light_position, normalize(input.light_normal) * side, input.position.xy) : 0.0;
    // The sun's contact shadows (ADR 0071; `contact` is a literal): their mask's bindless
    // index comes in `push.phase`, which the shading passes do not otherwise read.
    if (contact && visible > 0.0)
        visible *= asfloat(g_uint_textures[push.phase].Load(int3(int2(input.position.xy), 0)).r);
    float3 local_irradiance = 0.0;
    float3 reflect_weight = 0.0;
    // The lobe the reflections replace the sky's light of (ADR 0073).
    float3 reflect_normal = n;
    float reflect_roughness = roughness;
    // The sun: glTF's lobes as written out; the other models through `model_light`.
    float3 sun;
    if (M::model() == MODEL_LIT)
        sun = (surface_diffuse + specular) * frame.light_color.rgb * n_dot_l * visible;
    else
        sun = model_light(terms, n, v, l, surface_diffuse, f0, a) * frame.light_color.rgb * visible;
    float3 ambient;
    if (M::model() == MODEL_CLOTH)
    {
        // Cloth's environment light: the diffuse, and the sheen's lobe instead of GGX's.
        reflect_roughness = sheen_roughness;
        ambient = ambient_light(n, v, surface_diffuse, f0, roughness, occlusion, ambient_albedo, ao, reflective, reflect_weight, gi, input.position.xy, input.world_position, false, bent, sky_normal);
        ambient += sheen_environment(n, v, reflect_roughness, terms.sheen, occlusion, reflect_weight);
    }
    else
        ambient = ambient_light(n, v, surface_diffuse, f0, roughness, occlusion, ambient_albedo, ao, reflective, reflect_weight, gi, input.position.xy, input.world_position, true, bent, sky_normal);
    if (model_sheen_layer(M::model()))
    {
        // glTF's sheen over the lit base: the base's sky light through what the layer lets
        // pass (the reflections' weight with it: they still replace the base's lobe), and
        // the layer's lobe in the sky's light.
        float3 sheen_weight;
        ambient = ambient * terms.sheen_scaling + sheen_environment(n, v, sheen_roughness, terms.sheen, occlusion, sheen_weight);
        reflect_weight *= terms.sheen_scaling;
    }
    if (model_coated(M::model()))
    {
        // The coat over the base: what it lets through, and what it reflects of the
        // environment. Its lobe, the sharper, is the one the reflections replace. The
        // emission is under the coat too (KHR_materials_clearcoat).
        float fc = fresnel_schlick(max(dot(terms.coat_normal, v), 1e-4), float3(0.04, 0.04, 0.04)).x * terms.clearcoat;
        reflect_normal = terms.coat_normal;
        reflect_roughness = coat_roughness;
        ambient = ambient * (1.0 - fc) + coat_environment(reflect_normal, v, reflect_roughness, terms.clearcoat, occlusion, reflect_weight);
        emissive *= 1.0 - fc;
    }
    if (M::model() == MODEL_TOON)
    {
        // The rim: a flat band of light along the silhouette, where n·v < `rim_width`.
        ambient += surface.rim * (1.0 - smoothstep(surface.rim_width - terms.toon_softness, surface.rim_width + terms.toon_softness, dot(n, v)));
    }
    float3 color = sun
        + ambient
        + emissive
        + local_lights(input.world_position, input.position.xy, n, v, surface_diffuse, f0, a, local_irradiance, shadows, terms);
    sss = (SubsurfaceOut)0;
    refl = (ReflectOut)0;
    if (reflective)
    {
        refl.normal = reflect_normal;
        refl.roughness = reflect_roughness;
        refl.weight = reflect_weight;
    }
    if (subsurface)
    {
        uint bits = surface.subsurface;
        uint index = min(max(bits & 0xFFu, 1u) - 1u, max(frame.subsurface_count, 1u) - 1u);
        SubsurfaceProfile* profile = frame.subsurface + index;
        // Diffuse light per unit base color: sun, local lights, sky.
        float kd = (1.0 - metallic) / PI;
        float3 lit = kd * (frame.light_color.rgb * n_dot_l * visible + local_irradiance) + ambient_diffuse(n, occlusion, kd, gi, input.position.xy, input.world_position);
        // Light from behind, through the material (Jimenez et al. 2010): the profile's slab
        // integral over its thickness, or over the object's in the shadow map.
        uint transmission = (bits >> 16) & 3u;
        float back = saturate(-dot(n, l));
        if (transmission != 0 && back > 0.0)
        {
            float thickness = M::thickness(ctx, params);
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
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(loadAligned<8>(&push.frame.depth_size)));
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
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(loadAligned<8>(&push.frame.depth_size)));
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
    Frame frame = loadAligned<16>(push.frame);
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

// Motion vectors from the depth prepass: a view that draws the prepass gets its motion
// there, and its shading draws without (the shading's vertex stage keeps the still
// variants' registers: on the Radeon 49 VGPRs instead of 78, the environment variants'
// pixel stage 64 instead of 77-80). The clip position is `depthVertex`'s
// (`clip_position`, the same bits as the shading's), the previous one `vertexMain`'s.
struct DepthMotionOut
{
    float4 position : SV_Position;
    float4 previous_clip : TEXCOORD2;
};

[shader("vertex")]
DepthMotionOut depthVertexMotion(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    uint index = push.visible[slot];
    Instance inst = read_instance(push.instances, index);
    float3 p = load_position(vertex);
    DepthMotionOut o;
    o.position = clip_position(inst, p).clip;
    // Without a previous transform the instance counts as still.
    Instance previous = index < push.frame.previous_count ? read_instance(push.previous_instances, index) : inst;
    o.previous_clip = mul(read_matrix(&push.frame.previous_clip_from_world), float4(transform_point(previous, p), 1.0));
    return o;
}

[shader("fragment")]
float2 prepassMotion(DepthMotionOut input) : SV_Target0
{
    return motion(input.position, input.previous_clip);
}

// The prepass of masked materials with motion vectors: the alpha test, then the motion.
struct DepthMaskedMotionOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float4 previous_clip : TEXCOORD2;
    nointerpolation uint material : MATERIAL;
};

[shader("vertex")]
DepthMaskedMotionOut depthVertexMaskedMotion(uint vertex : SV_VulkanVertexID, uint slot : SV_VulkanInstanceID)
{
    uint index = push.visible[slot];
    Instance inst = read_instance(push.instances, index);
    float3 p = load_position(vertex);
    DepthMaskedMotionOut o;
    o.position = clip_position(inst, p).clip;
    o.uv = read_attributes(vertex).uv;
    Instance previous = index < push.frame.previous_count ? read_instance(push.previous_instances, index) : inst;
    o.previous_clip = mul(read_matrix(&push.frame.previous_clip_from_world), float4(transform_point(previous, p), 1.0));
    o.material = inst.material;
    return o;
}

[shader("fragment")]
float2 prepassMaskedMotion(DepthMaskedMotionOut input) : SV_Target0
{
    MaterialContext ctx = coverage_context(input.uv, input.material);
    MATERIAL.Params p = MATERIAL::load(ctx);
    if (MATERIAL::alpha(ctx, p) < MATERIAL::alpha_cutoff(p))
        discard;
    return motion(input.position, input.previous_clip);
}

// With MSAA (ADR 0075): `prepassCoverage`'s alpha to coverage, the motion in the same
// output's first two components (alpha to coverage reads its alpha; the motion target
// stores red and green).
[shader("fragment")]
float4 prepassCoverageMotion(DepthMaskedMotionOut input) : SV_Target0
{
    MaterialContext ctx = coverage_context(input.uv, input.material);
    MATERIAL.Params p = MATERIAL::load(ctx);
    float alpha = MATERIAL::alpha(ctx, p);
    alpha = (alpha - MATERIAL::alpha_cutoff(p)) / max(fwidth(alpha), 1e-4) + 0.5;
    return float4(motion(input.position, input.previous_clip), 0.0, alpha);
}

[shader("fragment")]
ShadedMotion fragmentMainMotion(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = float4(shade(input, front, false, false).rgb, 1.0);
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

// Reflections (ADR 0073): after the depth prepass of a view with reflection probes or
// screen-space reflections, the shading also writes, per pixel, what the reflection
// composite (reflections.slang) needs to replace the sky's specular light: the normal and
// roughness, and the weight of the prefiltered radiance. Entry points of their own (two
// more targets), so the other shading carries none of it.

// A unit vector in octahedral encoding (Cigolle et al. 2014), in [0, 1]².
float2 encode_octahedral(float3 n)
{
    n /= abs(n.x) + abs(n.y) + abs(n.z);
    float2 e = n.z >= 0.0 ? n.xy : (1.0 - abs(n.yx)) * float2(n.x >= 0.0 ? 1.0 : -1.0, n.y >= 0.0 ? 1.0 : -1.0);
    return e * 0.5 + 0.5;
}

struct Reflected
{
    float4 color : SV_Target0;
    float4 normal_roughness : SV_Target1;
    float4 weight : SV_Target2;
};

// `ao`, `shadows` (local lights', ADR 0071), `contact` (the sun's, ADR 0071) and `decals`
// (ADR 0082): literals, as in `shade`.
Reflected shade_reflected(VertexOut input, bool front, bool ao, bool shadows, bool contact, bool decals = false, bool gi = false, bool bent = false)
{
    SubsurfaceOut unused;
    ReflectOut refl;
    Reflected o;
    o.color = float4(shade_surface<MATERIAL>(input, front, false, ao, false, unused, refl, shadows, contact, true, decals, gi, bent).rgb, 1.0);
    o.normal_roughness = float4(encode_octahedral(refl.normal), refl.roughness, 1.0);
    o.weight = float4(refl.weight, 1.0);
    return o;
}

[shader("fragment")]
Reflected fragmentReflect(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, false);
}

[shader("fragment")]
Reflected fragmentReflectShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, false);
}

[shader("fragment")]
Reflected fragmentReflectContact(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectContactShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAo(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false);
}

[shader("fragment")]
Reflected fragmentReflectAoShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false);
}

[shader("fragment")]
Reflected fragmentReflectAoContact(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true);
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
        float4 g = read_float4(&p.gaussians[i]);
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
    float3 falloff = loadAligned<16>(&stored.falloff);
    // The spiral turned by the pixel's angle, scaled to pixels.
    float angle = 6.28318531 * gradient_noise(position.xy + 5.588238 * float(frame.scatter_frame & 63u));
    float2 turn = float2(cos(angle), sin(angle)) * pixels_per_unit;
    float inside2 = inside * inside;
    int2 last = int2(loadAligned<8>(&frame.depth_size)) - 1;
    uint first = BUDGET_FIRST[budget];
    uint samples = BUDGET_SAMPLES[budget];
    for (uint k = 0; k < samples; k++)
    {
        uint4 entry = loadAligned<16>(&stored.kernel[first + k]);
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

// `scatterFragment` with MSAA (ADR 0075): the center is this fragment's own samples'
// (`push.ao` the multisampled light, `push.phase` the multisampled albedo); the
// neighbours come from the nearest-sample resolve (`push.texture`). The far
// side of a silhouette between two subsurface surfaces keeps its own light, depth and
// profile.
[[vk::binding(0, 0)]] Texture2DMS<float4> g_ms_textures[];

[shader("fragment")]
float4 scatterFragmentMs(float4 position : SV_Position, uint coverage : SV_Coverage) : SV_Target0
{
    Frame* frame = push.frame;
    int2 pixel = int2(position.xy);
    Texture2D irradiance = g_textures[push.texture];
    // Of the samples the triangle covers, the one this surface lit: subsurface, at this
    // fragment's own view depth (others may hold a nearer surface the depth test hides).
    float own_depth = 1.0 / position.w;
    uint own = firstbitlow(coverage);
    float closest = 3.0e38;
    for (uint mask = coverage; mask != 0; mask &= mask - 1)
    {
        uint s = firstbitlow(mask);
        float a = g_ms_textures[push.ao].Load(pixel, s).a;
        float distance = abs(a - own_depth);
        if (a > 0.0 && distance < closest)
        {
            closest = distance;
            own = s;
        }
    }
    float4 center = g_ms_textures[push.ao].Load(pixel, own);
    float4 albedo = g_ms_textures[push.phase].Load(pixel, own);
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
    float3 falloff = loadAligned<16>(&stored.falloff);
    // The spiral turned by the pixel's angle, scaled to pixels.
    float angle = 6.28318531 * gradient_noise(position.xy + 5.588238 * float(frame.scatter_frame & 63u));
    float2 turn = float2(cos(angle), sin(angle)) * pixels_per_unit;
    float inside2 = inside * inside;
    int2 last = int2(loadAligned<8>(&frame.depth_size)) - 1;
    uint first = BUDGET_FIRST[budget];
    uint samples = BUDGET_SAMPLES[budget];
    for (uint k = 0; k < samples; k++)
    {
        uint4 entry = loadAligned<16>(&stored.kernel[first + k]);
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
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

// The entry points above with local lights' shadows (ADR 0071): drawn instead of them
// while shadowed lights reach the view, so the others carry no code for it.
[shader("fragment")]
float4 fragmentMainShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMaskedShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentBlendedShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    float4 c = shade(input, front, false, false, true);
    float alpha = saturate(c.a);
    return float4(c.rgb * alpha, alpha);
}

[shader("fragment")]
float4 fragmentBlendedAtmosphereShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    float4 c = shade(input, front, false, false, true);
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(loadAligned<8>(&push.frame.depth_size)));
    float alpha = saturate(c.a);
    return float4(c.rgb * alpha, alpha);
}

[shader("fragment")]
BlendedReactive fragmentBlendedReactiveShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    float4 c = shade(input, front, false, false, true);
    float alpha = saturate(c.a);
    BlendedReactive o;
    o.color = float4(c.rgb * alpha, alpha);
    o.reactive = float4(alpha, 0.0, 0.0, alpha);
    return o;
}

[shader("fragment")]
BlendedReactive fragmentBlendedAtmosphereReactiveShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    float4 c = shade(input, front, false, false, true);
    c.rgb = atmosphere_apply(push.frame.atmosphere, c.rgb, input.world_position, input.position.xy / float2(loadAligned<8>(&push.frame.depth_size)));
    float alpha = saturate(c.a);
    BlendedReactive o;
    o.color = float4(c.rgb * alpha, alpha);
    o.reactive = float4(alpha, 0.0, 0.0, alpha);
    return o;
}

[shader("fragment")]
ShadedMotion fragmentMainMotionShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = float4(shade(input, front, false, false, true).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

[shader("fragment")]
ShadedMotion fragmentMaskedMotionShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    ShadedMotion o;
    o.color = float4(shade(input, front, true, false, true).rgb, 1.0);
    o.motion = motion(input.position, input.previous_clip);
    return o;
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

// After the depth prepass of a view with the sun's contact shadows (ADR 0071): the
// shading entry points that read their mask, with and without ambient occlusion and local
// lights' shadows (the prepass writes the motion vectors).
[shader("fragment")]
float4 fragmentMainContact(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContact(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, true).rgb, 1.0);
}

// Decals (ADR 0082): after the depth prepass of a view with decals, the shading and
// reflection entry points above (with ambient occlusion, the sun's contact shadows,
// local lights' shadows) that also blend the decal buffer in, so the others carry no
// code for it.
[shader("fragment")]
float4 fragmentMainDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, true, true).rgb, 1.0);
}

[shader("fragment")]
Reflected fragmentReflectDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectContactDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, true);
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
    Frame frame = loadAligned<16>(push.frame);
    float4 previous = mul(frame.previous_clip_from_clip, float4(input.position.xy / float2(frame.depth_size) * 2.0 - 1.0 - frame.jitter.xy, 1.0, 1.0));
    float2 uv = input.position.xy / float2(frame.depth_size) - frame.jitter.xy * 0.5;
    if (abs(previous.w) <= 1e-6)
        return float2(0.0, 0.0);
    return uv - (previous.xy / previous.w * 0.5 + 0.5);
}

// The environment's sky along this pixel's ray, and the sun's disk (0.27° in radius: the
// light's illuminance spread over its solid angle; none over a panorama, which shows its
// own sun).
[shader("fragment")]
float4 skyFragment(Background input) : SV_Target0
{
    float2 ndc = input.position.xy / float2(loadAligned<8>(&push.frame.depth_size)) * 2.0 - 1.0;
    // Two points of the ray: the near plane and halfway in depth (finite with an infinite
    // far plane too).
    float4 near = mul(read_matrix(&push.frame.world_from_clip), float4(ndc, 0.0, 1.0));
    float4 mid = mul(read_matrix(&push.frame.world_from_clip), float4(ndc, 0.5, 1.0));
    float3 d = normalize(mid.xyz / mid.w - near.xyz / near.w);
    SamplerState s = g_samplers[push.frame.linear_sampler];
    float3 color = g_cubes[push.frame.sky_cube].SampleLevel(s, turned_back(d, loadAligned<8>(&push.frame.sky_turn)), 0.0).rgb * push.frame.env_intensity;
    float cos_sun = push.frame.sun_disk;
    if (dot(d, read_float4(&push.frame.to_light).xyz) > cos_sun)
        color += read_float4(&push.frame.light_color).rgb / (2.0 * PI * (1.0 - cos_sun));
    return float4(color, 1.0);
}


// Probe-volume GI (ADR 0088): after the depth prepass of a view with a probe volume, the
// shading, reflection and subsurface entry points above that take the diffuse ambient
// light from the GI pass's irradiance per pixel (`push.gi`) instead of the sky's
// harmonics, so the others carry no code for it (the register rule: under a
// specialization constant the dead read still cost the environment shading 6 VGPRs on
// the Radeon).

[shader("fragment")]
float4 fragmentMainGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, false, true, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, false, true, true, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, false, true, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade(input, front, false, true, true, true, true, true).rgb, 1.0);
}

[shader("fragment")]
Reflected fragmentReflectGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectContactGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, false, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, false, true, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, true, true);
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceGi(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, false, false, false, false, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, true, false, false, false, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

// The entry points that read the view's ambient occlusion, with its bent normal
// (`AoSettings::bent_normals`, ADR 0058): drawn instead of them while it is on, so the
// others carry no code for it.
[shader("fragment")]
float4 fragmentMainAoBent(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false).rgb, 1.0);
}

[shader("fragment")]
Reflected fragmentReflectAoBent(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContact(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, false, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, false, false, true);
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceBent(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, false, false, false, false, false, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

[shader("fragment")]
float4 fragmentMainAoBentShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true).rgb, 1.0);
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceBentShadowed(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, true, false, false, false, false, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

[shader("fragment")]
float4 fragmentMainAoBentContact(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactShadowed(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, true, true).rgb, 1.0);
}

[shader("fragment")]
Reflected fragmentReflectAoBentDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, true, false, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactShadowedDecals(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, true, false, true);
}

[shader("fragment")]
float4 fragmentMainAoBentGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, false, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, true, false, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, false, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, false, true, true, true).rgb, 1.0);
}

[shader("fragment")]
float4 fragmentMainAoBentContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace) : SV_Target
{
    return float4(shade_bent(input, front, false, true, true, true, true).rgb, 1.0);
}

[shader("fragment")]
Reflected fragmentReflectAoBentGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, false, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, false, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, false, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, false, true, true, true, true);
}

[shader("fragment")]
Reflected fragmentReflectAoBentContactShadowedDecalsGi(VertexOut input, bool front : SV_IsFrontFace)
{
    return shade_reflected(input, front, true, true, true, true, true, true);
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceBentGi(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, false, false, false, false, true, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}

[shader("fragment")]
SubsurfaceShaded fragmentSubsurfaceBentShadowedGi(VertexOut input, bool front : SV_IsFrontFace)
{
    SubsurfaceOut sss;
    SubsurfaceShaded o;
    ReflectOut unused;
    o.color = float4(shade_surface<GltfMaterial>(input, front, false, true, true, sss, unused, true, false, false, false, true, true).rgb, 1.0);
    o.irradiance = sss.irradiance;
    o.albedo = sss.albedo;
    return o;
}
