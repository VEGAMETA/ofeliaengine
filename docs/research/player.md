# The player: what a game's executable owns, and how it shows its frames

Checked 2026-10-15. Sources at the end. No code was copied. Godot, Wicked Engine and Granite
are MIT, Bevy is MIT/Apache-2.0, winit is Apache-2.0: they were read for their structure and
their choices. The window code is written from Microsoft's documentation and Raymond Chen's
article. Unreal and Unity were studied as products, through their documentation.

## Why

The second review (`core-audit-2.md` §3.7) found no crate that owns "a game". These
programs each wire the engine by hand:

- the golden test bed, `game_loop`, `flight`, `scene_bench`, `stream_bench`, `tools/stress`;
- the frame renderer's tests and the editor.

The consequences it found:

- `RenderDefaults::ambient_occlusion` is applied by nothing;
- the latency providers (Reflex, Anti-Lag 2, the limiter) and the CLAUDE.md §5 process
  setup run in the editor only;
- lights, skinned meshes, probes and the environment are passed by hand;
- some features are renderer-wide switches, so two views cannot differ.

The owner asked for this crate with three window modes, measured for latency with
PresentMon (2026-10-10):

- a window placed on the monitor, not larger than its work area;
- borderless full screen;
- exclusive full screen (`VK_EXT_full_screen_exclusive`).

The owner noted then that `flight`'s 1920 × 1080 window is larger than the 1080p screen
and composed by the desktop window manager.

## How Windows 11 shows a frame

- **Composition and flip.** The desktop window manager (DWM) composes every window into
  its own image, one refresh at a time.
  - A swapchain in the **flip model** shares its images with DWM: no copy.
  - The older **blt model** copies each frame into a surface DWM owns.
  - Microsoft (2018, page updated 2026-04-15): "Flip model presents go as far as making
    windowed mode effectively equivalent or better when compared to the classic
    'fullscreen exclusive' mode."
- **DirectFlip and independent flip.** When the swapchain's images can go to the screen
  as they are, DWM steps aside and the frames are flipped to the display directly.
  - The window's client area covers the monitor at its size (**DirectFlip**), or within the
    panel fitter's scaling, or any region through a hardware overlay plane
    (**multi-plane overlay**, MPO).
  - Once DirectFlip is taken and nothing else changes, the frames go out "independently,
    with the same efficiency as fullscreen exclusive" (**independent flip**).
  - Something drawn over the window sends it back to composition, unless an overlay plane
    keeps it independent.
  - **PresentMon** reports which path each present took.
- **Full screen exclusive is emulated.** Since Windows 10, "Fullscreen Optimizations"
  run a DXGI full-screen exclusive game as a borderless window covering its monitor, the
  game unaware (Microsoft, 2019-12-17): "almost all users who use Fullscreen Optimizations
  have equal performance to Full Screen Exclusive". Windows 11's "Optimizations for
  windowed games" moves DirectX 10 and 11 blt-model games to the flip model; Vulkan is not
  concerned.
- **Vulkan presents through the driver.** Windows has no Vulkan presentation of its own:
  each driver implements it.
  - NVIDIA's control panel has a "Vulkan/OpenGL present method": Auto (the default),
    Prefer native, or Prefer layered on DXGI swapchain.
  - NVIDIA describes the layered one as able to "add minor memory and performance
    overhead".
  - Users report that the native path shows as "Composed: Copy with GPU GDI" in
    PresentMon, and the layered one as independent flip (Khronos forum, 2025).
  - Which one Auto takes is not documented; it is measured below.
