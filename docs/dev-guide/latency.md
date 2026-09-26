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
    fps_limit: Some(141.0),    // with a variable-refresh (G-Sync/FreeSync) 144 Hz monitor
    max_queued_presents: 1,    // the generic path: how many frames may queue ahead
};
```

- **"Low latency" on** is the right default for every game with real-time input.
- **With variable refresh rate,** cap the frame rate a few frames below the monitor's
  maximum (141 on 144 Hz). That keeps the display in its variable range, and latency
  stays low.
- **Without a cap and with a GPU-bound game,** latency reduction matters most: without
  it, the CPU runs ahead and frames wait in a queue.

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
  - on NVIDIA, Reflex's own input-to-GPU-finish time.
- **For true input-to-photon numbers,** use NVIDIA's LDAT or an OSLTT-type sensor. The
  engine reports what the PC controls.
