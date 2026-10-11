// 2D quads: sprites and SDF shapes (rounded rectangles, circles, capsules, outlines) in one
// pipeline. Instances are read through a buffer device address, in submission order, via
// a draw-order index array when sorting moved anything (the CPU then writes 4 bytes per
// quad in order instead of gathering 64-byte instances); textures and samplers come from
// the bindless arrays, so texture changes never split a draw. Colors are linear; the
// output is premultiplied (alpha 0 = additive, sharing the pipeline with alpha blending).
//
// The rounded-box distance follows Inigo Quilez's "2D distance functions" article (the
// formula, re-derived here with per-quadrant radii; no code copied).

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const uint NO_TEXTURE = 0xFFFFFFFFu;
static const uint FLAG_SAMPLER_MASK = 0x7u; // bits 0-2: bindless sampler index
static const uint FLAG_SDF = 0x8u;          // bit 3: shape with an antialiased SDF edge
static const uint FLAG_ADDITIVE = 0x10u;    // bit 4: output alpha 0 (adds under premultiplied blending)
                                            // bits 16-31: f16 outline thickness (0 = filled)

struct Instance
{
    float2 axis_x;  // half-extent vectors of the quad in world units
    float2 axis_y;
    float2 center;
    uint2 color;    // f16×4, linear, straight alpha
    float4 uv;      // min.xy, max.xy
    uint texture;   // bindless sampled-image index or NO_TEXTURE
    uint flags;
    uint2 radii;    // f16×4 corner radii in world units, quadrants (+,+) (−,+) (−,−) (+,−)
};

struct Push
{
    float4x4 clip_from_world;
    Instance* instances;  // in submission order
    uint* order;          // draw position → instance index (when VIEW_INDIRECT is set)
    float aa_expand;      // world units per pixel: SDF quads grow by this for the edge ramp
    uint view_flags;
    float white;          // output value of white: 1, or SDR white in scRGB units for HDR
                          // output (ADR 0038); 1 for multiplied quads (factors, not light)
    uint depth_texture;   // fragmentDepthTested: the scene's depth, a bindless index
    float2 depth_scale;   // target pixel → depth texel (the render size over the output's)
    float depth_bias;     // a fragment is hidden when deeper than the scene by more
    uint _pad;
};

static const uint VIEW_Y_DOWN = 1;   // world +y points down on screen (pixel/UI views)
static const uint VIEW_INDIRECT = 2; // instances are drawn through `order`

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float2 local : TEXCOORD1;          // position from the center along the axes, world units
    nointerpolation float4 color : COLOR0;
    nointerpolation float4 radii : TEXCOORD2;
    nointerpolation float2 half_size : TEXCOORD3;
    nointerpolation uint texture : TEXCOORD4;
    nointerpolation uint flags : TEXCOORD5;
};

// Half → float by rebiasing the exponent with one multiply (2^112), without the Float16 /
// Int16 capabilities that Slang's f16tof32 declares. Half subnormals may flush to zero;
// infinities and NaNs are not expected.
float half_to_float(uint h)
{
    float magnitude = asfloat((h & 0x7FFFu) << 13) * asfloat(0x77800000u);
    return asfloat(asuint(magnitude) | ((h & 0x8000u) << 16));
}

float2 unpack_half2(uint v)
{
    return float2(half_to_float(v & 0xFFFFu), half_to_float(v >> 16));
}

// `position`: place in the draw order; `corner`: 0..3 → (−,−) (+,−) (−,+) (+,+).
VertexOut emit(uint position, uint corner)
{
    uint index = (push.view_flags & VIEW_INDIRECT) != 0 ? push.order[position] : position;
    Instance inst = push.instances[index];
    float2 half_size = float2(length(inst.axis_x), length(inst.axis_y));
    float2 unit_x = inst.axis_x / max(half_size.x, 1e-20);
    float2 unit_y = inst.axis_y / max(half_size.y, 1e-20);
    float2 grow = (inst.flags & FLAG_SDF) != 0 ? float2(push.aa_expand, push.aa_expand) : float2(0.0, 0.0);
    float2 c = float2((corner & 1) != 0 ? 1.0 : -1.0, (corner & 2) != 0 ? 1.0 : -1.0);
    float2 local = c * (half_size + grow);
    float2 world = inst.center + unit_x * local.x + unit_y * local.y;

    // UVs span the unexpanded quad. The image's top (v = min) is at the quad's local +y
    // edge in y-up views and at its local −y edge in y-down views, so unrotated sprites
    // are upright in both.
    float2 t = local / max(half_size, float2(1e-20, 1e-20)) * 0.5 + 0.5;
    if ((push.view_flags & VIEW_Y_DOWN) == 0)
    {
        t.y = 1.0 - t.y;
    }
    VertexOut o;
    o.position = mul(push.clip_from_world, float4(world, 0.0, 1.0));
    o.uv = lerp(inst.uv.xy, inst.uv.zw, t);
    o.local = local;
    o.color = float4(unpack_half2(inst.color.x), unpack_half2(inst.color.y));
    o.radii = float4(unpack_half2(inst.radii.x), unpack_half2(inst.radii.y));
    o.half_size = half_size;
    o.texture = inst.texture;
    o.flags = inst.flags;
    return o;
}

