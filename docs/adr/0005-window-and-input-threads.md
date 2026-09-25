# 0005. Window thread and raw input thread

- Status: Accepted
- Date: 2026-09-25
- Research: [docs/research/windows-and-input.md](../research/windows-and-input.md)

## Context

The engine needs windows (for Vulkan surfaces, the editor's multiple windows and HiDPI),
UI input and low-latency gameplay input (CLAUDE.md §7: raw input on a dedicated thread with
timestamps). While a window is dragged or resized, Win32 runs a modal loop, and a game loop
that is driven by the message pump stalls (Godot works around this with `WM_TIMER`
iterations at about 10 ms).

## Options

1. **Pump messages on the main thread** and run the frame loop there, as most engines do.
   It is simple, but the frame loop stalls or stutters during modal loops, and input pickup
   depends on frame timing.
2. **Window thread plus raw input thread** (chosen). Windows live on a thread that only pumps
   messages, and raw input is read on a separate time-critical thread. Both hand events to the
   engine through lock-free rings.
3. **The window thread also reads raw input.** This saves a thread, but input pickup then
   waits behind window messages and modal loops.

## Decision

- `engine_platform::window::WindowSystem`:
  - windows run on the `engine-window` thread;
  - commands go through a message-only control window (never thread messages, because
    those are lost in modal loops);
  - events arrive as `WindowEvent { window, time, kind }` through an `engine_core::spsc`
    ring. Draining does not allocate, and overflow is counted.
- **Events:** created, close requested, destroyed, resized (physical pixels), scale
  changed, focus, minimized/restored, cursor moved/left, mouse buttons, wheel, keys (for
  UI and shortcuts), text (`WM_CHAR` with surrogate pairs combined).
- **Windows are not destroyed by the close button.** The engine decides and then calls
  `destroy_window`, which lets the editor ask "save changes?".
- **DPI:** the process is Per-Monitor-V2 aware; sizes are requested in logical pixels, and
  `WM_DPICHANGED` applies the suggested rect.
- `engine_platform::raw_input::RawInput`:
  - an `engine-raw-input` thread at `TIME_CRITICAL` priority with a message-only window;
  - `MsgWaitForMultipleObjects(QS_RAWINPUT)` plus a stop event, and `GetRawInputBuffer`
    batches;
  - timestamps are interpolated across a batch;
  - `RIDEV_INPUTSINK` only when background input is requested, and never `RIDEV_NOLEGACY`.
- **New shared primitive** `engine_core::spsc`: a bounded SPSC ring for `Copy` values,
  loom-checked. A mutation test (a relaxed publish) is detected as a causality violation.

## Measurements

- Unit tests run on Windows CI: window lifecycle, keyboard and text (including surrogate
  pairs), titles, several windows, raw input record parsing, and raw input thread start/stop.
- `examples/window.rs` shows the main loop ticking at 240 Hz while the window is dragged, and
  prints the worst input age at pickup. It needs a real desktop, so it is run by hand on
  ref-1.

## Consequences

- The renderer creates surfaces from `WindowHandle { hwnd, hinstance }` on its own thread, and
  swapchain resizes follow `Resized` events.
- Only one `RawInput` per process. Nothing else may register mouse or keyboard raw input.
- Later: cursor capture and relative mode (`ClipCursor`), cursor shapes, IME composition
  events, drag and drop, device names, gamepads (separate ADR).