- **`VK_EXT_full_screen_exclusive`** (revision 4 on both ref-1 drivers; the Khronos
  sample calls it a solution "on Windows prior to the 11 version"). A swapchain states one
  of four choices (the specification's words):
  - **default**: "the implementation should determine the appropriate full-screen method
    by whatever means it deems appropriate";
  - **allowed**: "may use full-screen exclusive mechanisms when available. Such mechanisms
    may result in better performance and/or the availability of different presentation
    capabilities, but may require a more disruptive transition";
  - **disallowed**: "should avoid using full-screen mechanisms which rely on disruptive
    transitions";
  - **application controlled**: the application calls `vkAcquireFullScreenExclusiveModeEXT`
    (`VK_ERROR_INITIALIZATION_FAILED` when refused) and `vkReleaseFullScreenExclusiveModeEXT`.
    The mode is kept until it is released, the swapchain is destroyed, or a call returns
    `VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT` "because of platform-specific changes".
    The application "can attempt to acquire exclusive full-screen access again".
  - Both ref-1 drivers report `fullScreenExclusiveSupported` for a window's surface.
- **PresentMon** (Intel, MIT; 2.6.0 of 2026-09-21) records presents through Windows' event
  tracing, with:
  - each present's path: Hardware: Legacy Flip (exclusive), Hardware: Independent Flip,
    Hardware Composed: Independent Flip (an overlay plane), Composed: Flip,
    Composed: Copy with GPU GDI or CPU GDI;
  - the frame's times: `FrameTime`, `GPULatency`, `GPUTime`;
  - `DisplayLatency`: from the frame's start on the CPU to its scan-out;
  - `DisplayedTime`;
  - input to photon: `AllInputToPhotonLatency`, `ClickToPhotonLatency`.

## How engines do it

### Godot 4.7 (MIT)

- **Two full-screen modes** (`DisplayServer`, Windows: `display_server_windows.cpp` at
  `b62bb64`).
  - **`WINDOW_MODE_FULLSCREEN`**, "with full multi-window support":
    - a popup window 2 pixels larger than the monitor, towards a side with no
      neighbouring monitor;
    - the extra pixels clipped by a window region (`SetWindowRgn`);
    - it never covers the monitor exactly, so it stays composed and other windows can
      show over it without a switch.
  - **`WINDOW_MODE_EXCLUSIVE_FULLSCREEN`**: a popup covering the monitor exactly. It has
    "less overhead, but only one window can be open on a given screen at a time (opening
    a child window or application switching will trigger a full screen transition)".
  - Neither changes the display mode, and Godot's Vulkan driver does not use
    `VK_EXT_full_screen_exclusive`. Exclusive is left to the driver's and DWM's
    heuristics for a window that covers its monitor.
- **Windowed** is a framed window. Entering full screen saves its rectangle, and
  leaving restores it.
- **The main loop:** `Main::iteration` runs the due physics ticks, then one process
  step, then the rendering server's frame.

### Bevy 0.20 (MIT / Apache-2.0)

- **`App`** holds the main world and its schedules, sub-apps (the render world),
  plugins and a **runner**: the winit event loop calls `App::update` each frame.
- **`PipelinedRenderingPlugin`**: frame N renders on another thread while frame N + 1
  simulates. "Extract" copies what rendering needs from the main world in between.
  Throughput gains, at about a frame of latency.
- **`WindowMode`**:
  - `Windowed`;
  - `BorderlessFullscreen(MonitorSelection)`;
  - `Fullscreen(MonitorSelection, VideoModeSelection)`, "true"/"legacy"/"exclusive",
    which falls back to borderless when the video mode cannot be resolved.
- **Through winit on Windows** (`winit-win32/src/window.rs`):
  - exclusive changes the display mode (`ChangeDisplaySettingsExW` with
    `CDS_FULLSCREEN`) under a borderless window covering the monitor;
  - borderless covers the monitor without a mode change;
  - both mark the window full screen for the taskbar (`ITaskbarList2::MarkFullscreenWindow`,
    after Chromium) so that the taskbar goes behind it.

### Wicked Engine (MIT, `24e4e57`)

- **`wi::Application::Run`**, each frame:
  1. input;
  2. fixed updates from an accumulator (with "frameskip");
  3. `Update(dt)`;
  4. `Render`;
  5. `Compose` into the swapchain.
- The game is a stack of `RenderPath` objects with the same hooks.
- **Full screen** is Raymond Chen's borderless window: the overlapped style removed, the
  monitor's rectangle covered, the placement restored on the way back. The Vulkan device
  does not use `VK_EXT_full_screen_exclusive`.

### Granite (MIT, `wsi.cpp` at `18517ff`)

- **Uses `VK_EXT_full_screen_exclusive` explicitly.**
  - **disallowed** by default;
  - **allowed**, promoted to **application controlled** when the surface supports it, in
    its low-latency mode or by an environment variable;
  - **forced exclusive** for HDR on AMD's Windows driver, "or HDR will not work
    properly";
  - **never** on Intel's Windows driver ("broken").
- **After creation** it acquires the mode. On failure it logs "Using borderless
  windowed". It treats the mode lost like an out-of-date swapchain and recreates.

### Unreal Engine 5 and Unity 6.6 (products)

- **Unreal's `EWindowMode`:** `Fullscreen` (exclusive), `WindowedFullscreen` (borderless,
  covering the screen) and `Windowed`. The engine loop (`FEngineLoop::PreInit`, `Init`,
  `Tick`) owns the device, the viewport and the game's tick.
- **Unity's `FullScreenMode`:** `ExclusiveFullScreen` ("Windows platforms only"),
  `FullScreenWindow` ("covering the whole screen"), `MaximizedWindow` and `Windowed`.
  - Unity's flip-model option for Direct3D 11 "reduces input latency by one frame" in a
    window or borderless.
  - Its exclusive mode keeps the blt model, as flip-model swapchains "don't support frame
    latency waitable objects when running in exclusive full-screen mode".

