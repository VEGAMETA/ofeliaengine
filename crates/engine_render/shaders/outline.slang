// The outline of chosen instances (ADR 0109, `outline.rs`), drawn over the frame around the
// mask the outline's first pass made (`outlineVertex` in `forward.slang`): each pixel its
// item's class and number, `(class + 1) << 24 | (item + 1)`, 0 where none is.
//
// The mask covers the outlined instances' rectangle of the view only: its (0, 0) is the
// view's `mask_offset`, at the target's `origin`.
//
// A pixel outside the mask within a class's width of it takes that class's color: the
// strongest class first (the active instance, the selection, the hovered one), then the
// nearest, with a soft outer edge. Where the outlined surface lies behind the scene's own
// depth (another object hides it), the line keeps `hidden` of its alpha.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(0, 0)]] Texture2D<uint> g_uint_textures[];

// `EdgePush` in outline.rs (112 bytes).
struct Push
{
    float4 colors[3];      // by class: linear, straight alpha
    float4 widths;         // xyz: by class, pixels; w: the alpha kept where hidden
    uint mask;             // the mask (sampled)
    uint mask_depth;       // its depth (sampled)
    uint scene_depth;      // the scene's depth, at the render size (sampled)
    float white;           // the output's white: 1, or SDR white in scRGB units for HDR
    float2 scene_scale;    // scene depth texels per mask texel
    uint2 origin;          // the view's top left in the target: the mask's (0, 0)
    float near;            // the camera's planes, for distances from depths
    float far;
    uint2 mask_offset;     // the mask's (0, 0) in the view's pixels
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
};

[shader("vertex")]
VertexOut outlineEdgeVertex(uint vertex_id : SV_VertexID)
{
    // One triangle covering the view: (0,0), (2,0), (0,2) in UV.
    float2 uv = float2((vertex_id << 1) & 2, vertex_id & 2);
    VertexOut o;
    o.position = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// How strong a class is: the active instance, then the selection, then the hovered one
// (and none).
uint rank(uint class)
{
    return class == 1u ? 0u : (class == 0u ? 1u : (class == 2u ? 2u : 3u));
}

// The distance along the view of a depth (0 at the near plane, 1 at the far one).
float distance_of(float depth)
{
    return push.near * push.far / (push.far - depth * (push.far - push.near));
}

[shader("fragment")]
float4 outlineEdge(VertexOut input) : SV_Target0
{
    Texture2D<uint> mask = g_uint_textures[push.mask];
    uint width, height;
    mask.GetDimensions(width, height);
    int2 p = int2(input.position.xy) - int2(push.origin);
    // Inside the mask: the object itself, no line.
    if (mask.Load(int3(p, 0)) != 0u)
        discard;
    float best = 1e9;
    uint best_class = 3u;
    int2 best_at = p;
    int reach = int(ceil(max(push.widths.x, max(push.widths.y, push.widths.z))));
    for (int dy = -reach; dy <= reach; dy++)
    {
        for (int dx = -reach; dx <= reach; dx++)
        {
            int2 q = p + int2(dx, dy);
            if (any(q < int2(0, 0)) || q.x >= int(width) || q.y >= int(height))
                continue;
            uint v = mask.Load(int3(q, 0));
            if (v == 0u)
                continue;
            uint class = min((v >> 24) - 1u, 2u);
            float d = length(float2(dx, dy));
            if (d > push.widths[class] + 0.5)
                continue;
            uint r = rank(class);
            uint b = rank(best_class);
            if (r < b || (r == b && d < best))
            {
                best = d;
                best_class = class;
                best_at = q;
            }
        }
    }
    if (best_class == 3u)
        discard;
    float coverage = saturate(push.widths[best_class] + 0.5 - best);
    // Hidden where the outlined surface is farther than the scene's at that pixel, by more
    // than the scene's lower resolution and jitter can account for.
    float surface = distance_of(g_textures[push.mask_depth].Load(int3(best_at, 0)).r);
    int2 s = int2((float2(best_at + int2(push.mask_offset)) + 0.5) * push.scene_scale);
    float scene = distance_of(g_textures[push.scene_depth].Load(int3(s, 0)).r);
    float shown = surface > scene * 1.01 + 0.01 ? push.widths.w : 1.0;
    float4 color = push.colors[best_class];
    float alpha = color.a * coverage * shown;
    return float4(color.rgb * (alpha * push.white), alpha);
}
