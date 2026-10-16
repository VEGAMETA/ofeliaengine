# 0050. Spatial audio: emitters and a listener, computed on the audio thread per render call

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/spatial-audio.md](../research/spatial-audio.md)
- Related: [ADR 0024](0024-audio-output-and-mixer.md) (mixer, virtual voices),
  [ADR 0049](0049-reverb.md) (reverb), [ADR 0012](0012-math-library.md) (world conventions)

## Context

Games place sounds in the world: gain falls with distance, the pan follows the direction,
moving sounds shift in pitch, far and occluded sounds lose their highs, and each space
has its reverb. ADR 0024 left this for later; the mixer has gain, pan and pitch per voice
and virtualizes voices by audibility.

Requirements:
- no allocation, locks or blocking on the audio thread (ADR 0024);
- smooth movement although the game updates positions only once per frame;
- far sounds must stop costing mix time (virtualization by distance);
- one listener update must move every sound.

## Options

### Where the spatial math runs

1. **On the game thread:** a helper computes gain, pan and pitch per voice each frame and
   sends the existing voice commands.
   - Cons: a command per voice per frame, even when only the listener moved; updates at
     the frame rate; the audio thread would still need a filter per voice.
2. **On the audio thread, once per render call** (chosen).
   - The game sends the listener once per frame and positions only for sounds that
     moved; the mixer recomputes each spatial voice every device period (~5 ms) and
     ramps the gains over it.
   - Audibility for virtualization comes from the same numbers.
3. **Per sample on the audio thread:** smoothest, but the math (a square root, a sine and
   cosine, exponentials) per sample per voice costs far more than it gains over ramps.

### Direction on stereo output

1. **Constant-power pan from the direction's right component** (chosen), narrowed inside
   the source's minimum distance so a sound passing through the listener never jumps
   sides.
2. **HRTF binaural** for headphones: front/back and elevation. Needs an HRTF data set
   with a suitable license or a library (Steam Audio, Apache-2.0); later.

### Low-pass per voice

1. **One-pole, recursion unrolled four samples per step** (chosen): 6 dB/octave, enough
   for air and walls, and cheap.
2. **Biquad (12 dB/octave):** steeper, twice the state and math; not needed yet.

## Numbers

ref-1, `cargo bench -p engine_audio --bench mixer`, 256-frame blocks at 48 kHz (budget
5333 µs):

| | µs per block |
|---|---:|
| 128 mono voices, pitch 1.37 (not spatial) | 26.9 |
| 128 spatial voices, moving, low-pass as a plain loop | 126.3 |
| **128 spatial voices, low-pass unrolled** | **43.1** |
| 1024 spatial voices, 64 real + 960 virtual | 55.2 |
| 1024 voices, 64 real + 960 virtual (not spatial) | 20.0 |

## Decision

- **`spatial::Listener`** (position, forward, up, velocity) set by
  `Audio::set_listener` once per frame. Right = forward × up (right-handed, +Y up; ADR
  0012). Default: origin, facing −Z.
- **`spatial::Emitter`** (position, velocity, `min_distance`, `max_distance`, `Rolloff`,
  Doppler strength, air absorption strength) in `PlayParams::spatial`; moved with
  `Audio::set_voice_position`, occluded with `Audio::set_voice_occlusion` (0..1).
- **Per render call, per spatial voice:**
  - gain: `Rolloff::Inverse` (`min / d`, held beyond the maximum, OpenAL's inverse
    clamped) or `Rolloff::Linear` (silent at the maximum), times occlusion (−15 dB at 1);
  - pan: the direction's right component, scaled by `min(d / min_distance, 1)`, added to
    the voice's own pan;
  - pitch: OpenAL's Doppler formula, speeds clamped to half the speed of sound, times the
    voice's pitch;
  - low-pass cutoff: 20 kHz halved every 50 m of air (strength 1) and 5 octaves lower at
    full occlusion; not filtered at 16 kHz and above;
  - gains ramp over the render call; a fade in progress keeps its length.
- **Virtualization** uses the spatial gain, so sounds beyond a linear rolloff or drowned
  by louder ones stop being mixed.
- **`spatial::ReverbZone`**: a box with a fade margin; its `weight(listener)` drives a
  send's gain to the zone's reverb bus (listener-based zones).

## Consequences

- A spatial voice costs about 0.13 µs more per 256-frame block than a plain resampled
  one; the spatial update is ~35 ns per voice per render call, virtual or not.
- The Doppler shift and the low-pass change once per render call (~5 ms), not per
  sample: inaudible steps at normal speeds.
- Stereo only: front/back and elevation are not rendered.
- Next:
  - HRTF for headphones (Steam Audio or an HRTF set with a clear license);
  - several listeners for split screen;
  - per-voice reverb sends (source-based zones);
  - sound cones and custom rolloff curves.

## Addendum (2026-10-16): the player hears through the world

[ADR 0105](0105-sound-in-the-player.md) uses this in the player:
- occlusion from rays through the game's physics world;
- the listener's room reverb from a ray probe, under `ReverbZone` components that blend
  their room in by this weight.
