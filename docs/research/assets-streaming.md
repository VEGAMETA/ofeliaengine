# Cooked assets and streaming reads

Research for the third priority (2026-09-30): assets and streaming. Decision:
[ADR 0056](../adr/0056-cooked-packs-and-streaming-reads.md).

Checked on 2026-09-30 on ref-1:

- a hard disk (WDC WD10EZEX, SATA, 512-byte sectors) holds the repository;
- an NVMe drive (Apacer AS2280Q4) holds `C:`.

Codecs and I/O methods were measured with a throwaway tool; engines are described from
their public documentation.

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| Windows I/O rings (`ioringapi.h`: `CreateIoRing`, `BuildIoRingReadFile`, `SubmitIoRing`) | Windows 11 22000+ (ring version 4 here) | OS API, through `windows-sys` 0.61 (MIT OR Apache-2.0) | batched reads |
| `FILE_FLAG_NO_BUFFERING` | Win32 | OS API | reads straight into the caller's memory; offsets, sizes and buffers sector-aligned |
| zstd | 1.5.7 through `zstd` 0.13 | BSD-3-Clause (crates MIT / MIT OR Apache-2.0) | entry compression |
| lz4_flex 0.11, miniz_oxide 0.8 | — | MIT; MIT OR Zlib OR Apache-2.0 | measured, not used |
| rkyv | 0.8 (docs) | MIT | evaluated for zero-copy formats, not used |
| DirectStorage and GDeflate | public docs | — | GPU decompression's design; D3D12 only |
| Unreal (IoStore `.utoc`/`.ucas`, Zen loader), Unity (AssetBundles, Addressables) | public docs | products | a table of contents plus aligned chunks; per-chunk compression |

## Compression of cooked data

80 BC7/BC5 textures with their mips (160.8 MB, the derived data cache of Sponza and
FlightHelmet) and the two scenes' glTF geometry (12.8 MB); one thread; each file
compressed alone:

| codec | textures size | geometry size | encode | decode |
|---|---:|---:|---:|---:|
| lz4 | 77.8% | 75.0% | 1,480 MB/s | 4,449 MB/s |
| zstd 3 | 68.9% | 49.7% | 357 MB/s | 1,296 MB/s |
| zstd 9 | 66.9% | 48.5% | 105 MB/s | 1,346 MB/s |
| zstd 19 | 65.1% | 42.4% | 10 MB/s | 1,027 MB/s |
| deflate 6 | 68.5% | 59.3% | 37 MB/s | 491 MB/s |
| deflate 9 | 68.4% | 59.3% | 22 MB/s | 490 MB/s |

- BC7 is already dense: general-purpose codecs save a third at best.
  - Encoders with rate-distortion optimization (RDO BC7) trade a little quality for
    blocks that compress much better. That is a lever for later, a change to the texture
    cook, not to the pack.
- zstd decodes as fast at level 19 as at 3, so the cook spends the time once.
- On the cooked Sponza pack (textures, meshes with detail levels): 104.5 MB to 70.5 MB
  (67%).

## Reading from the drives

256 MiB file; unbuffered reads; the I/O ring with 8 or 32 reads in flight:

| method | hard disk | NVMe |
|---|---:|---:|
| `std::fs::read` (file in the OS cache) | 5.8 GB/s | 5.8 GB/s |
| 1 MiB reads, one at a time | 122 MB/s | 2.8 GB/s |
| 1 MiB reads, ring, 32 in flight | 127 MB/s | 2.8 GB/s |
| 8 MiB reads, one at a time | 139 MB/s | 4.2 GB/s |
| 8 MiB reads, ring, 32 in flight | 140 MB/s | 4.4 GB/s |
| 64 KiB at random offsets, one at a time | 12 MB/s (184/s) | 0.41 GB/s (6,279/s) |
| 64 KiB at random offsets, ring, 8 in flight | 25 MB/s | 2.6 GB/s |
| 64 KiB at random offsets, ring, 32 in flight | 29 MB/s (449/s) | 3.7 GB/s (57,203/s) |

- **Large sequential reads** saturate either drive with or without a ring.
- **Small scattered reads** (streaming mip levels, meshes) need many in flight:
  - 9× faster on NVMe;
  - 2.4× on the hard disk, whose queue reorders reads to save seeks.
- **The OS cache** serves a second read of the same file from memory. Streaming reads
  bypass it: a game's own caches (the texture pool) decide what stays in memory, and
  the OS cache would hold a second copy.

## Formats

- **Unreal's IoStore:**
  - a table of contents (`.utoc`) over one container (`.ucas`) of chunks;
  - chunks compressed in blocks, aligned for the platform's I/O, read in batches.
- **Unity's AssetBundles:** archives of serialized objects, LZ4 per chunk for loading
  (LZMA for downloads).
- **rkyv** gives zero-copy access to Rust structures with relative pointers. Cooked GPU
  data is flat (BC7 levels, vertex arrays): a table of fixed records read with
  `bytemuck` needs no more.
- Common to all: a table read first, entries aligned for unbuffered reads, compressed
  one by one so any subset loads alone.

## GPU decompression

- DirectStorage decompresses GDeflate on the GPU. GDeflate is deflate split into 64 KiB
  tiles decoded in parallel by a compute shader.
- The ratio is deflate's: 68% on BC7 here, no better than zstd.
- The gain is CPU time when a fast drive delivers more than the CPU decodes:
  - zstd decodes about 1 GB/s per thread; the NVMe drive gives 4.4 GB/s, so 4–5
    threads keep up;
  - Sponza's pack decodes in 116–241 ms of CPU spread over 16 threads, within 32 ms of
    wall time.
- On the Radeon iGPU a decompression pass takes GPU time from rendering.
- Revisit when loads are CPU-bound (open worlds streaming while playing).

## Measured

See ADR 0056:

- Sponza from its pack: 558–684 ms from the hard disk (3,078 ms from sources and cache),
  32–41 ms from NVMe (45 ms);
- the frame is identical to the source path's.
