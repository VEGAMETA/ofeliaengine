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
  `VK_KHR_video_decode_av1`, enabled when present and asked for (8b-2: a game without video
  does not pay for them).
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

## 8b-1 as built (2026-10-15)

`crates/engine_video`, no GPU yet:

- **`webm`:** `Webm::read` over a file in memory: the EBML header, `Info` (the timestamp
  scale, the duration), `Tracks` (codec, private data, picture size, sample rate, channels,
  codec delay, seek pre-roll), `Cues`. `blocks()` and `blocks_from(cue)` walk the clusters
  (unknown sizes too): each block's track, time in nanoseconds, keyframe flag and frames
  (Xiph, fixed and EBML lacing), without allocating.
- **`av1`:** the specification's headers, written from it:
  - OBUs and the `av1C` record (`obu`);
  - the sequence header (`sequence`);
  - the frame header, every part of `uncompressed_header()`, with the values its parsing
    derives (`frame`);
  - the reference frames' state (`RefState`), which the frame header reads and the
    reference frame update and loading processes (7.20, 7.21) write.
- **`Parser::parse`** turns a temporal unit (a WebM block) into its frames, in decoding
  order. Each frame comes with:
  - its header;
  - the offsets of its frame header OBU and of the header itself;
  - its tiles' offsets and sizes, which are Vulkan's `pTileOffsets` and `pTileSizes`.

  A `TemporalUnit` keeps the arrays from unit to unit. The parser takes the sequence header
  from the track's `av1C` record or from the stream (rav1e's record has none). It refuses
  spatial layers, and a frame whose reference it has not seen (`MissingReference`, after a
  seek anywhere but at a key frame).
- **`software`** (the default `software` feature): `SoftwareDecoder` over rav1d's dav1d
  interface.
  - `decode(unit, time, |picture| ..)` returns pictures in output order: every picture of
    the unit when decoding one frame at a time; with frame threads, those that are ready.
  - A `Picture` has its planes, size, depth, layout, time and rav1d's view of its headers.

**Tested** (`tests/clips.rs`, CPU only, 0.25 s) on nine generated clips from SVT-AV1,
libaom and rav1e (`tests/clips/README.md`, 260 KB):

- every field of every frame header, hidden frames included, equals rav1d's;
- every sequence header field equals rav1d's;
- every frame's tiles are all there, in order, inside the unit;
- one picture per frame, frames shown again included.

The test also checks that the clips use each feature it lists: 10 bits, error resilience
with frame ids, super-resolution, frame size overrides and render sizes, screen content
tools and intra block copy, tiles, quantizer matrices, chroma deltas, segmentation, delta
q and delta loop filter, lossless frames, CDEF, loop restoration of luma and chroma,
compound references, skip mode, warped and global motion, film grain.

No available encoder makes intra-only or switch frames, short reference signaling
(`set_frame_refs` has a unit test, worked by hand), non-uniform tile sizes, loop filter delta
updates, or film grain loaded from a reference.

**Found while matching rav1d:**

- What rav1d calls `all_lossless` is the specification's `CodedLossless`.
- For lossless and intra block copy frames, the loop filter's delta flags are not coded.
  They are reported as 1, as `setup_past_independence()` leaves them (rav1d does the same;
  the filter is off).
- CDEF strengths are kept as coded (a secondary strength of 3 stays 3): Vulkan takes them
  so.
- In the tile arrays, rav1d's last start is the frame's width in superblocks, rounded up.

**Measured** on ref-1 (Ryzen 7 7700, 8 cores / 16 threads), release, `video_bench`:

| clip (SVT-AV1 preset 8, CRF 32) | Mbit/s | rav1d, 1 thread | 16 threads, 1 frame at a time | 16 threads, frames in parallel |
|---|---:|---:|---:|---:|
| 1080p30, 8 bits | 13.4 | 194 pictures/s (6.5× real time) | 198 /s | 450 /s (15×) |
| 2160p30, 10 bits | 35.9 | 51 /s (1.7×) | 56 /s | 164 /s (5.5×) |

- **Our parser:** 0.3 µs a temporal unit.
- **Without frame threads, more threads gain nothing:** these streams have one tile.
- **Units with several frames take longest:** on one thread, up to 48 ms at 1080p and
  134 ms at 2160p (a key frame with its hidden frames). The player decodes ahead.
