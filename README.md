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
cargo build --profile profiling   # release + debug info, for Tracy
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

## Layout

| Path | Contents |
|---|---|
| `crates/engine_core` | Foundation: memory, containers, sync primitives, job system (in progress) |
| `docs/adr` | Architecture decision records |
| `docs/perf` | Performance budgets, reference hardware, measurement history |

Other crates from the target layout (CLAUDE.md §2) are added when work on them starts.
