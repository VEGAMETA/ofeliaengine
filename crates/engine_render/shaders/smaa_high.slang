// SMAA's high preset (`SMAA_PRESET_HIGH`): its edge detection and blending weight passes
// (`smaa_passes.hlsli`, ADR 0075).

#define SMAA_PRESET_HIGH
#include "smaa_passes.hlsli"

[shader("vertex")]
EdgesOut edgesVertex(uint vertex : SV_VulkanVertexID)
{
    return edges_vertex(vertex);
}

[shader("fragment")]
float4 edgesFragment(EdgesOut input) : SV_Target0
{
    return edges_fragment(input);
}

[shader("vertex")]
WeightsOut weightsVertex(uint vertex : SV_VulkanVertexID)
{
    return weights_vertex(vertex);
}

[shader("fragment")]
float4 weightsFragment(WeightsOut input) : SV_Target0
{
    return weights_fragment(input);
}
