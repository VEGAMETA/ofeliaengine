# Sorting on the GPU

Research for queue item 1 (GPU particles and GPU radix sort), 2026-10-10. Particles,
transparent instances and, later, light and decal binning need keys sorted on the GPU,
with the number of keys known only to the GPU. Decision:
[ADR 0080](../adr/0080-gpu-radix-sort.md).

## Sources (checked 2026-10-10)

| Source | License | How it sorts | What we take |
|---|---|---|---|
| AMD FidelityFX Parallel Sort ([GPUOpen-Effects/FidelityFX-ParallelSort](https://github.com/GPUOpen-Effects/FidelityFX-ParallelSort), `FFX_ParallelSort.h`; also in the FidelityFX SDK 1.x) | MIT | LSD radix, 4-bit digits; per digit five dispatches: count (a histogram per workgroup, a column per thread in shared memory), reduce (sums of chunks per bin), scan (one workgroup), scan add, scatter (128 keys at a time sorted by the digit in shared memory with two 2-bit splits on packed 8-bit counters, then written after the digit's earlier keys); 128 threads × 4 keys per block; at most 800 workgroups, each taking several blocks; direct and indirect (a setup dispatch writes the dispatch sizes) | The whole structure; the code is ours (Slang, buffer addresses) |
| Merrill and Garland, "Single-pass Parallel Prefix Scan with Decoupled Look-back" (NVIDIA tech report NVR-2016-002), used by CUB's scans and Onesweep (Adinets and Merrill 2022) | patented: [US 9,928,033 B2](https://patents.google.com/patent/US9928033B2/en), "Single-pass parallel prefix scan with dynamic look back" (NVIDIA, filed 2013) | One pass per digit: each workgroup waits for the prefix of the ones before it, looking back over their published partial sums | Nothing: the team avoids patents, and the look-back needs forward progress between workgroups, which Vulkan does not promise (a GPU may not schedule the workgroup waited for: a hang) |
| T. Smith et al., "Decoupled Fallback: A Portable Single-Pass GPU Scan" (Google, 2025) | paper | The look-back made safe without forward progress guarantees | Nothing yet: it builds on the patented look-back |
| Bitonic sort (Batcher 1968), the classic GPU particle sort (Wicked Engine's particles used it) | public domain algorithm | O(n log² n) compare-exchange passes | Kept in mind for one-workgroup sorts of a few thousand keys (not built) |

Reduce-then-scan radix sorting itself is decades old (Zagha and Blelloch, "Radix sort for
vector multiprocessors", 1991; Satish, Harris and Garland, 2009).

## Measured (ref-1, release, `sort_bench`)

GPU time of the sort pass, µs: random keys copied in every frame, median of 25 frames.

| keys | RTX 4060 Ti: 32-bit / + payload / 16-bit + payload | Radeon iGPU: 32-bit / + payload / 16-bit + payload |
|---:|---:|---:|
| 16 K | 93 / 95 / 50 | 306 / 322 / 163 |
| 64 K | 101 / 111 / 56 | 968 / 1,078 / 541 |
| 256 K | 198 / 212 / 108 | 3,529 / 4,001 / 2,006 |
| 1 M | 530 / 586 / 295 | 8,378 / 9,931 / 4,974 |
| 4 M | 1,762 / 2,739 / 1,391 | 24,300 / 29,613 / 14,796 |

- Small sorts are dispatch-bound: 41 dispatches with a barrier after each for 32-bit keys
  (2.3 µs each on the RTX, 7.5 on the Radeon); 16-bit keys take half.
- The scatter's local sort dominates on the Radeon: about 20 workgroup barriers per 128
  keys, on two compute units.

## Ways to make it faster (not built)

- A one-workgroup sort in shared memory for up to a few thousand keys (one dispatch).
- 8-bit digits: half the passes, larger histograms.
- A local sort of a workgroup's 512 keys at once, in registers and with fewer barriers.
  Ranking keys within a wave by ballots (multisplit) appears in later NVIDIA work; check
  the patents first.