// Vertex and instance indices use the Vulkan semantics (they include the draw's vertex
// offset / first vertex / first instance); SV_VertexID and SV_InstanceID follow D3D and
// exclude them.

// Indexed draws with a shared index buffer (0 1 2 2 1 3 + 4k): four vertex invocations per
// quad; the draw's vertex offset selects the first instance.
[shader("vertex")]
VertexOut vertexIndexed(uint vertex_id : SV_VulkanVertexID)
{
    return emit(vertex_id >> 2, vertex_id & 3);
}

static const uint LIST_CORNERS[6] = { 0, 1, 2, 2, 1, 3 };

// Non-indexed draws, six vertices per quad.
[shader("vertex")]
VertexOut vertexList(uint vertex_id : SV_VulkanVertexID)
{
    return emit(vertex_id / 6, LIST_CORNERS[vertex_id % 6]);
}

// One six-vertex instance per quad.
[shader("vertex")]
VertexOut vertexInstanced(uint vertex_id : SV_VulkanVertexID, uint instance_id : SV_VulkanInstanceID)
{
    return emit(instance_id, LIST_CORNERS[vertex_id]);
}

// Signed distance to a box of half size `b` with per-quadrant corner radii `r`.
float sd_round_box(float2 p, float2 b, float4 r)
{
    float2 side = p.x > 0.0 ? r.xw : r.yz;  // (upper, lower) radius on this side
    float radius = p.y > 0.0 ? side.x : side.y;
    float2 q = abs(p) - b + radius;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - radius;
}

float4 shade(VertexOut input)
{
    float4 color = input.color;
    // World units per pixel, rotation invariant; `local` is affine, so its derivatives
    // are constant over the quad.
    float2 dx = ddx(input.local);
    float2 dy = ddy(input.local);
    float pixel = sqrt(0.5 * (dot(dx, dx) + dot(dy, dy)));
    if (input.texture != NO_TEXTURE)
    {
        uint sampler_index = input.flags & FLAG_SAMPLER_MASK;
        color *= g_textures[NonUniformResourceIndex(input.texture)].Sample(
            g_samplers[NonUniformResourceIndex(sampler_index)], input.uv);
    }
    if ((input.flags & FLAG_SDF) != 0)
    {
        float d = sd_round_box(input.local, input.half_size, input.radii);
        float thickness = half_to_float(input.flags >> 16);
        if (thickness > 0.0)
        {
            d = abs(d + 0.5 * thickness) - 0.5 * thickness;
        }
        color.a *= saturate(0.5 - d / max(pixel, 1e-20));
    }
    float alpha = (input.flags & FLAG_ADDITIVE) != 0 ? 0.0 : color.a;
    return float4(color.rgb * (color.a * push.white), alpha);
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target0
{
    return shade(input);
}

// UI placed in the scene (ADR 0091): hidden where the scene drew something in front of it.
// The scene's depth runs from 0 at the near plane to 1 at the far plane, at the render size;
// the canvas is drawn with the same projection, so their depths compare.
[shader("fragment")]
float4 fragmentDepthTested(VertexOut input) : SV_Target0
{
    int2 texel = int2(input.position.xy * push.depth_scale);
    float scene = g_textures[NonUniformResourceIndex(push.depth_texture)].Load(int3(texel, 0)).r;
    if (input.position.z > scene + push.depth_bias)
    {
        discard;
    }
    return shade(input);
}
