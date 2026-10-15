# 0099. The player crate (`engine_app`) and its window modes

- Status: Accepted (done: steps 1 to 4, 2026-10-15)
- Date: 2026-10-15
- Related:
  - [ADR 0023](0023-latency.md): the latency providers;
  - [ADR 0055](0055-pipeline-cache-and-warm-up.md): the pipeline cache;
  - [ADR 0064](0064-gpu-scene-extraction-runtime.md): the runtime's frame;
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - research note `docs/research/player.md`;
  - the second review (`docs/research/core-audit-2.md` §3.7);
  - queue item 8a.

## Context

- **Nothing owns "a game".** Each program wires the device, the swapchain, latency, the
  frame loop, the process set-up and the pipeline cache itself (review §3.7):
  - the golden test bed, `game_loop`, `flight`, `scene_bench`, `stream_bench`,
    `tools/stress`, the frame renderer's tests and the editor.
- **What that left out:**
  - `RenderDefaults::ambient_occlusion` is applied by nothing;
  - the latency providers and the CLAUDE.md §5 process set-up run in the editor only;
  - lights, skinned meshes, probes and the environment are passed by hand;
  - several features are renderer-wide switches, so two views cannot differ.
- **The window.** The owner asked (2026-10-10) for three modes:
  - a window not larger than its monitor's work area;
  - borderless full screen;
  - exclusive full screen (`VK_EXT_full_screen_exclusive`), measured with PresentMon.
  `flight`'s 1920 × 1080 window was larger than the 1080p screen. On NVIDIA it was
  copied through GDI and showed 24 of its 603 frames a second (the research note).

## Options

**The crate.**

1. **Keep wiring each program.** Rejected: the review's drift, features applied by nothing.
2. **A player crate that owns the wiring, with hooks for the game** (chosen).
   - It owns the device, the window and its swapchain, latency, the process, the
     pipeline cache, the scene's streaming, the frame renderer and the runtime.
   - The game supplies start, tick, present and UI, as Wicked Engine's render paths and
     Unreal's game tick do.
3. **Bevy's `App` with plugins and schedules.**
   - The runtime (ADR 0064) already orders a frame for latency, with the job system inside
     it.
   - Plugins would add a layer the engine does not need yet.
   - Bevy's pipelined rendering adds about a frame of latency.

**The full-screen modes** (measured on ref-1, the research note):

1. **Borderless, the driver's default.** NVIDIA copies it through GDI without vsync:
   9.32 (10.47) ms from the frame's start to the screen.
2. **Borderless, stating "disallowed"** (chosen as the default). NVIDIA flips it
   independently through DXGI: 7.90 (7.97) ms. Equal with vsync, and on the Radeon.
3. **Exclusive, application controlled** (offered). NVIDIA grants it with vsync only,
   and holding it changed nothing measured; the Radeon refuses it across adapters.
4. **Godot's 2-pixel oversize.** It keeps drivers from treating the window as full
   screen; stating "disallowed" does that directly.
