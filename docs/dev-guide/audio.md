# Audio: sounds, buses, effects, timing

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
use engine_audio::{SendTap, dsp::{Biquad, Compressor, Delay, Limiter}};

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
  - `Delay`.
- **Your own effect:** implement `dsp::Effect`. `process` must not allocate or lock; it
  runs on the audio thread.
- **Parameters are set by index;** each effect lists its indices as constants
  (`Biquad::FREQUENCY`, `Compressor::THRESHOLD_DB`, ...).
- **Pre- and post-fader sends:** a `PreFader` send ignores the bus's own volume. A
  `PostFader` send follows it (the usual choice for reverb and echo).

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
- Streaming (Vorbis, Opus, FLAC) for long music comes later.
