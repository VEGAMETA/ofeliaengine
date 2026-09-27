# 0001. Workspace layout, toolchain and build profiles

- Status: Proposed. The linker choice and the dev profile must be confirmed by
  measurements on the reference hardware (see "Measurements").
- Date: 2026-09-25

## Context

CLAUDE.md fixes most of the stack (§1) and the profile set (§15). Several details are
not fixed and need a recorded decision:

1. how the Cargo workspace is organized;
2. whether the toolchain is pinned;
3. which linker is used and how it is installed;
4. how the `x86-64-v2` baseline (§5) is enforced;
5. exact profile settings;
6. how the code rules from §5 and §15 are enforced by tooling.

## Options

### Workspace layout

- **A. One virtual workspace, `crates/*` glob, shared `[workspace.package]`,
  `[workspace.dependencies]` and `[workspace.lints]`.** Crates inherit version, edition
  and lints. A new crate needs no root `Cargo.toml` edit.
- **B. Explicit member list.** Slightly more explicit, but every new crate touches
  the root file and it is easy to forget one.

### Toolchain

- **A. Pin the exact version in `rust-toolchain.toml` (`1.94.1`).** Same compiler in CI,
  on every developer machine and on the benchmark machine. Benchmark numbers compare
  code changes, not compiler changes. Updates are explicit commits with a benchmark run.
- **B. Track `stable`.** Always current, but a compiler update can silently move the
  benchmark numbers and fail the 3% regression gate (§5) on a commit that changed nothing.

### Linker (MSVC target)

- **A. MSVC `link.exe`** (default). Needs no setup, but it is the slowest option for
  incremental debug builds, which is the main loop of engine development.
- **B. LLVM `lld-link` from a separate LLVM install.** Fast, but every developer and CI
  machine needs LLVM installed and on `PATH`.
- **C. `rust-lld` from rustup** (`linker = "rust-lld.exe"`). This is the same LLD; for
  an MSVC target rustc passes `-flavor link`, so it behaves like `lld-link`. It is installed
  with the toolchain, so nothing extra is required. Bevy recommends this setup for
  fast Windows builds.

### CPU baseline

- **A. `-C target-cpu=x86-64-v2` in `.cargo/config.toml` for the MSVC target.** Applies to
  all builds, including dependencies. Easy to audit.
- **B. `#[target_feature]` per function only.** Leaves the baseline at `x86-64` (v1) and
  wastes the SSE4.2/POPCNT that every Windows 11 24H2 CPU has.

## Measurements / arguments

- **Not measured yet.** This ADR was written in a Linux container. The crates cross-check
  there for `x86_64-pc-windows-msvc`, but nothing can be linked because the MSVC import
  libraries are missing. Before the ADR is accepted, the following must be measured
  on the reference machine (`docs/perf/reference-hw.md`, still to be written by the team):
  - incremental dev rebuild + link time of the first executable (`engine_runtime` or a
    test binary) with `link.exe` and with `rust-lld`;
  - the same for `debug = "full"` vs `debug = "line-tables-only"` in the dev profile.
- The toolchain pin is argued above (reproducible perf gate); it has no measurable cost.
- `x86-64-v2` is the documented Windows 11 24H2 minimum (§5). No measurement is needed to
  enable it; raising it further needs an ADR with numbers.

## Decision

- Workspace: **A** (virtual workspace, `crates/*`, `resolver = "3"`, edition 2024,
  `publish = false`, no `license` field until the team decides, §1; since ADR 0020,
`license = "MIT"` as a placeholder).
- Toolchain: **A**, pinned to `1.94.1` with `rustfmt`, `clippy` and the
  `x86_64-pc-windows-msvc` target.
- Linker: **C** (`rust-lld.exe`), provisionally, until the measurement above is done.
- CPU baseline: **A**.
- Profiles (§15):
  - `dev`: our crates at `opt-level = 1`, all dependencies at `opt-level = 3`;
  - `release`: `opt-level = 3`, no debug info, non-incremental;
  - `profiling`: `release` + `debug = "full"` (Tracy callstacks, symbol resolution);
  - `shipping`: `release` + `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`.
    PGO will be evaluated separately once scene benchmarks exist.
- Enforced by tooling:
  - `unsafe_op_in_unsafe_fn = deny`, `clippy::undocumented_unsafe_blocks = deny`
    (every `unsafe` block needs `// SAFETY:`), `clippy::missing_safety_doc = deny`;
  - `missing_docs = warn` (public API documentation);
  - `unwrap_used`/`expect_used`/`todo`/`unimplemented`/`dbg_macro`/`print_*` = warn (no
    panics on hot paths, no stray output), allowed in tests;
  - `std::thread::sleep` is disallowed via `clippy.toml` (§5: waitable timers);
  - `cargo-deny`: license allow-list from §15, crates.io only, bans `tokio`, `async-std`,
    `smol` and `wgpu`.
- CI runs on a GitHub-hosted `windows-2025` runner, on pull requests and pushes to `main`, as a
  single job (fmt, clippy `-D warnings`, tests, docs with `-D warnings`, cargo-deny, build of
  all four profiles). One job instead of six saves the checkout + toolchain install on
  every check; hosted Windows minutes are limited for a private repository.
  - *Later:* the job moved to the self-hosted runner (#34), and since ADR 0037 changes are
    checked on the reference machine before merging (`tools/check.ps1`), with Actions on
    demand only.
- GPU work (Vulkan tests, golden images, benchmarks) runs on the self-hosted Windows runner
  on the developer's machine (labels `self-hosted, windows, x64`). It runs interactively
  (`run.cmd`), not as a service, because services live in session 0 without a desktop
  and cannot present to a window.

## Consequences

- Adding a crate means creating `crates/<name>/` with `[lints] workspace = true`.
- A toolchain bump is its own commit with a benchmark run and a `docs/perf/history.md`
  entry.
- If `rust-lld` turns out to be slower or broken for some dependency (e.g. a C/C++
  library built with MSVC-specific object features), switch to `link.exe` or LLVM
  `lld-link` via a new ADR.
- The `tokio`/`async-std`/`smol` ban is workspace-wide. When a tool needs an async runtime,
  add a `wrappers` exception in `deny.toml` that allows only that tool crate.
- The benchmark/perf-regression job (§5) is not part of CI yet: it needs the dedicated
  benchmark machine and the first benchmarks.
- GitHub Actions are referenced by version tag, not commit SHA. Pinning to SHAs is a
  supply-chain improvement to make once the repository is public.
