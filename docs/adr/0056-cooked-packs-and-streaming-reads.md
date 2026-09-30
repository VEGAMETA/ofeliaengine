# 0056. Cooked packs and streaming reads: our own container, an I/O ring, zstd per entry

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/assets-streaming.md](../research/assets-streaming.md)
- Related: [ADR 0033](0033-gltf-import.md) (glTF import),
  [ADR 0041](0041-texture-compression.md) (BC7/BC5, derived data cache),
  [ADR 0053](0053-mesh-lods.md) (detail levels)

## Context

Scenes were loaded from their sources every time:

- the glTF file parsed;
- every texture read and hashed to find its compressed form in the derived data cache
  (one file per texture);
- detail levels computed.

That path needs the sources and the cache, neither of which a game ships. It is also
slow from a cold disk. Sponza (a 104.5 MB scene once cooked) takes **3,078 ms** from the
hard disk of ref-1 (151 files read in scattered order) and 45 ms from its NVMe drive.

CLAUDE.md §13 asks for:

- a binary runtime format loaded with zero copies (evaluating `rkyv`);
- Windows 11 I/O rings for batched reads;
- research on GPU decompression.

## Options

### The container

1. **Our own pack** (chosen):
   - a header, entries each aligned to 4 KiB, a table of fixed 80-byte records at the
     end;
   - an entry's bytes are what the GPU or the engine takes: BC7 levels, vertex arrays,
     POD records;
   - the table is read in place with `bytemuck`, and entries are read unbuffered
     straight into memory.
2. **`rkyv` archives:** zero-copy access to rich Rust structures.
   - Our entries are flat arrays with no pointers, so nothing needs its relative
     pointers.
   - The archive layout depends on the crate's version and derives, a poor fit for a
     file format we version ourselves.
   - Worth another look for rich runtime data (scene graphs with strings) later.
3. **KTX2 files plus a mesh format, one file per asset:** standard for textures, but one
   file per texture is what makes the hard disk seek (above), and meshes, materials and
   scenes would still need a container.

### Reading

1. **Unbuffered reads through an I/O ring** (`CreateIoRing`, Windows 11; chosen).
   - Many reads per system call, straight into the destination, without the OS cache's
     copy.
   - Measured, 64 KiB reads at random offsets: NVMe 3.7 GB/s with 32 in flight against
     0.4 GB/s one at a time; hard disk 29 against 12 MB/s.
   - Large sequential reads: 4.4 GB/s (8 MiB) and 126–139 MB/s.
2. **Overlapped reads with an I/O completion port:** the same queue depth, with a system
   call per read. Kept out: the ring does the same with fewer calls, and a synchronous
   fallback covers systems without a ring.
3. **Buffered `std::fs::read`:** 5.8 GB/s when the OS cache already holds the file (a
   second load); the first load pays the drive and a copy, and the cache holds a second
   copy of everything.
4. **DirectStorage:** D3D12 only (CLAUDE.md §13).

### Compression

Measured on the cooked data (160.8 MB of BC7/BC5 textures, 12.8 MB of glTF geometry):

| codec | textures | geometry | decode, one thread |
|---|---:|---:|---:|
| lz4 | 77.8% | 75.0% | 4.4 GB/s |
| **zstd 19** (chosen) | **65.1%** | **42.4%** | **0.8–1.0 GB/s** |
| zstd 3 | 68.9% | 49.7% | 1.0–1.3 GB/s |
| deflate 9 | 68.4% | 59.3% | 0.5 GB/s |

- zstd saves a third of the textures and more than half of the geometry. Decoding costs
  the same at every level, so the cook uses 19; it takes 2.2 s for Sponza on 16
  threads.
