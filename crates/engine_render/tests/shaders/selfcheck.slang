// GPU hardware self-check for tests/gpu_selfcheck.rs: threads exchange values through
// groupshared memory (LDS) with barriers. The result is exact and computed on the CPU too;
// any difference is a hardware (or platform) fault, never a synchronization bug.
// See docs/research/radeon-igpu-lds-faults.md.
// Rebuild: ofeliaengine shaders build -O --out crates/engine_render/tests/shaders crates/engine_render/tests/shaders

struct Push
{
    uint* out;
    uint rounds;
};

[[vk::push_constant]] Push push;

groupshared uint g_values[256];

[shader("compute")]
[numthreads(256, 1, 1)]
void ldsExchange(uint3 id : SV_DispatchThreadID, uint local : SV_GroupIndex)
{
    uint x = id.x * 2654435761u + 1u;
    for (uint i = 0; i < push.rounds; i++)
    {
        g_values[local] = x;
        GroupMemoryBarrierWithGroupSync();
        uint y = g_values[(local + 1 + i * 37) & 255];
        GroupMemoryBarrierWithGroupSync();
        x ^= y + (x << 6) + (x >> 2);
    }
    push.out[id.x] = x;
}
