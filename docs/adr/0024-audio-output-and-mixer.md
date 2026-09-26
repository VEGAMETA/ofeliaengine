# 0024. Audio output and mixer: WASAPI low latency, lock-free commands, compiled bus schedule

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/audio.md](../research/audio.md)
- Related: [ADR 0004](0004-platform-bindings-and-timing.md) (bindings, MMCSS, thread placement)

## Context

CLAUDE.md §11 asks for:
- WASAPI directly, with `IAudioClient3` small shared-mode periods;
- a device buffer of at most 5 ms;
- an audio thread on MMCSS "Pro Audio" on a P-core, with no allocations, locks, system
  calls, logging or I/O in the render path;
- a bus graph compiled into a linear schedule; voices with priorities, virtualization and
  per-category limits;
- built-in DSP in Rust.

VST3 hosting, streaming decode and spatial audio come later and must fit this design.
In particular, effects need a latency report for delay compensation.

## Options

### Output

1. **cpal** (Apache-2.0).
   - Pros: portable and maintained.
   - Cons: never uses `IAudioClient3`, so the period is the device default (~10 ms); a
     callback closure with no control over thread placement; brings its own `windows` crate
     dependency tree.
2. **miniaudio** (C, public domain).
   - Pros: has the `IAudioClient3` path.
   - Cons: a C build step and a second mixer (its node graph) that we would not use; its
     graph takes spinlocks on attach and detach.
3. **Our own WASAPI layer** (chosen).
   - About 1,000 lines on `windows-sys`: seven hand-declared COM vtables, one
     notification client, one thread.
   - Full control of the period, thread, MMCSS, device following and priming.

### COM bindings

1. **`windows` crate:** generated and safe-ish COM wrappers, but a heavy compile and several
   new crates (`windows-core`, `windows-implement`, `windows-interface`, ...).
2. **Hand-declared vtables over `windows-sys`** (chosen).
   - Layouts come from the SDK headers; unused slots are same-size placeholders.
   - A small `Com<V>` owner releases on drop.
   - No new dependencies, and every call has a `// SAFETY:` comment.
   - The risk (a wrong slot) shows up immediately on ref-1, where the example exercises
     every method.

### Game ↔ audio communication

1. **A mutex around the mixer (Godot).** Simple, but the audio thread can block on the game
   thread (priority inversion).
2. **A lock-free graph with spinlocks for topology changes (miniaudio).** The render is
   lock-free, but detaching can stall the game thread for a whole processing pass.
3. **One SPSC command ring, one SPSC return ring, and a game-side mirror of all state**
   (chosen).
   - The game side allocates voice handles and bus, send and effect slots itself, so no
     call ever waits for an answer.
   - Objects the audio thread lets go of (finished voices' clips, old schedules, removed
     effects) travel back and are freed on the game thread.

### Graph execution

1. **Pull-based node graph (miniaudio):** recursion and per-node bookkeeping on the audio
   thread.
2. **Buses in an array where sends only go to lower indices (Godot):** fast, but it makes
   the user order buses.
3. **Compile any DAG on the game thread into a topological schedule** (chosen).
   - The schedule covers parents, sends and sidechain edges, with ties broken by index so
     it is deterministic.
   - It is sent as one message; the audio thread walks an array.
   - Bus and effect state lives in slot tables outside the schedule, so rerouting never
     resets a reverb tail or a compressor envelope.

## Measurements

**Mixer cost per 256-frame block** (5.33 ms of audio at 48 kHz):
- `cargo bench -p engine_audio --bench mixer`, ref-1 (Ryzen 7 7700), commit `be571d3`;
- the dev container (Xeon @ 2.8 GHz, shared) was 2–3× slower on every row.

| Case | µs per block | Share of real time |
|---|---:|---:|
| Master only | 0.2 | 0.00 % |
| 128 mono voices at the output rate (copy) | 6.9 | 0.13 % |
| 128 mono voices, pitch 1.37 (linear interpolation) | 26.8 | 0.50 % |
| 128 stereo 44.1 kHz voices (resampled) | 47.0 | 0.88 % |
| 512 mono voices, pitch 1.37 | 106.6 | 2.00 % |
| 1024 voices, 64 mixed + 960 virtual | 18.2 | 0.34 % |
| Game graph: 128 voices, 9 buses, 26 effects, 8 sends, sidechain, limiter | 62.7 | 1.18 % |

**Effects per 256-frame stereo block** (ref-1):

| Effect | µs | ns per frame |
|---|---:|---:|
| Biquad | 1.30 | 5.1 |
| Compressor | 3.10 | 12.1 |
| Limiter | 1.20 | 4.7 |
| Delay | 1.10 | 4.3 |

**Output device on ref-1** ("Speakers (2- USB Audio CODEC)"; `tone --mute`, 4 s per
configuration, 61 voices, 5 effects):
- **Periods.** The WASAPI layer worked on the first run: `IAudioClient3` accepted the
  stream. This USB audio device's engine periods are default = fundamental = min = max =
  480 frames, so no shared-mode period below 10 ms exists on it. The 5 ms request becomes
  10 ms.
- **Buffer and wake-ups.** The device buffer is 1,056 frames (22 ms).
  - Wake intervals p50 / p99 / max: 10.10 / 10.80 / 10.71–16.53 ms.
  - Render per wake: 45 / 70–75 / 77 µs.
  - 0 empty wakes, 0 late wakes, 0 reopens in every configuration.
