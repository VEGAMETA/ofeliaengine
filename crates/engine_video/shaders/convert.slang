// A video picture's Y'CbCr planes to the video's texture (ADR 0100, convert.rs).
//
// Each thread writes one texel. A texel's centre lands on the picture's luma grid
// (samples at whole numbers), scaled when the picture and the texture differ in size (a
// stream's render size, frames coded smaller); chroma is read where the stream's siting
// puts it on that grid. Both planes are interpolated bilinearly with clamped loads, so a
// picture of the texture's size reads its luma samples exactly.
//
// Samples are read as integer code values: a raw sample (an 8-bit byte, or a 16-bit word
// with the code in its high bits, P010, or in its low bits) shifted right. One affine map
// takes the codes to R'G'B' (the matrix, the range and the bit depth, from the stream's
// colour description). R'G'B' is written through the texture's UNORM view; materials
// sample its sRGB view, in linear light.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] RWTexture2D<float4> g_images[];

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
    uint _pad0;
    uint _pad1;
    uint _pad2;
};

[[vk::push_constant]]
ConstantBuffer<ConvertPush> push;

// Sample `p` of a plane, clamped to `last`, as code values.
float2 codes(uint plane, int2 p, int2 last)
{
    float2 v = g_textures[NonUniformResourceIndex(plane)].Load(int3(clamp(p, int2(0), last), 0)).xy;
    return float2(uint2(round(v * push.raw_scale)) >> push.shift);
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
    float3 ycc = float3(y, c);
    float3 rgb = float3(dot(push.to_red.xyz, ycc), dot(push.to_green.xyz, ycc), dot(push.to_blue.xyz, ycc))
        + float3(push.to_red.w, push.to_green.w, push.to_blue.w);
    // Rounded here, so the UNORM view's conversion has nothing left to round.
    g_images[NonUniformResourceIndex(push.target)][texel] = float4(round(saturate(rgb) * 255.0) / 255.0, 1.0);
}
