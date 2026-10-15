# Video playback

Checked 2026-10-15. Sources at the end.

- **Code read:**
  - Godot 4's Theora player (MIT);
  - the Khronos Vulkan Video samples with NVIDIA's AV1 parser (Apache-2.0);
  - rav1d's README and build (BSD-2-Clause).
- **Known from their documentation:** cros-codecs' AV1 parser (BSD-3-Clause), FFmpeg's
  Vulkan AV1 decoding (LGPL-2.1: ideas only), Unity's `VideoPlayer`, Unreal's Media
  Framework and Bink.
- **No code was copied.** What a later step ports from the Apache-2.0 or BSD sources is
  credited in `THIRD_PARTY_NOTICES.md` with it.

## Why

Queue item 8b: the engine plays no video. Games play cutscenes, menu backgrounds, tutorials,
screens in the world and video in their UI. A video frame should be an ordinary texture:
for a material, a UI element, a viewport or the whole screen.

## The reference GPUs

`vulkaninfo --show-video-props`, 2026-10-15.

| | RTX 4060 Ti (NVIDIA 616.92) | Radeon iGPU (AMD 26.8.1) |
|---|---|---|
| decode extensions | AV1, H.264, H.265, VP9 | AV1, H.264, H.265 |
| decode queue family | its own (also transfer) | its own (video decode only) |
| AV1 profiles | Main, 8 and 10 bits | Main, High, Professional, 8 and 10 bits |
| film grain | applied by the decoder | applied by the decoder |
| maximum level, size | 7.3, 8192 × 8192 | 6.1, 8192 × 4352 |
| reference slots, active references | 16, 16 | 9, 7 |
| decoded picture and reference | the same image (`DPB_AND_OUTPUT_COINCIDE`) | separate images (`DPB_AND_OUTPUT_DISTINCT`) |
| picture format | NV12 (`G8_B8R8_2PLANE_420_UNORM`); P010 for 10 bits | the same |
| bitstream offset and size alignment | 256 bytes | 4,096 bytes |

Both decode AV1 in hardware (NVDEC, VCN) on a queue of their own, beside the graphics and
compute work. They differ where Vulkan Video lets them:

- one writes the output into the reference picture, the other into a separate image;
- the alignments differ;
- the number of reference slots differs.

A decoder has to handle both.

## Which codec

- **AV1** is royalty-free (AOMedia Patent License 1.0) and compresses about as well as
  H.265. It is decoded in hardware by NVIDIA RTX 30 and later, AMD RX 6000 and later
  (most), Intel Xe and later, and by both reference GPUs.
- **H.264 and H.265** are licensed through patent pools (Via LA, Access Advance). Shipping
  their decoding in an engine others build games on is a licensing question CLAUDE.md
  sends to the humans; there is no need to ask it while AV1 serves.
