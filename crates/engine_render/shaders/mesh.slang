// Forward mesh shader: vertices pulled through a buffer device address, one directional
// light. Precompiled to mesh.vertexMain.spv / mesh.fragmentMain.spv with
//   engine shaders build crates/engine_render/shaders --out crates/engine_render/shaders
// (see README.md); a test recompiles and compares when the same Slang version is present.

struct Vertex
{
    float4 position; // xyz, w unused
    float4 normal;   // xyz, w unused
};

struct Push
{
    float4x4 clip_from_object;
    // Rows of the object-to-world normal matrix (w unused).
    float4 normal_row0;
    float4 normal_row1;
    float4 normal_row2;
    Vertex* vertices;
};

[[vk::push_constant]]
ConstantBuffer<Push> push;

struct VertexOut
{
    float4 position : SV_Position;
    float3 normal : NORMAL;
};

[shader("vertex")]
VertexOut vertexMain(uint vertex_id : SV_VertexID)
{
    Vertex v = push.vertices[vertex_id];
    VertexOut o;
    o.position = mul(push.clip_from_object, float4(v.position.xyz, 1.0));
    float3 n = v.normal.xyz;
    o.normal = float3(dot(push.normal_row0.xyz, n), dot(push.normal_row1.xyz, n), dot(push.normal_row2.xyz, n));
    return o;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    // Light from the camera side, slightly from above; small ambient term.
    float3 to_light = normalize(float3(0.0, 0.0, 1.0));
    float lambert = max(dot(normalize(input.normal), to_light), 0.0);
    float3 albedo = float3(0.8, 0.8, 0.8);
    return float4(albedo * (0.1 + 0.9 * lambert), 1.0);
}