- **Queue depth.** Filling the whole buffer every wake left 576 frames (12 ms) queued at
  wake-up, so each written sample waited about 22 ms before the Windows engine.
  - The engine takes exactly one period per event. So the output now keeps one period
    plus a margin queued (`OutputConfig::queue_margin_ms`, default 2 ms): 12 ms on this
    device instead of 22 ms.
  - Confirmed on ref-1 (commit `f237261`): in every mode with the margin there were 0
    empty and 0 late wakes, with 2.0 ms still queued at each wake-up (whole-buffer mode:
    12 ms). Output latency before the Windows engine is 12 ms instead of 22 ms, at the
    same wake intervals (p99 10.7 ms) and render cost (p50 70 µs with 61 voices).
- **Shorter latency on this device** needs exclusive mode (optional in §11), or a
  device/driver with small shared-mode buffers (the inbox HDAudio driver goes down to
  128 frames).

**Tests:**
- **Zero allocations:** 1,000 frames of plays, stops, parameter changes, sidechain,
  virtualization and varying render sizes, with 0 allocations and 0 frees
  (`tests/zero_alloc.rs`).
- **Render-size independence:** output is bit-identical for render calls of 256, 128 and
  480 frames and for a mix of 1, 7, 1000 and 13 frames, with a biquad, a delay and a
  limiter in the graph. The sound does not depend on the device's period.

## Decision

### `engine_audio`: the mixer, platform-independent and tested on Linux

- **Game side (`Audio`):**
  - `play` / `play_with` return a generational `VoiceHandle` immediately;
  - voice gain, pan and pitch with ramps; `stop` with a fade;
  - buses with parent, gain, balance, mute and voice limits;
  - pre- and post-fader sends; effects appended per bus, with parameters by index;
  - sidechains;
  - clock, peak meters, voice counts.
  - Graph changes are validated (cycles rejected, graph unchanged) and compiled on the
    game thread.
- **Audio side (`Renderer::render`):**
  - applies queued commands;
  - chooses real voices by priority, then audibility, within the global limit
    (`max_real_voices`, default 128) and per-bus limits;
  - renders in sub-blocks of at most 256 frames: voices into buses (32.32 fixed-point
    position, straight copy at unit rate, linear interpolation otherwise), then buses in
    schedule order (effects, pre-fader sends, fader, meters, post-fader sends, parent), then
    the master into the output.
  - It never allocates or frees. Returns that do not fit in the ring wait in a
    pre-reserved list, and as a last resort they are leaked and counted, never freed.
- **Rules that keep the sound clean:**
  - every gain change ramps over at least 64 frames;
  - voices going virtual fade out over one block and come back with a fade-in;
  - output samples are limited to ±4 and NaN becomes silence, so a broken effect cannot
    blast the speakers.
- **Timing:**
  - `start_at` on the mixer clock is sample-accurate;
  - a start time already past skips into the clip so it stays in time.
- **Built-in effects:**
  - biquad (RBJ cookbook, all eight types);
  - compressor (Giannoulis–Massberg–Reiss, soft knee, sidechain);
  - lookahead limiter (sliding minimum + release smoother + moving average; the output
    never exceeds the ceiling);
  - delay (fractional, gliding time).
  - `Effect::latency` exists for delay compensation from day one.
- **Clips:** planar `f32` behind an `Arc`; WAV decoding (PCM 8/16/24/32, float 32/64,
  extensible).

### `engine_audio::output`: WASAPI (Windows)

- **Thread:** a dedicated thread named `engine audio`, on P-cores, MMCSS "Pro Audio", COM
  MTA.
- **Stream:**
  - shared mode, event-driven, stream category `GameEffects`, in the device mix format;
  - `IAudioClient3`: the requested period (default 5 ms) rounded to the engine's
    fundamental period within [min, max]; `0` asks for the minimum;
  - fallback to `IAudioClient::Initialize` (the default ~10 ms) on refusal, with the
    reason in `OutputInfo::note`.
  - one period plus a margin (default 2 ms) is kept queued, not the whole device buffer:
    the engine consumes a period per event, and the rest of the buffer would only add
    latency (22 → 12 ms on ref-1). `queue_margin_ms: None` restores the whole-buffer
    mode.
- **Buffer:**
  - primed with real audio before `Start`;
  - each event renders up to the queue target (one period plus the margin);
  - float buffers are rendered in place, and 16/24/32-bit integer formats are converted
    from a scratch buffer.
- **Device changes:** an `IMMNotificationClient` signals an event; the thread reopens only
  if the default device actually changed or the stream reports
  `AUDCLNT_E_DEVICE_INVALIDATED`.
- **No device:** without a device, the thread renders on a timer at the last rate
  (voices still end on time) and retries every second.
- **Stats:**
  - intervals between events and render time (histograms);
  - empty and late wake-ups;
  - padding at wake-up;
  - reopens.

## Consequences

- **The game thread never blocks on audio,** and the audio thread never blocks on the game.
  A game that stops calling `Audio::update` only delays freeing; the command ring spills
  into a game-side queue.
- **A single producer:** `Audio` is used from one thread (the main thread). Sounds
  started from jobs go through ECS commands. An MPSC front end can be added if profiling
  shows the need.
- **Bus ids are reused after removal without a generation;** a stale `BusId` may address
  a newer bus. Send and effect ids carry generations.
- **The mix is stereo.** Surround output gets front left and right only; spatial audio and
  5.1/7.1 panning come with the spatial audio ADR.
- **Deferred:**
  - resampling quality beyond linear (polyphase sinc) for pitched music;
  - SIMD kernels (AVX2 dispatch) for the interpolating path;
  - a faster log/exp approximation for the compressor.
  - All wait for ref-1 profiles: the mixer is already below 4 % of real time for the
    game graph on the dev container.
- **Latency:** the output latency is the device buffer plus the Windows engine
  (≈1.3 ms). The ref-1 run reports the actual buffer and period per mode, and the §5
  budget (≤ 5 ms buffer) is checked there.
