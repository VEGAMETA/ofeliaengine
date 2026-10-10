// Skinning (ADR 0068): pre-skinning in a compute pass.
//
// `skin4` / `skin8` blend each vertex of a skinned mesh's bind pose by its joints'
// skinning matrices (linear blend skinning, 4 or 8 influences) and write the result into
// the instance's own mesh in the geometry pool (positions, and the normal and tangent of
// its attributes; texture coordinates stay). Every pass that draws from the pool (depth
// prepass, shadows, forward shading) then sees the skinned surface without knowing about
// skinning. Before overwriting a position, the thread copies the old one, last frame's
// skinned position, into the instance's `previous` buffer (or the new one after a reset):
// the motion of skinned meshes (skinned_motion.slang) starts from it.

// One skinned instance of a dispatch (`SkinInstance` in skinning.rs, 32 bytes).
struct SkinInstance
{
    float4* palette;      // three rows (3 × 4) per joint of the skin
    float* previous;      // three floats per vertex: last frame's positions
    uint first_vertex;    // the instance's mesh in the geometry pool
    uint flags;           // RESET: no previous position (first frame, teleport)
    uint _pad0;
    uint _pad1;
};

static const uint RESET = 1;

// `SkinPush` in skinning.rs (48 bytes).
struct SkinPush
{
    uint* source;         // the bind pose, `stride` words per vertex (layout below)
    SkinInstance* instances;
    float* positions;     // geometry pool: three floats per vertex
    uint* attributes;     // geometry pool: four words per vertex (normal, tangent, uv)
    uint vertex_count;
    uint stride;
    uint instance_count;
    uint _pad;
};

// A source vertex: position (3 words), octahedral normal, octahedral tangent (bitangent
// sign in bit 16), then per set of 4 influences two words of joint indices (16 bits
// each) and, after every set's joints, two words of weights (unorm16 each).

[[vk::push_constant]]
ConstantBuffer<SkinPush> push;

// A unit vector from two snorm16 in octahedral encoding (as forward.slang decodes it).
float3 decode_octahedral(uint bits)
{
    float2 e = max(float2(int2(bits << 16, bits) >> 16) / 32767.0, -1.0);
    float3 v = float3(e, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-v.z, 0.0);
    v.x += v.x >= 0.0 ? -t : t;
    v.y += v.y >= 0.0 ? -t : t;
    return normalize(v);
}

// `encode_octahedral` of mesh.rs, rounded to nearest.
uint encode_octahedral(float3 n)
{
    n /= max(abs(n.x) + abs(n.y) + abs(n.z), 1e-20);
    float2 e = n.xy;
    if (n.z < 0.0)
    {
        float2 s = float2(n.x >= 0.0 ? 1.0 : -1.0, n.y >= 0.0 ? 1.0 : -1.0);
        e = (1.0 - abs(n.yx)) * s;
    }
    int2 q = int2(round(clamp(e, -1.0, 1.0) * 32767.0));
    return (uint(q.x) & 0xFFFFu) | (uint(q.y) << 16);
}

void skin(uint3 id, uint sets)
{
    uint v = id.x;
    if (v >= push.vertex_count || id.y >= push.instance_count)
        return;
    // (16-byte aligned reads: a plain pointer access is declared 4-byte aligned.)
    SkinInstance inst = loadAligned<16>(push.instances + id.y);
    uint* s = push.source + v * push.stride;
    float3 p = asfloat(uint3(s[0], s[1], s[2]));
    // The blended matrix, as three rows.
    float4 r0 = 0.0;
    float4 r1 = 0.0;
    float4 r2 = 0.0;
    float total = 0.0;
    for (uint k = 0; k < sets; k++)
    {
        uint2 joints = uint2(s[5 + 2 * k], s[6 + 2 * k]);
        uint2 weights = uint2(s[5 + 2 * sets + 2 * k], s[6 + 2 * sets + 2 * k]);
        uint4 j = uint4(joints.x & 0xFFFFu, joints.x >> 16, joints.y & 0xFFFFu, joints.y >> 16);
        float4 w = float4(weights.x & 0xFFFFu, weights.x >> 16, weights.y & 0xFFFFu, weights.y >> 16) / 65535.0;
        for (uint i = 0; i < 4; i++)
        {
            if (w[i] > 0.0)
            {
                float4* m = inst.palette + j[i] * 3;
                r0 += loadAligned<16>(m) * w[i];
                r1 += loadAligned<16>(m + 1) * w[i];
                r2 += loadAligned<16>(m + 2) * w[i];
                total += w[i];
            }
        }
    }
    float inv = 1.0 / max(total, 1e-8);
    r0 *= inv;
    r1 *= inv;
    r2 *= inv;
    float3 skinned = float3(dot(r0.xyz, p) + r0.w, dot(r1.xyz, p) + r1.w, dot(r2.xyz, p) + r2.w);
    float3 n = decode_octahedral(s[3]);
    float3 t = decode_octahedral(s[4]);
    // The linear part (exact for rotations and uniform scale; the usual approximation
    // otherwise).
    float3 sn = normalize(float3(dot(r0.xyz, n), dot(r1.xyz, n), dot(r2.xyz, n)));
    float3 st = normalize(float3(dot(r0.xyz, t), dot(r1.xyz, t), dot(r2.xyz, t)));

    uint out = inst.first_vertex + v;
    float* pos = push.positions + out * 3;
    float3 old = float3(pos[0], pos[1], pos[2]);
    if ((inst.flags & RESET) != 0)
        old = skinned;
    inst.previous[v * 3] = old.x;
    inst.previous[v * 3 + 1] = old.y;
    inst.previous[v * 3 + 2] = old.z;
    pos[0] = skinned.x;
    pos[1] = skinned.y;
    pos[2] = skinned.z;
    uint* a = push.attributes + out * 4;
    a[0] = encode_octahedral(sn);
    a[1] = (encode_octahedral(st) & ~0x10000u) | (s[4] & 0x10000u);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void skin4(uint3 id : SV_DispatchThreadID)
{
    skin(id, 1);
}

[shader("compute")]
[numthreads(64, 1, 1)]
void skin8(uint3 id : SV_DispatchThreadID)
{
    skin(id, 2);
}
