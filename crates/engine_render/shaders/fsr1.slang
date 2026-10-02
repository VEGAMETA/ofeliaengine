// AMD FidelityFX Super Resolution 1 (ADR 0075): EASU, edge-adaptive spatial upsampling,
// and RCAS, robust contrast-adaptive sharpening. A port of the 32-bit paths of
// `ffx_fsr1.h` (FidelityFX SDK 1.1.4, MIT, vendored in third_party/FidelityFX-SDK):
// `ffxFsrEasuFloat`, `fsrEasuSetFloat`, `fsrEasuTapFloat`, `FsrRcasF` and the
// approximations they use, with the SDK's names in comments. One pixel per thread.
//
// FSR 1 works on perceptual colors in 0..1. The scene's HDR color is mapped reversibly
// (c · e / (1 + max(c · e)), e the exposure the tone mapper will apply) as it is read, and
// RCAS writes it back to HDR (the inverse), so bloom and tone mapping follow unchanged.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

// `Push` in fsr1.rs (96 bytes).
struct Push
{
    uint4 con0;            // ffxFsrPopulateEasuConstants' con0..con3
    uint4 con1;
    uint4 con2;
    uint4 con3;
    uint source;           // EASU: the scene (sampled); RCAS: EASU's output or the scene
    uint target;           // storage
    uint linear_sampler;
    float exposure;        // the tone mapper's exposure (fixed part)
    float4* auto_exposure; // x: the measured exposure, with `has_auto`
    float sharpness;       // RCAS: exp2(-stops)
    uint has_auto;         // 1: multiply by the measured exposure
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

float exposure()
{
    float e = push.exposure;
    if (push.has_auto != 0)
        e *= push.auto_exposure[0].x;
    return e;
}

float max3(float3 c)
{
    return max(c.r, max(c.g, c.b));
}

// Scene radiance → 0..1, reversibly. Infinite or huge values (a sun disk can overflow
// half floats) stay finite: the range above 10⁴ times white is lost, which no display
// shows anyway.
float3 compress(float3 c, float e)
{
    c = clamp(c * e, 0.0, 1.0e4);
    return c / (1.0 + max3(c));
}

float3 expand(float3 c, float e)
{
    return c / max(1.0 - max3(c), 1e-4) / e;
}

// ffxApproximateReciprocal, ffxApproximateReciprocalMedium,
// ffxApproximateReciprocalSquareRoot.
float rcp_approx(float v)
{
    return asfloat(0x7ef07ebbu - asuint(v));
}

float rcp_medium(float v)
{
    float b = asfloat(0x7ef19fffu - asuint(v));
    return b * (-b * v + 2.0);
}

float rsqrt_approx(float v)
{
    return asfloat(0x5f347d74u - (asuint(v) >> 1u));
}

// fsrEasuTapFloat.
void easu_tap(inout float3 accumulated, inout float weight_sum, float2 offset, float2 dir, float2 len, float lob, float clp, float3 c)
{
    float2 v;
    v.x = offset.x * dir.x + offset.y * dir.y;
    v.y = offset.x * -dir.y + offset.y * dir.x;
    v *= len;
    float d2 = min(v.x * v.x + v.y * v.y, clp);
    float wb = (2.0 / 5.0) * d2 - 1.0;
    float wa = lob * d2 - 1.0;
    wb *= wb;
    wa *= wa;
    wb = (25.0 / 16.0) * wb - (25.0 / 16.0 - 1.0);
    float w = wb * wa;
    accumulated += c * w;
    weight_sum += w;
}

// fsrEasuSetFloat; `corner` 0..3 is s, t, u, v.
void easu_set(inout float2 dir, inout float len, float2 pp, uint corner, float la, float lb, float lc, float ld, float le)
{
    float w = 0.0;
    if (corner == 0)
        w = (1.0 - pp.x) * (1.0 - pp.y);
    if (corner == 1)
        w = pp.x * (1.0 - pp.y);
    if (corner == 2)
        w = (1.0 - pp.x) * pp.y;
    if (corner == 3)
        w = pp.x * pp.y;
    float dc = ld - lc;
    float cb = lc - lb;
    float lx = rcp_approx(max(abs(dc), abs(cb)));
    float dx = ld - lb;
    dir.x += dx * w;
    lx = saturate(abs(dx) * lx);
    lx *= lx;
    len += lx * w;
    float ec = le - lc;
    float ca = lc - la;
    float ly = rcp_approx(max(abs(ec), abs(ca)));
    float dy = le - la;
    dir.y += dy * w;
    ly = saturate(abs(dy) * ly);
    ly *= ly;
    len += ly * w;
}

// Four texels' compressed colors (gather order: (0, 1), (1, 1), (1, 0), (0, 0)).
void gather(float2 p, float e, out float4 r, out float4 g, out float4 b)
{
    Texture2D t = g_textures[push.source];
    SamplerState s = g_samplers[push.linear_sampler];
    float4 r0 = t.GatherRed(s, p);
    float4 g0 = t.GatherGreen(s, p);
    float4 b0 = t.GatherBlue(s, p);
    [unroll]
    for (uint i = 0; i < 4; i++)
    {
        float3 c = compress(float3(r0[i], g0[i], b0[i]), e);
        r[i] = c.r;
        g[i] = c.g;
        b[i] = c.b;
    }
}

[shader("compute")]
[numthreads(8, 8, 1)]
void easu(uint3 id : SV_DispatchThreadID)
{
    uint2 size;
    g_images[push.target].GetDimensions(size.x, size.y);
    if (any(id.xy >= size))
        return;
    float e = exposure();
    // ffxFsrEasuFloat.
    float2 pp = float2(id.xy) * asfloat(push.con0.xy) + asfloat(push.con0.zw);
    float2 fp = floor(pp);
    pp -= fp;
    float2 p0 = fp * asfloat(push.con1.xy) + asfloat(push.con1.zw);
    float2 p1 = p0 + asfloat(push.con2.xy);
    float2 p2 = p0 + asfloat(push.con2.zw);
    float2 p3 = p0 + asfloat(push.con3.xy);
    float4 bczzR, bczzG, bczzB, ijfeR, ijfeG, ijfeB, klhgR, klhgG, klhgB, zzonR, zzonG, zzonB;
    gather(p0, e, bczzR, bczzG, bczzB);
    gather(p1, e, ijfeR, ijfeG, ijfeB);
    gather(p2, e, klhgR, klhgG, klhgB);
    gather(p3, e, zzonR, zzonG, zzonB);
    float4 bczzL = bczzB * 0.5 + (bczzR * 0.5 + bczzG);
    float4 ijfeL = ijfeB * 0.5 + (ijfeR * 0.5 + ijfeG);
    float4 klhgL = klhgB * 0.5 + (klhgR * 0.5 + klhgG);
    float4 zzonL = zzonB * 0.5 + (zzonR * 0.5 + zzonG);
    float bL = bczzL.x;
    float cL = bczzL.y;
    float iL = ijfeL.x;
    float jL = ijfeL.y;
    float fL = ijfeL.z;
    float eL = ijfeL.w;
    float kL = klhgL.x;
    float lL = klhgL.y;
    float hL = klhgL.z;
    float gL = klhgL.w;
    float oL = zzonL.z;
    float nL = zzonL.w;
    float2 dir = 0.0;
    float len = 0.0;
    easu_set(dir, len, pp, 0, bL, eL, fL, gL, jL);
    easu_set(dir, len, pp, 1, cL, fL, gL, hL, kL);
    easu_set(dir, len, pp, 2, fL, iL, jL, kL, nL);
    easu_set(dir, len, pp, 3, gL, jL, kL, lL, oL);
    float2 dir2 = dir * dir;
    float dirR = dir2.x + dir2.y;
    bool zro = dirR < 1.0 / 32768.0;
    dirR = rsqrt_approx(dirR);
    dirR = zro ? 1.0 : dirR;
    dir.x = zro ? 1.0 : dir.x;
    dir *= dirR;
    len = len * 0.5;
    len *= len;
    float stretch = (dir.x * dir.x + dir.y * dir.y) * rcp_approx(max(abs(dir.x), abs(dir.y)));
    float2 len2 = float2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = rcp_approx(lob);
    float3 f = float3(ijfeR.z, ijfeG.z, ijfeB.z);
    float3 g = float3(klhgR.w, klhgG.w, klhgB.w);
    float3 j = float3(ijfeR.y, ijfeG.y, ijfeB.y);
    float3 k = float3(klhgR.x, klhgG.x, klhgB.x);
    float3 min4 = min(min(f, g), min(j, k));
    float3 max4 = max(max(f, g), max(j, k));
    float3 aC = 0.0;
    float aW = 0.0;
    easu_tap(aC, aW, float2(0.0, -1.0) - pp, dir, len2, lob, clp, float3(bczzR.x, bczzG.x, bczzB.x));
    easu_tap(aC, aW, float2(1.0, -1.0) - pp, dir, len2, lob, clp, float3(bczzR.y, bczzG.y, bczzB.y));
    easu_tap(aC, aW, float2(-1.0, 1.0) - pp, dir, len2, lob, clp, float3(ijfeR.x, ijfeG.x, ijfeB.x));
    easu_tap(aC, aW, float2(0.0, 1.0) - pp, dir, len2, lob, clp, j);
    easu_tap(aC, aW, float2(0.0, 0.0) - pp, dir, len2, lob, clp, f);
    easu_tap(aC, aW, float2(-1.0, 0.0) - pp, dir, len2, lob, clp, float3(ijfeR.w, ijfeG.w, ijfeB.w));
    easu_tap(aC, aW, float2(1.0, 1.0) - pp, dir, len2, lob, clp, k);
    easu_tap(aC, aW, float2(2.0, 1.0) - pp, dir, len2, lob, clp, float3(klhgR.y, klhgG.y, klhgB.y));
    easu_tap(aC, aW, float2(2.0, 0.0) - pp, dir, len2, lob, clp, float3(klhgR.z, klhgG.z, klhgB.z));
    easu_tap(aC, aW, float2(1.0, 0.0) - pp, dir, len2, lob, clp, g);
    easu_tap(aC, aW, float2(1.0, 2.0) - pp, dir, len2, lob, clp, float3(zzonR.z, zzonG.z, zzonB.z));
    easu_tap(aC, aW, float2(0.0, 2.0) - pp, dir, len2, lob, clp, float3(zzonR.w, zzonG.w, zzonB.w));
    float3 pix = min(max4, max(min4, aC * (1.0 / aW)));
    g_images[push.target][id.xy] = float4(pix, 1.0);
}

// FSR_RCAS_LIMIT.
static const float RCAS_LIMIT = 0.25 - 1.0 / 16.0;

// FsrRcasF on `source`, whose colors are compressed already (EASU's output: `scene`
// false) or scene radiance (`scene` true: at the output size, RCAS alone). Written back
// as scene radiance.
void rcas(uint2 ip, bool scene)
{
    uint2 size;
    g_images[push.target].GetDimensions(size.x, size.y);
    if (any(ip >= size))
        return;
    float ex = exposure();
    Texture2D t = g_textures[push.source];
    int2 sp = int2(ip);
    int2 hi = int2(size) - 1;
    float3 b = t.Load(int3(clamp(sp + int2(0, -1), 0, hi), 0)).rgb;
    float3 d = t.Load(int3(clamp(sp + int2(-1, 0), 0, hi), 0)).rgb;
    float3 e = t.Load(int3(sp, 0)).rgb;
    float3 f = t.Load(int3(clamp(sp + int2(1, 0), 0, hi), 0)).rgb;
    float3 h = t.Load(int3(clamp(sp + int2(0, 1), 0, hi), 0)).rgb;
    if (scene)
    {
        b = compress(b, ex);
        d = compress(d, ex);
        e = compress(e, ex);
        f = compress(f, ex);
        h = compress(h, ex);
    }
    float3 mn4 = min(min(b, d), min(f, h));
    float3 mx4 = max(max(b, d), max(f, h));
    float2 peakC = float2(1.0, -4.0);
    float3 hitMin = mn4 * (1.0 / (4.0 * mx4));
    float3 hitMax = (peakC.x - mx4) * (1.0 / (4.0 * mn4 + peakC.y));
    float3 lobeRGB = max(-hitMin, hitMax);
    float lobe = max(-RCAS_LIMIT, min(max(lobeRGB.r, max(lobeRGB.g, lobeRGB.b)), 0.0)) * push.sharpness;
    float rcpL = rcp_medium(4.0 * lobe + 1.0);
    float3 pix = (lobe * b + lobe * d + lobe * h + lobe * f + e) * rcpL;
    g_images[push.target][ip] = float4(expand(pix, ex), 1.0);
}

// After EASU.
[shader("compute")]
[numthreads(8, 8, 1)]
void rcasScaled(uint3 id : SV_DispatchThreadID)
{
    rcas(id.xy, false);
}

// At the output size: the scene, sharpened.
[shader("compute")]
[numthreads(8, 8, 1)]
void rcasNative(uint3 id : SV_DispatchThreadID)
{
    rcas(id.xy, true);
}
