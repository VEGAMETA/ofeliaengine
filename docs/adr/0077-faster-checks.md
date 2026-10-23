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
- **The perf step waits for about 2 quiet minutes** (was 3 s): right after the check's
  builds the APU is still hot, and the Radeon read passes 5-30 % slower (CSM shadows
  +28 %), back within 1.5 % when measured again a few minutes later; 30 quiet seconds
  were not enough after a cold release build.

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
- Next steps (`docs/status.md`): fewer integration-test binaries per crate and the linker and
  debug-info measurements that close ADR 0001 (both done, see the addendum); sccache for
  agents' worktrees; the target directory on an NVMe drive; a variance study of the perf
  step's 3 runs × 200 frames per scene.

## Addendum (2026-10-10): fewer test binaries, test debug info, test targets only

Measured after the first part, on the same HDD target directory:

- **Each integration test file was a binary** that links the whole engine below it, with a
  ~22 MB PDB. After a change in `engine_render` (a touch), relinking its 36 test binaries
  took 7.6-15.8 s (median ~14 s). After a change in `engine_core`, the 98 integration-test
  binaries were 60 % of the test build's CPU time (782 of 1,297 s).
- **Debug info:** with `debug = "line-tables-only"` the PDBs shrink by two thirds (22.5 →
  7.8 MB), and rebuilding every test after a change in `engine_core` went 84 → 56 s (CPU
  1,297 → 844 s): on this HDD, writing debug info is a large part of the build.
- **Examples:** `cargo test`, and nextest's build, also compiled and linked every example
  (36), which clippy already type-checks and `-Profiles` builds.
- **The docs step** rebuilt the documentation of every dependent of a changed crate:
  36.6 s after a change in `engine_render`.

Decision:

- **One test binary per crate** for its plain tests (`tests/<crate>/main.rs`, the former
  files as modules; `engine_render`'s are `tests/render/`), one more for the allocation tests
  where a crate had several (`tests/zero_alloc/`: one counting allocator, the tests take
  turns behind a lock, so plain `cargo test` keeps them alone too). The Radeon self-check
  stays a binary of its own. 100 → 45 integration-test binaries. A new test is a module of
  its crate's binary (`mod name;` in its `main.rs`).
- **`[profile.test] debug = "line-tables-only"`**: what `cargo test` and nextest build.
  Panics and backtraces keep file and line; a debugger shows no variables in a test binary
  (`CARGO_PROFILE_TEST_DEBUG=full` for one session). The `dev` profile, used to debug the
  engine and its examples, keeps full debug info.
- **The test steps build the test targets only** (`--tests`).
- **The docs step documents the packages whose files changed** (every package when a
  manifest or the configuration changed, and with `-Full`): their dependents' compile errors
  are clippy's to find, and rustdoc's own complaints are about the changed docs.
- ADR 0001's linker is confirmed by measurement (rust-lld ~12 % faster than `link.exe` on
  the full test rebuild) and ADR 0001 is accepted.

Numbers (ref-1, `check.ps1 -Gpu`, HDD target):

| | before ADR 0077 | first part | second part |
|---|---:|---:|---:|
| nothing changed | 632 s | 168 s | 144 s (gpu 114 → 93 s, tracy 17 → 14 s) |
| `engine_render` touched | | | 225 s (test 45, tracy 25, docs 37, gpu 94; before the docs change) |
| touch `engine_render`, build its tests | 7.6-15.8 s | | 2.1-2.5 s |
| touch `engine_core`, build every test | 84 s | | 49-61 s |

## Addendum (2026-10-10): the build directory on the NVMe drive

The repository sits on ref-1's SATA HDD (`H:`); `C:` is an NVMe drive. The same scenarios
with each, measured back to back with the check of the first addendum:

| | HDD (`H:\Desktop\ofeliaengine\target`) | NVMe (`C:\cargo-target\ofeliaengine`) |
|---|---:|---:|
| `check.ps1 -Gpu`, nothing changed | 143 s | 141 s |
| `check.ps1 -Gpu`, `engine_render` touched | 203 s (test 43, tracy 19, docs 28) | 155 s (test 25, tracy 15, docs 6) |
| `engine_core` touched, every test target rebuilt | 40 / 58 s | 10.9 / 14.5 s |

With nothing to build the drive does not matter (the GPU tests take the time); the more a
change rebuilds, the more the HDD costs: incremental caches and linking write many files.

Decision: **`.cargo/config.toml` builds in `C:/cargo-target/ofeliaengine`** (cargo resolves
`CARGO_TARGET_DIR` first: agents keep their own `C:/cargo-target/<task>`).
`tools/check.ps1` takes the build directory from `cargo metadata` (its tracy and loom builds
go in it), `tools/clean.ps1` cleans it and removes agents' directories whose worktree is
gone (only inside `cargo-target`, never `ofeliaengine` or one in use; the nested tracy
and loom directories are cleaned only when given with `-Targets`, see `docs/status.md`).
Logs, golden-image
renders and the shader cache stay in the repository's `target\` (small files people read).
A machine without that drive layout overrides the directory with `CARGO_TARGET_DIR`.


