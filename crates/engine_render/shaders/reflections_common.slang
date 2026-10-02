// Shared by reflections.slang and forward.slang (ADR 0073): the reflection constants of a
// view, its probes, and how a point reads them. The includer declares `g_cube_arrays`
// (TextureCubeArray, binding 0) and `g_samplers`.

// The probes of a view are listed per cell of a coarse screen grid and per slice (z-bin)
// of view depth (`PROBE_CELLS` and `PROBE_BINS` in reflections.rs): one bit per visible
// probe whose screen rectangle covers the cell, or whose box reaches into the slice (at
// most 32 probes). A pixel tests the probes in both.
static const uint PROBE_CELLS_X = 16;
static const uint PROBE_CELLS_Y = 16;
static const uint PROBE_BINS = 64;

// The candidate probes at `pixel`, `view_depth` along the view.
uint probe_mask(ReflectFrame* f, float2 pixel, float view_depth)
{
    uint2 cell = min(uint2(pixel * (float2(PROBE_CELLS_X, PROBE_CELLS_Y) / float2(f.size.xy))), uint2(PROBE_CELLS_X - 1, PROBE_CELLS_Y - 1));
    uint bin = min(uint(view_depth * asfloat(f.probe_info.y)), PROBE_BINS - 1);
    uint c = cell.y * PROBE_CELLS_X + cell.x;
    return f.probe_cells[c >> 2][c & 3] & f.probe_bins[bin >> 2][bin & 3];
}

// `ProbeData` in reflections.rs (144 bytes).
struct Probe
{
    float4 influence[3];     // rows: world → influence box, scaled to [-1, 1]
    float4 extents;          // xyz: influence half extents; w: 1 / blend distance
    float4 parallax[3];      // rows: world → parallax box (rotation and translation)
    float4 parallax_extents; // xyz: parallax half extents; w: cube index in the array
    float4 position;         // xyz: capture point in the parallax box's frame; w: intensity
};

// `ReflectFrame` in reflections.rs.
struct ReflectFrame
{
    float4x4 clip_from_world;          // as the depth was drawn (with any jitter)
    float4x4 previous_clip_from_world; // the SSR history's
    float4 camera;                     // xyz: position
    float4 forward;                    // xyz: view direction; w: near plane distance
    float4 ray_base;                   // xyz: ray to view depth 1 at ndc 0; w: p22
    float4 ray_x;                      // xyz: per ndc.x; w: p32 (view depth = p32 / (depth + p22))
    float4 ray_y;                      // xyz: per ndc.y
    uint4 size;                        // xy: target; zw: SSR trace
    uint4 env;                         // x: prefiltered sky cube, y: its mips, z: linear sampler, w: probe array
    float4 params;                     // x: sky intensity; y: SSR max roughness; z: SSR thickness; w: SSR temporal blend
    uint4 probe_info;                  // x: visible probes, y: z-bins per unit of view depth (float bits), w: frame (SSR noise)
    float4* probes;                    // `Probe`s, 9 float4 each (read field by field)
    uint2 _unused0;
    uint2 _unused1;
    uint _pad0;
    uint _pad1;
    uint4 ssr;                         // x: steps, y: trace scale, z: history valid, w: pyramid levels
    uint4 pyramid_offsets[4];          // offset of level k (1-16) in floats, at [k - 1]
    // In the constants themselves: one memory read less before the probes' search.
    uint4 probe_cells[PROBE_CELLS_X * PROBE_CELLS_Y / 4]; // per cell, its probes' bits
    uint4 probe_bins[PROBE_BINS / 4];  // per z-bin, its probes' bits
};

// A unit vector from octahedral encoding in [0, 1]² (forward.slang's `encode_octahedral`).
float3 decode_normal(float2 e)
{
    e = e * 2.0 - 1.0;
    float3 v = float3(e, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-v.z, 0.0);
    v.x += v.x >= 0.0 ? -t : t;
    v.y += v.y >= 0.0 ? -t : t;
    return normalize(v);
}

// The world position at pixel `pixel` (centre) with depth-buffer value `depth`, and the
// view depth.
float3 world_position(ReflectFrame* f, float2 pixel, float depth, out float view_depth)
{
    float2 ndc = pixel / float2(f.size.xy) * 2.0 - 1.0;
    float3 ray = f.ray_base.xyz + ndc.x * f.ray_x.xyz + ndc.y * f.ray_y.xyz;
    view_depth = f.ray_x.w / (depth + f.ray_base.w);
    return f.camera.xyz + ray * view_depth;
}

