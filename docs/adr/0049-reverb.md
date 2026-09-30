# 0049. Reverb: diffusion into an 8-line feedback delay network

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/reverb.md](../research/reverb.md)
- Related: [ADR 0024](0024-audio-output-and-mixer.md) (mixer, `dsp::Effect`)

## Context

CLAUDE.md §11 lists a reverb among the built-in DSP effects. Until now the example game
graph used a feedback `Delay` as a stand-in, which is an echo, not a room.

What a game needs:
- a decay time in seconds that means what it says;
- a few parameters a sound designer understands (size, damping, pre-delay, early level,
  mix);
- a cost that allows one reverb bus per acoustic space (a handful at once);
- the `Effect` rules: no allocation, locks or blocking in `process`, parameters by index.

## Options

1. **Schroeder / Freeverb** (parallel combs, series all-passes; Freeverb is public domain).
   - Pros: well known, cheap per line.
   - Cons: metallic tail unless many combs (24 lines in stereo); the decay is a feedback
     coefficient, not a time.
2. **Dattorro plate.**
   - Pros: a good-sounding, published topology.
   - Cons: fixed and tuned for 29.8 kHz; one character; the decay is a gain, not a time.
3. **Convolution.**
   - Pros: exact rooms from impulse responses.
   - Cons: far more CPU, impulse-response assets, and changing decay or size means another
     impulse response.
4. **Multi-channel diffusion into a feedback delay network** (chosen; Jot 1991, Luff
   2021).
   - Pros: the decay time is exact by construction (per-line gains from the line lengths,
     orthogonal matrices), dense echoes from the start, one design for small rooms and
     halls, cheap matrices (Hadamard, Householder: adds only).
   - Cons: a little metallic on long tails without line modulation (see Consequences).

## Numbers

ref-1, `cargo bench -p engine_audio --bench mixer` (bench profile), 48 kHz:

| | µs per 256-frame block | ns per frame |
|---|---:|---:|
| `Reverb` (8 lines, decay 2 s, size 0.6), `%` per delay access | 14.4 | 56.2 |
| **`Reverb`, compare-and-wrap** | **10.1** | **39.5** |
| `Delay` (for scale) | 1.1 | 4.3 |
| game graph (128 voices, 26 effects) with a `Delay` send | 68.2 | |
| game graph with a `Reverb` send | 70.5 | |

Decay accuracy (T30 by Schroeder integration of the impulse response): requested 0.5 /
1.5 / 4 s → measured 0.490 / 1.500 / 4.004 s. The tests hold it within 10%.

## Decision

`dsp::Reverb`, a stereo `Effect`:

- **Signal path:** pre-delay → spread over 8 channels (even from the left, odd from the
  right) → 4 diffusion steps (a short delay per channel, random polarity flips, a Hadamard
  mix; each step spans twice the time of the one before, 80 ms in all at full size) → an
  8-line FDN (lines from 40% to 100% of 200 ms at full size, a one-pole low-pass per line,
  a Householder matrix, per-line gains) → even channels summed to the left, odd to the
  right.
- **Parameters:** `DECAY` (RT60, 0.1..30 s), `SIZE` (0..1, scales every length between 30%
  and 100%), `DAMPING` (Hz; 20 kHz and above disables the low-pass), `PRE_DELAY` (≤ 0.5 s),
  `EARLY` (level of the diffused signal in the output), `MIX` (0 dry .. 1 wet).
  `Reverb::new(decay, size)` is fully wet, for a send bus.
- **Deterministic:** the lengths and flips come from a fixed seed, so a reverb of the same
  size sounds the same in every run.
- **Allocation only in `prepare`**, sized for the longest room and pre-delay (about 0.5 MB
  at 48 kHz); parameter changes recompute lengths and gains on the next block.
- **Denormals:** a 1e-20 offset in the feedback write keeps decaying tails out of the
  denormal range.
- **Latency 0:** the dry path is not delayed; the pre-delay is a sound, not latency.

## Consequences

- Sound designers set rooms in seconds; one reverb bus per space costs about 0.2% of a
  core.
- Next, in order of value:
  - slow modulation of the line lengths (fractional reads) against metallic ringing on
    long tails;
  - decay calibrated per frequency band (Jot's absorption filters) instead of a plain
    low-pass;
  - crossfaded size changes (now a change while the tail rings may click);
  - SIMD over the 8 channels;
  - convolution for signature spaces, when a game asks for it;
  - blending reverb sends by listener position between zones belongs to spatial audio.
