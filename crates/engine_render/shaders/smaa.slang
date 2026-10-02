// SMAA's last pass (ADR 0075): neighbourhood blending by the weights, decoded into the
// output (or kept perceptual for SMAA T2x, `smaa_temporal.slang`). Independent of the
// preset.

#include "postaa_common.hlsli"

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

[shader("fragment")]
float4 blendFragment(BlendOut input) : SV_Target0
{
    float4 c = SMAANeighborhoodBlendingPS(input.uv, input.offset, g_textures[push.color], g_textures[push.blend]);
    return output(c.rgb);
}
