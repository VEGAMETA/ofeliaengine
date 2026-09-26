// Forward PBR shading with one directional light and a shadow map.
//
// BRDF after Filament's "Physically Based Rendering in Filament" (Apache-2.0 document):
// GGX distribution, height-correlated Smith-GGX visibility, Schlick Fresnel, Lambertian
// diffuse; metallic-roughness parameterization. Per-frame and per-draw data are read
// through buffer device addresses; the shadow map comes from the bindless arrays.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerComparisonState g_compare_samplers[];

struct Vertex
{
    float4 position;
    float4 normal;
};

struct Frame
{
    float4x4 clip_from_world;
    float4x4 light_clip_from_world;
    float4 camera_position;  // xyz
    float4 to_light;         // xyz: unit vector towards the light
    float4 light_color;      // rgb: color × intensity
    float4 ambient;          // rgb
    uint shadow_sampler;
    uint3 _pad;
};

struct Draw
{
    float4x4 world_from_object;
    float4 normal_row0;
    float4 normal_row1;
    float4 normal_row2;
    float4 base_color;       // linear rgb, a unused
    float4 metallic_roughness; // x metallic, y perceptual roughness
    Vertex* vertices;
    uint2 _pad;
};

struct Push
{
    Frame* frame;
    Draw* draws;
    uint draw_index;
    uint shadow_map;         // bindless index of the shadow map
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
    float3 world_position : POSITION;
    float3 normal : NORMAL;
    float4 light_position : TEXCOORD0;
};

[shader("vertex")]
VertexOut vertexMain(uint vertex_id : SV_VertexID)
{
    Draw draw = push.draws[push.draw_index];
    Vertex v = draw.vertices[vertex_id];
    float4 world = mul(draw.world_from_object, float4(v.position.xyz, 1.0));
    float3 n = v.normal.xyz;
    VertexOut o;
    o.position = mul(push.frame.clip_from_world, world);
    o.world_position = world.xyz;
    o.normal = float3(dot(draw.normal_row0.xyz, n), dot(draw.normal_row1.xyz, n), dot(draw.normal_row2.xyz, n));
    o.light_position = mul(push.frame.light_clip_from_world, world);
    return o;
}

[shader("vertex")]
float4 shadowVertex(uint vertex_id : SV_VertexID) : SV_Position
{
    Draw draw = push.draws[push.draw_index];
    Vertex v = draw.vertices[vertex_id];
    float4 world = mul(draw.world_from_object, float4(v.position.xyz, 1.0));
    return mul(push.frame.light_clip_from_world, world);
}

static const float PI = 3.14159265;

float distribution_ggx(float n_dot_h, float a)
{
    float a2 = a * a;
    float f = (n_dot_h * a2 - n_dot_h) * n_dot_h + 1.0;
    return a2 / (PI * f * f);
}

float visibility_smith_ggx_correlated(float n_dot_v, float n_dot_l, float a)
{
    float a2 = a * a;
    float ggx_l = n_dot_v * sqrt((-n_dot_l * a2 + n_dot_l) * n_dot_l + a2);
    float ggx_v = n_dot_l * sqrt((-n_dot_v * a2 + n_dot_v) * n_dot_v + a2);
    return 0.5 / (ggx_v + ggx_l);
}

float3 fresnel_schlick(float v_dot_h, float3 f0)
{
    float f = pow(1.0 - v_dot_h, 5.0);
    return f0 + (1.0 - f0) * f;
}

// 3×3 percentage-closer filtering with hardware comparison.
float shadow(float4 light_position, float n_dot_l)
{
    float3 ndc = light_position.xyz / light_position.w;
    float2 uv = ndc.xy * 0.5 + 0.5;
    if (any(uv < 0.0) || any(uv > 1.0) || ndc.z > 1.0)
        return 1.0;
    Texture2D map = g_textures[NonUniformResourceIndex(push.shadow_map)];
    SamplerComparisonState s = g_compare_samplers[push.frame.shadow_sampler];
    uint width, height;
    map.GetDimensions(width, height);
    float2 texel = 1.0 / float2(width, height);
    float lit = 0.0;
    [unroll]
    for (int y = -1; y <= 1; y++)
        [unroll]
        for (int x = -1; x <= 1; x++)
            lit += map.SampleCmpLevelZero(s, uv + float2(x, y) * texel, ndc.z);
    return lit / 9.0;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    Frame frame = push.frame[0];
    Draw draw = push.draws[push.draw_index];
    float3 n = normalize(input.normal);
    float3 v = normalize(frame.camera_position.xyz - input.world_position);
    float3 l = frame.to_light.xyz;
    float3 h = normalize(v + l);
    float n_dot_v = max(dot(n, v), 1e-4);
    float n_dot_l = saturate(dot(n, l));
    float n_dot_h = saturate(dot(n, h));
    float v_dot_h = saturate(dot(v, h));

    float3 base = draw.base_color.rgb;
    float metallic = draw.metallic_roughness.x;
    float roughness = max(draw.metallic_roughness.y, 0.045);
    float a = roughness * roughness;
    float3 f0 = lerp(float3(0.04, 0.04, 0.04), base, metallic);

    float3 f = fresnel_schlick(v_dot_h, f0);
    float3 specular = distribution_ggx(n_dot_h, a) * visibility_smith_ggx_correlated(n_dot_v, n_dot_l, a) * f;
    float3 diffuse = (1.0 - metallic) * base / PI;
    float visible = n_dot_l > 0.0 ? shadow(input.light_position, n_dot_l) : 0.0;
    float3 color = (diffuse + specular) * frame.light_color.rgb * n_dot_l * visible
        + frame.ambient.rgb * base;
    return float4(color, 1.0);
}
