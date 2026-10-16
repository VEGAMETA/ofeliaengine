# 0105. Sound in the player: sources and a listener as components, saved, heard through the world

- Status: Accepted (2026-10-16), with the addendum below (the owner's choices of the same
  day: sound files, two kinds of source).
- Date: 2026-10-16
- Related:
  - [ADR 0024](0024-audio-output-and-mixer.md), [0049](0049-reverb.md),
    [0050](0050-spatial-audio.md), [0051](0051-compressed-audio-and-streaming.md): the
    mixer, the reverb, spatial sound, decoders and streams;
  - [ADR 0099](0099-player-crate.md): the player;
  - [ADR 0101](0101-mesh-colliders.md): the game's physics world;
  - [ADR 0104](0104-save-games.md): saves (sound resumes from the same place, the owner,
    2026-10-16);
  - research note `docs/research/audio-in-player.md`;
  - core audit 3 §4, queue item 4.

## Context

- **The engine mixes, spatializes, decodes and streams sound,** but a game had to wire the
  mixer itself: no sound belonged to an entity.
- **The player played only video sound,** through a mixer and an output of its own.
- **Saves hold everything reflected,** but a voice is a handle in the mixer. A load must
  start each sound again where it was, and the mixer could neither start a clip
  elsewhere than its beginning nor say where a clip was.
- **The owner, 2026-10-16:** reverb from the 3D world and its walls. Of the options
  (zones by hand, automatic from the geometry, or both), both.

## Options

1. **`AudioSource` and `AudioListener` components** (Unity, Bevy), with a system keeping
   the voices in step with them. Chosen.
   - For: reflected, so saved; shown by the inspector.
2. **Calls on the mixer**, the game keeping the handles.
   - Against: saves see nothing; every game writes the bookkeeping.

**Through the world:**
- A) zones by hand;
- B) automatic from the geometry;
- C) both: the owner's choice.

Steam Audio's ray-traced convolution was not chosen: a large vendored library and a second
copy of the level, for what rays through the physics world give at a fraction of the cost.

**Sound files** (the owner's call): loose files beside the game, the pack, or loose files
while developing and the pack in a release (recommended).

## Decision

### The mixer (`engine_audio`)

- **`PlayParams::offset`:** where in the sound to start (frames at its rate). A clip
  starts there (inside it when looping; a one-shot past its end ends at once), and
  `play_stream` seeks there.
- **`Audio::position(voice)`:** where the mixer is in the voice's sound.
  - Clips: the audio thread publishes each active voice's frame after every render call,
    one relaxed store per voice. The game thread writes a slot's start before the voice
    plays, so a read is never another voice's.
  - Streams: the offset plus the frames taken, wrapped at the decoder's length when
    looping.
  - Exact through pitch, Doppler and virtual stretches.
- **`PreparedStream`:** the seek and the first decode made on any thread;
  `Audio::play_prepared` plays it without decoding on the game thread. An exact seek
  decodes from the start of the file: about 1 ms per second of offset.

### The player (`engine_app`)

- **`AudioSource`** (reflected, saved):
  - the sound by its path under the sounds folder, and the bus by name;
  - volume, pitch, `PlayMode` (`Once`, `Loop`, `Despawn`), `playing`;
  - `spatial: Option<Spatial>`: distances, falloff, Doppler, whether walls muffle it;
  - `position` in seconds: the player writes it back; a write by the game is a seek;
  - `restart_on_load`, priority, fade-in.
- **`AudioListener`** on the entity that hears. Without one, the main camera hears.
- **`ReverbZone`:** a box at its entity with a `Room` (decay, size, damping, pre-delay,
  level; presets for open air, a room, a corridor, a hall, a cave).
- **`PlayerSettings::audio`:** on or off, the folder, the buses (music, effects, voice and
  ui by default, effects and voice reverberant), the streaming threshold (10 s), the
  mixer's capacities, the acoustics.
- **One mixer for everything,** the videos' sound included.
  - Its device opens on a thread of its own with the first source or video sound: a game
    without sound opens none.
  - The world's **`Mixer`** resource sets the buses' volumes and mutes (a settings menu),
    gives bus ids for videos, the surfaces' absorption, and why there is no sound.
- **Sound files** load on a thread of their own (`sound-load`):
  - WAV, or a compressed file shorter than the threshold, is decoded whole and kept;
  - a longer one is measured, then opened, sought and decoded ahead for each play
    (`PreparedStream`).
- **The system, every frame after the ticks and the probes, before the videos:**
  - it places the listener and starts the voices of playing sources whose sound is
    ready, at their `position`;
  - it stops the voices of paused, changed, despawned or removed sources;
  - it sends volume and pitch changes, and the placed sources' positions and velocities;
  - it writes `position` back, and at a one-shot's end stops or despawns it;
  - with the game's physics (`Game::acoustics`), it runs the acoustics.
- **Acoustics,** at most `rays_per_frame` rays a frame (64), the static layer by default:
  - **occlusion:** a ray from the listener to each placed sound in turn; every body crossed
    takes 60 % more, followed over 0.1 s;
  - **the room:** 32 probe rays, 8 a frame. Mean free path, absorption (the game's per
    surface, 0.1 by default, open sky 1), Sabine's decay, the openness of the sky above;
    followed over 0.4 s into a reverb bus fed by the reverberant buses;
  - **zones** blend their room over the automatic one by their weight (the highest
    priority wins).
- **A load** stops every voice. The next frame starts each playing source again at its
  saved `position`, or at 0 when it asks to restart. `Game::after_load` runs after this.

## Numbers

ref-1, release (`sound::tests::measure_sound`), the game thread's sound per frame:

| sources | acoustics | update, median (p99) | mixing 16.7 ms of sound |
|---|---|---|---|
| 100 | off | 2 µs (3) | 173 µs |
| 1,000 (128 mixed) | off | 17 µs (23) | 250 µs |
| 1,000 (128 mixed) | 64 rays in a room of 70 bodies | 51 µs (72) | 288 µs |

## Consequences

- A game plays a sound by spawning a component; saves keep it without a line of code.
- Walls and rooms sound right without set-up once the game hands over its physics world;
  zones are there for intent.
- **Limits:**
  - spatial settings and the bus are read when a sound starts;
  - reverb is the listener's room: a sound in another room reverberates in the
    listener's;
  - the level's surfaces have the default absorption until the game sets theirs (no
    acoustic materials in the pack yet).
