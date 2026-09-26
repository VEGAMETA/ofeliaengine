# 0037. Checks run on the reference machine before merging; GitHub Actions on demand

- Status: Accepted (team decision, 2026-09-27)
- Date: 2026-09-27
- Changes: the CI part of [ADR 0001](0001-workspace-and-build.md) and CLAUDE.md §15 "CI"

## Context

- GitHub-hosted runners have no GPU and are billed on a private repository, so every
  workflow runs on the self-hosted runner: ref-1, the developer's desktop (Ryzen 7 7700,
  RTX 4060 Ti, Radeon iGPU; `docs/perf/reference-hw.md`).
- Since 2026-09-26 the agent also works on ref-1 directly, and checks every change there
  before merging.
- Every change was therefore built twice on the same 8 cores: once by the local check,
  once by Actions. After each merge, the GPU workflow and the benchmarks ran again, with
  windowed smoke tests opening on the desktop.
- On 2026-09-26 a CI job and a local build ran together and Explorer hung ("The shell
  stopped unexpectedly", event 1002). At that point 9 queued runs meant hours of load.

## Options

1. **Actions on every pull request push and every merge** (as before #43). Two builds of
   every commit on one machine, the second one adding nothing.
2. **Actions after merges only** (#43). Half the duplication, but still a full build, GPU
   tests and benchmarks after every merge, competing with the next change's check.
3. **Checks before merging on ref-1 (`tools/check.ps1`); Actions on demand.** One build
   per change. Actions stay available for a report on GitHub or a second run.

## Decision

Option 3.

`tools/check.ps1` runs the workflows' cargo commands at below-normal priority and writes
logs to `target\check-logs`. Before every merge it runs with the switches that the change
calls for:

| Change touches | Run |
|---|---|
| anything | `tools\check.ps1` (rustfmt, clippy, tests, tests with Tracy, docs) |
| GPU path: `engine_rhi`, `engine_render*`, `engine_shader`, `engine_text`, `engine_platform`, `engine_latency`, shaders, `tools/` | `-Gpu` (tests on the RTX and the Radeon, Slang tests) |
| dependencies, licenses | `-Deny` |
| lock-free code (`engine_core` queues, jobs) | `-Loom` |
| Cargo profiles, build configuration | `-Profiles` |
| before a milestone | `-Full` (everything CI ran) |

- **Measurements** (benchmarks, the stress scene, `draw_bench`) run on ref-1 by hand when
  a change can move them. The numbers go into `docs/perf/history.md` and the pull request.
- **GitHub Actions** keep every workflow, triggered by hand (`workflow_dispatch`) or by
  a label on a pull request: `ci` for CI and the GPU tests, `bench` for the benchmarks.
  Nothing runs automatically.

## Numbers (ref-1, incremental builds)

- Base check: rustfmt 1 s, clippy 15-30 s, tests 45-115 s, tests with Tracy 55 s (after
  #47), docs 3-15 s.
- `-Gpu`: 17 s for both GPUs and Slang together; `-Deny` 1.5 s; `-Loom` 28 s.
- Actions ran the same commands, plus the four profile builds, on every push.

## Consequences

- **The merged commit is not rebuilt from a clean checkout.** The check builds the
  working tree. The script lists uncommitted and untracked files before it starts, since
  a commit would not contain them. `--locked` stays.
- **One machine, one opinion.** A pull request that deserves a second run gets the `ci`
  label.
- **No regression gate yet.** The 3% gate of CLAUDE.md §5 needs a dedicated benchmark
  machine, which ref-1 is not. This is unchanged: it was advisory before too.
- **Revisit** when the team has a dedicated build or benchmark machine: automatic runs
  belong there, not on a desktop.
