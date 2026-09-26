# Audio output and mixer

Research for `engine_audio` (CLAUDE.md §11): WASAPI output, the audio thread, how the game
talks to it, and the mixer (buses, sends, voices, effects). VST3 hosting, streaming decode
and spatial audio get their own notes later. Decision: [ADR 0024](../adr/0024-audio-output-and-mixer.md).

Checked on 2026-09-26.

## Sources

| Source | Version / commit | License | What we read |
|---|---|---|---|
| miniaudio | v0.11.25, `9634bedb` | Public domain / MIT-0 | WASAPI backend (IAudioClient3 path, fallback), node graph, resource manager notes |
| cpal | `79275c23` | Apache-2.0 | `host/wasapi/{device,stream}.rs`: stream setup, run loop, device-change handling |
| Godot | `46173009` | MIT | `drivers/wasapi/audio_driver_wasapi.cpp`, `servers/audio/audio_server.cpp` |
| Microsoft, "Low Latency Audio" | windows-driver-docs, checked 2026-09-26 | CC-BY-4.0 docs | Windows 10+ audio path, IAudioClient3, driver buffer sizes |
| Ross Bencina, "Real-time audio programming 101: time waits for nothing" | 2011 | article | Rules for the audio thread |
| FMOD Studio, Wwise | public documentation | proprietary products | Studied as products only: buses, VCAs, DSP clock, virtual voices |

No code was copied. miniaudio and cpal are permissive, but the backend we write is small
and shaped by our own mixer; the vtable layouts come from the Windows SDK headers (the
same order miniaudio and Godot declare).

## The Windows audio path (Microsoft docs)

Render path: the application's buffer → the audio engine (Windows 10+: 1.3 ms of processing
for all applications) → the driver's buffer → hardware.

- Before Windows 10, the shared-mode buffer was always about 10 ms.
- Since Windows 10 the driver declares which buffer sizes it supports. The inbox HDAudio
  driver supports 128 to 480 samples (2.67 to 10 ms at 48 kHz).
- An application gets small buffers **only through `IAudioClient3`**
  (`GetSharedModeEnginePeriod`, `InitializeSharedAudioStream`). Otherwise it gets the default
  10 ms.
- While one application streams with a small period, the whole endpoint switches to it; the
  audio engine goes back to 10 ms when it stops. Asking for 2.67 ms therefore costs power for
  every application on the endpoint. That is acceptable for a game, but the period should be
  a setting.
- `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` / `AUDCLNT_E_ENGINE_FORMAT_LOCKED`: another
  application already runs the engine at a different period or format. We must fall back,
  not fail.
- Microsoft recommends MMCSS ("Audio" / "Pro Audio") for the thread that feeds WASAPI,
  either through the real-time work queue API or by registering the thread ourselves.

## How each implementation works

### miniaudio (WASAPI backend)

- Activates `IAudioClient3` when the user asks for low latency (`noAutoConvertSRC` and a
  small period). It rounds the requested period to a multiple of the **fundamental period**
  from `GetSharedModeEnginePeriod`, clamps it to [min, max], and calls
  `InitializeSharedAudioStream`.
- `InitializeSharedAudioStream` rejects `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` and
  `SRC_DEFAULT_QUALITY`, so this path only works at the engine's own mix rate and format.
  On any failure it falls back to `IAudioClient::Initialize`.
- Event-driven (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, `SetEventHandle`), with its own
  worker thread.
- The node graph's `read_pcm_frames` is lock-free. Attaching and detaching nodes take
  spinlocks, and detaching waits until the audio thread is done with the node, so the
  game thread can stall for up to one processing pass. The resource manager's job queue is
  also spinlock-based.

### cpal

- Does **not** use `IAudioClient3`. It always calls `IAudioClient::Initialize` with
  `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` and an event callback. The callback period is the
  device's default period, typically 10 ms, whatever buffer size the user asked for.
- Run loop: `WaitForMultipleObjectsEx` over the render event and a command event;
  `GetCurrentPadding` → `GetBuffer(buffer − padding)` → user callback → `ReleaseBuffer`.
- Primes the first buffer with real audio before `Start`, so playback does not begin with
  a period of silence.
- Default-device changes arrive via an `IMMNotificationClient` that `SetEvent`s a handle
  the run loop waits on. The stream reports `DeviceChanged` and the user rebuilds it; cpal
  does not follow the default device by itself.
- Priority: `audio_thread_priority` (MMCSS), falling back to `THREAD_PRIORITY_TIME_CRITICAL`.
- The callback is a user closure; cpal has no mixer.

### Godot

- **Driver:** tries `IAudioClient3` for output with `AudioCategory_GameEffects`, rounds
  the target latency to the fundamental period as miniaudio does, and falls back to
  `IAudioClient::Initialize` (with `RATEADJUST` when the project mix rate differs from the
  device).
- **Driver thread:** calls `audio_server_process` **under a mutex** shared with the main
  thread. That is simple, but the audio thread can block on game code.