5. **Display mode changes** (winit's exclusive). Not taken: the frame has a render size
   of its own (ADR 0072), and a mode change is a disruptive transition.

## Decision

A crate `engine_app`, built in four steps, each its own pull request.

### Step 1 (done 2026-10-15): window modes and exclusive full screen

- **`engine_platform::window::WindowMode`:**
  - `Windowed`: a framed window on its monitor (`WindowDesc::monitor`, the primary one by
    default), the whole window within the monitor's work area. A client area too large is
    scaled down with its aspect ratio kept, and the window is centered.
  - `FullScreen`: a popup window covering its monitor.
  - `WindowSystem::set_mode` switches at run time, as Raymond Chen describes: the frame's
    style off and the monitor covered, then the frame on and the placement restored.
  - A full-screen window keeps covering its monitor across DPI and resolution changes.
- **`engine_platform::display`:**
  - `monitors()`, `primary_monitor()`, `window_monitor(hwnd)`, `monitor_info(handle)`;
  - each `Monitor` with its rectangle, work area and DPI.
- **`engine_rhi::FullScreenExclusive`** in `SwapchainDesc`:
  - `Default` (nothing stated), `Allowed`, `Disallowed`, `Exclusive { monitor }`;
  - with the extension: `VK_EXT_full_screen_exclusive` and
    `VK_KHR_get_surface_capabilities2`, enabled where both exist
    (`Device::full_screen_exclusive_enabled`). Without it, every choice is the default.
- **An exclusive swapchain acquires the monitor at every (re)creation.**
  - Refused, it is destroyed and made again from nothing as `Disallowed`
    (`Swapchain::exclusive`, `Swapchain::exclusive_refused`).
  - Made as the refused one's successor instead, NVIDIA kept the GDI copy: 9.35 against
    7.90 ms.
  - Lost later: `VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT`, which
    `RhiError::is_out_of_date` counts with `VK_ERROR_OUT_OF_DATE_KHR`. The frame loop
    recreates the swapchain and tries again.
- **`SwapchainDesc` has a `Default`:** the surface's size, FIFO, sRGB, SDR.
- **`flight`** runs borderless and "disallowed" by default, at the monitor's size
  (1920 × 1080 on ref-1, as before).
  - `--window exclusive|windowed` and `--fse default|allowed|disallowed` select the
    other modes.
  - Its report says which mode it got, and why the monitor was refused.

### Step 2 (done 2026-10-15): the crate owns the player's wiring

As planned, with these differences (as built):
- **No `start` hook.** The game sets its world up between `Player::new` and the first
  frame, through `Player::setup` (the device, the pools, the skinning, the world, what the
  pack held).
- **More hooks:**
  - `event` (each window event);
  - `animate` (after the runtime, with the frame's interpolated transforms: skinned
    poses);
  - the UI as canvases the player keeps (`Player::ui_mut`), drawn on every presented
    frame.
- **Lights are still given by the game** (`Player::lights_mut`, `sun_mut`), until step 3.
- **Crash reports stay the game's `main`'s.** Their monitor process runs the same
  executable, which must ask first thing whether it is the monitor.
- **Reflex's swapchain** (`VK_NV_low_latency2`) only when latency control is on.
- **Input is the game's,** from the window's events. The player hands them over at the
  frame's start, before latency control's wait (corrected 2026-10-15: this said after it).
  Events that arrive during the wait reach the game in the next frame; reading them again
  after the wait is a follow-up.

Measured (ref-1, release, `flight` on Sponza's pack, 1,800 frames, alternated with the
previous binary):

- **Latency control on by default** (PresentMon, from the frame's start to its scan-out,
  median):
  - RTX 4060 Ti (Reflex): 7.90 → 5.66 ms;
  - Radeon iGPU across adapters (Anti-Lag 2): 69.3 → 52.3 ms;
  - both at the same frame rate.
- **What the player costs, latency control off.**
  - **RTX, CPU frame without the slot's wait:** 732 µs before, 742 µs on the player, 760
    µs with the process set-up.
  - MMCSS "Games" is the set-up's cost: 10–17 µs on a quiet machine.
  - **With 8 busy processes on 16 threads:** 926, 928 and 928 µs. MMCSS gains nothing
    measurable there either.
  - **24 busy processes:** single runs are too noisy to tell.
  - **Radeon, CPU frame:** 1,363 → 1,276 µs. The frame interval was the same, bound by
    the GPU.
  - MMCSS stays, as CLAUDE.md §5 sets it, until a heavier load study says otherwise.
- **No allocation in a steady-state frame** on either GPU: `the_players_steady_state_frames_do_not_allocate`.
  - The device's warm-up thread does allocate, for seconds after start-up, as it compiles
    shading variants. It is not the frame's work, so the test runs without it.

- **What it owns**, in one `Player`:
  1. **The process** (CLAUDE.md §5): power throttling off, the frame thread on P-cores
     with MMCSS "Games", the crash handler, logging.
  2. **The window and its swapchain:**
     - the mode and monitor from the settings, borderless and "disallowed" by default;
     - resizes, out-of-date and lost swapchains, HDR from the display (ADR 0038);
     - the frame generation presenters (ADR 0091).
  3. **The device and the latency provider:**
     - the adapter by preference (`ENGINE_ADAPTER`);
     - Reflex, Anti-Lag 2 or the limiter (ADR 0023), on by default, with the frame cap
       and the frames in flight from the settings.
  4. **The pipeline cache** (ADR 0055) in `%LOCALAPPDATA%\<game>\`, saved after start-up
     and at exit; the warm-up thread (ADR 0079).
  5. **The scene and the renderer:**
     - the pack's streaming (ADR 0076);
     - the GPU scene, the pools and the material table;
     - the frame renderer with `RenderDefaults::for_device` applied whole;
     - skinning, the environment, the UI layer.
  6. **The runtime** (ADR 0064): the world, the fixed step and extraction.
- **The game's hooks:** a trait, called by the frame loop:
  - `start` once the device, window and scene are up;
  - `tick` at the fixed rate;
  - `present` every rendered frame, after the ticks (the camera from the latest input);
  - `ui` for the frame's 2D and 3D canvases.
- **The frame loop**, in `flight`'s order:
  1. window events;
  2. the latency provider's wait;
  3. the frame slot and the swapchain image;
  4. input sampled;
  5. the runtime's ticks, presentation and interpolation;
  6. animation, streaming, the frame renderer, the UI;
  7. submit and present, with the latency markers.
  - `Player::frame` runs one frame and returns its report, for the benches and tests;
    `Player::run` loops until the window closes.
- **`flight` moves onto it first,** with its numbers compared before and after.

### Step 3 (done 2026-10-15): ECS components for lights and skinned meshes

As built:

- **`engine_runtime::{PointLight, SpotLight, Sun}`.**
  - Each is on an entity with a `GlobalTransform`.
  - A spot light and the sun shine down their entity's −Z axis.
  - `LightExtract` (in the runtime, after every tick's propagation) mirrors them into
    the renderer's light list. Like `RenderExtract`, it visits only the chunks where a
    light or its transform changed, and takes removals from the removal logs.
  - **A light keeps its slot while it lives:** the renderer knows a light's cached shadow
    by its index (ADR 0071).
    - A removed light leaves a dark slot (no range, no shadow), and the next new light
      takes it.
    - A light changing kind keeps its slot.
  - The first `Sun` found lights the world. Without one, the player draws no sunlight.
  - Lights follow ticks, not interpolated between them.
- **`engine_runtime::SkinnedMesh`** names an entity's skinned instance.
  - The game's animation sets its skinning matrices
    (`Skinning::set_palette`, split from `set_pose` with `set_transform`).
  - The player calls `Runtime::place_skinned` after `Game::animate`: each skinned
    instance gets its entity's scene instance and that instance's transform this frame,
    bit for bit. `Skinning::update_bounds` follows.
- **`Player::lights_mut` and `sun_mut` are gone.** `flight` spawns Sponza's seven lights
  and the sun as entities, and its characters carry `SkinnedMesh`.
- **Reflection probes and the GI volume move to step 4.** The player turns those features
  on there, and their probes become components with them.

Measured: `flight` on the RTX, alternated with the step 2 binary, latency control off:
- CPU frame 761 / 768 → 771 / 770 µs (noise);
- extraction 24.6–25.0 → 24.8–25.4 µs with the seven lights' sync;
- 0 allocations in a steady-state frame on both GPUs, the shadowed light and the sun
  extracted every tick.

### Step 4: features per view; the benches and the test bed on the crate

**Done first (2026-10-15): the features per view.**
- `ViewFeatures` (`engine_render::forward`, re-exported by `engine_frame`):
  - ambient occlusion, contact and local shadows, subsurface scattering, reflections,
    decals, global illumination, the depth prepass;
  - in `ForwardView::features` and `ViewSettings::features`, `ALL` by default.
- **A feature runs in a view when the renderer has it on and the view wants it.** The
  forward renderer already decided per view (a probe's capture draws without most of
  them); each decision now also asks the view:
  - the passes declared;
  - the shading variants;
  - the local shadow views planned;
  - subsurface materials' own pipeline states;
  - the cached cascades' caster settings.
- **Test:** `a_views_features_change_only_that_view`, both GPUs, under validation.
  - Ambient occlusion, contact shadows and the prepass change 369–371 of 6,144 pixels.
  - A view wanting none of them gives the same pixels as a renderer with them off.
  - A view wanting them all is unchanged by another view without them.

**Done second (2026-10-15): reflections and global illumination, their probes as
components.**

- **`engine_runtime::ReflectionProbe`:** a probe for the box around its entity (half
  extents, intensity, blend distance, manual refresh). The box turns and scales with the
  entity.
  - `ProbeExtract` mirrors the entities into `ReflectionProbes` as `LightExtract` mirrors
    lights: only the chunks where a probe or its transform changed, removals from the
    world's removal log.
  - A probe the set has no room for waits until another goes.
  - `ProbeSink` lets its unit tests mirror into a list.
- **`engine_runtime::GiVolume`:** the box a volume of GI probes covers, probes about
  `spacing` apart, at most `max_probes`. `GiExtract` finds the first; turned, the volume
  covers the world-aligned box around it.
- **The player:**
  - Settings: `reflection_probes` (`ProbeSettings`, on), `screen_space_reflections`
    (off: unlike probes they cost every frame) and `global_illumination` (the device
    class's `GiSettings`, on GPUs with ray queries).
  - The probe set and the renderer's reflections are made with the first
    `ReflectionProbe` entity.
  - The GI volume, the scene its rays trace and the renderer's GI are made with the
    world's `GiVolume`. The ray scene builds the meshes' coarsest levels (resident from
    the start in streamed packs) within `RaySettings::for_device`'s budget.
  - A changed grid makes the volume anew, once the GPU has finished the frames using the
    old one; without a volume, neither it nor its ray scene exist.
  - Probes capture and the volume's rays trace once the pack's textures hold their
    tails: an unloaded texture would read as white. When streaming first settles, the
    probes are captured again.
  - Screen-space reflections go into the view's base settings.
  - `FrameReport::probes` is the mirroring's time.
- **Probes in a moving world:** ADR 0073's addendum. The waiting probe captured longest
  ago goes first, the scene's changes capture a probe at most every 30 frames, and the
  render graph keeps the placements and objects of recent frame shapes.

Measured on ref-1 (`flight`, release, Sponza's pack, 2,000 movers and 64 characters, a
1797 × 1011 window, latency control off; RTX 1,800 frames, Radeon 900):

- **Without probes, volume or SSR the player costs what it did.** Main and the branch
  alternated, CPU frame median: RTX 844–863 µs against 845–853 µs; Radeon 1,236 against
  1,243–1,254 µs. GPU frames were the same: RTX 1,782–1,786 µs, Radeon 21.3 ms. The
  mirroring takes 0.2–0.4 µs a frame (0.5–1.1 µs with them).
- **What they cost**, GPU frame median (CPU frame median):

  | | RTX 4060 Ti | Radeon iGPU |
  |---|---:|---:|
  | none | 1,782 µs (851) | 21.3 ms (1,243) |
  | four probes along the courtyard | 1,914 µs (913) | 24.7 ms (1,257) |
  | the GI volume, 31 × 14 × 20 probes | 2,037 µs (941) | 23.5 ms (1,234) |
  | screen-space reflections, half resolution | 2,147 µs (948) | 26.9 ms (1,267) |
  | all three | 2,501 µs (1,075) | 32.6 ms (1,312) |

- **The volume converges in frame 14,** after the pack's tails arrived: 0.86–0.99 s
  after `main` on the RTX, 1.6–3.3 s on the Radeon. Its ray scene: 168 bottom levels of
  84,145 triangles (2.4 MiB on the RTX, 5.1 MiB on the Radeon); the volume 12.8 MiB.
- **Tests,** both GPUs, under validation:
  - `probes_and_the_gi_volume_follow_their_entities`: nothing made before the
    entities; the probe captured, visible, moved with its entity; the volume converged
    over 11 × 5 × 11 probes and made anew when resized; both gone with their entities.
  - `frames_with_probes_and_gi_do_not_allocate`: 0 allocations in 812–1,186
    steady-state frames, with screen-space reflections, the volume's rays, and the probe
    captured again every 30 frames for the spinning cubes inside it.
  - `probes_follow_their_entities_and_wait_for_room` and
    `the_volume_covers_its_entitys_box`: unit tests of the mirroring.

**Done third (2026-10-15): the tools on the crate, as far as they measure a game.**

The plan moved `scene_bench`, `stream_bench`, `tools/stress` and the golden test bed onto
the crate. Read again with the crate built, the four are of two kinds:

- **Programs that run a game:** the stress scene, `flight`, the golden shots of a game's
  frame. They belong on the player, so that what they measure is what games run.
- **Benches of one subsystem:**
  - `scene_bench` drives the forward renderer alone, pass by pass; its numbers anchor
    ADRs 0039 to 0098, and its `--pipeline-stats` is the register check's tool.
  - `stream_bench` drives streaming under controlled draws, budgets and feedback
    settings.
  - Most of the test bed's shots turn one renderer feature on in isolation: draw lists,
    MSAA variants, procedural scenes, Brixelizer GI, readbacks of the GI.

  On the player they would either lose that control or make the player's API a list of
  test knobs. **They stay below the player.** The player's own golden shot and perf
  scene keep the game's path covered; `flight` measures a game's frame end to end.

As built:

- **The offscreen player** (`Window::Offscreen { width, height }`): no window, surface,
  swapchain or latency control (nor frame generation).
  - Each frame is drawn into an 8-bit sRGB image, kept by a pass that is never culled.
  - `Player::capture` runs a frame that copies the image into a readback buffer and
    returns its pixels once the GPU has finished.
  - `PlayerSettings::validation` turns synchronization validation on.
  - For tests, benchmarks and renders without a screen.
- **What games needed for it, too:**
  - `Setup::environment` (the sky, atmosphere, fog and panorama);
  - `Player::view_settings_mut` (bloom, the tone curve, grading, color vision, depth of
    field, occlusion culling, cached shadows, the clear color);
  - `PlayerSettings::contact_shadows`;
  - `Player::set_global_illumination` (the volume's settings at run time, or off);
  - the probes captured again once the volume converges;
  - `Player::scene_ready` and `scene_settled` (a loading screen's end).
- **The golden bed's `Shot::player`:** a shot renders through an offscreen player.
  - The scene is cooked as the bed's and written to a pack the player streams.
  - The sun, lights, probes and GI volume are entities.
  - Capture waits until streaming has been quiet for 70 frames, the probes are captured
    and the volume has converged; then the camera's cut starts the history as the bed's
    does.
  - Fields the path does not take fail the shot.
  - **`sponza_by_day_on_the_player`** renders `sponza_day` this way and holds it to the
    bed's own golden image:
    - mean FLIP 0.015 on the RTX and 0.016 on the Radeon (99.9th percentile 0.08 and
      0.09), within a mean of 0.03. The frame histories differ: TAAU's and SSR's phases,
      and the GI volume's updates since it converged.
    - The player against itself: 0.002.
    - A lost feature moves it far more: without GI or the probes the mean is 0.29–0.63;
      without SSR or contact shadows the 99.9th percentile is 0.39–0.80.
  - **Perf gate scene `sponza_day_player`** (1920 × 1080), baselines on both GPUs. Its
    five pipelines that the other scenes do not make were added to the register
    baselines (191 → 196).
- **`tools/stress` is a game on the player:**
  - its simulation is the game's tick (one 240 Hz tick a frame);
  - its cubes are entities, the frame is the device class's, uncapped with latency
    control off.
  - The draw-list mode and `--gpu-scene` are gone.
  - Measured against the old `--gpu-scene` frame:

    | | RTX 4060 Ti | Radeon iGPU |
    |---|---:|---:|
    | extraction | 134 → 75–83 µs | 251 → 99 µs |
    | CPU, pools to presenting | 439 → 321–363 µs | 701 → 509 µs |
    | GPU | 292 µs → 1,136 µs (499 FPS) | 8.1 → 15.0 ms |

    The GPU now draws a game's frame, not just the forward pass and the tone map.

## Consequences

- **A game is a few hooks.** The latency provider, the process set-up and the device
  class's defaults run in every game, not just the editor.
- **`flight` has the presentation games get.** Its frames now flip independently on
  NVIDIA instead of being copied through GDI. Its history before 2026-10-15 is a window
  larger than the screen.
- **Exclusive full screen is offered but never the default.** It is revisited when a
  driver or display shows a measured gain: Granite reports HDR on AMD's Windows driver.
- **Steps 2 to 4** each change many programs. Each lands alone, with before and after
  numbers.
- **Not done in step 1:**
  - the taskbar's full-screen marking (`ITaskbarList2::MarkFullscreenWindow`);
  - variable refresh;
  - a monitor driven by the iGPU: ref-1's second display was inactive;
  - input-to-photon with real input.
