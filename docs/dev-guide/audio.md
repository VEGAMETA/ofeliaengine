# Audio: sounds, buses, effects, timing, sounds in the world

`engine_audio` plays sounds through a mixer that runs on its own thread. Your game code
talks to it through `Audio`; every call returns immediately and never waits for the audio
thread. The output goes straight to the Windows audio engine with a buffer of a few
milliseconds.

## 1. Start

```rust
use engine_audio::{Audio, AudioConfig, Clip};
use engine_audio::output::{Output, OutputConfig};

let (mut audio, renderer) = Audio::new(&AudioConfig::default());
let mut output = Output::start(renderer, OutputConfig::default())?; // default device

let jump = Clip::from_wav(&std::fs::read("assets/jump.wav")?)?;
audio.play(&jump)?;

// Once per frame:
audio.update();
```

- **Keep `output` alive:** dropping it stops the audio thread.
- **Call `audio.update()` once per frame.** It reclaims finished sounds and frees their
  memory on your thread; the audio thread never frees anything.
- **Headphones plugged in, or the default device changed in Windows?** The output
  follows it by itself.
- **No device at all?** The mixer keeps running silently, so sounds still end on time, and
  it picks up a device as soon as one appears.
- **`output.info()`** tells you the device, sample rate, period and buffer.

## 2. Playing sounds

```rust
use engine_audio::PlayParams;

let footstep = audio.play_with(&step_clip, &PlayParams {
    bus: sfx,          // which bus (default: master)
    gain: 0.8,         // linear volume
    pan: -0.3,         // -1 left .. 1 right
    pitch: 1.05,       // playback rate ratio
    priority: 64,      // who wins when too many sounds play (default 128)
    ..PlayParams::default()
})?;

audio.set_voice_gain(footstep, 0.2, 0.5);  // fade to 0.2 over 0.5 s
audio.stop(footstep, 0.1);                 // fade out over 0.1 s, then end
if audio.is_playing(footstep) { /* ... */ }
```

- **A handle stays valid until the sound ends.** After that, calls with it are ignored,
  and a new sound never inherits an old handle.
- **Every change ramps** (at least about 1 ms). Volume changes and stops never click.
- **Clips can have any sample rate** (they are resampled). Mono clips pan with constant
  power; stereo clips balance.

## 3. Buses

A bus groups sounds: music, dialogue, footsteps, UI. Every bus feeds a parent, and all of
them end in `BusId::MASTER`.

```rust
use engine_audio::BusId;

let music = audio.add_bus("music", BusId::MASTER)?;
let world = audio.add_bus("world", BusId::MASTER)?;
let footsteps = audio.add_bus("footsteps", world)?;

audio.set_bus_gain(music, 0.5, 1.0);        // over 1 s
audio.set_bus_mute(world, true);
audio.set_bus_voice_limit(footsteps, Some(8));
```

- **A muted or silent bus costs nothing:** its sounds become *virtual*.
- **Virtual sounds keep their place** in time but are not mixed. When they become audible
  again they fade back in where they would have been.
- **Voice limits:** at most `AudioConfig::max_real_voices` sounds (default 128) are
  mixed at once, plus per-bus limits. The most important sounds win: higher `priority`
  first, then the loudest. You can start thousands of sounds.

## 4. Sends and effects

```rust
use engine_audio::{SendTap, dsp::{Biquad, Compressor, Delay, Limiter, Reverb}};

let hall = audio.add_bus("hall", BusId::MASTER)?;
audio.add_effect(hall, Reverb::new(2.2, 0.8))?;                     // 2.2 s to fade 60 dB
audio.add_send(world, hall, 0.25, SendTap::PostFader)?;

let echo = audio.add_bus("echo", BusId::MASTER)?;
audio.add_effect(echo, Delay::new(0.25, 0.4, 1.0))?;                // fully wet echo bus
audio.add_send(footsteps, echo, 0.3, SendTap::PostFader)?;

let muffle = audio.add_effect(world, Biquad::low_pass(20_000.0, 0.7))?;
audio.set_effect_param(muffle, Biquad::FREQUENCY, 800.0);           // underwater

audio.add_effect(BusId::MASTER, Limiter::new(-1.0))?;              // no clipping, ever
```

- **Built-in effects:**
  - `Biquad`: low / high / band pass, notch, peak, shelves, all-pass;
  - `Compressor`: with sidechain;
  - `Limiter`: lookahead, never exceeds its ceiling;
  - `Delay`;
  - `Reverb`: an algorithmic room (see below).
- **Your own effect:** implement `dsp::Effect`. `process` must not allocate or lock; it
  runs on the audio thread.
