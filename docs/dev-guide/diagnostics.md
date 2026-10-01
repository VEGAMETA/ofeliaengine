# Logs, crash reports and GPU crashes

When something goes wrong, the engine leaves a trail: a log, a crash report with a
minidump, and for GPU crashes the pass the GPU was in. This page says where to find each
and how to control them. Design: ADR 0061.

## Logging

Log with the standard [`log`](https://docs.rs/log) macros; the engine installs the
backend:

```rust
log::info!("level loaded: {name} in {ms:.1} ms");
log::warn!(target: "my_game::ai", "no path from {from:?} to {to:?}");
log::error!("save failed: {e}");
```

- **Where lines go:** stderr, the debugger's output window (Visual Studio, RAD Debugger)
  while one is attached, and a file — the editor writes
  `%LOCALAPPDATA%\ofeliaengine\logs\editor.log`. In profiling builds every line also
  appears in Tracy's message list, colored by level, at the moment it was logged.
- **Filters:** the `ENGINE_LOG` environment variable, a comma-separated list:
  `info` sets the default level; `engine_render=debug` sets a target and everything under
  it (`engine_render::graph`…); a bare name (`vulkan`) enables everything for it. The
  longest match wins. Levels: `off`, `error`, `warn`, `info`, `debug`, `trace`. Examples:
  `ENGINE_LOG=warn`, `ENGINE_LOG=info,engine_rhi=trace,vulkan=off`. The target is the
  module path unless you pass `target:`. At run time: `engine_log::set_filter(...)`.
- **Cost:** a disabled level costs nothing measurable, a disabled target ~5 ns, a
  formatted line 40–120 ns on the calling thread; nothing is allocated and nothing waits
  for the disk (a separate thread writes the file). Logging every frame is fine; logging
  thousands of lines per frame loses the oldest lines (the log says how many).
- **Vulkan validation** messages (debug builds, or `--validate` in the editor) arrive
  under the target `vulkan`.
- Lines longer than 3,480 bytes are cut with `...`.

Your own program (not the editor or `ofeliaengine`) installs it once, early in `main`:

```rust
let _log = engine_log::init(&engine_log::LogConfig {
    file: Some(engine_log::report::log_file("mygame")),
    ..engine_log::LogConfig::default()
})?;
```

## Crash reports

The editor and the `ofeliaengine` command install crash reporting at start-up. A crash
(access violation, stack overflow, `abort`, heap corruption…) or a panic writes two
files into `%LOCALAPPDATA%\ofeliaengine\reports`:

- `<app>-<date>-<time>-<pid>.dmp` — a minidump: open it in Visual Studio (File → Open,
  then "Debug with Native Only") or WinDbg with the build's PDB next to the executable to
  see every thread's stack;
- `<app>-<date>-<time>-<pid>.txt` — what happened (exception or panic message with its
  backtrace), the GPU breadcrumbs of every device, and the last 300 log lines.

The dump is written by a second, idle copy of the executable (the crash monitor), so it
works even when the crashed process's memory is corrupt. You will see it in Task Manager;
it exits with the program.

In your own program:

```rust
fn main() {
    if engine_crash::run_monitor_if_requested() {
        return; // this process was started as the crash monitor
    }
    let _log = engine_log::init(&engine_log::LogConfig::default());
    let _crash = engine_crash::install(&engine_crash::CrashConfig::new("mygame"));
    // ...
}
```

`run_monitor_if_requested` must come first in `main`. To add your own context to reports
(the level being played, the save slot), register a section:

```rust
let _section = engine_log::report::add_section("game", move |out| {
    writeln!(out, "level: {}", LEVEL_NAME)
});
```

The closure may run on a crashing thread: only read memory, do not allocate or lock.

## GPU crashes

When the GPU crashes (a shader reads a bad address, a dispatch never ends, a driver bug),
Vulkan reports the device lost. The engine then logs an error report and writes
`gpu-fault-<date>-<time>-<pid>.txt` into the reports folder:

```text
GPU device lost (in vkWaitSemaphores)
device: NVIDIA GeForce RTX 4060 Ti (discrete, Vulkan 1.4.351, 15.7 GiB, NVIDIA 616.92)
  invalid write: address 0x00007f00df852000 (somewhere in 0x7f00df852000..0x7f00df853000)
  instruction pointer of the faulting instruction: address 0x000000020000b970 (...)
  vendor crash dump: 1220084 bytes
  breadcrumbs:
    track 0 (render graph, frame slot 0): frame 1, 3 passes: the GPU last started pass 1 'fault' and last finished pass 0 'before the fault'
      running: 1 'fault'
```

- **Breadcrumbs** name the render graph pass the GPU was running (between the last pass
  it started and the last it finished). Start there: the pass's shaders and the buffers
  they address.
- **Addresses** (GPUs with `VK_EXT_device_fault`): what kind of access failed and where.
  Compare with the device addresses of your buffers.
- **Vendor crash dump** (`gpu-fault-...-device0.bin`, NVIDIA): the driver's own dump; send
  it with a bug report to the GPU vendor.
- A GPU that stops making progress without the driver resetting it is reported after 5 s
  as a **GPU hang**, with the breadcrumbs.

Breadcrumbs cost about 0.1 µs of CPU and up to 0.2 µs of GPU time per render graph pass
on NVIDIA (nothing measurable on AMD) and are on by default. `ENGINE_GPU_BREADCRUMBS=0`
turns them off. For a crash that needs more detail than one marker per pass, run with the
Vulkan SDK's Crash Diagnostic Layer (`VK_LAYER_LUNARG_crash_diagnostic`), which marks
every command.

## Environment variables

| Variable | Effect |
|---|---|
| `ENGINE_LOG` | log filter (see above); default `info` (the command line: `warn`) |
| `ENGINE_GPU_BREADCRUMBS` | `0` turns GPU breadcrumbs off |
| `ENGINE_DEVICE_FAULT` | `0` leaves `VK_EXT_device_fault` disabled |
