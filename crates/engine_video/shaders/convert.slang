// A video picture's Y'CbCr planes to the video's texture (ADR 0100, convert.rs).
//
// Each thread of `convertPicture` writes one texel. A texel's centre lands on the
// picture's luma grid (samples at whole numbers), scaled when the picture and the texture
// differ in size (a stream's render size, frames coded smaller); chroma is read where the
// stream's siting puts it on that grid. Both planes are interpolated bilinearly with
// clamped loads, so a picture of the texture's size reads its luma samples exactly.
//
// `convertHalf` does the same for the usual case, 4:2:0 at the texture's size, a thread
// to 2 × 2 texels: their four luma samples in one gather and the chroma samples they
// share (two columns, three rows) in four, a fifth of the general entry's fetches, with
// the same arithmetic, so the same results.
//
// Samples are read as integer code values: a raw sample (an 8-bit byte, or a 16-bit word
// with the code in its high bits, P010, or in its low bits) shifted right. One affine map
// takes the codes to R'G'B' (the matrix, the range and the bit depth, from the stream's
// colour description). R'G'B' is written through the texture's UNORM view; materials
// sample its sRGB view, in linear light.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] RWTexture2D<float4> g_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct ConvertPush
{
    float4 to_red;       // R' = dot(to_red.xyz, (Y, Cb, Cr)) + to_red.w, in code values
    float4 to_green;
    float4 to_blue;
    uint luma;           // bindless index of the luma plane
    uint chroma;         // bindless index of the chroma plane: Cb in red, Cr in green
    uint target;         // storage index of the texture's UNORM view
    uint shift;          // the low bits a raw sample drops (6 in P010)
    uint2 source_size;   // the picture, in luma samples
    uint2 target_size;   // the texture
    float2 source_step;  // luma samples per texel (source_size / target_size)
    float2 chroma_step;  // chroma samples per luma sample (0.5 subsampled; 0 without chroma)
    float2 chroma_bias;  // the chroma position of luma sample 0 (the siting)
    uint2 chroma_size;   // the chroma plane, in samples
    float raw_scale;     // a raw sample's largest value: 255 or 65535
    uint sampler;        // bindless index of a nearest, clamp-to-edge sampler (gathers)
    uint _pad1;
    uint _pad2;
};

[[vk::push_constant]]
ConstantBuffer<ConvertPush> push;

// Raw samples as code values.
float2 to_codes(float2 v)
{
    return float2(uint2(round(v * push.raw_scale)) >> push.shift);
}

float4 to_codes4(float4 v)
{
    return float4(uint4(round(v * push.raw_scale)) >> push.shift);
}

// Sample `p` of a plane, clamped to `last`, as code values.
float2 codes(uint plane, int2 p, int2 last)
{
    return to_codes(g_textures[plane].Load(int3(clamp(p, int2(0), last), 0)).xy);
}

// A plane of `size` samples at `x` (in samples, centres at whole numbers), bilinearly.
float2 bilinear(uint plane, float2 x, uint2 size)
{
    float2 corner = floor(x);
    float2 f = x - corner;
    int2 p = int2(corner);
    int2 last = int2(size) - 1;
    float2 top = lerp(codes(plane, p, last), codes(plane, p + int2(1, 0), last), f.x);
    float2 bottom = lerp(codes(plane, p + int2(0, 1), last), codes(plane, p + int2(1, 1), last), f.x);
    return lerp(top, bottom, f.y);
}

// R'G'B' of code values, rounded so the UNORM view's conversion has nothing left to round.
float4 rgb(float y, float2 c)
{
    float3 ycc = float3(y, c);
    float3 v = float3(dot(push.to_red.xyz, ycc), dot(push.to_green.xyz, ycc), dot(push.to_blue.xyz, ycc))
        + float3(push.to_red.w, push.to_green.w, push.to_blue.w);
    return float4(round(saturate(v) * 255.0) / 255.0, 1.0);
}

[shader("compute")]
[numthreads(8, 8, 1)]
void convertPicture(uint3 id : SV_DispatchThreadID)
{
    uint2 texel = id.xy;
    if (any(texel >= push.target_size))
        return;
    float2 x = (float2(texel) + 0.5) * push.source_step - 0.5;
    float y;
    if (all(push.source_step == 1.0))
        y = codes(push.luma, int2(texel), int2(push.source_size) - 1).x;
    else
        y = bilinear(push.luma, x, push.source_size).x;
    float2 c = bilinear(push.chroma, x * push.chroma_step + push.chroma_bias, push.chroma_size);
    g_images[push.target][texel] = rgb(y, c);
}

