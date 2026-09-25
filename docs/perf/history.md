# Measurement history

One line per measurement (CLAUDE.md §5). Newest at the top. Hardware refers to an entry in
`reference-hw.md`; numbers from non-reference machines are marked as such.

| Date | Commit | Scene / benchmark | Hardware | Profile | Numbers | Notes |
|---|---|---|---|---|---|---|
| 2026-09-25 | 009d234 | `engine_profiler` benches/zones.rs | ref-1 | bench | zone / 2 nested / plot: on-demand 6.0 / 8.3 / 1.5 ns; capture-from-start (no profiler) 18.5 / 37.7 / 9.0 ns | ADR 0003 |
| 2026-09-25 | ff35272 | `benches/jobs.rs` (5 cases) | ref-1 | bench | engine/rayon median ratio: join 1.02, parallel_for even 1.16, uneven 1.10, scope 1.31, cold burst 2.13; engine 0 allocs, rayon 256 per 256-spawn scope | ADR 0002, run 36195882582 |
| 2026-09-25 | job-system PR | `benches/jobs.rs` (5 cases) | cloud container, 4 vCPU (**not reference**) | bench | engine/rayon median ratio 1.01–1.31 in all cases; engine 0 allocs, rayon 256 allocs per 256-spawn scope | see ADR 0002 |
