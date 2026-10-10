# 0077. Faster checks: one build per check, tests under cargo-nextest, unified features

- Status: Accepted
- Date: 2026-10-10
- Changes: how [ADR 0037](0037-checks-on-the-reference-machine.md)'s `tools/check.ps1` runs
  its steps, and which crates [ADR 0062](0062-golden-images-and-perf-gate.md)'s GPU step
  runs on both GPUs

## Context

`tools/check.ps1 -Gpu -Perf` gates every merge (ADR 0037). By 2026-10-02 it took 10-25 min
and grew with every crate and test; the owner put "faster checks" first in the queue after
the second core review (`docs/status.md` §5, 2026-10-10). Measured on ref-1 before any
change (target directory on the HDD, as for every earlier run):

| step | gate on PR #118 (stale target) | nothing changed |
|---|---:|---:|
| rustfmt | 12 s | 12 s |
| clippy | 78 s | 5 s |
| test | 361 s (258 build, 82 tests, ~20 doc tests) | 177 s (78 build) |
| tracy | 346 s (201 build, 86 tests) | 197 s |
| docs | 55 s | 17 s |
| gpu | 340 s (~140 build, 195 tests on both GPUs) | 220 s |
| slang | 11 s | 3 s |
| perf | 310 s | (not run) |
| **total** | **1,515 s** | **632 s without perf** |

With nothing changed, the check still rebuilt 20 crates in the test step, every crate in
the tracy step and the GPU crates in the gpu step. Causes, found with cargo's fingerprint
log (`CARGO_LOG=cargo::core::compiler::fingerprint=info`) and `cargo tree -e features`:

1. **Cargo keys a compiled crate by its own features, not its dependencies'.** The gpu
   step (`cargo test -p <9 crates>`) and the slang step (`-p engine_shader`) resolved
   `windows-sys` with 21 of the workspace's 44 features and `bytemuck` with fewer than the
   test step's `--workspace` build, so every crate above them was rebuilt into the same
   files, and the next step rebuilt them back: on every run. The tracy step did the same
   with the `engine_profiler/tracy` feature, which every crate depends on.
2. **The test step ran the GPU crates' tests** (on the RTX) that the gpu step then ran
   again on both GPUs: the antialiasing-modes suite alone takes 33-37 s per run, three
   runs per check.
3. **The tracy step reran the whole suite** (705 tests in 140 binaries) with Tracy on,
   though only `engine_profiler` compiles differently with it (no other crate has
   `cfg(feature = "tracy")`). What Tracy can break elsewhere is allocation counts: it
   registers zones on first use and runs a thread (commits 12a9993, b1f39b6, 8038617).
4. **Test binaries ran one after another**, each with its tests in threads.
5. **Doc tests are compiled on every run** (rustdoc does not cache them): ~20 s for 35
   crates, the same whether a crate changed or not. **rustfmt checks every file** (12 s).
6. **The gpu step's hand list of crates missed `engine_streaming`**: its GPU tests never
   ran on the Radeon. `engine_text` was in it with no GPU test.

## Options

1. **Keep `cargo test`, exclude packages per step.** Excluding packages changes the
   feature resolution again (cause 1): every exclusion is another rebuild.
2. **Run the test binaries directly** from one `cargo test --no-run --message-format json`
   build, filtering and parallelizing in the script: a test runner of our own.
3. **cargo-nextest** (0.9.148, Apache-2.0 OR MIT, maintained by the authors of guppy and
   cargo-hakari): builds the workspace once and filters tests with an expression language
   (`rdeps(engine_rhi)`, `package(...)`, `binary(...)`), runs every test in its own
   process with every binary's tests in parallel, limits groups of tests (GPU tests).
   It does not run doc tests (cargo test does). A developer tool only: nothing ships with it.

For cause 1, independently of the runner:

- **A.** Never build a subset in the checks. Agents' own `cargo test -p` runs and release
  examples would still flip the shared builds back and forth.
- **B.** A workspace-hack crate (the cargo-hakari approach, written by hand): the shared
  dependencies whose features differ, declared once with the union of their features;
  every engine crate depends on it, so any subset resolves them as the whole workspace.
- **C.** cargo-hakari itself: generates and verifies that crate, at the price of another
  tool and a generated list far longer than the six dependencies that differ here.

## Decision

Option 3 with B, and A for the check's own steps:

