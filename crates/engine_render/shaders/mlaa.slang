// MLAA as Jimenez et al. made it practical on the GPU (GPU Pro 2, 2011; ADR 0075): luma
// edges against a fixed threshold, line searches through bilinear fetches, the area
// table; none of SMAA's additions (diagonal patterns, corner rounding, local contrast
// adaptation): SMAA's reference code with those off, 8 search steps as in the MLAA paper.

#define SMAA_THRESHOLD 0.1
#define SMAA_MAX_SEARCH_STEPS 8
#define SMAA_DISABLE_DIAG_DETECTION
#define SMAA_DISABLE_CORNER_DETECTION
// Local contrast adaptation off: no edge is ever dropped for a stronger neighbour.
#define SMAA_LOCAL_CONTRAST_ADAPTATION_FACTOR 1.0e9
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
