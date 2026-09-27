# The editor

The editor is the team's Godot layout (`docs/research/ux/editor-layout.md`) in a window:
scene tabs with play controls, the tool bar, the 3D viewport, the file browser under it,
the scene tree and the inspector on the right, the bottom panels.

```sh
cargo run -p engine_editor --release --bin editor
```

At this stage the panels show demo data; binding them to the scene (ECS, reflection, undo,
the command palette) comes next.

## Viewport navigation

| Action | Input |
|---|---|
| Orbit around the pivot | middle drag |
| Pan | Shift + middle drag |
| Zoom toward the pivot | mouse wheel |
| Look around (the pivot follows) | right drag |

The bindings follow Godot (`docs/research/ux/viewport-navigation.md`). Flying with
right button + WASD and focusing the selection come later.

## Panels

- Clicking a row of the scene tree selects it; clicking the selected row opens or closes
  it.
- Tabs, tools, ► and ■ react to clicks; while "playing", a colored edge frames the
  viewport. ‖ and keyboard navigation of the panels come with play mode and the command
  palette.
- The status bar shows the frame rate, the editor's CPU time per frame (UI, painting,
  recording the frame), the GPU time per frame, and the output (SDR, or HDR with its
  white level and peak).

## HDR

While Windows runs the display in HDR, the editor presents in HDR (scRGB): the UI at
Windows' SDR white level, like every other window, and the 3D view with highlights up to
the display's peak. It follows HDR being turned on or off, the SDR brightness slider and
moves to another monitor while running (`docs/dev-guide/hdr.md`, ADR 0038).

## Options

| Option | Effect |
|---|---|
| `--fps N` | Frame-rate cap; `--fps 0` for none. By default the cap is 97% of the monitor's refresh rate, re-read as the window moves between monitors. |
| `--present fifo\|mailbox\|immediate` | Present mode (drivers may override v-sync, which is why there is a cap). |
| `--validate` | Vulkan validation with synchronization validation (always on in debug builds). |
| `--no-throttle` | Keeps Windows from slowing the editor while its window is not in front. By default the editor saves power in the background. |
| `--hdr scrgb\|hdr10\|off` | Output while the display runs in HDR: scRGB (default), HDR10, or stay in SDR. |
| `--frames N` | Renders N frames and exits, printing the cold start time, frame statistics and the average GPU time of every pass. |

`ENGINE_ADAPTER=radeon` (or another name fragment) picks the GPU.

## What it costs (ref-1, release, 1600×900)

| | |
|---|---|
| Cold start to the first frame | 0.39 s (budget 2 s): window and Vulkan device 0.35 s, pipelines (cached) 2 ms, fonts and UI 3 ms |
| CPU per frame | UI 22 µs, painting 25 µs, recording the frame 60 µs |
| Frame rate | capped at 233 FPS on a 240 Hz monitor (Reflex on NVIDIA, Anti-Lag 2 plus the CPU limiter on AMD) |
| GPU per frame (RTX 4060 Ti at the cap, low clocks) | SDR 515 µs, HDR scRGB 585 µs, HDR10 670 µs |
