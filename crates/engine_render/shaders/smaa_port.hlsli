// SMAA's porting macros for the engine (`SMAA_CUSTOM_SL`; ADR 0075): textures from the
// bindless set, the samplers whose indices the passes push, every read at level 0 (the
// images have one level, and reads in divergent branches must not need derivatives).
// Include after `postaa_common.hlsli` and the preset's defines.

#define SMAA_CUSTOM_SL
#define SMAA_RT_METRICS push.metrics
#define SMAATexture2D(tex) Texture2D tex
#define SMAATexturePass2D(tex) tex
#define SMAASampleLevelZero(tex, coord) tex.SampleLevel(g_samplers[push.linear_sampler], coord, 0)
#define SMAASampleLevelZeroPoint(tex, coord) tex.SampleLevel(g_samplers[push.point_sampler], coord, 0)
#define SMAASampleLevelZeroOffset(tex, coord, offset) tex.SampleLevel(g_samplers[push.linear_sampler], coord, 0, offset)
#define SMAASample(tex, coord) tex.SampleLevel(g_samplers[push.linear_sampler], coord, 0)
#define SMAASamplePoint(tex, coord) tex.SampleLevel(g_samplers[push.point_sampler], coord, 0)
#define SMAASampleOffset(tex, coord, offset) tex.SampleLevel(g_samplers[push.linear_sampler], coord, 0, offset)
#define SMAA_FLATTEN [flatten]
#define SMAA_BRANCH [branch]
#define SMAAGather(tex, coord) tex.Gather(g_samplers[push.linear_sampler], coord)
#include "SMAA.hlsl"
