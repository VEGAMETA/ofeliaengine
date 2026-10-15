# Your game on the player

The player (`engine_app`) runs a game. It starts the process, the window, the GPU, latency
control, the renderer and your scene. Then, every frame, it calls your game's hooks.
Your game is the hooks; the player does the rest (ADR 0099).

## A game in a few lines

```rust
use engine_app::{Game, Player, PlayerSettings};
use engine_ecs::{Transform, World};

struct MyGame;

impl Game for MyGame {
    // The simulation, at a fixed 64 ticks a second: movement, physics, AI.
    fn tick(&mut self, world: &mut World, dt: f32) {}

    // Every rendered frame, after the ticks: what follows input at once (the camera).
    fn present(&mut self, world: &mut World, alpha: f32) {}
}

fn main() {
    let mut player = Player::new(&PlayerSettings {
        title: String::from("My game"),
        pack: Some("game.pack".into()), // `ofeliaengine cook scene.gltf -o game.pack`
        ..PlayerSettings::default()
    })
    .unwrap();
    // A camera: what the player renders through.
    let camera = player.camera(1.2, 0.05, 500.0);
    player
        .world_mut()
        .spawn((Transform::from_xyz(0.0, 2.0, 8.0), camera));
    player.run(&mut MyGame).unwrap();
}
```

- **The pack's scene** is streamed within the GPU's memory budget.
- **Its instances become entities** in the world: move them, add components, despawn them.
- **`player.setup()`** gives you the meshes, materials, textures and skinned meshes, to
  add your own before the first frame.

## What the player does for you

- **The process.**
  - Windows does not slow the game down when its window is covered.
  - The frame thread runs under MMCSS "Games", on performance cores where the CPU has two
    kinds.
- **The window.** Borderless full screen by default (`Window::FullScreen`); a window that
  fits the screen; or exclusive full screen (`Window::Exclusive`). See
  [latency.md](latency.md#full-screen-and-windows).
- **Latency control, on by default.**
  - NVIDIA Reflex or AMD Anti-Lag 2, without DLLs, or the engine's own limiter.
  - Measured on ref-1, from a frame's start to the screen:
    - RTX 4060 Ti: 7.9 → 5.7 ms;
    - Radeon iGPU: 69 → 52 ms.

    The frame rate stayed the same.
- **The pipeline cache.** It is kept in `%LOCALAPPDATA%\<title>\`, so the second start
  does not compile shaders again.
- **The renderer with your GPU's defaults:**
  - ambient occlusion, antialiasing and render size, meshlet culling;
  - `PlayerSettings` overrides any of them (`antialiasing`, `quality`,
    `ambient_occlusion`, `meshlet_culling`).
- **Frame generation** when you ask for it (`frame_generation`):
  - FSR 3.1 on any GPU;
  - DLSS through the player's own DLLs.

## Each frame

1. Window events go to `Game::event`.
2. Latency control waits, so that input is read as late as possible.
3. The frame waits for its slot and its swapchain image.
4. The due ticks run: `Game::tick`, then transforms propagate to the GPU scene.
5. `Game::present` runs (the camera, from the latest input).
6. `Game::animate` runs: skinned poses, set with each mesh's transform for this frame.
7. Streaming, then the frame's rendering and its UI (`player.ui_mut()`: 2D and 3D
   canvases).
8. Submission and presentation.

`player.frame(&mut game)` runs one frame and returns where its time went
(`FrameReport`); `player.run` loops until the window closes. Neither allocates once the
game is running.

## Settings players change

| setting | what |
|---|---|
| `window` | `FullScreen`, `Windowed { width, height }`, `Exclusive` |
| `monitor` | which monitor (`engine_platform::display::monitors()`) |
| `vsync` | wait for the display's refresh |
| `low_latency` | `Off`, `On`, `Boost` (NVIDIA: clocks kept up) |
| `fps_limit` | a frame cap (a few frames under the refresh rate with G-Sync/FreeSync) |
| `frame_generation` | `Off`, `Fsr`, `Dlss` |
| `antialiasing`, `quality` | TAAU, FSR, DLSS and the render size |

## Crash reports

The crash reporter runs your executable a second time, as a monitor. So the first line of
your `main` decides which of the two it is:

```rust
if engine_crash::run_monitor_if_requested() {
    return; // this process was the crash monitor
}
let _crash = engine_crash::install(&engine_crash::CrashConfig::new("mygame"));
```

## An example to read

`crates/engine_app/examples/flight.rs` is a whole game on the player. It flies through
Sponza with 2,000 moving cubes, 64 animated characters and seven shadowed lights, and
prints where every frame's time went:

```text
cargo run -p engine_app --release --example flight -- sponza.pack
```
