# 0051. Compressed audio and streaming: lewton, vendored libopus, claxon; an SPSC ring per stream

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/streaming-decode.md](../research/streaming-decode.md)
- Related: [ADR 0024](0024-audio-output-and-mixer.md) (mixer, audio-thread rules)

## Context

`engine_audio` played only WAV clips decoded into memory. Games need compressed files
(Opus for music and dialogue, Vorbis for existing assets, FLAC for lossless) and
streaming, so a 5-minute stereo track takes half a second of samples in memory, not
about 110 MB.

Constraints:
- the audio thread never allocates, frees, locks or blocks (ADR 0024);
- dependencies from the allowed licenses (MPL only via ADR);
- the mixer stays platform independent and quick to build.

## Options

### Decoders

1. **symphonia** (MPL-2.0): one crate for all formats; needs an MPL exception and its
   Opus decoder is incomplete.
2. **Per format** (chosen):
   - Vorbis: **lewton** 0.10 (MIT OR Apache-2.0), complete, pure Rust;
   - Opus: **libopus 1.5.2** vendored (BSD-3-Clause), the reference decoder, built with
     `cc` like Box2D; Ogg pages through **ogg** 0.8 (BSD-3-Clause, the version lewton
     already uses);
   - FLAC: **claxon** 0.4 (Apache-2.0), pure Rust.

### Where the decoders live

1. In `engine_audio`: every user of the mixer would build libopus.
2. **A crate `engine_audio_decode`** (chosen) implementing a `Decoder` trait that
   `engine_audio` defines; the mixer knows nothing about codecs.

### Streaming

1. **Decode on the game thread in `Audio::update`:** no thread, but decoding costs the
   frame, and a slow frame starves the stream.
2. **A stream thread and one single-producer single-consumer ring per stream** (chosen):
   the stream thread decodes ahead; the audio thread reads like a clip.

## Numbers

ref-1 (release):

| file | open µs | decode µs per second of audio | × real time |
|---|---:|---:|---:|
| Vorbis 44.1 kHz stereo | 1,172 | 939 | 1,065 |
| Opus 48 kHz stereo | 2 | 1,279 | 782 |
| FLAC 16-bit | 0 | 541 | 1,850 |
| FLAC 24-bit | 0 | 589 | 1,699 |

Mixer, 256-frame block: 32 stereo streams 12.0 µs, 32 stereo clips 2.0 µs.

## Decision

- **`engine_audio::Decoder`**: sample rate, channels (1 or 2), optional length, `read`
  interleaved `f32`, `seek` exact to the frame. `Clip::from_decoder` decodes a whole file.
- **`engine_audio_decode`**: `open` (sniffs the format), `open_file`, `open_bytes`,
  `decode` (to a `Clip`); `VorbisDecoder`, `OpusDecoder`, `FlacDecoder`.
  - Exact lengths from the granule positions: Opus pre-skip and end trim; Vorbis start
    and end trims (tracked here, not through lewton's convenience reader).
  - Seeking rewinds and decodes to the frame: exact, free for loops, about 1 ms per
    second of position.
  - libopus: float build, SSE/SSE2/SSE4.1 presumed on x86-64 (the x86-64-v2 baseline), no
    runtime CPU detection; AVX2 left out (it would need runtime dispatch).
  - More than two channels are refused.
- **Streams** (`Audio::play_stream(decoder, params)`):
  - a ring of `AudioConfig::stream_buffer_ms` (default 500 ms, rounded up to a power of
    two of frames), allocated on the game thread and returned there when the voice
    ends; samples are relaxed atomics published by release/acquire counters;
  - the first half is decoded before `play_stream` returns, so the voice starts at once;
    a stream that fits entirely never involves the stream thread;
  - the stream thread (`audio-stream`, started with the first stream) refills rings in
    1024-frame chunks, polling every 10 ms while streams play and sleeping otherwise;
    looping streams seek to 0 at the end;
  - the voice reads the ring with the clip resampler's interpolation (pitch, Doppler,
    rate conversion work unchanged) and a copy path at the output rate;
  - when the ring runs dry the voice pauses on its frame and resumes there
    (`Audio::stream_underruns` counts it); decode errors end the stream
    (`Audio::stream_errors`).

## Consequences

- Music and ambience cost half a second of samples each, whatever their length.
- A new C dependency to keep updated (libopus); its tarball checksum is recorded in the
  research note.
- Next:
  - seeking by page with pre-roll for long jumps;
  - contiguous ring reads to bring streams to the cost of clips;
  - more than two channels with a surround mixer;
  - file reads through the engine's I/O layer.