- **Mixer:**
  - buses are a linear array, and a bus may only send to a bus with a lower index, so array
    order is already a valid processing order;
  - playbacks sit in a lock-free list;
  - volumes are lerped across one block;
  - a 64-frame lookahead buffer lets a sudden stop fade out instead of clicking;
  - it mixes in stereo `AudioFrame` pairs with a fixed block size.
- **Device changes:** the driver follows the default device by reinitializing on
  `AUDCLNT_E_DEVICE_INVALIDATED` or on a notification.

### FMOD Studio and Wwise (products)

- Mixer model: bus hierarchy plus sends; VCAs (gain groups that are not in the signal
  path); snapshots (sets of mixer parameter overrides, blended); parameters (RTPCs) that
  drive effect parameters.
- **FMOD's DSP clock:** every channel can start or stop at an exact sample on the mixer
  clock (`setDelay`). Rhythm-game and music-sync features rely on this.
- **Virtual voices:** both engines keep far more logical voices than real ones. An
  inaudible or low-priority voice keeps its playback position but is not mixed; it comes
  back when it becomes audible again. Limits apply per event, per bus and globally.
- Both run their mixer on their own thread with a mix block of 512 to 1024 frames by
  default and a command queue from the game thread. Latency is dominated by that block
  (10 to 20 ms), which is exactly what §11 asks us to beat.

### Bencina, "time waits for nothing"

- No locks, no allocation, no system calls with unbounded time, no I/O, no waiting on other
  threads in the callback.
- Communicate via lock-free queues; move deallocation off the audio thread by sending
  objects back to be freed.
- Priority inversion: a mutex held by a low-priority thread can stall the audio thread for
  as long as that thread is not scheduled.

## What we take

| Idea | From | Why |
|---|---|---|
| `IAudioClient3` with period rounded to the fundamental, fallback to `IAudioClient::Initialize` | miniaudio, Godot | The only shared-mode path below 10 ms. The fallback keeps old drivers working. |
| Event-driven thread, `WaitForMultipleObjects` over render + control events | cpal | One wait per period; control (device change, shutdown) wakes it without polling. |
| Prime the buffer before `Start` | cpal | No silent first period. |
| `AudioCategory_GameEffects` | Godot | The correct category for game audio (ducking and policy). |
| Follow the default device automatically | Godot | Plugging in headphones must just work in a game. |
| Buses in a linear order where sends go "forward" | Godot | The schedule is an array walk, with no graph traversal on the audio thread. We compile any DAG into this order instead of making the user order buses. |
| Parameter lerp across the block, fade on stop | Godot | No zipper noise, no clicks. |
| Sample-accurate start/stop on the mixer clock | FMOD | Music sync and rhythm gameplay; cheap to support from day one. |
| Virtual voices with priority and per-bus limits | FMOD, Wwise | Thousands of logical sounds, a bounded mixing cost. |
| Send-back queue for deallocation | Bencina | The audio thread never frees memory. |

## What we do differently, and why

- **No lock anywhere between game and audio.** Godot's mutex and miniaudio's spinlocks on
  attach and detach both let the game thread stall the audio thread (or vice versa). We use
  one SPSC command ring (game → audio) and one return ring (audio → game). The game side
  mirrors every piece of state it needs (voice slots, bus layout), so it never has to ask
  the audio thread and wait.
- **The graph is compiled on the game thread.** Adding a bus, changing routing or adding an
  effect builds a new schedule (topological order, send lists, sidechain edges) off the
  audio thread and sends it as one message. The audio thread swaps a pointer and returns the
  old schedule to be freed. Cycles are rejected at compile time with an error that names
  the buses.
- **Effect and bus state outlives schedules.** Buses and effects live in fixed slot tables
  on the audio side, and a schedule only references them by index. Rerouting does not
  reset a reverb tail or a compressor's envelope.
- **Block size = what the device asks for**, processed in sub-blocks of at most 256 frames.
  The mixer adds no buffering of its own, and bus buffers stay in L2
  (64 buses × 2 channels × 256 frames × 4 B = 128 KiB).
- **Game-side handles are allocated on the game side.** `play()` returns a generational
  handle immediately. The audio thread reports "voice ended" back, and only then is the
  slot reused. No round trip, no waiting.
- **No device, no problem.** Without an endpoint (none attached, or a Windows service
  session), the output thread keeps rendering on a waitable timer at the nominal rate.
  Game logic that waits for "sound ended" behaves the same, and when a device appears it
  switches over.

## Open questions (later notes and ADRs)

- Streaming and decoding (Vorbis/Opus/FLAC): which decoders (lewton, claxon, libopus from
  source), and the streaming thread and IoRing integration.
- Spatial audio: Steam Audio (Apache-2.0, C library) versus our own HRTF/panning;
  surround output layouts (5.1/7.1) beyond stereo.
- Exclusive mode: optional, measure whether it beats a 128-frame shared period enough to
  justify it.
- VST3 hosting (`engine_audio_vst3`): plugin delay compensation needs `Effect::latency`
  from day one, which this design includes.
- Higher-quality resampling (polyphase sinc) for pitch-shifted music; linear is the v1
  default, as in FMOD's default resampler.