// The 2 × 2 samples of a plane from `corner` (clamped to the image by the sampler):
// (x, y, z, w) = (0, 1), (1, 1), (1, 0), (0, 0), as a gather orders them.
float4 gather_red(uint plane, int2 corner)
{
    uint2 size;
    g_textures[plane].GetDimensions(size.x, size.y);
    // The point the four texel centres share.
    float2 uv = (float2(corner) + 1.0) / float2(size);
    return g_textures[plane].GatherRed(g_samplers[push.sampler], uv);
}

float4 gather_green(uint plane, int2 corner)
{
    uint2 size;
    g_textures[plane].GetDimensions(size.x, size.y);
    float2 uv = (float2(corner) + 1.0) / float2(size);
    return g_textures[plane].GatherGreen(g_samplers[push.sampler], uv);
}

// The texels of a group's 16 × 16 tile, stored after the group computed them all: a
// thread's 2 × 2 texels stored by themselves would leave every store instruction's lanes
// two texels apart (on the Radeon iGPU half the pass's time).
groupshared float4 g_tile[16][16];

[shader("compute")]
[numthreads(8, 8, 1)]
void convertHalf(uint3 id : SV_DispatchThreadID, uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID)
{
    int2 base = int2(id.xy) * 2;
    int2 last = int2(push.chroma_size) - 1;
    float4 y = to_codes4(gather_red(push.luma, base));
    // Chroma columns i, i + 1 and rows j - 1 .. j + 1 (the siting uses two of them).
    int2 c = int2(id.xy);
    int2 above = int2(c.x, c.y - 1);
    float4 cb_a = to_codes4(gather_red(push.chroma, above));
    float4 cr_a = to_codes4(gather_green(push.chroma, above));
    float4 cb_b = to_codes4(gather_red(push.chroma, c));
    float4 cr_b = to_codes4(gather_green(push.chroma, c));
    // Rows j - 1, j and j + 1, columns i and i + 1, as (Cb, Cr) pairs.
    float2 r0[2] = { float2(cb_a.w, cr_a.w), float2(cb_a.z, cr_a.z) };
    float2 r1[2] = { float2(cb_a.x, cr_a.x), float2(cb_a.y, cr_a.y) };
    float2 r2[2] = { float2(cb_b.x, cr_b.x), float2(cb_b.y, cr_b.y) };
    // Past the picture (its image may be larger), the picture's edge, as the general
    // entry's clamped loads read it; row j - 1 above the top is the sampler's clamp.
    if (c.x + 1 > last.x)
    {
        r0[1] = r0[0];
        r1[1] = r1[0];
        r2[1] = r2[0];
    }
    if (c.y + 1 > last.y)
        r2 = r1;
    float luma[4] = { y.w, y.z, y.x, y.y };
    for (int k = 0; k < 4; k++)
    {
        int2 d = int2(k & 1, k >> 1);
        float2 t = float2(base + d) * push.chroma_step + push.chroma_bias;
        float2 corner = floor(t);
        float2 f = t - corner;
        // The first of the two rows used: j - 1 or j (rows are relative to j - 1).
        bool upper = int(corner.y) < c.y;
        float2 top0 = upper ? r0[0] : r1[0];
        float2 top1 = upper ? r0[1] : r1[1];
        float2 bottom0 = upper ? r1[0] : r2[0];
        float2 bottom1 = upper ? r1[1] : r2[1];
        float2 chroma = lerp(lerp(top0, top1, f.x), lerp(bottom0, bottom1, f.x), f.y);
        g_tile[2 * local.y + d.y][2 * local.x + d.x] = rgb(luma[k], chroma);
    }
    GroupMemoryBarrierWithGroupSync();
    // The tile stored row by row: each store instruction's lanes on neighbouring texels.
    uint index = local.y * 8 + local.x;
    for (uint k = 0; k < 4; k++)
    {
        uint p = index + 64 * k;
        uint2 at = uint2(p % 16, p / 16);
        uint2 texel = group.xy * 16 + at;
        if (all(texel < push.target_size))
            g_images[push.target][texel] = g_tile[at.y][at.x];
    }
}
