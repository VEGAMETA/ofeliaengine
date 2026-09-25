# engine

A game engine for Windows 11 x64 written in Rust, with Vulkan 1.3, Slang, Luau and Tracy.
The working name is `engine`; the team will pick the final name.

Principles, stack and working rules are in [`CLAUDE.md`](CLAUDE.md). Architectural decisions
are in [`docs/adr`](docs/adr).

## Building

Requirements: Windows 11 x64 and [rustup](https://rustup.rs). The pinned toolchain,
components and linker (`rust-lld`) are installed automatically from `rust-toolchain.toml`.

```sh
cargo build                       # dev profile
cargo profiling                   # release + debug info + Tracy (docs/dev-guide/profiling.md)
cargo build --profile shipping    # fat LTO, panic = abort
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                  # needs `cargo install cargo-deny`
```

## GPU runner

GitHub-hosted runners have no GPU, so Vulkan tests and benchmarks run on a self-hosted
Windows runner (Settings → Actions → Runners). Prerequisites on that machine: Git, rustup,
Visual Studio Build Tools with the C++ workload (MSVC + Windows SDK), a current GPU driver
and the [Vulkan SDK](https://vulkan.lunarg.com) (latest 1.4.x). Start the runner with
`run.cmd` in a logged-in session, not as a service.

## Workflow

Every task is a pull request into `main`. CI runs on a GitHub-hosted Windows runner;
GPU jobs run on the self-hosted runner and post their reports as commit comments.
A pull request is merged once all its checks are green. Findings that need the team go
to the pull request description under "Questions for the team".

## Layout

| Path | Contents |
|---|---|
| `crates/engine_core` | Foundation: job system (`jobs`), SPSC ring (`spsc`), sync primitives |
| `crates/engine_platform` | Windows layer: windows (own thread), raw input (own thread), timers, CPU topology, thread placement, power settings |
| `crates/engine_profiler` | Profiling instrumentation (Tracy zones, plots, frames) |
| `crates/engine_rhi` | Vulkan 1.3 layer: instance, adapter selection, device, queues, swapchain, frames in flight |
| `docs/adr` | Architecture decision records |
| `docs/research` | Research notes behind the decisions |
| `docs/perf` | Performance budgets, reference hardware, measurement history |
| `docs/dev-guide` | Documentation for game developers |

Other crates from the target layout (CLAUDE.md §2) are added when work on them starts.
