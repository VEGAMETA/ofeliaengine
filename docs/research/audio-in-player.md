# Sound in the player: sources, the listener, sound files, saves

Research for queue item 4 (core audit 3 §4: "audio in the player"). Checked 2026-10-16.

## What the engine has

- **`engine_audio`** (ADR 0024):
  - a lock-free mixer with buses, sends, effects and virtual voices;
  - WASAPI output;
  - the game thread holds an `Audio`, the audio thread its `Renderer`.
- **Spatial sound** (ADR 0050): `Listener`, `Emitter` with distance rolloff, reverb zones.
- **`engine_audio_decode`** (ADR 0051):
  - Vorbis, Opus and FLAC;
  - a whole file decoded into a `Clip`, or streamed through a ring by a stream thread;
  - exact seeking.
- **The player** plays video sound through an `Audio` and an output of its own, started
  with the first video that has sound.
- **Missing:** sound a game plays as part of its world (a footstep at a place, music)
  without wiring the mixer itself; and sound in saves (the owner, 2026-10-16: sound
  resumes from the same place unless the developer starts it over).

## How engines do it

| Engine | Source | Listener | Buses | Position, seeking |
|---|---|---|---|---|
| Godot 4 | `AudioStreamPlayer`, `AudioStreamPlayer2D`/`3D` nodes: stream, volume (dB), pitch, `playing`, `autoplay`, bus by name, distance and attenuation | the current `Camera3D` or an `AudioListener3D` | an `AudioBusLayout` resource of named buses with effects | `play(from_position)`, `get_playback_position()`, `seek()` |
| Unity 6 | `AudioSource` component: clip, mixer group, volume, pitch, loop, play on awake, spatial blend, min and max distance, rolloff | one active `AudioListener` (the main camera's) | `AudioMixer` assets: groups, effects, snapshots | `time`, `timeSamples` |
| Bevy 0.19 | `AudioPlayer` component with `PlaybackSettings` (mode, volume, speed, paused, spatial) | `SpatialListener` component | none built in | through the source's sink |
| Unreal 5 | `UAudioComponent` on an actor: a sound asset, attenuation settings | the player controller's camera | submixes | `Play(StartTime)` |

What they agree on:
- a source belongs to an object in the world, its position its transform's;
- one listener, by default the camera;
- buses are data, chosen by name;
- the play position can be read and set, which is what a save needs.

## Options

**The source:**
1. **Components on entities** (Unity, Bevy): `AudioSource` and `AudioListener`.
   - For: reflected like the rest, so saved like the rest; the inspector shows them.
   - Against: a system has to keep voices in step with the components.
2. **Calls on the audio engine** (`audio.play(...)` with handles), the game keeping the
   handles.
   - Against: nothing for saves to see; every game writes the bookkeeping itself.

**The sound files** (the owner's call: a format on disk):
- **A)** Loose files beside the game (WAV, Ogg Vorbis, Opus, FLAC), read as they are,
  long ones streamed. Simple, and an edited sound plays at once (hot reload); many files
  in a release.
- **B)** The cook puts the sounds into the pack as they are (no re-encoding). One file,
  a faster start; every change of a sound needs a cook.
- **C)** Both: loose files with hot reload while developing, the pack in a release (as
  saves write text while developing and binary in a release). No encoder is needed:
  files keep their own codec.

## What we take

- **Components** (option 1):
  - `AudioSource`: the sound by its path (a stable key, readable in a text save), the bus
    by name, volume, pitch, looping, playing, spatial distances, the play position
    (saved), restart on load;
  - `AudioListener` (default: the active camera).
- **The player owns one audio engine** and its output, the videos' sound included. A
  system keeps the voices in step with the components every frame:
  - it starts, stops and tunes voices;
  - it moves emitters with their entities;
  - it reads the positions back for saves.
- **Short sounds are decoded into clips once; long ones stream** (by length).
- **Buses come from the player's settings** (music, effects, voice and UI by default).
- **Saves:** a load stops every voice; the system then starts each playing source at its
  saved position, or from the start when it asks to.
- **Sound files:** behind a loader that reads loose files now; the pack follows the
  owner's choice.

## Sound through the world: walls and rooms

The owner, 2026-10-16: reverb that follows the 3D world and its walls. Checked
2026-10-16, from each engine's documentation:

| Engine | Walls between a sound and the listener | The room's reverb |
|---|---|---|
| Unreal 5 | attenuation settings: occlusion by a line trace on a chosen channel, a low-pass and a volume drop, interpolated over a time | Audio Volumes: reverb and submix sends set per volume |
| Unity 6 | none built in (games ray cast and set a low-pass themselves) | `AudioReverbZone`: a sphere with a preset |
| Godot 4 | none built in | `Area3D` reverb bus: amount and uniformity per area |
| Steam Audio (Apache-2.0) | occlusion by rays or by a volume of rays; transmission through walls with their materials | reflections ray traced in real time or baked, rendered by convolution; paths around corners |
| Source (Valve) | | an automatic preset from traces around the listener (`dsp_automatic`) |

**What they agree on:**
- occlusion is a ray from the listener, smoothed over time;
- reverb is listener-based: the room the listener is in decides it;
- hand-placed zones are how designers say what a place sounds like.

**Steam Audio** is the full model, at a cost: convolution reverb per source or per mix, a
scene of triangles for its ray tracer, and a C++ library of its own (with Embree as an
option). Its sources being open still leaves a large vendored dependency and a second copy
of the level's geometry.

**What we take:**
- **Occlusion:** the physics world's rays, as Unreal does. Every body the ray crosses
  takes 60 % more of what is left, and the result is followed over 0.1 s.
- **The room, automatic:** Source's idea, with numbers instead of a preset.
  - 32 rays from the listener (a Fibonacci sphere), 8 a frame.
  - The mean free path is 1.2 × the mean distance to the walls (4V/S is 0.67 L for a
    cube of side L seen from its centre, where the mean distance is about 0.6 L).
  - The walls' absorption, with open sky absorbing everything, gives the decay by
    Sabine's formula: RT60 = 0.161 V / (S·α) ≈ 0.04 · path / α.
  - The size follows the path, the pre-delay the nearest wall, the damping the walls'
    absorption.
  - The share of upward rays that reach no wall takes the level down: open sky does not
    answer.
  - The FDN reverb (ADR 0049) changes its decay, damping and level smoothly. Its size and
    pre-delay change its delay lengths, so they follow only real changes of room.
- **Zones as components:** Unity's and Godot's, a box with a room, over the automatic
  one.
- **Not now:**
  - Steam Audio's convolution and paths;
  - per-source rooms (a sound in another room reverberating there);
  - early reflections with a direction.

No code from GPL projects was read. Godot is MIT; Unity's, Unreal's, Steam Audio's and
Source's behaviour comes from their documentation.
