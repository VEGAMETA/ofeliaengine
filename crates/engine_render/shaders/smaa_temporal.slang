// SMAA T2x's passes (Jimenez et al. 2012; ADR 0075): neighbourhood blending that keeps
// each pixel's (antialiased) speed in alpha, then the temporal resolve with last frame's
// result along the motion vectors, the history's weight lowered where the speeds differ
// (CryENGINE 3's reprojection, as in the reference). In perceptual space, as the
// reference does it after tone mapping.

#include "postaa_common.hlsli"

#define SMAA_REPROJECTION 1
#include "smaa_port.hlsli"

struct BlendOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float4 offset : TEXCOORD1;
};

[shader("vertex")]
BlendOut blendVertex(uint vertex : SV_VulkanVertexID)
{
    BlendOut o;
    fullscreen(vertex, o.position, o.uv);
    SMAANeighborhoodBlendingVS(o.uv, o.offset);
    return o;
}

// Into an intermediate (perceptual), speed in alpha.
[shader("fragment")]
float4 blendFragment(BlendOut input) : SV_Target0
{
    return SMAANeighborhoodBlendingPS(input.uv, input.offset, g_textures[push.color],
        g_textures[push.blend], g_textures[push.velocity]);
}

// The resolve: this frame (`color`: SMAA's result, with SMAA 4x the average of both
// samples') and the last (`previous`, kept by a copy), into the output. As
// `SMAAResolvePS` with `SMAA_REPROJECTION`: the motion vectors point back to last
// frame's pixel; the history counts half, less where the speeds differ.
[shader("fragment")]
float4 resolveFragment(BlendOut input) : SV_Target0
{
    float2 uv = input.uv;
    float4 current = SMAASamplePoint(g_textures[push.color], uv);
    float2 velocity = SMAA_DECODE_VELOCITY(SMAASamplePoint(g_textures[push.velocity], uv).rg);
    float4 previous = SMAASamplePoint(g_textures[push.previous], uv - velocity);
    float delta = abs(current.a * current.a - previous.a * previous.a) / 5.0;
    float weight = 0.5 * saturate(1.0 - sqrt(delta) * SMAA_REPROJECTION_WEIGHT_SCALE);
    return output(lerp(current, previous, weight).rgb);
}