float view_depth_of(ReflectFrame* f, float depth)
{
    return f.ray_x.w / (depth + f.ray_base.w);
}

// Probe `p`'s rows of `Probe` (float4 each): 0-2 influence, 3 extents, 4-6 parallax, 7
// parallax extents, 8 position. Read one by one: a whole `Probe` read through its pointer
// would take its 36 registers.
static const uint PROBE_FLOAT4S = 9;

// How much probe `p` covers `world`: 1 inside its influence box less the blend
// distance, falling to 0 at the box.
float probe_influence(float4* p, float3 world)
{
    float4 r0 = p[0];
    float4 r1 = p[1];
    float4 r2 = p[2];
    float4 e = p[3];
    float3 local = float3(dot(r0.xyz, world) + r0.w, dot(r1.xyz, world) + r1.w, dot(r2.xyz, world) + r2.w);
    float3 inside = (1.0 - abs(local)) * e.xyz;
    return saturate(min(inside.x, min(inside.y, inside.z)) * e.w);
}

// Where the ray from `world` along `r` leaves probe `p`'s parallax box, as a direction from
// its capture point (Lagarde and Zanuttini 2012). A point outside the box starts from the
// nearest point inside (surfaces on the box's faces, such as a room's floor, lie on
// either side by rounding).
float3 probe_direction(float4* p, float3 world, float3 r)
{
    float4 r0 = p[4];
    float4 r1 = p[5];
    float4 r2 = p[6];
    float3 e = p[7].xyz;
    float3 o = clamp(float3(dot(r0.xyz, world) + r0.w, dot(r1.xyz, world) + r1.w, dot(r2.xyz, world) + r2.w), -e, e);
    float3 d = float3(dot(r0.xyz, r), dot(r1.xyz, r), dot(r2.xyz, r));
    // The exit through the face each axis heads for.
    float3 t = (float3(d.x >= 0.0 ? e.x : -e.x, d.y >= 0.0 ? e.y : -e.y, d.z >= 0.0 ? e.z : -e.z) - o) / (abs(d) > 1e-8 ? d : 1e-8);
    float distance = min(t.x, min(t.y, t.z));
    // From the capture point (`position` holds it in the box's frame), back to world axes.
    float3 local = o + d * distance - p[8].xyz;
    return local.x * r0.xyz + local.y * r1.xyz + local.z * r2.xyz;
}

// The probes' reflection along `r` at `world` (pixel `pixel`, `view_depth` along the
// view), the rest of the weight from
// `sky`: the first two probes in priority order (smallest first) whose influence reaches
// the point, the first over the second. The search runs over the probes of the wave's
// cells one at a time for the whole wave (their influence data in scalar registers) and
// keeps only indices and weights; the two cube reads follow it, issued together.
float3 probe_radiance(ReflectFrame* f, float3 world, float3 r, float lod, float2 pixel, float view_depth, float3 sky)
{
    if (f.probe_info.x == 0)
        return sky;
    uint mask = probe_mask(f, pixel, view_depth);
    uint first = 0;
    uint second = 0;
    float w0 = 0.0;
    float w1 = 0.0;
    uint merged = WaveActiveBitOr(mask);
    while (merged != 0 && !WaveActiveAllTrue(w1 > 0.0 || w0 >= 0.999))
    {
        uint bit = firstbitlow(merged);
        merged &= merged - 1u;
        float influence = probe_influence(f.probes + WaveReadLaneFirst(bit) * PROBE_FLOAT4S, world);
        if (((mask >> bit) & 1u) != 0 && influence > 0.0 && w1 <= 0.0 && w0 < 0.999)
        {
            if (w0 <= 0.0)
            {
                first = bit;
                w0 = influence;
            }
            else
            {
                second = bit;
                w1 = influence;
            }
        }
    }
    TextureCubeArray cubes = g_cube_arrays[f.env.w];
    SamplerState s = g_samplers[f.env.z];
    float share0 = w0;
    float share1 = (1.0 - w0) * w1;
    float3 sum = (1.0 - share0 - share1) * sky;
    if (w0 > 0.0)
    {
        float4* p = f.probes + first * PROBE_FLOAT4S;
        sum += share0 * p[8].w * cubes.SampleLevel(s, float4(probe_direction(p, world, r), p[7].w), lod).rgb;
    }
    if (w1 > 0.0)
    {
        float4* p = f.probes + second * PROBE_FLOAT4S;
        sum += share1 * p[8].w * cubes.SampleLevel(s, float4(probe_direction(p, world, r), p[7].w), lod).rgb;
    }
    return sum;
}
