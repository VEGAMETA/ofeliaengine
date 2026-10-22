# Low latency and frame pacing

The engine keeps the time from a player's input to the picture on screen as short as
the hardware allows:
- **NVIDIA GPUs:** NVIDIA Reflex;
- **AMD GPUs:** AMD Anti-Lag 2;
- **every GPU:** the engine's own frame pacing.

None of this needs extra DLLs: it goes through the graphics driver.

## Settings to offer players

```rust
use engine_latency::{LatencySettings, LowLatency};

let settings = LatencySettings {
    mode: LowLatency::On,      // Off, On, or Boost (Boost keeps GPU clocks up, NVIDIA)
    fps_limit: Some(138.0),    // with a variable-refresh (G-Sync/FreeSync) 144 Hz monitor
    max_queued_presents: 1,    // the generic path: how many frames may queue ahead
};
```

- **"Low latency" on** is the right default for every game with real-time input.
- **With variable refresh rate,** cap the frame rate a few frames below the monitor's
  maximum (`hz − hz²/3600`: 138 on 144 Hz, 224 on 240 Hz). That keeps the display in its
  variable range, and latency stays low.
  - **The player does this by default in release builds** (`PlayerSettings::frame_cap`,
    [player.md](player.md)).
  - With vsync it caps as far over the rate instead. The display paces then, and the cap
    stops only a path that does not wait for it: a laptop's GPUs presenting across
    adapters, or a driver forcing vsync off.
- **Without a cap and with a GPU-bound game,** latency reduction matters most: without
  it, the CPU runs ahead and frames wait in a queue. In the engine's flight on ref-1's
  RTX, input to GPU done took 3.6 ms without it and 2.0 ms with Reflex.
- **Above about 500 FPS, Reflex costs frames** (NVIDIA's driver 616.92): its sleep is
  never shorter than about 1.5 ms, so a frame takes at least that plus the CPU's work.
  The flight's 1.44 ms GPU frames ran at 2.0 ms intervals, against 1.83 ms without
  Reflex; from 2 ms GPU frames on, Reflex costs nothing. For a benchmark of the GPU's
  throughput, turn it off.

## Full screen and windows

The window decides how the frame reaches the screen (ADR 0099):

```rust
use engine_platform::window::{WindowDesc, WindowMode};
use engine_rhi::{FullScreenExclusive, SwapchainDesc};

let window = WindowDesc {
    mode: WindowMode::FullScreen, // borderless, covering the monitor
    ..WindowDesc::default()
};
let swapchain = SwapchainDesc {
    // Never let the driver switch the screen over to the game.
    full_screen_exclusive: FullScreenExclusive::Disallowed,
    ..SwapchainDesc::default()
};
```

- **Borderless full screen is the right default.** Windows shows the game's frames
  directly, without composing the desktop ("independent flip"), when the game covers its
  monitor and nothing else is on top.
  - Alt+Tab is instant.
  - Overlays and notifications can show over the game without a mode switch.
  - On ref-1's RTX, without vsync, it reached the screen 7.9 ms after the frame began.
    With the driver left to choose, it was 9.3 ms: the frames were copied instead of
    flipped. So the engine states "disallowed".
- **A window** fits into its monitor's work area: a size too large for the screen is
  scaled down, keeping its aspect ratio.
  - A window larger than the screen was copied by NVIDIA's driver, and only 1 frame in
    25 reached the screen.
- **Exclusive full screen** (`FullScreenExclusive::Exclusive { monitor }`) asks the
  driver for the monitor each time the swapchain is made.
  - On Windows 11 it changed nothing measured on ref-1.
  - NVIDIA grants it only with vsync. AMD refuses it when another GPU drives the monitor.
  - Refused, the swapchain falls back to borderless by itself. Lost (Alt+Tab), acquiring
    or presenting reports `is_out_of_date()`: recreate the swapchain.
- **Players can check** which path their frames take with PresentMon (Intel, free): look
  for "Hardware: Independent Flip" or "Hardware Composed: Independent Flip" in the
  `PresentMode` column, not "Composed: Copy".

## What the engine does each frame

1. `begin_frame`: waits the right amount of time. The driver (Reflex, Anti-Lag 2) or
   the engine (other GPUs) decides how long, so the frame finishes just as the GPU gets
   to it.
2. Input is read, the game simulates, the frame is recorded and submitted. Markers
   around each step go to the driver and the profiler.
3. `before_present`, then present.

Your game code does not call these. The engine's frame loop does.

## Measuring

- **In Tracy:** the plot `latency::input_to_present_ms`.
- **`cargo run -p engine_latency --example latency --release`** prints, for each
  available configuration:
  - frame time;
  - time from input to the present call;
  - time from input to the frame being displayed;
  - on NVIDIA, Reflex's own input-to-GPU-finish time. With the frame's submissions
    tagged by present id, as the extension asks, driver 616.92 puts the next frame's GPU
    times in a frame's report, so this reads about one frame interval high.
- **`flight --input-latency`** (`engine_app`'s example) times input to GPU done on the
  CPU's clock, from a thread waiting on the frames' timeline, under any latency control.
- **For true input-to-photon numbers,** use NVIDIA's LDAT or an OSLTT-type sensor. The
  engine reports what the PC controls.
