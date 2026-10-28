// What a toon graph and a water graph would generate (ADR 0123's measurement; the materials
// UX note's question 3): each twice, its numbers compiled in (`*Literals`) and read from its
// parameters (`*Numbers`, as while a graph is open in the editor). Textures are parameters
// either way. The fragment shaders' registers on a GPU:
//
//   ENGINE_PIPELINE_STATS=1 cargo run --release -p engine_render --example material_registers
//     -- --module bench/graph-numbers/graph_numbers.slang
//     --types ToonLiterals,ToonNumbers,WaterLiterals,WaterNumbers
//
// (ENGINE_ADAPTER=radeon for ref-1's iGPU). The numbers of 2026-10-28 are in ADR 0123.

// The shared tail of `IMaterial`.
#define GRAPH_TYPE_COMMON(P)                                                                \
    typealias Params = P;                                                                   \
    static P load(MaterialContext ctx) { return material_params<P>(ctx); }                  \
    static float occlusion(MaterialContext ctx, P p) { return 1.0; }                        \
    static float3 emission(MaterialContext ctx, P p) { return 0.0; }                        \
    static float alpha_cutoff(P p) { return 0.5; }                                          \
    static float thickness(MaterialContext ctx, P p) { return 0.0; }

// A tangent-space normal map's normal at `xy` (two channels, z rebuilt), `strength` of it.
float3 mapped_normal(MaterialContext ctx, float2 xy, float strength)
{
    float3 geometric = ctx.normal * ctx.side;
    float3 t = ctx.tangent.xyz * ctx.side;
    float3 b = ctx.tangent.w * cross(ctx.normal, ctx.tangent.xyz) * ctx.side;
    xy = (xy * 2.0 - 1.0) * strength;
    float z = sqrt(saturate(1.0 - dot(xy, xy)));
    return normalize(xy.x * t + xy.y * b + z * geometric);
}

// Toon: an albedo texture tinted, three bands, a highlight and a rim.
struct ToonLiteralsParams
{
    TextureRef albedo;
};

struct ToonLiterals : IMaterial
{
    GRAPH_TYPE_COMMON(ToonLiteralsParams)
    static uint model() { return MODEL_TOON; }
    static Surface surface(MaterialContext ctx, ToonLiteralsParams p, bool masked)
    {
        Surface s;
        s.base = ctx.sample(p.albedo) * float4(1.0, 0.85, 0.7, 1.0);
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = 0.0;
        s.roughness = 0.6;
        s.subsurface = 0;
        s = surface_terms(s);
        s.toon_steps = 3.0;
        s.toon_softness = 0.04;
        s.toon_highlight = float3(0.9, 0.9, 0.8);
        s.rim = float3(0.25, 0.3, 0.45);
        s.rim_width = 0.35;
        return s;
    }
    static float alpha(MaterialContext ctx, ToonLiteralsParams p) { return 1.0; }
}

struct ToonNumbersParams
{
    TextureRef albedo;
    float4 tint;
    float roughness;
    float steps;
    float softness;
    float rim_width;
    float3 highlight;
    float3 rim;
};

struct ToonNumbers : IMaterial
{
    GRAPH_TYPE_COMMON(ToonNumbersParams)
    static uint model() { return MODEL_TOON; }
    static Surface surface(MaterialContext ctx, ToonNumbersParams p, bool masked)
    {
        Surface s;
        s.base = ctx.sample(p.albedo) * p.tint;
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = 0.0;
        s.roughness = p.roughness;
        s.subsurface = 0;
        s = surface_terms(s);
        s.toon_steps = p.steps;
        s.toon_softness = p.softness;
        s.toon_highlight = p.highlight;
        s.rim = p.rim;
        s.rim_width = p.rim_width;
        return s;
    }
    static float alpha(MaterialContext ctx, ToonNumbersParams p) { return 1.0; }
}

// Water: two normal maps at world scales, blended; deep and shallow colors by height;
// glossy, a little transparent.
struct WaterLiteralsParams
{
    TextureRef ripples;
};

struct WaterLiterals : IMaterial
{
    GRAPH_TYPE_COMMON(WaterLiteralsParams)
    static uint model() { return MODEL_LIT; }
    static Surface surface(MaterialContext ctx, WaterLiteralsParams p, bool masked)
    {
        float2 at = ctx.world_position.xz;
        float2 a = sample_material(p.ripples.packed, at * 0.15 + float2(0.31, 0.07)).xy;
        float2 b = sample_material(p.ripples.packed, at * 0.43 + float2(0.12, 0.58)).xy;
        float depth = saturate(ctx.world_position.y * -0.8 + 0.2);
        Surface s;
        s.base = float4(lerp(float3(0.1, 0.35, 0.4), float3(0.02, 0.08, 0.15), depth), 1.0);
        s.normal = mapped_normal(ctx, lerp(a, b, 0.45), 0.6);
        s.metallic = 0.0;
        s.roughness = 0.04;
        s.subsurface = 0;
        return surface_terms(s);
    }
    static float alpha(MaterialContext ctx, WaterLiteralsParams p) { return 0.85; }
}

struct WaterNumbersParams
{
    TextureRef ripples;
    float scale_a;
    float scale_b;
    float2 offset_a;
    float2 offset_b;
    float blend;
    float strength;
    float3 shallow;
    float depth_scale;
    float3 deep;
    float depth_bias;
    float roughness;
    float opacity;
};

struct WaterNumbers : IMaterial
{
    GRAPH_TYPE_COMMON(WaterNumbersParams)
    static uint model() { return MODEL_LIT; }
    static Surface surface(MaterialContext ctx, WaterNumbersParams p, bool masked)
    {
        float2 at = ctx.world_position.xz;
        float2 a = sample_material(p.ripples.packed, at * p.scale_a + p.offset_a).xy;
        float2 b = sample_material(p.ripples.packed, at * p.scale_b + p.offset_b).xy;
        float depth = saturate(ctx.world_position.y * p.depth_scale + p.depth_bias);
        Surface s;
        s.base = float4(lerp(p.shallow, p.deep, depth), 1.0);
        s.normal = mapped_normal(ctx, lerp(a, b, p.blend), p.strength);
        s.metallic = 0.0;
        s.roughness = p.roughness;
        s.subsurface = 0;
        return surface_terms(s);
    }
    static float alpha(MaterialContext ctx, WaterNumbersParams p) { return p.opacity; }
}
