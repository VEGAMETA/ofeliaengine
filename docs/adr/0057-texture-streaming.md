# 0057. Texture streaming: reads straight into a host-cached staging ring, levels arriving smallest first, a streaming thread

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/assets-streaming.md](../research/assets-streaming.md)
- Related: [ADR 0040](0040-materials-and-textures.md) (the texture pool),
  [ADR 0056](0056-cooked-packs-and-streaming-reads.md) (packs, the I/O ring)

## Context

ADR 0056 loads a scene's pack whole before the first frame. Sponza's pack takes 0.5–0.9 s
from a hard disk, and every byte passes through `Vec`s on its way to the GPU. The packs
are laid out for streaming: each texture's small levels (≤ 128 texels) form one entry, the
tail, and each larger level is an entry of its own. The tails come first in the file.

Streaming asks three questions:

- where the bytes go between the drive and the GPU;
- how a texture becomes usable before all its levels have arrived;
- which thread does the work, so no frame waits for the drive or for decompression.

## Measurements

`staging_bench`: 128 MiB of BC7 data, NVMe drive, GB/s:

| staging memory | drive reads into it | zstd, 1 thread | zstd, 16 threads | GPU copy (RTX / Radeon) |
|---|---:|---:|---:|---:|
| upload (write-combined) | 5.09 | **0.03** | **0.4** | 10.0 / 5.8 |
| **host-cached** | 5.08 | 1.27 | 9.9 | 10.0 / 5.7 |
| process memory | 5.07 | 1.27 | 9.8 | — |

- Unbuffered reads land in Vulkan's mapped memory at full speed, and the data checks out.
- zstd reads back what it just wrote (its matches), so decompressing into write-combined
  memory is 25–40× slower. Into host-cached memory it runs as fast as into plain memory.
- The GPU copies from host-cached memory as fast as from write-combined memory, on both
  GPUs.

Sponza streamed from its pack (`scene_bench --pack … --stream`). Before the first frame
the scene record and meshes are loaded: 5.6 MB, 7 ms from NVMe, 13–316 ms from the hard
disk (seeks). Times below count from the first frame:

| | hard disk | NVMe |
|---|---:|---:|
| every texture visible (its tail) | 26–61 ms | 14 ms |
| every level resident | 394–564 ms | 40–41 ms |
| on the frame's thread, average / worst frame | 0.00 / 0.13–0.26 ms | 0.01 / 0.11–0.44 ms |
| whole pack before the first frame (ADR 0056), for comparison | 520–906 ms | 34–51 ms |

- With decompression in the frame's own update (the first version), the frame's thread
  spent 1.7 ms per frame on average on NVMe and up to 6.1 ms. That is a hitch at 60 Hz, so
  decompression moved to a thread of its own.
- Once every level has arrived, the frame is identical, byte for byte, to the frame from
  the glTF path.

## Options

### Where the bytes go

1. **A staging ring in host-cached memory** (chosen):
   - drive reads land in it directly (raw entries);
   - compressed entries are read into a buffer and decompressed into it;
   - the GPU copies levels from it into the image.
2. **Write-combined upload memory** (what `TexturePool::add` uses): fine for the CPU's
   sequential copies, 40× too slow for decompression (above).
3. **Plain memory, then a copy into upload memory:** a copy of every byte for nothing.

### How a texture becomes usable

1. **Resident from the smallest level up** (chosen):
   - a texture's view covers its resident levels;
   - when a larger level joins, the texture gets a new view and a new bindless index;
   - materials pick the new index up through the pool's generation counter (they already
     did for new textures);
   - the old view and index are freed once the frames using them have finished;
   - rewriting the old descriptor is not allowed while in-flight frames use it.
2. **A per-texture minimum-level clamp in the shaders** (a buffer of resident levels):
   one view, but every sample pays a load and a clamp, and the shaders change.
3. **Sparse residency:** memory per level and eviction without copies, but sparse images
   have limits (tile sizes, the mip tail) that differ by driver. Worth it only if
   eviction by copying proves too slow.

### Which thread

1. **A streaming thread** (chosen, `StreamThread`):
   - it waits for the I/O ring and decompresses on a job pool of its own, at below-normal
     priority;
   - the frame's thread only queues requests and takes finished entries, through
     single-producer single-consumer queues, without locks or allocation.
2. **The frame's thread, with decompression on the shared job pool:** measured above, up
   to 6 ms per frame.

## Decision

- **`engine_rhi::MemoryLocation::Streaming`:** host-visible, host-cached, never
  device-local.
- **`TexturePool`:**
  - `add_empty(width, height, format, levels)` creates the image;
  - `staging(bytes)` lends a span of the pool's ring (64 MiB by default,
    `set_staging_size`). Spans are 4 KiB-aligned (unbuffered reads) and come back in any
    order: to the ring once the frame copying from them has finished, or through
    `cancel_staging`;
  - `upload_levels(id, first, count, span)` copies levels laid out as in a pack entry,
    within the per-frame budget, with barriers on those levels only;
  - residency grows from the smallest level (above); `resident_level` and `image` are for
    feedback and tools;
  - images are also `TRANSFER_SRC`, to be copied into images of another size when
    residency shrinks later;
  - `add` (whole textures from memory) is unchanged.
- **`engine_platform::io::ReadQueue::push_into`:** reads into memory of the caller's
  (unsafe: the memory must stay valid until the completion).
- **`engine_assets::pack::PackStreamer`:** requests entries into targets of the caller's.
  - Raw entries are read straight in and checked there.
  - Compressed entries are read into reused buffers, then their chunks decompress into
    the target in parallel.
  - `update` does the reads and decoding.
- **`StreamThread`** runs a `PackStreamer` on its own thread: `request` / `poll` from the
  frame, a job pool of its own for decompression.
- **`engine_assets::open_scene`:** a pack's scene and meshes, the textures' records, and
  their entries in file order, tails first (`SceneLayout`, `TextureEntry`). `load_pack`
  is built on it.
- `scene_bench --pack <file> --stream` wires it together. A scene streaming system, which
  decides what to load, lives with the scene model when it comes (below).

## Consequences

- A scene appears with every texture (blurry) within about 15 ms of its first frame on
  NVMe and 30–60 ms on a hard disk. Full resolution follows while it runs, and the frame's
  thread spends microseconds on it.
- The texture pool's memory is still whole images: streaming saves time, not memory, yet.
- Next steps:
  - **which levels to load:** the GPU reports each texture's finest sampled level
    (feedback from the material shader); the streaming thread loads what is missing,
    nearest first;
  - **a memory budget:** evict levels nobody samples by copying the resident levels into
    a smaller image (`TRANSFER_SRC`);
  - mesh detail levels streamed the same way;
  - a home for the glue (entries to textures to requests), once the scene model owns
    assets.