- **One core decodes both streams faster than they play** (6.5× and 1.7×). Frame threads
  give the headroom (15× and 5.5×).
- **Building rav1d:** 20 s in the dev profile (dependencies are optimized there) and 17 s in
  release, beside the rest of the workspace.

**Dependency changes:**

- `rav1d` 1.1.0 and its crates (`THIRD_PARTY_NOTICES.md`).
- NASM 2.14 or later on `PATH` (the README).
- `syn`'s `extra-traits` in the workspace hack: strum_macros enables it (ADR 0077).
- One advisory ignored in `deny.toml`: `paste`, a build-time macro of rav1d's, is archived
  (RUSTSEC-2024-0436, no vulnerability).

**Not done here:** decoding on the GPU (8b-2), the picture as a texture, sound and the
clock (8b-3).

**Video with transparency** (an alpha channel, e.g. as a second, monochrome AV1 stream) is
not part of 8b. The owner deferred it on 2026-10-15; it stays in the status's list of
things for later.

## 8b-2 as built (2026-10-16)

**`engine_rhi`:**

- `OptionalCaps::video_decode_av1` and `Adapter::video_decode_family()`: the three video
  extensions and a queue family whose `VkQueueFamilyVideoPropertiesKHR` include AV1 decode.
- **The decode queue is created only when asked for** (`ExtraQueues::video_decode`, then
  `Device::video_decode`), not whenever present as planned. It costs the RTX 2.5 ms of
  `vkCreateDevice` (50 against 47.5 ms) and the Radeon nothing, so games without video do
  not pay it. `ENGINE_VIDEO_DECODE=0` leaves it out, to test the software path.
- `Adapter::driver_id`, for the one workaround below.

**`engine_video::gpu`:**

- **`GpuCaps`** reads the profile's capabilities and picture formats.
- **`GpuDecoder`** holds:
  - the session and its memory;
  - session parameters per sequence header;
  - the reference pictures: one image of nine layers, AV1's eight references and the
    frame being decoded;
  - output images (12 by default), shared with the graphics queue;
  - four command buffers with their bitstream buffers;
  - a timeline semaphore.
- **`decode()`** takes a parsed temporal unit and returns `GpuPicture`s (image, sizes,
  layout, the semaphore value to wait for). The caller gives each back with `release()`
  once nothing reads it.
- **The two arrangements:**
  - **Coinciding (the RTX):** a frame decodes into its reference layer. A frame that may be
    shown is copied into an output image on the decode queue (its family copies).
  - **Distinct (the Radeon):** a frame decodes into an output image, with its reference
    beside it.
  - **Film grain:** grain frames decode into an output image on both. The reference stays
    without grain, as the specification requires.
- **AV1's eight reference slots map to the image's layers.** A hidden frame that may be
  shown later keeps its output image until it is (`show_existing_frame` then just hands it
  out). A shown key frame refreshes every slot.
- **The structures:** `StdVideo*` (`std_av1.rs`) are filled in place for each frame,
  without allocating. `LoopRestorationSize` goes as `log2(size) - 5` and CDEF's secondary
  strengths as coded, as the Video Std definitions say.

**Two workarounds, each limited to where it is needed:**

- **AMD's PAL-based drivers read the tiles' starts in superblocks**
  (`driverID` `AMD_PROPRIETARY` or `AMD_OPEN_SOURCE`). Other drivers get mode info units,
  as the specification says.
- **Frames larger than the sequence header's maximum** (SVT-AV1 1.8 at heights not a
  multiple of 8) get session parameters whose maximum holds them, within the session's
  pictures. A conformant stream never triggers it.

**Tested** (`tests/gpu.rs`, on the GPU `ENGINE_ADAPTER` picks, under the validation layer):

- Every shown picture of the test clips is read back and compared with rav1d's, sample by
  sample, without film grain. A second test applies film grain on both sides.
- The two clips that showed SVT's non-conformance gained conformant 176-line twins.
- **The Radeon decodes all eleven clips** (312 pictures) bit-exact.
- **The RTX decodes eight** (264 pictures) bit-exact. It refuses the lossless 160 × 90 clip and both resize
  clips: their pictures are below its 128 × 128 minimum, and the test allows only size
  refusals.
- Film grain is bit-exact on both GPUs.
- The validation layer reports no errors or warnings.