## Addendum (2026-10-11): dependencies built for the host

Proc macros and build scripts get their dependencies built for the host, and Cargo
resolves those features apart from the target's: the hack crate's `[dependencies]` do not
reach them. `half` (ADR 0083) brought zerocopy-derive, which builds `syn` 2 with `full`
in every build that has engine_assets and without it in the others (engine_crash,
engine_editor), so each switch rebuilt `syn` and every proc macro and crate above it. The
hack crate now has an empty build script and `syn` with `full` in its
`[build-dependencies]`, which Cargo unifies with the host's. `tools/feature-drift.ps1` keys
features by name and version (`cargo tree --format "{p}|{f}"`): `syn` 3 (bytemuck's
derive, engine_reflect_derive) has features of its own, and by name alone the check
reported 32 false pairs for it.

## Addendum (2026-10-23): single packages' builds made copies of their own

- **What was seen:**
  - `C:\cargo-target\ofeliaengine` grew from 37 GB (2026-10-18, after `tools/clean.ps1`)
    to 178 GB in five days.
  - In the last day alone, `debug\deps` held 20 copies of the editor's test binary.
  - A build of a few packages (`cargo test -p`, the checks' `-Affected`) built its own
    copy of most of the workspace beside the full check's, instead of reusing it.
- **How it was found:**
  - Cargo's unit graph (`--unit-graph`) lists each unit with its features and profile.
    It is unstable, so `RUSTC_BOOTSTRAP=1` enabled it, for this diagnosis only and never
    in the checks.
  - Each package's own build was compared with the workspace's build of the same kind:
    48 packages, `cargo test --no-run` and `cargo build`.
- **Two causes:**
  1. **Features: seven dependencies drifted unseen.**
     - `tools/feature-drift.ps1` compared features one by one, so a copy resolved with no
       features at all left it nothing to compare.
     - Built for the target:
       - `bitflags`: `serde`, from ron;
       - `crossbeam-utils` and `either`: `std`, from rayon;
       - `num-traits`: `std`, from mlua and the minidump crates;
       - `smallvec`: `const_new`, from ICU4X's fixed_decimal;
       - `thiserror`: `std`, from gpu-allocator and fluent-syntax.
     - Built for the host: `cc`, `parallel` from luau0-src, in every build that has
       engine_script. Without it, every C and C++ build script (mimalloc's under
       engine_core, Jolt's, Box2D's) was built again, and every crate above them.
  2. **The debug info of what build scripts and proc macros use.**
     - Cargo's default for them is "no debug info when possible". A crate that the build's
       own code also uses keeps its debug info, so that one build serves both.
     - Which crates those are depends on the packages a build selects. unicode-ident is
       under proc-macro2 and syn, and also under ron (engine_assets).
     - So every build without engine_assets built it without debug info. With it went
       every proc macro, the hack crate's build script among them, and every crate above.
- **Fixes:**
  - The hack crate has the seven with the workspace's features: six under
    `[dependencies]`, and `cc` with `parallel` under `[build-dependencies]`.
  - `[profile.dev.build-override] debug = false` (the test profile inherits it):
    - what build scripts and proc macros use has no debug info, in every build;
    - a crate that both sides use is built twice, once for each, in every build alike.
  - `tools/feature-drift.ps1` compares copies whole (`<dependency>@<version>
    [<features>]`):
    - with and without dev-dependencies (`cargo build -p`, `cargo run -p`);
    - it found the seven, in 22 s.
  - `tools/clean.ps1` also cleans `examples\` and `build\`, and `-Days` takes a fraction
    of a day: after a build of everything, `-Days 0.05` removes what that build did not
    write.
- **Numbers** (ref-1): units of a package's own build that the workspace's build of the
  same kind lacks, over the 48 packages' `cargo test --no-run` and `cargo build`:

  | | builds with a copy of their own | units |
  |---|---:|---:|
  | before | 88 of 96 | 2,303 |
  | the hack crate's seven | 80 of 96 | 1,897 |
  | and `build-override` | 0 of 96 | 0 |

  - `cargo check --all-targets` (clippy's) and `cargo build --release`: 0 after.
  - Builds right after the workspace's `cargo build --all-targets`:

    | build | before | after |
    |---|---|---|
    | `-p engine_editor` | 20 units, 38.4 s | 0, 0.5 s |
    | `-p engine_crash` | 32 units, 18.7 s | 0, 0.3 s |
    | `-p engine_anim` | 10 units, 7.9 s | 0, 0.3 s |
    | `-p engine_rhi` | 11 units, 4.4 s | 0, 0.3 s |

  - The "before" builds compiled only what copies of earlier that day lacked.
  - The changes rebuilt every engine crate once: 429 units, 242 s.