- **`engine_workspace_hack`**: bytemuck, windows-sys, serde, serde_json, parking_lot and
  lock_api with the workspace's union of features. Every engine crate but the proc macros
  depends on it (`engine_core` and the eight crates that do not depend on `engine_core`).
  `tools/feature-drift.ps1` compares each package's resolved features with the workspace's
  (`cargo tree -e features,normal,build,dev`, 10 s) and fails when one differs:
  `check.ps1 -Deny`, the check for dependency changes, runs it.
- **One build per check.** The test, gpu and slang steps run `cargo nextest run
  --workspace` with filters, on one and the same build:
  - test: everything; with `-Gpu`, `not (rdeps(engine_rhi) | package(engine_shader))`;
  - gpu: `rdeps(engine_rhi)` (every crate that links the RHI, `engine_streaming`
    included) on the RTX, 4 tests at a time, then on the Radeon, 2 at a time, after the
    Radeon's platform self-check;
  - slang: `package(engine_shader)` with Slang required.
- **The tracy step** builds in its own directory (`target\tracy`, as loom does) and runs
  the allocation tests (`--test *zero_alloc*`; the editor's `text_cache` test became
  `text_cache_zero_alloc`) and `engine_profiler`'s own tests with Tracy on.
- **Doc tests and formatting follow the branch's changes.** Doc tests run for the packages
  the branch changed (against its merge base with origin/main, plus uncommitted and
  untracked files) and those that depend on them; rustfmt checks the `.rs` files the branch
  changed. Both cover everything when a manifest, the lock file, the toolchain or the
  build, format or check configuration changed, and doc tests with `-Full`. What the
  branch did not touch passed both when main was gated, and depends on nothing else.
- **`-Affected`**: the test, tracy, gpu and slang steps narrowed to the same packages, for
  runs between commits. The full run stays the gate before every merge.
- **The perf step waits for about 30 quiet seconds** (was 3): right after the check's
  builds the APU is still hot, and the Radeon read passes 5-30 % slower (CSM shadows
  +28 %), back within 1.5 % when measured again a few minutes later.

## Numbers

ref-1, `check.ps1 -Gpu` (no perf), target directory on the HDD:

| step | before, nothing changed | after, nothing changed |
|---|---:|---:|
| rustfmt | 12.3 s | 12.4 s (every file: this branch changes the manifests) |
| clippy | 5.2 s | 0.6 s |
| test | 177.3 s | 21.1 s (CPU tests 2.3 s, doc tests ~18 s) |
| tracy | 196.6 s | 17.2 s |
| docs | 17.4 s | 0.4 s |
| gpu | 219.6 s | 113.6 s (299 tests on each GPU) |
| slang | 2.8 s | 1.1 s |
| **total** | **632 s** | **168 s (−73 %)** |

- A first run after the change (everything rebuilt, `target\tracy` built for the first
  time): 289 s, against 1,515 s for the stale-target gate before it.
- `cargo nextest run` runs the 396 tests outside the GPU crates in 2.3 s.
- The affected packages are found in 0.3-0.5 s: a change in `engine_net` affects 1
  package, in `engine_rhi` 13, in `engine_core` 31 of 38, in `docs/` none.
- The GPU tests running side by side surfaced a real bug in the first run: a texture level
  uploaded twice in one frame (fixed with this change; `tests/streaming.rs`
  `a_level_asked_for_again_is_uploaded_once`).

## Consequences

- `cargo install cargo-nextest --locked` on every machine that runs the checks (ref-1 has
  0.9.148); check.ps1 stops with that command when it is missing.
- **Every new crate depends on `engine_workspace_hack`**, directly or through
  `engine_core`. A new dependency whose features differ between packages shows up in
  `check.ps1 -Deny`; add it to the hack crate.
- `cargo test -p <crate>` during development no longer rebuilds the workspace's crates
  that the full check builds, and release builds of different examples share their crates.
- Test output is nextest's: a failing test's output at the end of the step's log. One test:
  `cargo nextest run -E 'test(name)'`. Plain `cargo test` still works; tests that count
  allocations stay one per binary, so they run alone under it too.
- GPU tests now run side by side (4 on the RTX, 2 on the Radeon): a test that is not safe
  next to another GPU test fails here first. Revisit the numbers if one turns flaky for
  that reason rather than for a bug of its own.
- Next steps (`docs/status.md`): fewer integration-test binaries per crate (`engine_render`
  links 36 for 52 tests), the linker and debug-info measurements that close ADR 0001,
  sccache for agents' worktrees, and the target directory on an NVMe drive.
