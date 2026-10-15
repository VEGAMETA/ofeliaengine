# 0100. Video playback: AV1 in WebM, decoded by the GPU's video engine

- Status: Accepted (2026-10-15; the owner's direction of 2026-10-10)
- Date: 2026-10-15
- Related:
  - [ADR 0051](0051-compressed-audio-and-streaming.md): compressed audio (Opus, the stream ring);
  - [ADR 0056](0056-cooked-packs-and-streaming-reads.md): asset packs;
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - [ADR 0099](0099-player-crate.md): the player;
  - research note `docs/research/video-playback.md`;
  - queue item 8b.

## Context

- **The engine plays no video.** Games need cutscenes, menu backgrounds, screens in the
  world and video in their UI.
- **The owner's direction (2026-10-10):**
  - a video frame is an ordinary texture;
  - AV1 (royalty-free) decoded by the GPU through Vulkan Video, a software decoder on
    worker threads where that is missing;
  - WebM read by our own code;
  - Opus sound through the existing decoder, kept in sync with the audio clock;
  - the cost per frame measured on both GPUs.
- **Both reference GPUs decode AV1 in hardware**, 8 and 10 bits, with film grain, on a
  queue of their own (the research note). They differ in what Vulkan Video leaves open:
  - the RTX writes the decoded picture into its reference slot; the Radeon into a separate
    output image;
  - their reference slots (16 and 9) and alignments (256 bytes and 4 KiB) differ.

## Options

**The codec.**

1. **AV1** (chosen): royalty-free, decoded by both GPUs, the best compression of the
   royalty-free codecs.
2. H.264 / H.265: patent pools, a licensing question for the humans (CLAUDE.md); not needed
   while AV1 serves.
3. VP9: older; the AMD driver does not expose its Vulkan decoding.
4. Theora (Godot's): far behind.

**Decoding.**

1. **Vulkan Video on the GPU's video engine** (chosen, the main path):
   - the frame costs the shaders nothing;
   - the picture stays on the GPU;
   - the work runs on the decode queue beside rendering.
2. **A software decoder on worker threads** (chosen, the fallback): for GPUs without AV1
   decoding, and for streams beyond the device's limits (level, size, profile).
   - **rav1d** (BSD-2-Clause, Rust port of dav1d, the same assembly) rather than dav1d (C,
     built with meson): it builds with cargo. About 5 % slower than dav1d.
   - Its pictures go up to the GPU like a texture's.
   - It is also the bit-exact reference of the tests.
3. Media Foundation (the OS's decoders, D3D11 textures shared with Vulkan): rejected. It is
   a second API path, and its H.264 / H.265 raise the licensing question again.

**The AV1 headers.** Vulkan Video decodes tiles; the application parses the sequence and
frame headers into `StdVideoAV1*` and keeps the reference frames' state.

1. **Our own parser in Rust** (chosen):
   - following the AV1 specification (sections 5 and 7);
   - NVIDIA's Apache-2.0 parser in the Khronos samples as the reference for what Vulkan
     needs;
   - rav1d's output as the test of it.
2. cros-codecs' parser (BSD-3-Clause): a dependency still under heavy development, shaped
   for VA-API.
3. NVIDIA's parser built as C++: its framework (callbacks, frame buffer) comes with it.

**The container.** Our own WebM reader (EBML, `Tracks`, `Cluster`s, `Cues`), WebM's subset
of Matroska only: a few hundred lines; a Matroska crate would bring the rest.

**The picture as a texture.**

1. **A compute pass converts each shown picture** (NV12 or P010) to RGB (chosen).
   - It uses the sequence header's colour description: BT.709, BT.601 or BT.2020 matrices,
     limited or full range, chroma siting.
   - It writes into the video's texture: `RGBA8_SRGB` for SDR, so sampling filters in
     linear light; `RGBA16F` later for HDR (PQ, HLG).
   - The texture is in the bindless table, as any texture.
2. Sampler Y'CbCr conversion: immutable samplers, which the bindless table does not take,
   and every material reading a video would need its own descriptor. Rejected.

**The clock.**

1. **The sound's clock when the video has sound** (chosen): the samples its stream has
   played, minus the device's output latency. The picture shown is the newest whose time
   is not past it. Late pictures are skipped and early ones held.
2. The frame's delta (Godot's): the picture and the sound drift apart. Rejected.

Without sound, a video follows the game's time (it pauses with the game) or real time (a
loading screen), as its owner chooses.

## Decision

- **A new crate, `engine_video`:**
  - the WebM reader;
  - the AV1 header parser;
  - the Vulkan Video AV1 session;
  - the rav1d path (a default feature, `software`);
  - the conversion pass;
  - the clock;
  - `VideoPlayer`:
    - open from a pack or a file;
    - play, pause, seek, loop, speed;
    - the current frame's texture;
    - events: a frame shown, the end, the loop point, an error.
- **`engine_rhi`:** an optional decode queue (a family with `VIDEO_DECODE`) and the
  extensions `VK_KHR_video_queue`, `VK_KHR_video_decode_queue` and
  `VK_KHR_video_decode_av1`, enabled when present.
- **Decoding** runs ahead of the clock on the decode queue (a few pictures), with a timeline
  semaphore the frame's conversion pass waits on. Pictures stay in their output or
  reference images until shown. Both of Vulkan Video's arrangements are supported:
  - output and reference coinciding (NVIDIA);
  - output and reference distinct (AMD).
- **Sound:** the Opus track's packets go to `engine_audio_decode`'s libopus decoder and
  play as an `engine_audio` stream; its played position is the clock.
- **The player** (`engine_app`): a video as an entity whose texture materials, the UI and
  viewports name.
- **Packs:** a WebM file is cooked as it is and streamed through the pack's reads.
- **nasm** becomes a build requirement of the `software` feature: rav1d's x86 assembly
  needs it (ref-1 has it; the quick start says so).

## Steps

1. **8b-1:** `engine_video` with the WebM reader, the AV1 header parser and rav1d's
   decoding to YUV pictures. Tested on generated clips (rav1e, BSD-2-Clause, as a tool)
   against rav1d's own view of the stream.
2. **8b-2:** the decode queue and the Vulkan Video AV1 session on both GPUs. Every picture
   bit-exact against rav1d's, under validation.
3. **8b-3:**
   - the conversion pass and the video texture;
   - Opus and the clock;
   - `VideoPlayer` and the player's video entity;
   - a golden shot of a video in a material and in the UI;
   - the cost on both GPUs (decode, conversion, CPU);
   - documentation (`docs/dev-guide`).

## Consequences

- AV1 in WebM is the one format. Games encode their videos with any AV1 encoder (SVT-AV1,
  rav1e, libaom, hardware encoders) into WebM; the dev guide gives a command.
- The decode queue is a third queue the device may create. It costs nothing when no video
  plays.
- The software path adds rav1d's compile time to the default build; measured in 8b-1, and
  a feature to leave out.
- Frames are bit-exact against a software decoder, so the decoding itself needs no golden
  images; the conversion and the texture get one.