**Measured** with `video_bench --gpu` on ref-1, release, the 8b-1 clips:

| | RTX 4060 Ti | Radeon iGPU | rav1d, 16 threads (8b-1) |
|---|---:|---:|---:|
| 1080p30 8-bit, 13.4 Mbit/s: pictures/s (× real time) | 1,024 (34×) | 548 (18×) | 450 (15×) |
| 2160p30 10-bit, 35.9 Mbit/s | 276 (9.2×) | 107 (3.6×) | 164 (5.5×) |
| CPU per unit (recording, submitting), 1080p / 2160p | 41 / 59 µs | 19 / 31 µs | all cores |
| one unit, submitted to decoded, median / slowest: 1080p | 0.73 / 8.6 ms | 1.8 / 13 ms | |
| the same, 2160p | 3.0 / 23 ms | 10 / 58 ms | |

- The slowest units are key frames with their hidden frames.
- The Radeon's decode queue has no timestamps, so these are wall-clock times on the CPU.

**Left for 8b-3 and later:**

- the picture as a texture (the conversion pass);
- the sound and the clock;
- `VideoPlayer`;
- the decode queue shared by several players (one queue per device, submits serialized);
- decode status queries (a damaged stream reported, not just decoded wrong);
- recovery at the next key frame after an error.

## 8b-3 as built (2026-10-16)

**The sound and the clock** (`engine_video::audio`, `engine_audio`):

- `engine_audio_decode::OpusPackets` decodes raw Opus packets (from WebM rather than Ogg),
  with each packet's length for exact seeks.
