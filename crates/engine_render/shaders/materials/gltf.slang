// glTF's material extensions on the shading models (ADR 0092): the built-in types the
// import gives the materials that use `KHR_materials_unlit`, `KHR_materials_clearcoat` and
// `KHR_materials_sheen` (engine_assets' `apply_gltf_models`;
// docs/research/gltf-material-extensions.md). Each is glTF's metallic-roughness material,
// whose record in the material table it reads as `GltfMaterial` does, with its extension's
// parameters in the type's own slot (`material_params`). A module of types, included by
// `forward.slang` (`MATERIAL_FILE`): no entry points of its own.

// KHR_materials_unlit: the base color and its alpha; no light, no emission (the
// extension ignores everything about lighting).
struct GltfUnlit : IMaterial
{
    // Nothing of its own: the glTF record holds all it reads.
    typealias Params = Material;

    static uint model()
    {
        return MODEL_UNLIT;
    }

    static Material load(MaterialContext ctx)
    {
        return read_material(ctx.material);
    }

    static Surface surface(MaterialContext ctx, Material m, bool masked)
    {
        return GltfMaterial::surface(ctx, m, masked);
    }

    static float occlusion(MaterialContext ctx, Material m)
    {
        return 1.0;
    }

    static float3 emission(MaterialContext ctx, Material m)
    {
        return 0.0;
    }

    static float alpha(MaterialContext ctx, Material m)
    {
        return GltfMaterial::alpha(ctx, m);
    }

    static float alpha_cutoff(Material m)
    {
        return m.alpha_cutoff;
    }

    static float thickness(MaterialContext ctx, Material m)
    {
        return 0.0;
    }
}

// The coat of KHR_materials_clearcoat on `s`: its strength (the factor times the texture's
// red), its roughness (times green) and its own normal map (none: the interpolated normal).
Surface gltf_clearcoat(MaterialContext ctx, Surface s, float factor, float roughness, float normal_scale, TextureRef texture, TextureRef roughness_texture, TextureRef normal_texture)
{
    s.clearcoat = factor * ctx.sample(texture).r;
    s.clearcoat_roughness = roughness * ctx.sample(roughness_texture).g;
    if (normal_texture.packed != NO_TEXTURE)
    {
        // As the base's normal map (MikkTSpace), on the interpolated normal.
        float3 geometric = ctx.normal * ctx.side;
        float3 t = ctx.tangent.xyz * ctx.side;
        float3 b = ctx.tangent.w * cross(ctx.normal, ctx.tangent.xyz) * ctx.side;
        float2 xy = ctx.sample(normal_texture).xy * 2.0 - 1.0;
        float z = sqrt(saturate(1.0 - dot(xy, xy)));
        xy *= normal_scale;
        s.clearcoat_normal = normalize(xy.x * t + xy.y * b + z * geometric);
    }
    return s;
}

// The layer of KHR_materials_sheen on `s`: its color (the factor times the texture's sRGB
// color) and roughness (times the texture's alpha).
Surface gltf_sheen(MaterialContext ctx, Surface s, float3 color, float roughness, TextureRef color_texture, TextureRef roughness_texture)
{
    s.sheen = color * ctx.sample(color_texture).rgb;
    s.sheen_roughness = roughness * ctx.sample(roughness_texture).a;
    return s;
}

// KHR_materials_clearcoat's parameters, and the glTF record's alpha cutoff (for the
// masked depth passes' `alpha_cutoff`, which has no context to read the record with).
struct GltfClearcoatParams
{
    float clearcoat;
    float clearcoat_roughness;
    float clearcoat_normal_scale;
    float alpha_cutoff;
    TextureRef clearcoat_texture;
    TextureRef clearcoat_roughness_texture;
    TextureRef clearcoat_normal_texture;
};

// glTF's material under KHR_materials_clearcoat.
struct GltfClearcoat : IMaterial
{
    typealias Params = GltfClearcoatParams;

    static uint model()
    {
        return MODEL_CLEARCOAT;
    }

    static GltfClearcoatParams load(MaterialContext ctx)
    {
        return material_params<GltfClearcoatParams>(ctx);
    }

    static Surface surface(MaterialContext ctx, GltfClearcoatParams p, bool masked)
    {
        Surface s = GltfMaterial::surface(ctx, read_material(ctx.material), masked);
        return gltf_clearcoat(ctx, s, p.clearcoat, p.clearcoat_roughness, p.clearcoat_normal_scale, p.clearcoat_texture, p.clearcoat_roughness_texture, p.clearcoat_normal_texture);
    }

    static float occlusion(MaterialContext ctx, GltfClearcoatParams p)
    {
        return GltfMaterial::occlusion(ctx, read_material(ctx.material));
    }