- **Chunks of 1 MiB**, each compressed alone (stored raw when it does not shrink by 5%):
  - any entry loads alone, and one large entry decompresses on many threads (a 4K
    texture's top level is 16 chunks);
  - chunking costs little ratio:

    | chunk | textures | geometry |
    |---|---:|---:|
    | whole entry | 65.1% | 42.4% |
    | **1 MiB** | **65.7%** | **44.7%** |
    | 256 KiB | 66.6% | 49.6% |
    | 64 KiB | 68.1% | 55.8% |

  - the chunk table lives in the table of contents, so an entry whose chunks are all raw
    is stored as its raw bytes and can be read straight to where it is used.
- **GPU decompression (GDeflate):** deflate compresses BC7 no better than zstd here
  (68%), and on the iGPU it would take GPU time from rendering. Worth measuring once CPU
  decoding limits loads on fast drives. Today it does not: Sponza's raw pack loads in 29
  ms from NVMe, its zstd pack in 34 ms.

## Numbers

Sponza, cooked with detail levels (361 entries: 71.5 MB compressed, 104.5 MB unpacked;
`scene_bench --pack`):

| | hard disk | NVMe |
|---|---:|---:|
| glTF + derived data cache, cold | 3,078 ms | 45 ms |
| glTF + derived data cache, OS cache warm | 28 ms | 28 ms |
| **pack, zstd** (always from the drive) | **520–663 ms** | **34–36 ms** |
| pack, raw (104.5 MB) | 868–1,073 ms | 29–30 ms |

- zstd plus hash checks: 108–177 ms of CPU across 16 threads. The raw pack's checks take
  7–14 ms.
- The hard disk reads the pack at 107–136 MB/s, its sequential maximum.
- The frame is identical to the glTF path's, byte for byte, with and without detail
  levels.

## Decision

- **`engine_platform::io`:**
  - `ReadFile` (unbuffered), `AlignedBuffer` (4 KiB aligned);
  - `ReadQueue`: an I/O ring, or synchronous reads where there is none (older Windows,
    Linux CI);
  - a read owns its buffer until its completion hands it back, so no memory can be
    reused while the drive writes to it.
- **`engine_assets::pack`**, format version 1:
  - a 64-byte header: magic `OFPK`, version, entry and chunk counts, chunk size, table
    offset, size and hash, file length;
  - entries aligned to 4 KiB, each its chunks one after another;
  - a table of contents of 80-byte `Entry` records (content id, offset, sizes, chunks,
    kind, 28 bytes of kind-specific meta), then 16-byte `Chunk` records (offset, stored
    size with a raw flag, `state_hash64` of the raw bytes);
  - opening checks every entry's and chunk's bounds; loading checks every chunk's hash,
    so damage is reported to the chunk;
  - ids are content hashes, so duplicates are stored once;
  - `Pack::load` keeps up to N reads in flight, decompresses and checks each entry's
    chunks as parallel jobs, and hands raw entries over in their read buffers (no copy).
- **`engine_assets::cook`:**
  - `CookedScene`: textures with their levels, meshes with detail levels, materials and
    instances;
  - `cook_scene` builds it from glTF, through the derived data cache, textures and
    detail levels in parallel;
  - `write_pack` and `load_pack` store and read it. Texture, mesh and scene entries are
    laid out as the module documents;
  - **each texture is split for streaming:**
    - its levels of at most 128 texels share one entry, the tail (22 KiB of BC7);
    - each larger level has an entry of its own;
    - the scene's `TextureRecord` maps levels to entries;
  - **the file is in load order:** meshes and every tail first (the whole scene at low
    resolution, read in one pass on a hard disk), then the large levels from the
    smallest, the scene record last.
- **`ofeliaengine cook <scene.gltf> -o <out.pack>`** (`--no-lods`, `--level N`, `--ddc`).
- `scene_bench --pack <file>`: both paths go through `CookedScene`.

## Consequences

- A game can ship packs instead of sources: loads are 5× faster from a hard disk, a
  little faster from NVMe, and a third smaller.
- The format is versioned: a change bumps `VERSION`, and older packs are refused with a
  clear message (re-cook).
- Data still passes through `Vec`s on the way to the GPU: one copy of 104 MB, about 10
  ms. Next steps, in order:
  - read uncompressed entries straight into the texture pool's staging memory;
  - store each large mip level as its own entry and stream levels by screen size
    (texture streaming);
  - the same for mesh detail levels;
  - hot reload of cooked assets (cook on change, swap);
  - measure GPU decompression if CPU decoding becomes the limit.
- The writer needs one entry in memory at a time. Entries are compressed in parallel
  before writing, so a cook holds its compressed entries at once (71 MB for Sponza).