- **Parameters are set by index;** each effect lists its indices as constants
  (`Biquad::FREQUENCY`, `Compressor::THRESHOLD_DB`, ...).
- **Pre- and post-fader sends:** a `PreFader` send ignores the bus's own volume. A
  `PostFader` send follows it (the usual choice for reverb and echo).

### Reverb

`Reverb::new(decay, size)` makes a fully wet reverb for a send bus: put one bus per
acoustic space (cave, hall, small room) and send to it from the buses that are in that
space.

| Parameter | Meaning | Default |
|---|---|---|
| `Reverb::DECAY` | Seconds for the tail to fade by 60 dB (RT60), 0.1..30. Measured within 2% | from `new` |
| `Reverb::SIZE` | 0..1: how far apart the echoes are (a closet ~0.1, a hall ~0.8) | from `new` |
| `Reverb::DAMPING` | Hz where the tail starts losing highs; lower sounds softer and further away, 20 kHz and above is bright | 8,000 |
| `Reverb::PRE_DELAY` | Seconds before the reverb starts (big spaces: 20–60 ms), up to 0.5 | 0.02 |
| `Reverb::EARLY` | Level of the early reflections, 0..1 | 0.5 |
| `Reverb::MIX` | 0 dry .. 1 wet; on an insert (not a send) use 0.2–0.4 | 1 |

- **Set `SIZE` per room, not continuously:** changing it while the tail rings may click.
  `DECAY`, `DAMPING`, `EARLY` and `MIX` can move freely (for example, damping down when
  the player goes underwater).
- **Cost:** about 10 µs per 256-frame block (0.2% of a core); a handful of reverb buses is
  fine. ADR 0049 has the design and the numbers.
- **The same size sounds the same every run:** the echo pattern is fixed, not random per
  instance.

### Ducking music under dialogue

```rust
let duck = audio.add_effect(music, Compressor::new(-30.0, 6.0))?;
audio.set_sidechain(duck, Some(dialogue))?;
```

The compressor on the music listens to the dialogue bus: when someone speaks, the music
dips.

Loops are rejected. A bus cannot feed itself through parents, sends or sidechains; the
call returns `AudioError::Cycle` and nothing changes.

## 5. Timing sounds to the beat

The mixer has a sample clock. Start sounds at an exact sample, for music, rhythm games or
anything that must line up:

```rust
let clock = audio.clock();
let bar = clock.frames_after(0.5); // half a second from now
audio.play_with(&kick, &PlayParams { start_at: Some(bar), ..PlayParams::default() })?;
audio.play_with(&snare, &PlayParams {
    start_at: Some(bar + u64::from(clock.sample_rate) / 2),
    ..PlayParams::default()
})?;
```

- **Schedule a little ahead** (tens of milliseconds is plenty).
- **A time that has already passed** starts the sound at once, skipped ahead so it stays
  on the beat.
- **The mix does not depend on the device:** it is bit-identical for any device buffer
  size.

## 6. Latency and performance

- **Default device period: 5 ms** (Windows 10+ `IAudioClient3`), rounded to what the
  driver supports.
  - `OutputConfig { period_ms: 0.0, .. }` asks for the driver's minimum (often 128
    samples, 2.7 ms).
  - Drivers without small-buffer support fall back to the standard ~10 ms; `output.info()`
    says which mode is active and why.
- **Small periods wake the CPU more often and cost battery** on laptops. Consider a
  setting for players.
- **Queue depth.** The output keeps one period plus 2 ms queued, not the whole device
  buffer. `OutputConfig::queue_margin_ms` raises the margin, and `None` queues the whole
  buffer, for machines that drop out.
