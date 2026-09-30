# Compressed audio and streaming

Research for decoding Vorbis, Opus and FLAC and for streaming long sounds (CLAUDE.md §11;
ADR 0024 and `docs/research/audio.md` left both for later). Decision:
[ADR 0051](../adr/0051-compressed-audio-and-streaming.md).

Checked on 2026-09-30.

## Sources

| Source | Version | License | What we read |
|---|---|---|---|
| libopus | 1.5.2 (release tarball, SHA-256 65c1d2f7…9a7ce1 as published by Xiph) | BSD-3-Clause | `opus.h`, `opus_decode_float`, the build's source lists and x86 presumption macros |
| RFC 7845, Ogg Encapsulation for the Opus Audio Codec | 2016 | IETF | `OpusHead` (pre-skip, output gain, mapping family), granule positions, end trimming |
| Vorbis I specification, appendix A (Ogg encapsulation) | xiph.org | public spec | granule positions; start and end trimming |
| lewton | 0.10.2 | MIT OR Apache-2.0 | `inside_ogg` (reader, `read_headers`), `audio::read_audio_packet_generic` |
| ogg | 0.8.0 | BSD-3-Clause | `PacketReader`, packet flags and granule positions |
| claxon | 0.4.3 | Apache-2.0 | `FlacReader`, `FrameReader::read_next_or_eof` |
| symphonia | 0.5 | MPL-2.0 | considered as a single dependency for all formats |
| FMOD Studio, Wwise (products) | public documentation | proprietary | which formats games stream and how much they buffer |

## Formats

- **Opus:** the best quality per bit (music at 96 kbit/s stereo is transparent for most
  listeners), always 48 kHz; the codec for music and dialogue. The reference decoder is C
  (libopus); pure-Rust decoders are incomplete.
- **Vorbis:** the older royalty-free codec; many existing game assets. lewton is a
  complete pure-Rust decoder.
- **FLAC:** lossless; for sounds that must stay exact or be edited later. claxon is a
  pure-Rust decoder.
- MP3 and AAC: patent history (MP3 now free) and no advantage over Opus; not included.

## Options for the decoders

1. **symphonia** (MPL-2.0): one crate with all formats and a common interface. MPL needs
   an ADR by our rules, and its Opus support is not complete.
2. **Per-format crates** (chosen): lewton + ogg for Vorbis, claxon for FLAC, libopus
   vendored and built with `cc` for Opus. All permissive; each small; libopus is the
   reference implementation.

## Details that decide correctness

- **Ogg Opus:** the first `pre_skip` samples are encoder delay and must be dropped; the
  last page's granule position says where the stream ends (the last packet is cut). Output
  gain (Q7.8 dB) applies to everything.
- **Ogg Vorbis:** the last page's granule position ends the stream the same way; a first
  page whose granule position is below the samples it decodes to starts later (samples
  dropped at the start). lewton's convenience reader only cuts the end when it already
  knows an earlier page's position, so a short file came out 30 frames long; the decoder
  here tracks the positions itself.
- **Seeking:** Ogg seeks by page (granule positions), then decodes up to the frame; Opus
  needs 80 ms of pre-roll after a page seek. Exact seeking by rewinding and decoding is
  simple and exact; its cost is proportional to the position (numbers below).
- **Vorbis setup header:** building its codebooks takes about a millisecond; seeking must
  not redo it.

## Streaming

- Products keep a few hundred milliseconds decoded per stream and decode on a worker
  thread; the mixer reads from a buffer and never waits.
- Our constraints (ADR 0024): the audio thread never allocates, frees, locks or blocks.
  So: a single-producer single-consumer ring of samples per stream, allocated and freed on
  the game thread; the stream thread only writes, the audio thread only reads.
- When the decoder falls behind, the voice pauses on its current frame (silence for the
  rest of the block) and continues from there: no skipped audio, and the count is visible.

## Measured

ref-1, `cargo run -p engine_audio_decode --release --example decode_bench` (test files,
0.5 s stereo; steady decoding loops the file through `seek(0)`):

| file | open µs | decode µs per second of audio | × real time |
|---|---:|---:|---:|
| Vorbis 44.1 kHz, q4 | 1,172 | 939 | 1,065 |
| Opus 48 kHz, 96 kbit/s | 2 | 1,279 | 782 |
| FLAC 16-bit 44.1 kHz | 0 | 541 | 1,850 |
| FLAC 24-bit 48 kHz | 0 | 589 | 1,699 |

- Seeking to minute 1 of a file decodes that minute: about 56 ms (Vorbis), 77 ms (Opus),
  33 ms (FLAC) on the stream thread. Loops seek to 0, which costs nothing.
- Mixer (`benches/mixer.rs`, 256-frame block): 32 stereo streams 12.0 µs against 2.0 µs
  for 32 stereo clips. Each ring sample is an atomic load (a plain load on x86), one at a
  time; clips copy with `memcpy`.

## Open questions

- Seeking by page with pre-roll, for games that jump around long files.
- Reading ring samples in contiguous runs (unsafe, with the SPSC proof) to bring streams
  to the cost of clips.
- More than two channels (5.1 music) once the mixer has more than stereo.
- Reading files through the engine's I/O layer (IoRing) instead of `std::fs`.