- **VP9** is royalty-free but older, and the AMD driver does not expose its Vulkan decoding.
- **Theora** (Godot's) is royalty-free but well behind AV1.

So: **AV1**, in WebM, with Opus sound.

## How others do it

### Godot 4 (MIT): Theora in Ogg

`modules/theora/video_stream_theora.cpp`.

- libtheora decodes on the CPU, the YUV picture is turned into RGBA on the CPU
  (`yuv420_2_rgb8888`), and the texture is uploaded every frame.
- Sound (Vorbis) goes to the audio server through a mix callback and a ring buffer.
- **The clock** is the frame's delta (`time += p_delta`), not the audio's. Its output-latency
  compensation is commented out with a FIXME: picture and sound can drift apart.
- **What we take:** the shape of the API (a stream resource, a playback that updates per
  frame and gives a texture, a player that loops and signals the end).
- **What we don't:** CPU decoding and conversion, and a clock that is not the sound's.

### Unity (product)

`VideoPlayer`:

- plays to a render texture, a material's property, the camera's near or far plane, or an
  API texture;
- the sound goes to an audio source, straight to the output, or is muted;
- events: the frame is ready, the loop point is reached, an error happened;
- play, pause, step, seek, playback speed, loop.

Decoding is the platform's (Media Foundation on Windows).

### Unreal (product)

- **Media Framework:** platform players (WMF, Electra) play into media textures, with
  sound through a media sound component.
- **Bink** (free with Unreal, closed) for cutscenes, decoded on the CPU, with GPU-assisted
  paths.

What both products show: video is used as a texture everywhere, and timing follows the sound.

### Khronos Vulkan Video samples (Apache-2.0)

`KhronosGroup/Vulkan-Video-Samples`, `vk_video_decoder`.

- **The full chain for H.264, H.265, AV1 and VP9:** a container reader, NVIDIA's elementary
  stream parser (`libs/NvVideoParser`), the session and its parameters, the reference
  pictures, the decode queue, the frame pool, presentation.
- **`VulkanAV1Decoder.cpp`** (95 KB) parses the sequence and frame header OBUs into
  `StdVideoAV1*` structures and the tile layout into
  `VkVideoDecodeAV1PictureInfoKHR`. It keeps the reference frames' state the spec's
  "reference frame update process" asks for (order hints, saved loop filter and
  segmentation parameters, global motion).
- **What we take:** the mapping from the AV1 headers to Vulkan's structures, and the
  reference bookkeeping, as a reference beside the AV1 specification.
- **What we don't:** its framework (a C++ parser interface with callbacks, its frame
  buffer). Chromium, too, did not reuse the parser, since it expects parsed structures.

### FFmpeg (LGPL-2.1: ideas only, no code read in detail or carried over)

`libavcodec/vulkan_av1.c` fills the same Vulkan structures from FFmpeg's own parsed frame
header (`CodedBitstreamAV1`). Parsing is the application's job, separate from the decoding.

### Software decoders: dav1d and rav1d

- **dav1d** (BSD-2-Clause, C, VideoLAN) is the reference fast AV1 decoder. Its DSP is in
  x86 assembly (AVX2, AVX-512), built with meson and nasm.
- **rav1d** (BSD-2-Clause, Rust; 1.1.0, August 2026) is its Rust port with the same
  assembly.
  - About 5 % slower than dav1d (May 2025).
  - Builds with cargo on stable for x86-64; the assembly needs `nasm` (the `asm` feature,
    on by default; it is on ref-1).
  - Its API is dav1d's C API: open, send data, get picture.
- **What we take:** rav1d, for GPUs without AV1 decoding, and as the bit-exact reference
  in the tests (AV1 decoding is exact: the GPUs' pictures must equal it bit for bit).

### AV1 parsers

- **NVIDIA's** (above, Apache-2.0), written for Vulkan Video.
- **cros-codecs** (BSD-3-Clause, Rust, ChromeOS) has an AV1 parser for its stateless
  (VA-API) decoders. It is still under heavy development.
- **What we take:** an AV1 header parser of our own, in Rust:
  - the sequence header, frame header and tile group OBUs, following the AV1 specification
    (sections 5 and 7);
  - NVIDIA's parser as the reference for what Vulkan needs;
  - in the tests, rav1d's output says whether it is right.

### Containers

- **WebM** is a subset of Matroska (RFC 9559) on EBML (RFC 8794): an EBML header, a segment
  with `Info` (the timestamp scale), `Tracks` (codec ids `V_AV1` and `A_OPUS`, their
  private data: the AV1 configuration record, `OpusHead`), `Cluster`s of `SimpleBlock`s
  and `BlockGroup`s with timestamps, and `Cues` for seeking.
- **AV1 in Matroska:** a block is a temporal unit, low-overhead OBUs without the temporal
  delimiter.
- **Opus in Matroska:** a block is an Opus packet; `CodecDelay` and `SeekPreRoll` are given.
- **Our own reader:** about the size of the glTF reader's container part. It reads WebM
  only: no lacing beyond what Opus needs, no other codecs.

## What a video costs, the parts

- **Decoding in hardware** runs on the video engine, beside the shaders: a frame costs
  them nothing.
- **What costs the GPU:**
  - the conversion of each new picture from YUV (NV12 or P010, BT.709 or BT.2020, limited
    range) to RGB, a compute pass over its pixels;
  - for DPB_AND_OUTPUT_DISTINCT (the Radeon), nothing more: the output image is read.
- **What costs the CPU:** reading the file, demuxing and parsing the headers (well under a
  millisecond for a 1080p frame), and recording the decode command.
- **The software path** costs worker threads: dav1d decodes 1080p AV1 at several hundred
  frames a second on 8 cores.

## Measured on ref-1 (2026-10-16)

What the drivers report for AV1's main profile (`vkGetPhysicalDeviceVideoCapabilitiesKHR`
and `vkGetPhysicalDeviceVideoFormatPropertiesKHR`, 8 and 10 bits alike, film grain or not):

