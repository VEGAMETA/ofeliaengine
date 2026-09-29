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
cargo run --release -p engine_cli -- shaders build <dir>   # Compile All Shaders (docs/dev-guide/shaders.md)
```

## GPU runner

GitHub-hosted runners have no GPU, so Vulkan tests and benchmarks run on a self-hosted
Windows runner (Settings → Actions → Runners). Prerequisites on that machine: Git, rustup,
Visual Studio Build Tools with the C++ workload (MSVC + Windows SDK), a current GPU driver
and the [Vulkan SDK](https://vulkan.lunarg.com) (latest 1.4.x). Start the runner with
`run.cmd` in a logged-in session, not as a service.

## Workflow

Every task is a pull request into `main`. Before merging, it is checked on the reference
machine (ref-1) with `powershell -File tools\check.ps1`: rustfmt, clippy, tests, tests with
Tracy, docs; `-Gpu` adds the Vulkan tests on both GPUs and the Slang tests, `-Deny`
cargo-deny, `-Loom` the loom model checks, `-Profiles` the builds of all profiles, `-Full`
all of them (ADR 0037). Benchmarks run on ref-1 by hand and go to `docs/perf/history.md`.
GitHub Actions run on demand only, on the self-hosted runner: by hand, or on a pull request
with the `ci` label (`bench` for the benchmarks); reports are posted as commit comments.
Findings that need the team go to the pull request description under "Questions for the
team".

## Layout

| Path | Contents |
|---|---|
| `crates/engine_assets` | Asset import: glTF 2.0 into memory (meshes, PBR materials, node hierarchy) and into the ECS (ADR 0033) |
| `crates/engine_audio` | Audio: lock-free mixer (buses, sends, sidechains, virtual voices, sample-accurate starts), effects (biquad, compressor, limiter, delay), WAV decoding, WASAPI output with `IAudioClient3` low latency; example `tone` |
| `crates/engine_core` | Foundation: job system (`jobs`), frame arena (`arena`), SPSC ring (`spsc`), stable and state hashing (`hash`), sync primitives |
| `crates/engine_ecs` | Entity component system: archetypes in growing chunks, cached and parallel queries, commands, change tracking, resources, a deterministic parallel system schedule, transform propagation, snapshots and state hashes |
| `crates/engine_input` | Input actions in the spirit of Godot: named actions, bindings (keys by position, mouse, gamepads, composites, chords), contexts with priorities, tick-accurate state, network user commands; raw input and XInput on Windows |
| `crates/engine_reflect` (+ `_derive`) | Reflection: `#[derive(Reflect)]` static field tables with offsets and attributes, for serialization, the inspector, scripting and replication |
| `crates/engine_latency` | Input latency and frame pacing: NVIDIA Reflex (`VK_NV_low_latency2`), AMD Anti-Lag 2 (`VK_AMD_anti_lag`), a generic limiter with `present_wait`, shared markers; example `latency` |
| `crates/engine_text` | Text: fonts from bytes, HarfRust shaping, UAX #14 wrapping with fallback fonts and alignment, coverage rasterizer, glyph atlas; bundled Go fonts |
| `crates/engine_platform` | Windows layer: windows (own thread), raw input (own thread), timers, CPU topology, thread placement, power settings, directory watching |
| `crates/engine_profiler` | Profiling instrumentation (Tracy zones, plots, frames) |
| `crates/engine_rhi` | Vulkan 1.3 layer: instance, adapter selection, device, queues, swapchain, frames in flight, memory allocator (TLSF) |
| `crates/engine_render` | Render graph (culling, transient memory aliasing, automatic barriers, GPU timings), forward PBR with cascaded shadow maps, z-binned point and spot lights and image-based light from an environment, bloom and automatic exposure, motion vectors and projection jitter, per-frame upload ring, textures, viewports (split screen, nesting, pixel-perfect); example `cubes` |
| `crates/engine_upscale` | Temporal antialiasing and upsampling (TAAU, ADR 0043): resolves the forward renderer's jittered frames along their motion vectors; example `taau_bench` |
| `crates/engine_render2d` | 2D renderer: sprites and SDF shapes batched through bindless textures, deterministic layer / y-sort order, clipping; examples `sprites`, `sprite_bench`, `viewports` |
| `crates/engine_script` | Luau scripting: sandboxed VM with native codegen, module hot reload with state, time and memory limits (ADR 0032) |
| `crates/engine_editor` | The editor: the Godot-layout shell in a window over a 3D viewport (`cargo run -p engine_editor --release --bin editor`, `docs/dev-guide/editor.md`), undo (ADR 0031), command palette core, UI painting, viewport camera |
| `crates/engine_scene` | Text scenes: line-oriented, merge-friendly save and load of entities and reflected components (ADR 0029) |
| `crates/engine_ui` | UI core for the editor and games: immediate-mode building, retained state, full-tree layout, keyboard focus, draw commands (ADR 0030) |
| `crates/engine_physics` | Physics interface (`PhysicsBackend`), shared types, gravity fields, benchmark scenes |
| `crates/engine_physics_jolt` | Jolt Physics 5.6 backend through vendored JoltC (`third_party/`) |
| `crates/engine_physics2d` | 2D physics on vendored Box2D v3.1.1 behind a small C shim |
| `crates/engine_shader` | Slang → SPIR-V: in-process compiler, content-addressed cache, Compile All, hot reload |
| `tools/engine` | The `engine` command line (`engine shaders build`, `engine shaders watch`) |
| `docs/adr` | Architecture decision records |
| `docs/research` | Research notes behind the decisions |
| `docs/perf` | Performance budgets, reference hardware, measurement history |
| `docs/dev-guide` | Documentation for game developers |

Other crates from the target layout (CLAUDE.md §2) are added when work on them starts.

## License

MIT (`LICENSE`), as a placeholder: the final license is chosen before the first release
(ADR 0020). Third-party code, fonts and icons keep their own licenses (`THIRD_PARTY_NOTICES.md`).
