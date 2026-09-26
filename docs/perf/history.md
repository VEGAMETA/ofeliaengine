# Measurement history

One line per measurement (CLAUDE.md §5). Newest at the top. Hardware refers to an entry in
`reference-hw.md`; numbers from non-reference machines are marked as such.

| Date | Commit | Scene / benchmark | Hardware | Profile | Numbers | Notes |
|---|---|---|---|---|---|---|
| 2026-09-26 | render-graph PR | `engine_render` benches/graph.rs (declare + compile + record, steady state) | cloud container, lavapipe (**not reference**) | bench | 16 / 64 / 256 passes: 6.2 / 27.3 / 112.4 µs per frame (0.38–0.44 µs per pass; first version 501 µs at 256); transient memory aliased 1.0 MiB vs 71.8 MiB unaliased at 256; 0 heap allocations per frame | ADR 0011 |
| 2026-09-26 | 48a0275 | `engine_rhi` benches/memory.rs (free+allocate churn, 1k/4k/16k live) | ref-1 (RTX 4060 Ti; Radeon iGPU) | bench | engine TLSF 105/112/137 ns (NVIDIA), 106/112/142 ns (AMD), 0 heap allocs; gpu-allocator 0.28 11 520/33 262/93 204 ns (NVIDIA), 11 512/32 582/92 723 ns (AMD), 4 heap allocs | ADR 0010 |
| 2026-09-26 | gpu-allocator PR | `engine_rhi` benches/memory.rs (free+allocate churn, 1k/4k/16k live) | cloud container, lavapipe (**not reference**) | bench | engine TLSF 160/168/242 ns per pair, 0 heap allocs; gpu-allocator 0.28 10 484/29 071/80 435 ns, 4 heap allocs; overhead 2.55/1.28/1.25 vs 1.36/1.24/1.26 | ADR 0010 |
| 2026-09-26 | c1958b5 | `engine_shader` examples/shader_bench.rs, 48 generated shaders | ref-1 | release | Slang 2026.8 (SDK): session 115 ms; compile one 154 ms; Compile All cold j1/2/4/8/16 7470/4693/2880/1963/2102 ms; all hits 2.2 ms; hot reload (OS watcher) one shader 167 ms median / 181 p90; shared module → first 325 ms, all 1966 ms | ADR 0009, run 36203519942 |
| 2026-09-26 | shader hot reload PR | `engine_shader` examples/shader_bench.rs, 48 generated shaders | cloud container, 4 vCPU (**not reference**) | release | compile one 66 ms; Compile All cold j1/j2/j4 3563/1989/1548 ms; all hits 1.1 ms; hot reload one shader 77 ms median / 90 p90; shared module → first 131 ms, all 1370 ms | ADR 0009, Slang 2026.12, notify path (no OS watcher) |
| 2026-09-25 | 0f9a973 | `engine_platform` benches/timers.rs | ref-1 | bench | sleep error median/p99 µs @4.167 ms: std 331/507, HighResTimer 335/1069, PreciseSleeper 0/3.2 (spin margin ~0.6 ms); timer quantized to 0.5 ms | ADR 0004 |
| 2026-09-25 | 009d234 | `engine_profiler` benches/zones.rs | ref-1 | bench | zone / 2 nested / plot: on-demand 6.0 / 8.3 / 1.5 ns; capture-from-start (no profiler) 18.5 / 37.7 / 9.0 ns | ADR 0003 |
| 2026-09-25 | ff35272 | `benches/jobs.rs` (5 cases) | ref-1 | bench | engine/rayon median ratio: join 1.02, parallel_for even 1.16, uneven 1.10, scope 1.31, cold burst 2.13; engine 0 allocs, rayon 256 per 256-spawn scope | ADR 0002, run 36195882582 |
| 2026-09-25 | job-system PR | `benches/jobs.rs` (5 cases) | cloud container, 4 vCPU (**not reference**) | bench | engine/rayon median ratio 1.01–1.31 in all cases; engine 0 allocs, rayon 256 allocs per 256-spawn scope | see ADR 0002 |