## Measured on ref-1 (2026-10-15)

The setup:

- `flight` on Sponza's pack (no characters), the device class's defaults, two frames in
  flight.
- PresentMon 2.6.0, 6 s per run.
- ref-1's one active monitor: 1920 × 1080 at 239.76 Hz, driven by the RTX, its taskbar
  30 pixels tall.
- NVIDIA's present method at its default (Auto).
- "Display latency" is PresentMon's `DisplayLatency`, from the frame's start on the CPU
  to its scan-out: median (99th percentile).

**RTX 4060 Ti (driver 616.92).**

| window | the swapchain's choice | without vsync (immediate): path, presents/s, latency, ms | vsync (FIFO): path, latency, ms |
|---|---|---|---|
| a window with a 1920 × 1080 client, larger than the screen (`flight` before) | default | copied through GDI (native); 603/s, of which 24/s were displayed; 9.38 (14.79) | independent flip, overlay plane (DXGI); 12.27 (12.37) |
| a window fit into the work area: 1797 × 1011 | disallowed | independent flip (DXGI), every other present on an overlay plane; 480/s, 240/s displayed; **7.92 (7.97)** | independent flip, overlay plane; 12.23 (12.35) |
| borderless over the monitor | disallowed | independent flip (DXGI); 480/s; **7.90 (7.97)** | independent flip, overlay plane; 12.26 (12.36) |
| borderless | default | copied through GDI (native); 594/s; 9.32 (10.47) | independent flip, overlay plane; 12.26 (12.37) |
| borderless | allowed | copied through GDI (native); 587/s; 9.37 (12.03) | independent flip, overlay plane; 12.26 (12.36) |
| borderless | application controlled | refused (`VK_ERROR_INITIALIZATION_FAILED`, also 300 frames later), made again as disallowed: as disallowed, 7.90 (7.97) | held; independent flip, overlay plane; 12.25 (12.36) |

- **NVIDIA's Auto chooses the path from what the swapchain states.**
  - Without vsync, "disallowed" goes through DXGI and flips independently.
  - The driver's default and "allowed" are copied through GDI: 1.4 ms later on screen,
    and their 99th percentile is worse (10.5–12.0 against 8.0 ms).
  - Vsync goes through DXGI whatever is stated.
- **NVIDIA's DXGI path does not tear** (`AllowsTearing` 0). Without vsync, frames beyond
  two a refresh wait for an image: 480 presents/s against 594, the GPU idle in between.
- **`flight`'s old window** was copied through GDI, and only 24 of its 603 frames a second
  reached the screen. A window that fits flips independently, part of the time on an
  overlay plane: the multi-plane overlay keeps a window independent.
- **Held, the monitor changes nothing measured.** NVIDIA grants it with vsync only. It
  refused immediate swapchains every time.
- **A refused swapchain's successor keeps the GDI copy.** Made as the successor of the
  refused one (its old swapchain), the disallowed swapchain was copied through GDI: 9.35
  (11.89) ms. Made from nothing, it flips: 7.90 (7.97).

**Radeon iGPU (driver 26.8.1), presenting across adapters to the RTX's monitor.**

| window | the swapchain's choice | without vsync: frames/s, latency, ms | vsync: latency, ms |
|---|---|---|---|
| a window larger than the screen (before) | default | 46.1; 68.2 (75.2) | 67.7 (75.8) |
| a window fit into the work area: 1797 × 1011 | disallowed | 50.9; 62.5 (71.9) | 63.7 (70.4) |
| borderless | disallowed | 45.9; 70.3 (77.3) | 70.0 (76.9) |
| borderless | application controlled | refused, then as disallowed: 46.1; 70.3 (76.8) | 69.3 (76.8) |

- **Every present is a DXGI independent flip with tearing.** The hybrid path copies the
  frame to the RTX and presents it there, whatever the window or the choice.
- **The monitor is refused across adapters.**
- **The latency is about three frames.** Each frame waits 43–48 ms between its start on
  the CPU and its start on the GPU (`GPULatency`): two frames in flight, and `flight` runs
  no latency control. A latency provider, as the player will run, bounds that queue
  (ADR 0023: Anti-Lag 2 on AMD).

**Not measured:**
- a monitor driven by the iGPU: ref-1's second display was not active;
- input to photon with real input (`flight` takes none) or with hardware (no LDAT);
- variable refresh.

## What we take

- **Borderless full screen by default, its swapchain stating "disallowed".**
  - The lowest latency measured on NVIDIA: 7.9 ms, against 9.3 ms when the driver chooses.
  - The same with vsync, and the same on the Radeon.
  - No switch on Alt+Tab, and other windows can show over the game.
  - Godot's 2-pixel oversize does the same through the window's shape, for drivers that
    take a window covering its monitor; stating it is direct.