- `VideoAudio` is the Opus track as an `engine_audio` stream. Its time 0 is the first
  block's: the codec's delay (`CodecDelay`, pre-skip) is decoded and dropped. **Seeks count
  the packets' own lengths** from the start: muxers round block times to the millisecond
  (an early version landed 1,030 frames off). 80 ms of pre-roll (WebM's `SeekPreRoll`).
- `Audio::stream_position`: the frames of a stream voice the mixer has taken. The clock is
  that position at the stream's rate, less the output's latency (`OutputInfo`'s queue and
  stream latency) times the speed.

**The picture as a texture** (`engine_video::convert`):

- **`VideoTexture`**: RGBA8, R'G'B' written through a UNORM view and sampled through an
  sRGB one (filtering in linear light), in the bindless table. Not mipmapped.
- **`Conversion`** from the colour description (H.273 code points): BT.709, BT.601,
  BT.2020 (its constant-luminance form taken as the usual one), FCC, SMPTE 240M, YCgCo, GBR;
  studio and full range at any depth; chroma siting from `chroma_sample_position`
  (4:2:0's chroma halfway down unless colocated; across, on luma sample 0). **An
  unspecified matrix is guessed from the size** as players guess (BT.709 from 1280 wide or
  577 high). HDR transfers and BT.2020's primaries are not converted yet (shown as SDR
  BT.709's).
- **The pass** (`convert.slang`) reads samples as integer codes: an 8-bit byte, P010's
  words shifted by six, or the low bits of the software decoder's 16-bit words. Planes are
  interpolated bilinearly with exact weights; a picture of another size than the texture
  (AV1's render size; the 184-line SVT frames under their 180-line picture) is scaled.
  - **`convertHalf`**, the usual case (4:2:0 at the texture's size), takes a 2 × 2 block a
    thread: its luma in one gather, the chroma it shares (two columns, three rows) in four,
    the block stored through group-shared memory so that each store instruction's lanes
    write neighbouring texels. Same arithmetic, same results. A 1080p picture: 1.3 against
    2.9 ms on the Radeon iGPU, 25 against 44 µs on the RTX.
- **The pass is a fence of the frame graph** (`ComputePass::fence`): shaders reach the
  texture through its bindless index without declaring it, so every pass declared after
  it runs after it, on both queues. It records its own barriers: the texture from its
  readers (`ALL_COMMANDS`) to the compute writes and back to `READ_ONLY_OPTIMAL`.
- **GPU pictures are read where they were decoded.** The decoder now leaves every picture
  it hands out in `READ_ONLY_OPTIMAL` (a barrier on the decode queue, before its semaphore
  signal) and makes single-plane views of its output images (`R8`/`R8G8`, or
  `R16`/`R16G16` holding P010's words; `MUTABLE_FORMAT`, which both GPUs offer).
  `DecoderPlanes` puts them in the bindless table. **`wait_for_decode`** submits an empty
  batch that waits for the picture's semaphore value before the frame graph's first
  submission. The player converts only pictures it has seen decoded, so the wait costs the
  GPU nothing.
- **Software pictures** (`PlaneUpload`): copied each picture into the frame slot's staging
  memory (Cb and Cr interleaved), then by the pass into two plane images sized for the
  stream's largest frame (rounded to 64).

**Playing** (`engine_video::player::VideoPlayer`):

- Decodes ahead (3 pictures by default, at most 8 units an update), on the GPU when the
  device has the decode queue and takes the stream, else rav1d on a thread of its own
  (half the cores, at most 8, frame threading). **When the GPU refuses a frame on the way**
  (a size outside its range), rav1d takes over from the key frame before the clock; the GPU
  decoder is destroyed once the frames reading its pictures finish.
- The newest picture the clock has reached is shown; late ones are skipped, early ones
  held, one still decoding stays off a frame longer. After opening or a seek the clock
  waits for the picture sought (shown only once a later one or the end proves it the last
  before the target).
- Play, pause, seek, looping, speed (the sound's pitch with it); events: a picture shown,
  the end, the loop point.
- **A fix found here:** a picture shown by `show_existing_frame` took the time of the unit
  it was decoded in; it takes the showing unit's now. The GPU test checks every picture's
  time against rav1d's.

**The player** (`engine_app::Videos`):

- The game opens a video from its bytes and controls it by id, from `Setup` or any frame's
  `FrameParts`; calls take effect at the next frame.
- `show_in` binds the video to a material's base colour or emission through an **external
  texture** of the texture pool (`TexturePool::add_external`, `set_external`): an image its
  owner keeps, named by materials like any pool texture, missing until its first picture.
  Closing gives the material back. `texture_index` serves UI sprites.
- The player updates the videos after the game's `animate` and declares their conversions
  first in the frame graph. Their sound plays through an audio output started with the
  first video that has sound.
- `PlayerSettings::video_decode` asks for the decode queue (off by default, as 8b-2
  decided).

**Tested**, under the validation layer with synchronization validation, on both GPUs:

- the conversion against its CPU reference, within one per channel: synthetic planes of
  every layout (4:2:0, 4:2:2, 4:4:4 GBR, luma only), depth (8, 10, 12), range, siting and
  scaling (one texel of 18,393 off by one on each GPU); and the GPU decoder's pictures of
  three clips against rav1d's planes;
- `VideoPlayer`: each of 60 pictures shown once and in order (GPU and rav1d); seeks to 1.0,
  0.5 and 1.9 s showing exactly that picture (the texture compared with rav1d's); 0.750 s
  mixed moving the clock 0.750 s, a pause holding it; a loop; twice the speed; every clip
  to its end (the RTX's resized clips go from the GPU to rav1d mid-stream);
- the player: a video on a screen and in the UI, the material given back when it closes;
- **a golden shot per vendor** (`video_player`): Sponza with a screen showing the clip's
  picture at 1.0 s, paused, and the same picture as a UI sprite.

**Measured** with `examples/player_bench.rs` on ref-1 (release; 60 simulated frames a
second; the GPU kept busy after the conversion so its clocks stay up), each new picture:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| conversion, 1080p30 8-bit (GPU decoded) | 25 µs | 1.30 ms |
| conversion, 2160p30 10-bit | 0.17 ms | 4.3 ms |
| conversion with the upload, rav1d, 1080p / 2160p 10-bit | 0.26 / 2.0 ms | 1.7 / 10.4 ms |
| `update` on the CPU, median / p99 / largest (GPU decoding) | 1 / 182 / 820 µs | 1 / 74 / 810 µs |
| `add_pass` on the CPU, GPU decoding / rav1d's copy at 1080p | 9 / 177 µs | 4 / 170 µs |

**Left:**

- WebM files cooked into packs and streamed through the pack's reads (videos open from
  bytes today);
- HDR (PQ, HLG) and BT.2020's primaries: an RGBA16F texture and the curve;
- transparency (deferred by the owner);
- spatial sound for a video in the world (the stream plays as is);
- a seamless loop (the first picture of the next round decoded before the end);
- decode status queries; one decode queue shared by several players.