| | RTX 4060 Ti, NVIDIA 616.92 | Radeon 780M, AMD 26.8.1 |
|---|---|---|
| decode queue family | 3: decode + transfer, 1 queue, 32 timestamp bits | 4: decode only, 1 queue, no timestamps |
| decoded picture and reference | coincide only | distinct only |
| coded size | 128 × 128 to 8192 × 8192 | 16 × 16 to 8192 × 4352 |
| level | 7.3 | 6.1 |
| reference slots, active references | 16, 16 | 9, 7 |
| bitstream offset and size alignment | 256 bytes | 4,096 bytes |
| picture access granularity | 16 × 16 | 16 × 16 |
| reference picture images | one format for every usage (decode, reference, sampled, copies) | reference usage only |
| output images | the same | decode output, sampled, storage, copies |

Found while decoding the test clips:

- **AMD's driver reads `pMiColStarts` and `pMiRowStarts` in superblocks.** The
  specification has mode info units and advises drivers to ignore the arrays. Given mode
  info units, every tile but the first decodes wrong. NVIDIA ignores them; Mesa's RADV
  (`radv_video.c`, read 2026-10-16) derives the starts from `pWidthInSbsMinus1` and the
  frame size, its comment noting that applications send either.
- **SVT-AV1 1.8.0 writes frames larger than the sequence header's maximum** at heights that
  are not a multiple of 8 (180 lines coded as 184 under a 180 maximum). rav1d and NVIDIA
  decode them. AMD's decoder keeps to the maximum: with super-resolution the last rows are
  lost, with resized frames the reference scaling is wrong.
- **The RTX refuses pictures below 128 × 128**, which some resized frames of small videos
  are.
- Every other picture of the clips, film grain included, is bit-exact against rav1d on
  both GPUs.

## Sources

- AV1 Bitstream & Decoding Process Specification, AOMedia, 1.0.0 with errata 1.
- Vulkan: `VK_KHR_video_queue`, `VK_KHR_video_decode_queue`, `VK_KHR_video_decode_av1`
  (Vulkan 1.3.277), the `vulkan_video_codec_av1std_decode.h` header.
- KhronosGroup/Vulkan-Video-Samples (Apache-2.0): `vk_video_decoder`,
  `libs/NvVideoParser/src/VulkanAV1Decoder.cpp`, read 2026-10-15.
- Godot 4 `modules/theora/video_stream_theora.cpp` (MIT), master, read 2026-10-15.
- rav1d 1.1.0 (BSD-2-Clause), github.com/memorysafety/rav1d; memorysafety.org's
  performance notes (2024, 2025).
- cros-codecs (BSD-3-Clause), android.googlesource.com/platform/system/cros-codecs.
- RFC 9559 (Matroska), RFC 8794 (EBML), the WebM container guidelines, the AV1 codec mapping
  for Matroska, Opus in Matroska (`A_OPUS`).
- Unity `VideoPlayer` and Unreal Media Framework documentation.
- The Vulkan specification's AsciiDoc sources (KhronosGroup/Vulkan-Docs `main`:
  `chapters/videocoding.adoc`, `chapters/video/decode.adoc`, `chapters/video/av1_decode.adoc`),
  read 2026-10-16.
- Mesa `src/amd/vulkan/radv_video.c` (MIT), `main`, read 2026-10-16: how AMD's video firmware
  is given AV1's tiles, sizes and references.
