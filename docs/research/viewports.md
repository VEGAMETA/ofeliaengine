# Viewports

Checked on 2026-09-26. Read as code. No code was copied.

## Sources

| Source | Version | License | What was read |
|---|---|---|---|
| Godot 4, `servers/rendering/renderer_viewport.{h,cpp}` | master `46173009dc4a` (2026-09-25) | MIT | viewport state, update modes, ordering, blit to screen, 3D scaling |
| Bevy, `bevy_camera/src/camera.rs`, `bevy_render/src/camera.rs`, `bevy_core_pipeline/src/core_2d` | main `0f38358f573a` | MIT / Apache-2.0 | cameras with render targets, viewport rectangles, ordering, output modes |
| Unity, Unreal (product documentation) | current | proprietary (products, not code) | Camera `targetTexture` and viewport rect; `SceneCapture2D` and render targets |

## How the others do it

### Godot 4

- **What a viewport is:** a render target with its own settings:
  - size and internal size;
  - 3D scaling mode and scale (FSR included);
  - MSAA and screen-space AA;
  - update mode;
  - clear mode;
  - 2D snapping options;
  - canvas layers and cull mask;
  - `measure_render_time` (CPU and GPU timestamps per viewport).
- **Update modes:** `DISABLED`, `ONCE`, `WHEN_VISIBLE` (the default), `WHEN_PARENT_VISIBLE`,
  `ALWAYS`.
  - `WHEN_VISIBLE` for a sub-viewport means its render target was sampled in the previous
    frame (`render_target_was_used`), so there is a frame of latency after it becomes
    visible.
- **Order** (`_sort_active_viewports`): topological, children first, siblings in their
  original order. A parent therefore samples its children's results in the same frame.
- **Display:**
  - root viewports are blitted to their window rectangles (`blit_to_screen_list`);
  - sub-viewports are shown through `ViewportTexture`s like any texture, including in 3D
    materials.
- **Pixel art:** integer scaling is a window stretch setting (`scale_mode = integer`)
  applied when blitting the root viewport, not a viewport property.

### Bevy

- **Cameras, not viewports:**
  - a camera has a `RenderTarget` (a window, an image, or a texture view);
  - an optional `Viewport` rectangle within it (physical position and size, depth range);
  - an `order`, an `is_active` flag, a clear color;
  - an output mode (`Write` with an optional blend state, or `Skip`);
  - `sub_camera_view` for tiling one view over several targets.
- **Rendering:** each camera renders into its own intermediate "main texture" through its
  own render sub-graph; an upscaling node writes it into the target.
- **Composition:** cameras on the same target are drawn in `order`, blending by output
  mode. Split screen is several cameras with viewport rectangles on one window.
- **Render to texture:** a camera whose target is an image; a sprite or material samples
  it. Ordering between cameras is by `order` only.

### Unity and Unreal (as products)

- **Unity:**
  - a Camera renders into the screen or a `RenderTexture` (`targetTexture`);
  - a normalized viewport rect gives split screen;
  - camera depth orders composition;
  - `Pixel Perfect Camera` (URP 2D) gives integer scaling with letterboxing.
- **Unreal:**
  - `SceneCapture2D` renders into a render target, every frame or on demand
    (`bCaptureEveryFrame`, `CaptureScene()`);
  - split screen is a player-count layout of the main view;
  - the editor's viewports are a separate system from the game's.

## What we take, what we don't

1. **A viewport owns a persistent texture, registered in the bindless set.**
   - Showing it anywhere (a 2D sprite, UI, a 3D material) is only its index. Neither Godot
     nor Bevy needs a special node type for that.
   - Persistence lets update modes skip frames: the last contents stay valid.
2. **Update modes: `Always`, `Every { n, offset }`, `OnRequest`, `Disabled`.**
   - `Every` is the "render tick" of CLAUDE.md §6. Offsets spread several such viewports
     over frames.
   - We do not take Godot's `WHEN_VISIBLE` (a frame of latency, and a global "was used"
     flag). Visibility-driven updates can come later through the parent relation.
3. **Order: Godot's topological order** (children first, siblings in creation order), not
   Bevy's global integer order. It is always correct for nesting without the user
   assigning numbers.
4. **Nested viewports are published inside the one render graph.**
   - Other passes sample viewport textures through bindless without declaring them. After
     a viewport's content, a no-op "publish" pass declares a sampled read in vertex,
     fragment and compute stages, so the graph transitions the texture before any later
     pass.
   - This is the same idea as the texture uploads (ADR 0015).
   - Unlike Bevy's per-camera sub-graphs, there is one graph: transient memory aliases
     across viewports and barriers are derived globally.
5. **Composition into the window** is a list of placed viewports:
   - normalized rectangles plus an order: split screen and picture-in-picture;
   - drawn by the tonemapper into sub-rectangles of the backbuffer, so each viewport has
     its own exposure and tone mapping (its own post-processing, CLAUDE.md §6);
   - the first composite clears: that is the letterbox color.
6. **Pixel-perfect is a viewport property** (Godot has it only on the window): any viewport,
   including a split-screen half, can be integer scaled. The largest integer scale that
   fits, centered, nearest filtering.
7. **Graph scopes** for per-viewport cost (Godot's `measure_render_time`, done through the
   graph):
   - a viewport's passes form a scope with its id and name;
   - captures (RenderDoc, Nsight) show them under a debug label;
   - GPU timings carry the scope id, so the viewport system sums them per viewport.
8. **Not taken (yet):**
   - Godot's per-viewport MSAA, AA and 3D scaling: they come with the upscaler work
     (CLAUDE.md §6, "Upscaling"; `ViewportSize::Placement { scale }` is the hook);
   - Bevy's `sub_camera_view`;
   - visibility layers: CLAUDE.md §6 asks for them, but they belong to the scene model
     (ECS, §17 step 4). The content callback receives the viewport id and decides what to
     draw.

## API consequence found while building it

`Renderer2d::add_pass` took `&'g mut Canvas` (it sorted during the call). A mutable borrow
for the frame's lifetime cannot be taken repeatedly in a loop or a closure over
viewports, so content code could not choose among several canvases.

- `Canvas::prepare` is now an explicit step after recording, and `add_pass` takes
  `&'g Canvas`. It returns `Render2dError::NotPrepared` if the canvas changed since.
- Sorting moves to recording time, and can run on the job that recorded the canvas.
