// A fabric for the golden shots (ADR 0092): a texture and a normal map from the material
// file, lit as cloth (Charlie sheen). Sponza's curtains through a cooked pack.
struct FabricParams
{
    TextureRef albedo;      // sRGB color
    TextureRef normal;      // tangent-space normal map
    float4 tint;            // multiplies the albedo
    float3 sheen;           // the sheen's color
    float sheen_roughness;
    float roughness;
};

struct FabricMaterial : IMaterial
{
    typealias Params = FabricParams;

    static uint model() { return MODEL_CLOTH; }

    static FabricParams load(MaterialContext ctx)
    {
        return material_params<FabricParams>(ctx);
    }

    static Surface surface(MaterialContext ctx, FabricParams p, bool masked)
    {
        Surface s;
        s.base = ctx.sample(p.albedo) * p.tint;
        float3 geometric = ctx.normal * ctx.side;
        float3 t = ctx.tangent.xyz * ctx.side;
        float3 b = ctx.tangent.w * cross(ctx.normal, ctx.tangent.xyz) * ctx.side;
        float2 xy = ctx.sample(p.normal).xy * 2.0 - 1.0;
        float z = sqrt(saturate(1.0 - dot(xy, xy)));
        s.normal = normalize(xy.x * t + xy.y * b + z * geometric);
        s.metallic = 0.0;
        s.roughness = p.roughness;
        s.subsurface = 0;
        s = surface_terms(s);
        s.sheen = p.sheen;
        s.sheen_roughness = p.sheen_roughness;
        return s;
    }

    static float occlusion(MaterialContext ctx, FabricParams p) { return 1.0; }
    static float3 emission(MaterialContext ctx, FabricParams p) { return 0.0; }
    static float alpha(MaterialContext ctx, FabricParams p) { return 1.0; }
    static float alpha_cutoff(FabricParams p) { return 0.0; }
    static float thickness(MaterialContext ctx, FabricParams p) { return 0.0; }
}
