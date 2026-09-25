# Windows and raw input on Win32: research

Date: 2026-09-25. Input for [ADR 0005](../adr/0005-window-and-input-threads.md). No code was
copied from the projects below.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [SDL3](https://github.com/libsdl-org/SDL) `src/video/windows/SDL_windowsrawinput.c`, `SDL_windowsevents.c` | commit `fef0d24` (2026-09-24) | Zlib | dedicated raw input thread |
| [Godot](https://github.com/godotengine/godot) `platform/windows/display_server_windows.cpp` | commit `4617300` (2026-09-25) | MIT | modal move/resize loop, raw input on the main thread |
| Microsoft Win32 documentation: `PostThreadMessage`, `RegisterRawInputDevices`, `GetRawInputBuffer`, `WM_DPICHANGED`, DPI awareness | checked 2026-09-25 | — | API contracts |

## The modal loop problem

When the user drags or resizes a window, `DefWindowProc` runs its own modal message loop
until the mouse button is released. The application's `GetMessage` loop, and with it any game
loop driven from it, does not run during that time.

- **Godot** starts a `WM_TIMER` with `USER_TIMER_MINIMUM` on `WM_ENTERSIZEMOVE` and runs a
  main-loop iteration on each `WM_TIMER`. The game keeps running at the timer's granularity
  (about 10–16 ms), and frames stutter while the window moves.
- **Messages posted with `PostThreadMessage` are lost during modal loops.** Microsoft's
  documentation says this explicitly, which rules out thread messages for waking a window
  thread.

**Our choice:** all windows live on a dedicated **window thread** that only pumps messages.
The main loop runs elsewhere at full rate and receives events through a lock-free SPSC ring,
so rendering and presenting never wait for the pump. Commands such as create, set title and
destroy go through a hidden **message-only control window**. Window messages are dispatched
inside modal loops; thread messages are not.

## Raw input

- **SDL3** runs a dedicated thread with a message-only window registered as `hwndTarget`, at
  `THREAD_PRIORITY_TIME_CRITICAL`. The thread waits in
  `MsgWaitForMultipleObjects(..., QS_RAWINPUT)` after `GetQueueStatus(QS_RAWINPUT)` (which marks
  the wake bits as seen), reads in batches with `GetRawInputBuffer` without dispatching `WM_INPUT`,
  and **interpolates timestamps** across a batch between the poll start and the poll end, because
  raw input carries no per-event time. Mouse registration uses flags 0; keyboard `INPUTSINK` is
  optional (background input); `NOLEGACY` is optional.
- **Godot** registers raw input on the main thread and reads it while pumping messages, so input
  pickup is tied to the main loop's timing.
- Registration is **per process and device class**. A second `RegisterRawInputDevices` for the
  same class moves delivery to the new target. `RIDEV_NOLEGACY` suppresses legacy mouse and
  keyboard messages for the whole process, which would break UI input in our windows, so we do
  not use it.

What we take:
- SDL3's thread design and batch timestamp interpolation, with timestamps as `Instant` (QPC).
- Our own ring (`engine_core::spsc`) instead of a locked event queue.
- Window messages for UI (cursor position, text, shortcuts) and raw input for gameplay.

## DPI

- The process is set to Per-Monitor-V2 awareness (`SetProcessDpiAwarenessContext`) before any
  window is created.
- Window sizes are requested in logical pixels, and the client area is sized with
  `AdjustWindowRectExForDpi` for the window's monitor. `WM_DPICHANGED` applies the suggested
  rect and reports the new DPI.

## Open questions

- Behavior with more than one keyboard or mouse: events carry the device handle, and mapping
  handles to device names comes later (`GetRawInputDeviceInfo`).
- Gamepads: XInput vs GameInput is a separate decision (CLAUDE.md §13).
- Input-to-photon measurement needs the renderer. The example already reports the worst input
  age at pickup.