- **Many USB audio devices only offer 10 ms periods** in shared mode (the reference
  machine's does). `output.info().engine_periods` shows what the driver allows.
- **Mixing is cheap:** 128 resampled voices through a full bus graph with 26 effects take
  a few percent of one core (ADR 0024 has the numbers).
- **`output.stats()`** shows the audio thread's timing: wake-up intervals, render time,
  late or empty wake-ups (dropouts).

## 7. Clips

- `Clip::from_wav`: PCM 8/16/24/32-bit, float 32/64-bit, mono or stereo.
- `Clip::from_interleaved`: generated or decoded samples.
- Clips are decoded into memory and shared: playing a clip 100 times does not copy it.
- **Compressed files:** `engine_audio_decode` reads Ogg Opus (`.opus`), Ogg Vorbis
  (`.ogg`) and FLAC (`.flac`), mono or stereo. For short sounds, decode once at load:

```rust
let door = engine_audio_decode::decode(std::fs::read("sounds/door.opus")?)?; // a Clip
```

### Streaming music and long sounds

```rust
let music = engine_audio_decode::open_file("music/theme.opus")?;
let voice = audio.play_stream(music, &PlayParams { bus: music_bus, looping: true, ..PlayParams::default() })?;
```

- **Memory:** a stream keeps `AudioConfig::stream_buffer_ms` (500 ms by default) decoded
  ahead, whatever the file's length. A clip holds all of it: a 5-minute stereo track is
  about 110 MB as a clip.
- **Starts at once:** `play_stream` decodes the first quarter second before returning
  (a few milliseconds on the calling thread). A file shorter than the buffer is decoded
  whole right there and needs nothing more.
- **Everything else is the same** as for clips: buses, gain, pan, pitch, fades, `stop`,
  spatial placement, virtual voices.
- **Looping** starts the file over without a gap (`looping: true`).
- **Which format:** Opus for music and dialogue (smallest for the quality; 96–128 kbit/s
  stereo is plenty), FLAC when it must stay exact, Vorbis for existing assets.
- **Decoding is cheap:** 800–1,800 times faster than real time on the reference machine;
  a stream costs its decoder under 0.2% of one core.
- **If the disk falls behind,** the stream pauses and resumes where it was (no skipped
  audio). `audio.stream_underruns()` counts it; raise `stream_buffer_ms` if it grows.
  `audio.stream_errors()` counts damaged files (such a stream ends).
- **Seeking inside a file** is exact but decodes from the start (about 1 ms per second of
  position), so prefer separate files over jumping around a long one.

## 8. Sounds in the world

Give a sound an `Emitter` and it follows a place in the world: quieter with distance,
panned by direction, higher in pitch as it approaches, muffled when far or behind a wall.

```rust
use engine_audio::{Emitter, Listener, PlayParams, Rolloff};
use engine_core::math::Vec3;

let engine = audio.play_with(&engine_loop, &PlayParams {
    looping: true,
    spatial: Some(Emitter {
        min_distance: 3.0,          // full volume within 3 m (also the sound's size)
        max_distance: 80.0,         // where the rolloff ends
        rolloff: Rolloff::Inverse,  // physical; Linear goes silent at max_distance
        doppler: 0.5,               // half the physical effect (cameras move fast)
        ..Emitter::at(car.position)
    }),
    ..PlayParams::default()
})?;

// Every frame:
audio.set_listener(&Listener {
    position: camera.position,
    forward: camera.forward,
    up: camera.up,
    velocity: camera.velocity,
});
audio.set_voice_position(engine, car.position, car.velocity);
```

- **World conventions:** metres, +Y up, right-handed (as the renderer). The default
  listener stands at the origin facing −Z.
- **The listener is one command per frame** and moves every sound. Update a voice's
  position only when it moves.
- **Smooth by construction:** the mixer recomputes every spatial voice each device period
  (~5 ms) and ramps between; a sound teleporting from left to right glides over 5 ms.
- **Distance curves:**
  - `Inverse`: −6 dB per doubling of the distance after `min_distance`, held from
    `max_distance` on (a 1 m sound is at −40 dB from 100 m);
  - `Linear`: silent at `max_distance`. Use it for sounds that must disappear at a range.
- **Far sounds are free:** audibility includes the distance, so drowned or silent voices
  become virtual (position advances, nothing is mixed).
- **Doppler:** from the velocities you pass (m/s); `doppler: 0.0` disables it (music
  boxes, voices).
- **Air absorption:** highs fade with distance (20 kHz at the source, 5 kHz at 100 m);
  `air_absorption: 0.0` disables it, 2.0 doubles it (fog, rain).
- **Occlusion:** `audio.set_voice_occlusion(voice, 1.0)` behind a wall: 15 dB quieter and
  muffled to ~600 Hz. Take it from a ray cast between the listener and the sound, a few
  times a second; values in between for partial cover.
- **Stereo only for now:** left/right, not front/back or above/below (HRTF for
  headphones comes later).
- **Cost:** a spatial voice costs about 0.13 µs more per 256-frame block than a plain
  one; 128 moving spatial voices take 0.8% of a core (ADR 0050).

### A reverb per space

```rust
use engine_audio::spatial::ReverbZone;

let cave_reverb = audio.add_bus("cave reverb", BusId::MASTER)?;
audio.add_effect(cave_reverb, Reverb::new(3.5, 0.9))?;
let to_cave = audio.add_send(world, cave_reverb, 0.0, SendTap::PostFader)?;
let cave = ReverbZone { center: cave_center, half_extents: Vec3::new(20.0, 8.0, 30.0), fade: 6.0 };

// Every frame: full reverb inside the cave, fading out over 6 m around it.
audio.set_send_gain(to_cave, 0.4 * cave.weight(camera.position), 0.1);
```
