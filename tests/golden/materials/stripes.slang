// A material type for the golden shots (ADR 0092): stripes along world y, two colors from
// its parameters. Compiled into `forward.slang` as `-D MATERIAL=StripesMaterial`.
struct StripesParams
{
    float4 color_a;
    float4 color_b;
    float frequency;  // stripes per world unit along x and z
    float roughness;
    float metallic;
    float _pad;
};

struct StripesMaterial : IMaterial
{
    typealias Params = StripesParams;

    static uint model()
    {
        return MODEL_LIT;
    }

    static StripesParams load(MaterialContext ctx)
    {
        return material_params<StripesParams>(ctx);
    }

    static Surface surface(MaterialContext ctx, StripesParams p, bool masked)
    {
        Surface s;
        float2 cells = floor(ctx.world_position.xz * p.frequency);
        float checker = frac((cells.x + cells.y) * 0.5) * 2.0;
        s.base = lerp(p.color_a, p.color_b, checker);
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = p.metallic;
        s.roughness = p.roughness;
        s.subsurface = 0;
        return surface_terms(s);
    }

    static float occlusion(MaterialContext ctx, StripesParams p) { return 1.0; }
    static float3 emission(MaterialContext ctx, StripesParams p) { return 0.0; }
    static float alpha(MaterialContext ctx, StripesParams p) { return 1.0; }
    static float alpha_cutoff(StripesParams p) { return 0.0; }
    static float thickness(MaterialContext ctx, StripesParams p) { return 0.0; }
}
