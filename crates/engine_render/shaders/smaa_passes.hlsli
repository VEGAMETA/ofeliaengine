// SMAA's edge detection and blending weight passes (ADR 0075) around the reference code
// (`SMAA.hlsl`, Jimenez et al. 2013, MIT, unmodified), for the preset the including file
// defines (`smaa_low.slang`, ... `mlaa.slang`, which declare the entry points): each preset
// is its own set of entry points,
// so a view pays only for its own (searches, diagonals and corners are compile-time).
//
// Textures come from the bindless set (`postaa_common.hlsli`); the area and search
// tables are the reference's (`data/smaa_area.bin`, `data/smaa_search.bin`).

#include "postaa_common.hlsli"

#include "smaa_port.hlsli"

struct EdgesOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float4 offset0 : TEXCOORD1;
    float4 offset1 : TEXCOORD2;
    float4 offset2 : TEXCOORD3;
};

EdgesOut edges_vertex(uint vertex)
{
    EdgesOut o;
    fullscreen(vertex, o.position, o.uv);
    float4 offset[3];
    SMAAEdgeDetectionVS(o.uv, offset);
    o.offset0 = offset[0];
    o.offset1 = offset[1];
    o.offset2 = offset[2];
    return o;
}

float4 edges_fragment(EdgesOut input) : SV_Target0
{
    float4 offset[3] = { input.offset0, input.offset1, input.offset2 };
    return float4(SMAALumaEdgeDetectionPS(input.uv, offset, g_textures[push.color]), 0.0, 0.0);
}

struct WeightsOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float2 pixel : TEXCOORD1;
    float4 offset0 : TEXCOORD2;
    float4 offset1 : TEXCOORD3;
    float4 offset2 : TEXCOORD4;
};

WeightsOut weights_vertex(uint vertex)
{
    WeightsOut o;
    fullscreen(vertex, o.position, o.uv);
    float4 offset[3];
    SMAABlendingWeightCalculationVS(o.uv, o.pixel, offset);
    o.offset0 = offset[0];
    o.offset1 = offset[1];
    o.offset2 = offset[2];
    return o;
}

float4 weights_fragment(WeightsOut input) : SV_Target0
{
    float4 offset[3] = { input.offset0, input.offset1, input.offset2 };
    return SMAABlendingWeightCalculationPS(input.uv, input.pixel, offset,
        g_textures[push.edges], g_textures[push.area], g_textures[push.search],
        push.subsample_indices);
}
