# Reverb

Research for the built-in reverb in `engine_audio` (CLAUDE.md §11: "built-in DSP effects in
Rust: EQ, compressor/limiter, reverb, delay, filters"). Decision:
[ADR 0049](../adr/0049-reverb.md).

Written on 2026-09-30. The sources are papers and talks, cited from their published
descriptions; no code was read or copied.

## Sources

| Source | What we take |
|---|---|
| M. R. Schroeder, "Natural sounding artificial reverberation", JAES 1962 | Parallel combs and series all-passes: the classic structure and its coloration problem |
| M. R. Schroeder, "New method of measuring reverberation time", JASA 1965 | Backward integration of the squared impulse response; our tests measure T30 this way |
| J.-M. Jot, A. Chaigne, "Digital delay networks for designing artificial reverberators", AES 90th Convention, 1991 | Feedback delay networks: a unitary feedback matrix plus per-line gains set from the decay time, independent of the matrix |
| J. Dattorro, "Effect Design, Part 1: Reverberator and Other Filters", JAES 1997 | The plate "tank" (a figure-of-eight of all-passes and delays), input diffusion, damping inside the loop |
| Jezar, Freeverb (2000, public domain) | Studied as a known design only: 8 combs + 4 all-passes per channel |
| G. Luff, "Let's Write a Reverb", ADC 2021 | Multi-channel diffusion (short delays, polarity flips, a Hadamard mix, several steps of growing length) in front of an FDN with a Householder matrix |

## The designs

### Schroeder / Freeverb (parallel combs, series all-passes)

- Each comb's feedback sets its own decay; the combs are independent, so the echo density
  grows only linearly and the tail sounds metallic ("flutter") unless many combs are used.
- Freeverb uses 8 combs and 4 all-passes per channel, 24 delay lines in stereo; the decay
  is a feedback coefficient ("room size"), not a time in seconds.

### Dattorro plate

- Input diffusion by 4 all-passes, then a tank of two cross-fed branches with modulated
  all-passes, delays and a damping low-pass; output taps at fixed positions.
- A good sound, but a fixed topology tuned for 29.8 kHz: lengths and taps are rescaled
  per sample rate, the decay is a single gain (not a time in seconds), and it has one
  character (a plate).

### Feedback delay network (Jot)

- N delay lines whose outputs are mixed by a unitary matrix and fed back. A unitary matrix
  neither gains nor loses energy, so the loop's loss is exactly the per-line gains.
- Gain per line for a decay time T60: `g = 10^(−3 · length / (T60 · fs))`. Every path
  through the network loses 60 dB in T60 whatever it passes through, because each sample
  of delay costs the same number of decibels.
- Echo density grows exponentially with the round trips (every line feeds every line).
- A low-pass in each loop makes high frequencies decay faster (air absorption, soft
  surfaces). Jot calibrates these filters so that the decay time itself is
  frequency-dependent; a plain one-pole is the simple form.

### Diffusion in front (Luff)

- An FDN alone starts sparse: the first echoes are single copies of the input. Several
  steps of (short delays per channel → random polarity flips → a Hadamard mix) turn one
  input sample into a dense cloud of echoes within tens of milliseconds, before the
  network. That cloud doubles as the early reflections.
- Hadamard and Householder matrices cost O(N log N) and O(N) adds, not O(N²) multiplies.

### Convolution

- Exact rooms from measured or rendered impulse responses; partitioned FFT convolution.
- Costs far more per voice bus, needs impulse-response assets, and a decay or size change
  means a different impulse response. A later option for cinematics and signature spaces.

## What we take

- 8 channels: 4 diffusion steps, then an 8-line FDN with a Householder matrix.
- Decay as RT60 in seconds, exact by construction (per-line gains from lengths).
- Size scales every length (30%..100% of the longest room: 80 ms diffusion span, 200 ms
  longest line); the lengths are fixed pseudo-random numbers, so every reverb of the same
  size sounds the same.
- A one-pole low-pass per line for damping, a pre-delay, a level for the early part
  (the diffuser's output), and a dry/wet mix.
- The stereo image: even channels are fed from and summed to the left, odd to the right;
  the mixing matrices spread each side over both.

## Measured

- T30 (Schroeder integration) of the impulse response against the requested decay:
  0.490 s for 0.5 s (size 0.3), 1.500 s for 1.5 s (size 0.6), 4.004 s for 4 s (size 1).
- Cost on ref-1 (bench profile): 10.1 µs per 256-frame stereo block, 39.5 ns per frame,
  0.19% of real time at 48 kHz. Replacing a `%` per delay access (two integer divisions
  per line, 42 accesses per frame) with a compare took it from 14.4 µs.
- Memory: about 0.5 MB at 48 kHz (a 0.5 s pre-delay per side, the diffusers, the lines).

## Open questions

- Modulating the line lengths slightly (as Dattorro and most products do) breaks up the
  remaining metallic ringing on long, sparse tails; it needs fractional reads.
- Frequency-dependent decay calibrated per line (Jot's absorption filters), so that
  `DAMPING` becomes "RT60 at high frequencies" rather than a filter corner.
- Changing `SIZE` while the tail rings clicks (the read positions jump). Crossfading two
  read positions would fix it; rooms usually set size once.
- SIMD: the 8 channels map onto one AVX register (or two SSE registers) per step.
