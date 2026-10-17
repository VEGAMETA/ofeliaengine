# Your game on the player

New here? The [quick start](quick-start.md) has a game on screen in five minutes.

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

## A scene file

What the editor saves (ADR 0108) loads the same way:

```rust
let mut player = Player::new(&PlayerSettings {
    scene: Some("scenes/main.scene".into()), // relative to `project`
    project: Some("C:/games/mygame".into()), // `None`: the working folder
    ..PlayerSettings::default()
})?;
```

- **Every reflected component** of your program loads, without registration: those made
  with `reflect_component!` in any crate the game links.
- **`Model`** names a mesh and a material by asset, and the player draws it. It makes the
  entity's `MeshRenderer` in the frame and keeps it in step: change the model and the
  drawing follows; remove it and the drawing goes. In code,
  `world.spawn((Transform::from_xyz(0.0, 1.0, 0.0), Model::new(builtin::SPHERE, builtin::MATERIAL)))`
  is enough.
- **Built-in meshes** are `builtin::CUBE`, `SPHERE`, `PLANE` and `CYLINDER`, each 1 across;
  the built-in material is `builtin::MATERIAL`. Your own materials are `.material` files
  ([scenes.md](scenes.md#materials)).
- **`player.load_scene(path)`** adds another scene to the world (a level, a room).
- **An asset the player cannot find** leaves its entity undrawn and says why once in the
  log; `player.assets().problem(asset)` tells you too.

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
- **Reflections and global illumination** when your world has their entities (below).
- **Frame generation** when you ask for it (`frame_generation`):
  - FSR 3.1 on any GPU;
  - DLSS through the player's own DLLs.

## Lights and animated characters

Lights are components on entities. The player hands them to the renderer after every
tick:

```rust
use engine_runtime::{PointLight, SkinnedMesh, SpotLight, Sun};

// A warm lamp casting shadows, a spot shining down its entity's −Z axis, the sun.
world.spawn((GlobalTransform(lamp_at), PointLight::new(Vec3::new(24.0, 15.0, 8.0), 8.0).with_shadow()));
world.spawn((GlobalTransform(spot_at), SpotLight::new(Vec3::splat(90.0), 12.0, 0.4, 0.65)));
world.spawn((GlobalTransform(sun_turned), Sun { color: Vec3::splat(3.0), ambient: Vec3::splat(0.08) }));
```

- **A light keeps its place in the renderer's list while it lives:** its cached shadow is
  redrawn only when it or something near it moves.
- **Without a `Sun` entity there is no sunlight.**

A skinned character is a `SkinnedMesh` next to its `MeshRenderer`. Set its skinning
matrices in `Game::animate`; the player puts it where its entity is this frame:

```rust
fn animate(&mut self, parts: &mut FrameParts<'_>, dt: f32) {
    for c in &mut self.characters {
        c.sample(dt); // the animation (engine_anim)
        parts.skinning.set_palette(c.skinned, &c.palette);
    }
}
```

## Reflections and global illumination

Reflection probes and the volume of global illumination probes are components too. Each
is made when its first entity appears, and costs nothing before:

```rust
use engine_runtime::{GiVolume, ReflectionProbe};

// A probe for a room: captured from its entity, reflecting inside the box around it.
world.spawn((GlobalTransform(room_center), ReflectionProbe::new(Vec3::new(6.0, 3.0, 9.0))));
// Light bouncing over the whole level: probes about a meter apart.
world.spawn((GlobalTransform(level_center), GiVolume::new(level_half_size, 1.0)));
```

- **A probe follows its entity.** Moved, turned or scaled, it is captured again.
  - Something moving inside its box captures it again at most every 30 frames
    (`ProbeSettings::scene_interval`), each probe in turn.
  - With `manual: 1` it is captured only when it changes, or when you ask
    (`player.reflection_probes()` and `player.probe_extract()` give its `refresh`).
  - Up to 16 probes by default (`reflection_probes` in the settings): about 1 MiB each.
- **One GI volume lights the world:** the first `GiVolume` found.
  - It needs a GPU with ray queries (NVIDIA RTX, AMD RX 6000 and newer, their integrated
    GPUs). Without one, the world keeps the sky's light.
  - It converges in its first 11 frames, once the pack's textures have arrived. Then a
    ray budget a frame keeps it following the sun, the sky and what moves.
  - Moving or resizing it makes it anew, and the GPU waits for its frames: place it once
    per level.
- **Screen-space reflections** are a setting (`screen_space_reflections`), off by default.
  Unlike probes they cost every frame, whatever the world holds.

What each costs in `flight` (Sponza with 2,000 moving cubes and 64 characters, a
1797 × 1011 window, the GPU frame's median):

| | RTX 4060 Ti | Radeon iGPU (TAAU at 67 %) |
|---|---:|---:|
| none of them | 1.78 ms | 21.3 ms |
| four probes along the courtyard | 1.91 ms | 24.7 ms |
| the GI volume (8,680 probes 0.96 m apart) | 2.04 ms | 23.5 ms |
| screen-space reflections | 2.15 ms | 26.9 ms |
| all three | 2.50 ms | 32.6 ms |

On a small integrated GPU each is a choice to make with your players: offer them as
settings.

## The world's look, and a loading screen

- **The sky, the atmosphere and fog** are set up with the world: `player.setup()`'s
  `environment` (`set_atmosphere`, `set_fog`, `set_panorama`, `set_intensity`).
- **Bloom, the tone curve, color grading, depth of field** and the clear color live in
  `player.view_settings_mut()`. The camera entity gives the antialiasing, the render size,
  the exposure and whether it is cinematic.
- **Graphics options at run time:** `player.set_global_illumination(...)` changes the GI
  volume's settings or turns it off; the volume is made again and converges again.
- **When to lift a loading screen:** `player.scene_settled()` says that streaming has gone
  quiet once and the probes have been asked for new captures. Add that
  `reflection_probes()` have no capture pending and `gi_volume()` has converged.

## Without a window

`Window::Offscreen { width, height }` renders without a window, a swapchain or latency
control: tests, benchmarks, renders on a machine without a screen. `player.capture(&mut
game)` runs a frame and returns its pixels (8-bit sRGB RGBA, rows top to bottom):

```rust
let mut player = Player::new(&PlayerSettings {
    window: Window::Offscreen { width: 1280, height: 720 },
    validation: Some(true), // with synchronization validation
    ..PlayerSettings::default()
})?;
for _ in 0..60 {
    player.frame(&mut game)?;
}
let rgba = player.capture(&mut game)?.expect("a frame");
```

The golden tests render `sponza_day` this way and hold it to the image the test bed
draws by hand (`docs/dev-guide/testing.md`).

`player.assume_display(info)` draws an offscreen player's frames for a display of your
choosing, HDR included (see below): captures then come back in the output's own format.

## HDR

While Windows runs the window's display in HDR ("Use HDR" in the display settings), the
player presents in HDR by itself, and follows the display as it changes: HDR turned on or
off (Win+Alt+B), the "SDR content brightness" slider, the window moved to another monitor.

- **Light brighter than white stays brighter:** the tone mapper rolls off towards the
  display's peak instead of SDR white (ADR 0038).
- **The UI is drawn at Windows' SDR white level**, as the desktop's own windows are.
- **`PlayerSettings::hdr`** (or `player.set_hdr`, for a settings menu) picks the output:
  - `OutputColor::ScRgb`, the default: linear half floats; the cheapest;
  - `OutputColor::Hdr10`: PQ in 10 bits; one more full-screen pass;
  - `OutputColor::Sdr` to stay in SDR.
- **With frame generation:** FSR's presents scRGB or SDR; DLSS's presents SDR until it is
  verified in HDR.
- `player.levels()` tells what the output is now (its color, reference white and peak in
  nits). An overlay (the editor's panels) learns of every change through
  `Overlay::output_changed`.

| What HDR costs the GPU, 1920 × 1080 | RTX 4060 Ti | Radeon iGPU (2 CU) |
|---|---|---|
| scRGB | +11 µs (the tone mapper) | +37 µs |
| HDR10 | +39 µs (the tone mapper, the encode pass) | +590 µs |

Captured offscreen (`hdr_output_cost`), so without Windows' composition of the window.

## Sound

Sound is components too ([audio.md](audio.md), section 0):
- `AudioSource` plays a file as it is (music, the UI);
- `AudioSource3D` plays one at its entity in the world;
- `AudioListener` marks who hears (the main camera by default);
- `ReverbZone` gives a place a reverb of its own.

Sounds come from `sounds/` while you develop (a changed file plays at once) and from
`sounds.pack` (`ofeliaengine sounds build`) in a release.

With `Game::acoustics` returning your physics world, walls muffle what is behind them and
the room around the listener sets the reverb. Saves keep where every sound was. The
world's `Mixer` sets the buses' volumes.

## Co-op

With `settings.network = Some(NetConfig::new::<Input>(schema))` the player runs the
network session your game asks for through the world's `Network` resource: host, invite,
join, leave ([networking.md](networking.md), section 8). Hosting, it runs the server around
your ticks; joined, the client runs them (its predictions); offline, nothing changes.

## Each frame

1. Window events go to `Game::event`.
2. Latency control waits, so that input is read as late as possible.
3. The frame waits for its slot and its swapchain image.
4. The due ticks run: `Game::tick`, then transforms propagate to the GPU scene.
5. `Game::present` runs (the camera, from the latest input).
6. `Game::animate` runs (skinning matrices); the player places the skinned meshes and
   mirrors the probes and the GI volume. Then sound follows the sources and the listener,
   and the videos play.
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
| `hdr` | the output while the display runs in HDR: `ScRgb` (default), `Hdr10`, `Sdr` |
| `low_latency` | `Off`, `On`, `Boost` (NVIDIA: clocks kept up) |
| `fps_limit` | a frame cap (a few frames under the refresh rate with G-Sync/FreeSync) |
| `frame_generation` | `Off`, `Fsr`, `Dlss` |
| `antialiasing`, `quality` | TAAU, FSR, DLSS and the render size |
| `screen_space_reflections` | off, or `SsrSettings` (half-resolution rays by default) |
| `global_illumination` | the GPU class's (`None`), your `GiSettings`, or off (`Some(None)`) |
| `reflection_probes` | the probes' room and captures, or off (`None`) |
| `contact_shadows` | the sun's contact shadows, off by default |
| `languages` | the player's languages for your text; `None`: Windows' display languages |
| `input` | your input actions (`engine_input::InputMap`): the world's `Input` resource, read from raw keyboard and mouse input and XInput pads, sampled before every tick and before `present` ([input.md](input.md)); `None`: only the window's events |

## Your game's text

`text: Some(TextSource::Folder { dir: "loc".into(), source: "en".into() })` in development,
`TextSource::Pack("strings.pack".into())` in a shipped game: the localizer is then the
world's `Localizer` resource, in the first of the player's languages your text has. A
folder is kept live, an edit showing in the running game. See `localization.md`.

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
