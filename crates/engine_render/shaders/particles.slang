// GPU particles (ADR 0081): a pool of particles that lives on the GPU, emitted, simulated,
// sorted and drawn without the CPU reading anything back.
//
// - A dead list holds the free slots, an alive list the living ones; two alive lists
//   take turns (the one simulated this frame, the one for the next). Their counts are
//   `Counters`, on the GPU only.
// - Per frame: `particlesBegin` (one thread: how many spawn, at most the free slots, and
//   the emit dispatch's size) → `particlesEmit` (one thread per spawn: a free slot taken
//   from the dead list's end, the particle placed by its emitter, appended to the alive
//   list) → `particlesSimulateArgs` (one thread) → `particlesSimulate` (one thread per
//   living particle: aged, moved; the dead go back to the dead list, the others to the
//   next alive list with a key for the sort: farthest first, ties by birth) → the GPU
//   radix sort (`sort.slang`) → `particlesVertex` / `particlesFragment` (a camera-facing
//   quad per particle, back to front, blended premultiplied, faded where it meets the
//   scene).
// - Random numbers come from a particle's birth number (`serial`) and its emitter's
//   seed, never from its slot: the same emitters and steps make the same particles.

[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

static const uint GROUP = 64;
static const uint NO_TEXTURE = 0xFFFFFFFFu;

// `Particle` in particles.rs (48 bytes).
struct Particle
{
    // World position, seconds lived.
    float4 position_age;
    // World velocity (units per second), lifetime in seconds.
    float4 velocity_life;
    // Slot of its emitter in the table.
    uint emitter;
    // The system's spawn count when it was born.
    uint serial;
    float rotation;
    // Radians per second.
    float spin;
};

// `GpuEmitter` in particles.rs (176 bytes).
struct Emitter
{
    // World from emitter: the rows of a 3 × 4 transform.
    float4 rows[3];
    // Linear color and opacity at birth and at death.
    float4 color_start;
    float4 color_end;
    // World acceleration (gravity), then drag per second.
    float4 accel_drag;
    // Emitter-space direction (unit), then the cosine of the cone's half angle.
    float4 direction_spread;
    // Spawn radius, size at birth, size at death, soft-edge distance.
    float4 shape;
    // Lifetime min and max, speed min and max.
    float4 ranges;
    // Spin min and max, additive share (0 blended, 1 added), alive (0: its particles die).
    float4 spin_additive;
    // This frame's first spawn and spawn count, texture (bindless, or NO_TEXTURE), seed.
    uint4 spawn;
};

// `Counters` in particles.rs (64 bytes).
struct Counters
{
    // Particles in the alive list simulated this frame.
    uint alive;
    // Free slots in the dead list.
    uint dead;
    // Particles left alive by the simulation (the next alive list's).
    uint next;
    // This frame's spawns: those asked for, at most the free slots.
    uint emitted;
    uint4 emit_args;
    uint4 simulate_args;
    // `vkCmdDrawIndirect`: 6 vertices per particle, one instance per particle.
    uint4 draw_args;
};

// `ParticlePush` in particles.rs (112 bytes).
struct ParticlePush
{
    Particle* particles;
    // The alive list simulated this frame, and the next one.
    uint* alive_in;
    uint* alive_out;
    uint* dead;
    Counters* counters;
    Emitter* emitters;
    // Per next-alive particle: its sort key and slot.
    uint* keys;
    uint* values;
    // Camera position (the sort's depth), then the step in seconds.
    float4 eye_dt;
    // Camera forward (unit), then the depth the keys span.
    float4 forward_far;
    // Spawns asked for this frame (the emitters' counts summed).
    uint requested;
    // The system's spawn count before this frame.
    uint serial;
    uint capacity;
    uint emitters_count;
};

[[vk::push_constant]]
ConstantBuffer<ParticlePush> push;

// A 32-bit hash (PCG's output function over a mixed input).
uint hash(uint x)
{
    uint state = x * 747796405u + 2891336453u;
    uint word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// A random number in [0, 1) for stream `k` of particle `serial` of an emitter `seed`.
float random(uint serial, uint seed, uint k)
{
    return float(hash(serial ^ hash(seed + k * 0x9E3779B9u)) >> 8) * (1.0 / 16777216.0);
}

Emitter load_emitter(uint e)
{
    return loadAligned<16>(push.emitters + e);
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void particlesInit(uint3 id : SV_DispatchThreadID)
{
    uint i = id.x;
    if (i < push.capacity)
        push.dead[i] = push.capacity - 1 - i;
    if (i == 0)
    {
        Counters c;
        c.alive = 0;
        c.dead = push.capacity;
        c.next = 0;
        c.emitted = 0;
        c.emit_args = uint4(0, 1, 1, 0);
        c.simulate_args = uint4(0, 1, 1, 0);
        c.draw_args = uint4(6, 0, 0, 0);
        push.counters[0] = c;
    }
}

[shader("compute")]
[numthreads(1, 1, 1)]
void particlesBegin()
{
    uint emitted = min(push.requested, push.counters[0].dead);
    push.counters[0].emitted = emitted;
    push.counters[0].emit_args = uint4((emitted + GROUP - 1) / GROUP, 1, 1, 0);
}

// A unit vector in the cone around `axis` whose half angle's cosine is `cos_max`,
// uniform over its solid angle.
float3 in_cone(float3 axis, float cos_max, float u, float v)
{
    float z = lerp(cos_max, 1.0, u);
    float r = sqrt(max(0.0, 1.0 - z * z));
    float phi = 6.2831853 * v;
    float3 t = abs(axis.y) < 0.99 ? normalize(cross(axis, float3(0, 1, 0))) : float3(1, 0, 0);
    float3 b = cross(axis, t);
    return t * (r * cos(phi)) + b * (r * sin(phi)) + axis * z;
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void particlesEmit(uint3 id : SV_DispatchThreadID)
{
    uint i = id.x;
    Counters c = push.counters[0];
    if (i >= c.emitted)
        return;
    // Its emitter: the one whose spawn range holds `i`.
    uint e = 0;
    for (uint k = 0; k < push.emitters_count; k++)
    {
        uint2 range = push.emitters[k].spawn.xy;
        if (i >= range.x && i < range.x + range.y)
        {
            e = k;
            break;
        }
    }
    Emitter em = load_emitter(e);
    uint serial = push.serial + i;
    uint seed = em.spawn.w;
    // Where: uniform in a ball of the spawn radius.
    float3 dir = in_cone(float3(0, 0, 1), -1.0, random(serial, seed, 0), random(serial, seed, 1));
    float3 local = dir * (em.shape.x * pow(random(serial, seed, 2), 1.0 / 3.0));
    // Which way and how fast: in the emitter's cone.
    float3 heading = in_cone(
        em.direction_spread.xyz, em.direction_spread.w, random(serial, seed, 3), random(serial, seed, 4));
    float speed = lerp(em.ranges.z, em.ranges.w, random(serial, seed, 5));
    float3x4 world = float3x4(em.rows[0], em.rows[1], em.rows[2]);
    Particle p;
    p.position_age = float4(mul(world, float4(local, 1.0)), 0.0);
    p.velocity_life = float4(
        mul(world, float4(heading * speed, 0.0)), lerp(em.ranges.x, em.ranges.y, random(serial, seed, 6)));
    p.emitter = e;
    p.serial = serial;
    p.rotation = 6.2831853 * random(serial, seed, 7);
    p.spin = lerp(em.spin_additive.x, em.spin_additive.y, random(serial, seed, 8));
    // A free slot from the dead list's end, into the alive list after the living.
    uint slot = push.dead[c.dead - 1 - i];
    storeAligned<16>(push.particles + slot, p);
    push.alive_in[c.alive + i] = slot;
}

[shader("compute")]
[numthreads(1, 1, 1)]
void particlesSimulateArgs()
{
    Counters c = push.counters[0];
    c.alive += c.emitted;
    c.dead -= c.emitted;
    c.next = 0;
    c.simulate_args = uint4((c.alive + GROUP - 1) / GROUP, 1, 1, 0);
    push.counters[0] = c;
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void particlesSimulate(uint3 id : SV_DispatchThreadID)
{
    uint i = id.x;
    uint alive = push.counters[0].alive;
    bool live = i < alive;
    bool keep = false;
    uint slot = 0;
    Particle p;
    if (live)
    {
        slot = push.alive_in[i];
        p = loadAligned<16>(push.particles + slot);
        Emitter em = load_emitter(p.emitter);
        float dt = push.eye_dt.w;
        float age = p.position_age.w + dt;
        keep = age < p.velocity_life.w && em.spin_additive.w != 0.0;
        if (keep)
        {
            float3 velocity = p.velocity_life.xyz + em.accel_drag.xyz * dt;
            velocity *= max(0.0, 1.0 - em.accel_drag.w * dt);
            p.position_age = float4(p.position_age.xyz + velocity * dt, age);
            p.velocity_life.xyz = velocity;
            p.rotation += p.spin * dt;
            storeAligned<16>(push.particles + slot, p);
        }
    }
    // The dead back to the dead list, the living to the next alive list: one atomic per
    // wave for each.
    bool died = live && !keep;
    uint dead_count = WaveActiveCountBits(died);
    uint dead_base = 0;
    if (WaveIsFirstLane() && dead_count > 0)
        InterlockedAdd(push.counters[0].dead, dead_count, dead_base);
    dead_base = WaveReadLaneFirst(dead_base);
    if (died)
        push.dead[dead_base + WavePrefixCountBits(died)] = slot;

    uint keep_count = WaveActiveCountBits(keep);
    uint keep_base = 0;
    if (WaveIsFirstLane() && keep_count > 0)
        InterlockedAdd(push.counters[0].next, keep_count, keep_base);
    keep_base = WaveReadLaneFirst(keep_base);
    if (keep)
    {
        uint j = keep_base + WavePrefixCountBits(keep);
        push.alive_out[j] = slot;
        // Farthest first; equal depths in birth order (the sort is stable, but the
        // list's order comes from atomics).
        float depth = dot(p.position_age.xyz - push.eye_dt.xyz, push.forward_far.xyz);
        uint q = uint(saturate(depth / push.forward_far.w) * 65535.0);
        push.keys[j] = ((65535u - q) << 16) | (p.serial & 0xFFFFu);
        push.values[j] = slot;
    }
}

[shader("compute")]
[numthreads(1, 1, 1)]
void particlesFinish()
{
    Counters c = push.counters[0];
    c.alive = c.next;
    c.draw_args = uint4(6, c.next, 0, 0);
    push.counters[0] = c;
}

// `DrawPush` in particles.rs (128 bytes, the push constants' limit).
struct DrawPush
{
    Particle* particles;
    // Slots back to front.
    uint* order;
    Emitter* emitters;
    // The scene's depth (bindless index) and a linear sampler.
    uint depth;
    uint sampler;
    // Clip from world, jitter included (the scene's).
    float4x4 clip_from_world;
    // Camera right in world space (the quads face the camera), then the depth terms:
    // distance = p32 / (depth + p22).
    float4 right_p22;
    // Camera up in world space, then p32.
    float4 up_p32;
};

// The draw's push constants (its entry points use no other).
[[vk::push_constant]]
ConstantBuffer<DrawPush> draw;

struct VertexOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    // Premultiplied color.
    float4 color : COLOR0;
    // Distance along the view, the soft-edge distance.
    float2 depth_soft : TEXCOORD1;
    nointerpolation uint texture : TEXCOORD2;
};

[shader("vertex")]
VertexOut particlesVertex(uint vertex : SV_VertexID, uint instance : SV_InstanceID)
{
    uint slot = draw.order[instance];
    Particle p = loadAligned<16>(draw.particles + slot);
    Emitter em = loadAligned<16>(draw.emitters + p.emitter);
    float t = saturate(p.position_age.w / max(p.velocity_life.w, 1e-6));
    float size = lerp(em.shape.y, em.shape.z, t);
    float4 color = lerp(em.color_start, em.color_end, t);
    // Two triangles: corners (0,0) (1,0) (0,1), (0,1) (1,0) (1,1).
    static const float2 CORNERS[6] = {
        float2(0, 0), float2(1, 0), float2(0, 1), float2(0, 1), float2(1, 0), float2(1, 1)
    };
    float2 corner = CORNERS[vertex];
    float2 offset = (corner - 0.5) * size;
    float s = sin(p.rotation);
    float c = cos(p.rotation);
    float2 turned = float2(offset.x * c - offset.y * s, offset.x * s + offset.y * c);
    float3 world = p.position_age.xyz + draw.right_p22.xyz * turned.x + draw.up_p32.xyz * turned.y;
    VertexOut o;
    o.position = mul(draw.clip_from_world, float4(world, 1.0));
    o.uv = float2(corner.x, 1.0 - corner.y);
    float additive = em.spin_additive.z;
    o.color = float4(color.rgb * color.a, color.a * (1.0 - additive));
    o.depth_soft = float2(o.position.w, em.shape.w);
    o.texture = em.spawn.z;
    return o;
}

struct FragmentOut
{
    float4 color : SV_Target0;
};

struct FragmentReactiveOut
{
    float4 color : SV_Target0;
    // How much of the pixel the particle covers: temporal resolves trust history less.
    float4 reactive : SV_Target1;
};

// The particle's color at this pixel, faded where it nears the scene behind it; false
// where the scene is in front.
bool shade(VertexOut input, out float4 color)
{
    float4 texel = float4(1.0, 1.0, 1.0, 1.0);
    if (input.texture != NO_TEXTURE)
        texel = g_textures[NonUniformResourceIndex(input.texture)].Sample(g_samplers[draw.sampler], input.uv);
    else
    {
        // A soft disc.
        float r = length(input.uv * 2.0 - 1.0);
        texel = saturate(1.0 - r).xxxx;
        texel *= texel;
    }
    float scene = g_textures[draw.depth].Load(int3(int2(input.position.xy), 0)).r;
    float behind = draw.up_p32.w / (scene + draw.right_p22.w) - input.depth_soft.x;
    color = float4(0, 0, 0, 0);
    if (behind <= 0.0)
        return false;
    float fade = input.depth_soft.y > 0.0 ? saturate(behind / input.depth_soft.y) : 1.0;
    color = input.color * texel * fade;
    return true;
}

[shader("fragment")]
FragmentOut particlesFragment(VertexOut input)
{
    float4 color;
    if (!shade(input, color))
        discard;
    FragmentOut o;
    o.color = color;
    return o;
}

[shader("fragment")]
FragmentReactiveOut particlesFragmentReactive(VertexOut input)
{
    float4 color;
    if (!shade(input, color))
        discard;
    FragmentReactiveOut o;
    o.color = color;
    float coverage = saturate(max(color.a, max(color.r, max(color.g, color.b))));
    o.reactive = float4(coverage, 0.0, 0.0, coverage);
    return o;
}