    static float3 emission(MaterialContext ctx, GltfClearcoatParams p)
    {
        return GltfMaterial::emission(ctx, read_material(ctx.material));
    }

    static float alpha(MaterialContext ctx, GltfClearcoatParams p)
    {
        return GltfMaterial::alpha(ctx, read_material(ctx.material));
    }

    static float alpha_cutoff(GltfClearcoatParams p)
    {
        return p.alpha_cutoff;
    }

    static float thickness(MaterialContext ctx, GltfClearcoatParams p)
    {
        return GltfMaterial::thickness(ctx, read_material(ctx.material));
    }
}

// KHR_materials_sheen's parameters, and the record's alpha cutoff (as above).
struct GltfSheenParams
{
    float3 sheen_color;
    float sheen_roughness;
    float alpha_cutoff;
    TextureRef sheen_color_texture;
    TextureRef sheen_roughness_texture;
};

// glTF's material under KHR_materials_sheen.
struct GltfSheen : IMaterial
{
    typealias Params = GltfSheenParams;

    static uint model()
    {
        return MODEL_SHEEN;
    }

    static GltfSheenParams load(MaterialContext ctx)
    {
        return material_params<GltfSheenParams>(ctx);
    }

    static Surface surface(MaterialContext ctx, GltfSheenParams p, bool masked)
    {
        Surface s = GltfMaterial::surface(ctx, read_material(ctx.material), masked);
        return gltf_sheen(ctx, s, p.sheen_color, p.sheen_roughness, p.sheen_color_texture, p.sheen_roughness_texture);
    }

    static float occlusion(MaterialContext ctx, GltfSheenParams p)
    {
        return GltfMaterial::occlusion(ctx, read_material(ctx.material));
    }

    static float3 emission(MaterialContext ctx, GltfSheenParams p)
    {
        return GltfMaterial::emission(ctx, read_material(ctx.material));
    }

    static float alpha(MaterialContext ctx, GltfSheenParams p)
    {
        return GltfMaterial::alpha(ctx, read_material(ctx.material));
    }

    static float alpha_cutoff(GltfSheenParams p)
    {
        return p.alpha_cutoff;
    }

    static float thickness(MaterialContext ctx, GltfSheenParams p)
    {
        return GltfMaterial::thickness(ctx, read_material(ctx.material));
    }
}

// Both extensions' parameters (the coat over the sheen), and the record's alpha cutoff.
struct GltfSheenClearcoatParams
{
    float3 sheen_color;
    float sheen_roughness;
    float clearcoat;
    float clearcoat_roughness;
    float clearcoat_normal_scale;
    float alpha_cutoff;
    TextureRef sheen_color_texture;
    TextureRef sheen_roughness_texture;
    TextureRef clearcoat_texture;
    TextureRef clearcoat_roughness_texture;
    TextureRef clearcoat_normal_texture;
};

// glTF's material under KHR_materials_sheen and KHR_materials_clearcoat.
struct GltfSheenClearcoat : IMaterial
{
    typealias Params = GltfSheenClearcoatParams;

    static uint model()
    {
        return MODEL_SHEEN_CLEARCOAT;
    }

    static GltfSheenClearcoatParams load(MaterialContext ctx)
    {
        return material_params<GltfSheenClearcoatParams>(ctx);
    }

    static Surface surface(MaterialContext ctx, GltfSheenClearcoatParams p, bool masked)
    {
        Surface s = GltfMaterial::surface(ctx, read_material(ctx.material), masked);
        s = gltf_sheen(ctx, s, p.sheen_color, p.sheen_roughness, p.sheen_color_texture, p.sheen_roughness_texture);
        return gltf_clearcoat(ctx, s, p.clearcoat, p.clearcoat_roughness, p.clearcoat_normal_scale, p.clearcoat_texture, p.clearcoat_roughness_texture, p.clearcoat_normal_texture);
    }

    static float occlusion(MaterialContext ctx, GltfSheenClearcoatParams p)
    {
        return GltfMaterial::occlusion(ctx, read_material(ctx.material));
    }

    static float3 emission(MaterialContext ctx, GltfSheenClearcoatParams p)
    {
        return GltfMaterial::emission(ctx, read_material(ctx.material));
    }

    static float alpha(MaterialContext ctx, GltfSheenClearcoatParams p)
    {
        return GltfMaterial::alpha(ctx, read_material(ctx.material));
    }

    static float alpha_cutoff(GltfSheenClearcoatParams p)
    {
        return p.alpha_cutoff;
    }

    static float thickness(MaterialContext ctx, GltfSheenClearcoatParams p)
    {
        return GltfMaterial::thickness(ctx, read_material(ctx.material));
    }
}
