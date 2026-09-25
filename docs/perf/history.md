# Measurement history

One line per measurement (CLAUDE.md §5). Newest at the top. Hardware refers to an entry in
`reference-hw.md`; numbers from non-reference machines are marked as such.

| Date | Commit | Scene / benchmark | Hardware | Profile | Numbers | Notes |
|---|---|---|---|---|---|---|
| 2026-09-25 | job-system PR | `benches/jobs.rs` (5 cases) | cloud container, 4 vCPU (**not reference**) | bench | engine/rayon median ratio 1.01–1.31 in all cases; engine 0 allocs, rayon 256 allocs per 256-spawn scope | see ADR 0002 |
