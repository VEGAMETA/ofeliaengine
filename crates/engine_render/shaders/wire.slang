// Wireframes (ADR 0116, `wire.rs`): instances' triangles drawn as their edges (the
// pipeline's line mode) through `outlineVertex` (`forward.slang`), at the view's output size
// with the camera without jitter.
//
// An edge is drawn where its surface is the one in front: the scene's depth is sampled
// against the outline's tolerance, so a line never fights the surface it lies on. Behind
// another surface it keeps `hidden` of its alpha (0: not drawn). Without the scene's depth
// (lines only) every edge is drawn, over the background `wireFill` paints first.
//
// The vertex stage reads `forward.slang`'s push constants. The wire's own parameters are
// at the address in `batches`, and the scene's depth is `texture`: `outlineVertex` reads
// neither.

[[vk::binding(0, 0)]] Texture2D g_textures[];

static const uint NO_TEXTURE = 0xffffffffu;

// `WireParams` in wire.rs (80 bytes).
struct Params
{
    float4 color;        // the lines': linear, straight alpha
    float4 background;   // `wireFill`'s: linear, straight alpha
    float2 scene_scale;  // scene depth texels per output pixel
    uint2 origin;        // the view's top left in the target
    float near;          // the camera's planes, for distances from depths
    float far;
    float hidden;        // the alpha an edge keeps behind another surface
    float white;         // the output's white: 1, or SDR white in scRGB units for HDR
    float4 _pad;
};

// `Push` in forward/mod.rs and forward.slang (128 bytes), its pointers as addresses.
struct Push
{
    uint64_t frame;
    uint64_t instances;
    Params* params;      // `batches` in forward.slang
    uint64_t visible;
    uint64_t commands;
    uint64_t visibility;
    uint64_t pyramid;
    uint64_t positions;
    uint64_t attributes;
    uint64_t materials;
    uint64_t previous_instances;
    uint64_t light_tiles;
    uint instance_count;
    uint scene_depth;    // `texture` in forward.slang: the scene's depth at the render size
    uint phase;
    uint ao;
    uint decal_albedo;
    uint decal_surface;
    uint gi;
    uint _pad;
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

// `OutlineOut` in forward.slang.
struct WireIn
{
    float4 position : SV_Position;
    nointerpolation uint value : OUTLINE;
};

// The distance along the view of a depth (0 at the near plane, 1 at the far one).
float distance_of(Params p, float depth)
{
    return p.near * p.far / (p.far - depth * (p.far - p.near));
}

[shader("fragment")]
float4 wireFragment(WireIn input) : SV_Target0
{
    Params p = loadAligned<16>(push.params);
    float alpha = p.color.a;
    if (push.scene_depth != NO_TEXTURE)
    {
        // Behind the scene's surface at this pixel by more than its lower resolution and
        // jitter account for (the outline's tolerance).
        int2 at = int2(input.position.xy) - int2(p.origin);
        int2 s = int2((float2(at) + 0.5) * p.scene_scale);
        float scene = distance_of(p, g_textures[push.scene_depth].Load(int3(s, 0)).r);
        float edge = distance_of(p, input.position.z);
        if (edge > scene * 1.01 + 0.01)
            alpha *= p.hidden;
    }
    if (alpha <= 0.0)
        discard;
    return float4(p.color.rgb * (alpha * p.white), alpha);
}

struct FillOut
{
    float4 position : SV_Position;
};

[shader("vertex")]
FillOut wireFillVertex(uint vertex_id : SV_VertexID)
{
    // One triangle covering the view: (0,0), (2,0), (0,2) in UV.
    float2 uv = float2((vertex_id << 1) & 2, vertex_id & 2);
    FillOut o;
    o.position = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

[shader("fragment")]
float4 wireFill(FillOut input) : SV_Target0
{
    Params p = loadAligned<16>(push.params);
    return float4(p.background.rgb * (p.background.a * p.white), p.background.a);
}
