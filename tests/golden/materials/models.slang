// Material types for the shading models' golden shots (ADR 0092): one type per model, on
// the procedural scene's bottom row of spheres, each set by a material file
// (`<model>.material.ron`). Their parameters are shared: the base and every model's terms.
struct ModelParams
{
    float4 base;                // linear base color, alpha
    float roughness;
    float metallic;
    float clearcoat;            // clear coat
    float clearcoat_roughness;
    float3 sheen;               // cloth
    float sheen_roughness;
    float hair_shift;           // hair
    float toon_steps;           // toon
    float toon_softness;
    float3 toon_highlight;
    float3 rim;
    float rim_width;
};

// What every type here reads first: the color, the interpolated normal.
Surface model_surface(MaterialContext ctx, ModelParams p)
{
    Surface s;
    s.base = p.base;
    s.normal = normalize(ctx.normal * ctx.side);
    s.metallic = p.metallic;
    s.roughness = p.roughness;
    s.subsurface = 0;
    return surface_terms(s);
}

// The rest of `IMaterial`, the same for every type here.
#define MODEL_TYPE_COMMON                                                                   \
    typealias Params = ModelParams;                                                         \
    static ModelParams load(MaterialContext ctx) { return material_params<ModelParams>(ctx); } \
    static float occlusion(MaterialContext ctx, ModelParams p) { return 1.0; }              \
    static float3 emission(MaterialContext ctx, ModelParams p) { return 0.0; }              \
    static float alpha(MaterialContext ctx, ModelParams p) { return 1.0; }                  \
    static float alpha_cutoff(ModelParams p) { return 0.0; }                                \
    static float thickness(MaterialContext ctx, ModelParams p) { return 0.0; }

// glTF's lighting, as a type: the baseline of `material_registers` (engine_render's
// examples).
struct LitMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_LIT; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        return model_surface(ctx, p);
    }
}

struct UnlitMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_UNLIT; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        return model_surface(ctx, p);
    }
}

struct ClearcoatMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_CLEARCOAT; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        Surface s = model_surface(ctx, p);
        s.clearcoat = p.clearcoat;
        s.clearcoat_roughness = p.clearcoat_roughness;
        return s;
    }
}

struct ClothMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_CLOTH; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        Surface s = model_surface(ctx, p);
        s.sheen = p.sheen;
        s.sheen_roughness = p.sheen_roughness;
        return s;
    }
}

struct HairMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_HAIR; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        Surface s = model_surface(ctx, p);
        // Strands combed down the sphere: world −y in the surface's plane (any direction
        // at the poles).
        float3 down = float3(0.0, -1.0, 0.0) - s.normal * -s.normal.y;
        s.hair_tangent = dot(down, down) > 1e-6 ? normalize(down) : float3(1.0, 0.0, 0.0);
        s.hair_shift = p.hair_shift;
        return s;
    }
}

struct ToonMaterial : IMaterial
{
    MODEL_TYPE_COMMON
    static uint model() { return MODEL_TOON; }
    static Surface surface(MaterialContext ctx, ModelParams p, bool masked)
    {
        Surface s = model_surface(ctx, p);
        s.toon_steps = p.toon_steps;
        s.toon_softness = p.toon_softness;
        s.toon_highlight = p.toon_highlight;
        s.rim = p.rim;
        s.rim_width = p.rim_width;
        return s;
    }
}