- **A window fit into the monitor's work area**, centered, its client area scaled down
  with its aspect ratio kept. A window larger than its monitor was copied through GDI on
  NVIDIA and showed one frame in 25.
- **Exclusive full screen as a choice, not the default.**
  - The swapchain asks for the monitor (application controlled) at every creation.
  - Refused, it is made again from nothing as "disallowed". As a successor, NVIDIA kept
    the GDI copy.
  - Lost later (`VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT`), it counts as out of date:
    the frame loop recreates it, as Granite does.
  - On ref-1 it changed nothing measured. Kept for drivers and displays where it does
    (Granite: HDR on AMD's driver).
- **Full screen covers the monitor at its current mode.** The display mode is never
  changed (winit's exclusive does): the frame is rendered at a size of its own and
  upscaled (ADR 0072), and a mode change is a disruptive transition.
- **One thread per frame by default, with the job system inside it** (ADR 0064's order).
  Bevy's pipelined rendering adds about a frame of latency; it can come later as a setting
  for games bound by the CPU.
- **The player owns the wiring** that the review found duplicated:
  - the process set-up of CLAUDE.md §5;
  - the window, the device and the swapchain;
  - the latency provider, the pipeline cache and the frame loop;
  - the scene's streaming;
  - the frame renderer with `RenderDefaults` applied whole.
  The game supplies hooks, as Wicked Engine's render paths and Unreal's game tick do:
  start, tick, present, UI.
- **Not taken:**
  - the NVIDIA present method: it is the player's (a driver setting), and Auto was
    measured;
  - `ITaskbarList2::MarkFullscreenWindow` (winit, Chromium): to add if the taskbar is ever
    seen over the game;
  - display mode lists.

## Sources

- [Microsoft: For best performance, use DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/for-best-performance--use-dxgi-flip-model) (2018, updated 2026-04-15)
- [Microsoft DirectX blog: Demystifying Full Screen Optimizations](https://devblogs.microsoft.com/directx/demystifying-full-screen-optimizations/) (Hannah Fisher, 2019-12-17)
- [Raymond Chen: How do I switch a window between normal and fullscreen?](https://devblogs.microsoft.com/oldnewthing/20100412-00/?p=14353) (2010-04-12)
- [Khronos: VkFullScreenExclusiveEXT](https://docs.vulkan.org/refpages/latest/refpages/source/VkFullScreenExclusiveEXT.html), [vkAcquireFullScreenExclusiveModeEXT](https://docs.vulkan.org/refpages/latest/refpages/source/vkAcquireFullScreenExclusiveModeEXT.html)
- [Khronos Vulkan-Samples: Full Screen Exclusive](https://docs.vulkan.org/samples/latest/samples/extensions/full_screen_exclusive/README.html)
- [Khronos forum: Vulkan present mode, Preferred Native vs. Layered on DXGI Swapchain](https://community.khronos.org/t/vulkan-present-mode-preferred-native-vs-layered-on-dxgi-swapchain/112055)
- [PresentMon 2.6.0](https://github.com/GameTechDev/PresentMon/releases/tag/v2.6.0) and its [console documentation](https://github.com/GameTechDev/PresentMon/blob/main/README-ConsoleApplication.md)
- [Godot: display_server_windows.cpp](https://github.com/godotengine/godot/blob/b62bb647be33810f0733c738a80bc2580779af28/platform/windows/display_server_windows.cpp), [DisplayServer.xml](https://github.com/godotengine/godot/blob/master/doc/classes/DisplayServer.xml)
- [Bevy 0.20: WindowMode](https://docs.rs/bevy/latest/bevy/window/enum.WindowMode.html), [PipelinedRenderingPlugin](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html), [App](https://docs.rs/bevy/latest/bevy/app/struct.App.html)
- [winit: winit-win32/src/window.rs](https://github.com/rust-windowing/winit/blob/master/winit-win32/src/window.rs)
- [Wicked Engine: wiApplication.cpp, wiPlatform.h](https://github.com/turanszkij/WickedEngine/tree/24e4e57f3a7823105a2302939b85c0499c6d0095/WickedEngine)
- [Granite: vulkan/wsi.cpp](https://github.com/Themaister/Granite/blob/18517ff2c7df2f754e130fcf7917766e398ec9cd/vulkan/wsi.cpp)
- [Unreal Engine: EWindowMode](https://docs.unrealengine.com/4.27/API/Runtime/ApplicationCore/GenericPlatform/EWindowMode__Type/index.html)
- [Unity 6.6: FullScreenMode](https://docs.unity3d.com/ScriptReference/FullScreenMode.html), [PlayerSettings.useFlipModelSwapchain](https://docs.unity3d.com/2023.1/Documentation/ScriptReference/PlayerSettings-useFlipModelSwapchain.html)
