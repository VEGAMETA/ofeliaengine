# Profiling your game

The engine is instrumented with [Tracy](https://github.com/wolfpld/tracy), a frame
profiler that shows CPU and GPU zones, locks, plots, messages and context switches
with nanosecond resolution.

## 1. Get the profiler

Download **Tracy 0.14.1** for Windows from the
[Tracy releases page](https://github.com/wolfpld/tracy/releases) and unpack it anywhere.
The profiler and the engine must use the same version; the engine's version is listed in
`crates/engine_profiler/Cargo.toml`.

## 2. Build with Tracy

```sh
cargo profiling
```

This builds the whole workspace with the `profiling` profile (optimized, with debug info)
and Tracy enabled. Normal `dev`, `release` and `shipping` builds contain no profiling code.

## 3. Profile

1. Start your game (or the editor) from the `profiling` build.
2. Start `tracy-profiler.exe` and connect to `localhost`. The game records from process
   start, so you can connect after launch and still see start-up.
3. For context switches and CPU sampling, run the game **as administrator**. Without it,
   those views stay empty. Everything else works.

Scripts' hooks have a zone each, named after the component and the hook:
`script Door.on_tick` ([scripting.md](scripting.md#measuring-your-scripts)).

## 4. Instrument your own code

```rust
fn update_enemies(enemies: &mut [Enemy]) {
    engine_profiler::zone!("game::update_enemies");
    for enemy in enemies {
        engine_profiler::zone!("game::enemy");
        enemy.think();
    }
    engine_profiler::plot!("game::enemies", enemies.len());
}
```

- `zone!()` without a name uses the function name.
- Zones cost a few nanoseconds in a Tracy build and nothing otherwise, so instrument freely.
- `plot!` draws a value over time.
- `engine_profiler::message("boss spawned")` puts an event on the timeline.

## Options

| Feature | When to use it |
|---|---|
| `engine_profiler/tracy` (default for `cargo profiling`) | Normal profiling sessions. Accepts connections from this machine only. |
| `engine_profiler/tracy-ondemand` | Long sessions (for example a whole day in the editor) where Tracy is attached only occasionally. Data is collected only while connected, so start-up is not captured. |
| `engine_profiler/tracy-remote` | Profiling a game on another PC (for example a test machine) from yours. The game accepts connections from the network, so use it on trusted networks only. |

Example: `cargo build --profile profiling --features engine_profiler/tracy-ondemand`.

## Memory: the engine's allocator

Set the engine's allocator in your game's executable (ADR 0086): mimalloc, 1.5–4.7× as
fast as the Windows heap where your game allocates (loading, tools, UI), and in debug
builds it counts what Rust holds.

```rust
#[global_allocator]
static ALLOCATOR: engine_core::alloc::EngineAllocator =
    engine_core::alloc::EngineAllocator::new();
```

- `engine_core::alloc::heap_stats()` (debug builds): bytes Rust holds now and at peak,
  allocations made.
- mimalloc keeps freed memory in the heap of the thread that allocated it. The job
  pool's workers give theirs back when they go idle; after loading or unloading a
  level, call `jobs.release_memory()` (what the workers allocated and your main thread
  freed) and `engine_core::alloc::release_thread_memory()` on the main thread.

## Common mistakes

- **Profiling a `dev` build.** Our crates are compiled at `opt-level = 1` there, so the
  numbers are not representative. Use `cargo profiling`.
- **Leaving a Tracy build running without the profiler attached.** Events pile up in memory
  and slow the game down over time. Attach Tracy or use `tracy-ondemand`.
- **Profiler version mismatch.** Tracy refuses to connect, or shows a protocol error.
  Download the version listed above.
- **Slow start of short-lived Tracy processes (tests, tools).** At start the Tracy client
  loads debug symbols for every module (4-15 s per process on the reference machine, most
  when run as administrator). When no profiler will connect, set
  `TRACY_SYMBOL_OFFLINE_RESOLVE=1`: start drops to ~0.3 s. `tools\check.ps1` and CI do this
  for the tests with Tracy. Leave it unset when profiling, or call stacks show addresses
  instead of function names.

## Shader registers and occupancy

On integrated GPUs a shader's register count often decides its speed: more registers,
fewer waves in flight, less latency hidden. To see what the driver compiled:

```sh
set ENGINE_PIPELINE_STATS=1
cargo run -p engine_render --release --example scene_bench -- test-assets/Sponza/Sponza.gltf --pipeline-stats
```

- `ENGINE_PIPELINE_STATS=1` makes the device enable `VK_KHR_pipeline_executable_properties`
  and every pipeline capture its statistics.
- `Pipeline::statistics(&device)` returns them; the names and meanings are the driver's.
  - AMD: `numUsedVgprs` per stage;
  - NVIDIA: `Register Count`, `Binary Size`.
- `ForwardRenderer::shading_statistics` gives them for each shading variant (with or
  without local lights and an environment).
- The perf gate checks them for every pipeline its scenes make: `perf_gate --registers`
  (run by `tools\check.ps1 -Perf` on both GPUs) lists each pipeline whose vector registers
  or spills grew against `tests/golden/perf/<vendor>.registers.tsv`, as a warning. A
  change that moves them on purpose records the new values with
  `perf_gate --registers --update` on both GPUs (`ENGINE_ADAPTER=radeon` for the iGPU).
