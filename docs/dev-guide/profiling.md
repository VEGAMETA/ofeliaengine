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

## Common mistakes

- **Profiling a `dev` build.** Our crates are compiled at `opt-level = 1` there, so the
  numbers are not representative. Use `cargo profiling`.
- **Leaving a Tracy build running without the profiler attached.** Events pile up in memory
  and slow the game down over time. Attach Tracy or use `tracy-ondemand`.
- **Profiler version mismatch.** Tracy refuses to connect, or shows a protocol error.
  Download the version listed above.