- **Later:**
  - sound files in the pack (the owner's choice);
  - acoustic materials from the glTF materials at cook time;
  - per-source rooms, early reflections with a direction, paths around corners;
  - the videos' position in saves.

## Addendum (2026-10-16): sound files, and two kinds of source

The owner chose, the same day:
- **sound files:** a folder while developing, a pack in a release (option C);
- **two kinds of source:** `AudioSource` plain, without extras, and `AudioSource3D` for a
  sound in space, the one walls and the room act on (Godot's `AudioStreamPlayer` and
  `AudioStreamPlayer3D`).

### Sound files

- **`ofeliaengine sounds build <folder> -o <pack>`** (`engine_assets::sounds`):
  - every file under the folder, by its path with `/`, as it is: no re-encoding;
  - an entry of kind `Sound` each, its format, rate, channels and length in its meta
    (Ogg lengths counted once here, since Ogg files do not state them);
  - one `SoundIndex` entry of the paths (pack version 10);
  - compressed audio stored raw, so it streams in place; WAV compressed with zstd, since
    it is decoded whole.
- **`SoundPack`:** a sound's whole bytes, checked against its chunks' hashes, or a
  `SoundRegion`: a `Read + Seek` over its bytes in the pack's file, which the decoders
  stream from.
- **`AudioSettings::files`:**
  - `ByBuild` (the default): the folder in development builds, the pack in release ones,
    each falling back to the other where it is missing;
  - or `Folder` or `Pack` in every build.
- **The folder is kept live:**
  - a changed file is read again once it has been quiet for 50 ms;
  - the sources playing it start again where they were;
  - a file read half-written leaves the previous version playing.
- **Ogg files in the folder:** decoded up to the streaming threshold. One that ends first
  is a clip; one that goes on streams.

### Two kinds of source

- **`AudioSource`:** sound, bus (`music` by default), volume, mode, playing, position,
  restart on load. Nothing else: no distance, no walls, no reverb. A fade is a `Tween` of
  its volume.
- **`AudioSource3D`:**
  - its `source` is an `AudioSource` (through `Deref`: `shot.playing`), on the `effects`
    bus by default;
  - plus pitch, distances, falloff, Doppler, `occlusion`, `reverb` and priority.
- **Routing:** a bus with `reverb` gets a child, `<bus>.world`, the only feed of the
  reverb.
  - A 3D source with `reverb` plays on the child; every other source plays on the bus.
  - The bus's volume covers both, and plain sounds stay dry whatever bus they are on.
- **Tracks:** one table per kind, so an entity may hold both kinds; occlusion rays go to
  3D sources only.
