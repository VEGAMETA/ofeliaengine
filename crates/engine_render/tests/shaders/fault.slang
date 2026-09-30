// A deliberate GPU page fault for the gated test in tests/breadcrumbs.rs (ADR 0061):
// every thread writes through a pointer the test sets to an unmapped address.
// Rebuild: ofeliaengine shaders build -O --out crates/engine_render/tests/shaders crates/engine_render/tests/shaders

struct Push
{
    uint* target;
    uint value;
    uint stride;
};

[[vk::push_constant]] Push push;

[shader("compute")]
[numthreads(64, 1, 1)]
void faultWrite(uint3 id : SV_DispatchThreadID)
{
    push.target[id.x * push.stride] = push.value;
}

// A hang: 2^48 iterations of a dependent chain (the result is stored, so the loop stays).
// Windows resets the GPU after 2 s (TDR) and the device is lost.
[shader("compute")]
[numthreads(64, 1, 1)]
void faultHang(uint3 id : SV_DispatchThreadID)
{
    uint v = push.value + id.x;
    for (uint64_t i = 0; i < 0xFFFFFFFFFFFFull; i++)
    {
        v = v * 1664525u + 1013904223u;
    }
    push.target[id.x * push.stride] = v;
}
