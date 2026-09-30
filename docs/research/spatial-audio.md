# Spatial audio

Research for sounds placed in the world (CLAUDE.md §11; ADR 0024 left spatial audio for
later). Decision: [ADR 0050](../adr/0050-spatial-audio.md).

Written on 2026-09-30. The sources are specifications and product documentation, cited
from their published descriptions; no code was read or copied.

## Sources

| Source | What we take |
|---|---|
| OpenAL 1.1 specification (Creative, 2005) | Distance models (inverse, linear, exponent; clamped variants), the Doppler formula with speeds clamped below the speed of sound, listener orientation as forward and up |
| FMOD Studio and Wwise documentation (products) | Min/max distance as the designer's controls; occlusion (direct path muffled) vs obstruction; virtual voices by audibility; one listener per player; reverb sends by zone ("snapshots", auxiliary sends) |
| ISO 9613-1 (air absorption) | High frequencies are absorbed more over distance: on the order of 0.1 dB/m at 10 kHz, next to nothing below 1 kHz |
| Steam Audio (Valve, Apache-2.0 since 2024), Resonance Audio (Google, Apache-2.0) | What a full solution adds: HRTF binaural rendering, geometric occlusion and propagation, baked reverb. Candidates for later, not used now |

## The parts

### Distance

- Physical point sources fall off as 1/distance (−6 dB per doubling). Games clamp it: full
  volume up to a minimum distance (the source is not a point), and a maximum where the
  curve stops.
- OpenAL's "inverse clamped" keeps the gain constant beyond the maximum, so a sound never
  quite disappears; its "linear clamped" reaches silence at the maximum. Products expose
  both, plus custom curves.
- Audibility decides which voices are mixed (ADR 0024): distance must feed it, so far
  sounds become virtual by themselves.

### Direction

- On stereo output, the direction's component along the listener's right is the pan
  (constant power for mono sources, as the mixer already does).
- A sound passing through the listener would flip from one side to the other in a block;
  treating the minimum distance as the source's size and narrowing the pan inside it
  removes the jump (products call it "spread" or "focus").
- Front/back and elevation need HRTF filtering (headphones) or more speakers; stereo
  panning cannot tell them apart.

### Doppler

- `f' = f · (c + v_listener) / (c + v_source)`, with `v_listener` the listener's speed
  towards the source and `v_source` the source's speed away from the listener (OpenAL).
- Speeds are clamped below the speed of sound; games scale the effect down (0.3–0.5),
  since cameras move far faster than people do.
- On the mixer this is only a pitch ratio: voices are already resampled.

### Air absorption and occlusion

- Both are mostly a low-pass: air takes the highs over tens of metres; a wall takes more
  highs and some level.
- A one-pole low-pass per voice (6 dB/octave) is the usual cheap form. The recursion is
  latency-bound; unrolled over 4 samples with precomputed powers of the feedback
  coefficient, each step waits for one multiply and add instead of four.
- The game decides occlusion (ray casts against its geometry); the mixer only maps an
  amount 0..1 to gain and cutoff.

### Update rate

- Positions come from the game once per frame (60–240 Hz); the mixer runs every device
  period (~5 ms). Recomputing gain, pan, pitch and cutoff once per render call and
  ramping the gains over it gives smooth movement with one computation per voice per
  period.

### Reverb zones

- Products blend reverbs by where the listener is (FMOD snapshots, Wwise auxiliary sends
  per game object). The simple form: a weight per zone, 1 inside and fading to 0 at a
  margin, applied to the send from the world bus to each zone's reverb bus.

## Measured

ref-1, `cargo bench -p engine_audio --bench mixer`, 256-frame blocks at 48 kHz:

| | µs per block |
|---|---:|
| 128 mono voices, pitch 1.37 (not spatial) | 26.9 |
| 128 spatial voices, moving (Doppler), low-pass as a plain loop | 126.3 |
| the same without the low-pass | 31.0 |
| **128 spatial voices, low-pass four samples per step** | **43.1** |
| 1024 voices, 64 real + 960 virtual (not spatial) | 20.0 |
| 1024 spatial voices, 64 real + 960 virtual | 55.2 |

The spatial update itself is about 35 ns per voice per render call.

## Open questions

- HRTF for headphones (Steam Audio, or our own convolution with a measured HRTF set whose
  license allows it).
- Several listeners (split screen): each voice heard by the nearest listener, as FMOD does.
- Per-voice reverb sends (a voice in the cave sends to the cave's reverb while the
  listener is outside).
- Cones (directional sources), custom distance curves.
