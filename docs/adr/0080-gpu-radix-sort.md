# 0080. GPU radix sort: reduce-then-scan, 4-bit digits, counts from the GPU

- Status: Accepted
- Date: 2026-10-10
- Research: [docs/research/gpu-sort.md](../research/gpu-sort.md)
- Related: [ADR 0052](0052-compute-passes.md) (compute passes),
  [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (subgroup requirements), queue item 1
  (GPU particles)

## Context

Particles must be drawn back to front, and their number lives on the GPU: the CPU never
reads it back. The engine had no GPU sort. The same primitive will serve transparent
instances, binning of lights and decals, and sorting work by material.

## Options

1. **Reduce-then-scan LSD radix sort** (chosen): per 4-bit digit, per-workgroup
   histograms, their scan, a stable scatter. Several dispatches per digit, no
   communication between workgroups inside a dispatch. AMD's FidelityFX Parallel Sort
   (MIT) is a well-measured reference, and the method is decades old.
2. **Single-pass radix sort with decoupled look-back** (Onesweep, CUB): the fastest on
   NVIDIA, but NVIDIA patents the look-back (US 9,928,033 B2), and it needs forward
   progress between workgroups, which Vulkan does not promise (a hang on a GPU that does
   not schedule the workgroup being waited for).
3. **Bitonic sort:** simple, O(n log² n): fine for a few thousand keys, slow beyond.

## Measurements

`sort_bench` (ref-1, release): GPU time of the sort pass in µs, random keys, median of
25 frames.

| keys | RTX 4060 Ti: 32-bit, + payload, 16-bit + payload | Radeon iGPU: same |
|---:|---:|---:|
| 16 K | 93, 95, 50 | 306, 322, 163 |
| 64 K | 101, 111, 56 | 968, 1,078, 541 |
| 256 K | 198, 212, 108 | 3,529, 4,001, 2,006 |
| 1 M | 530, 586, 295 | 8,378, 9,931, 4,974 |
| 4 M | 1,762, 2,739, 1,391 | 24,300, 29,613, 14,796 |

- 1.8 billion keys with a payload per second on the RTX at 1 M keys; 106 million on the
  Radeon iGPU (two compute units).
- Below about 64 K keys the time is the dispatches': 41 for 32-bit keys, each followed by
  a barrier (2.3 µs on the RTX, 7.5 on the Radeon); 16-bit keys need half.
- Correctness: the test sorts 1 to 1 M keys (partial blocks, one beyond 800 workgroups),
  70 K keys with 37 distinct values (the payload shows they keep their order), 16-bit
  keys, a count written by the GPU (the keys after it untouched) and one above the
  capacity. All match the standard library's stable sort on both GPUs, with
  synchronization validation clean.

## Decision

- **`engine_render::sort::GpuSort`**: `add_pass(fg, bindless, name, SortJob { keys,
  values, capacity, count, key_bits })` sorts the first keys of a `u32` buffer in place,
  ascending, with an optional `u32` payload, in one compute pass of the frame graph:
  - `SortCount::Keys(n)`, or `SortCount::Gpu { buffer, offset }`: a count an earlier pass
    wrote, clamped to the capacity;
  - `key_bits`: only the low bits are sorted. The number of 4-bit passes is rounded up to
    an even count, so the keys end where they started;
  - scratch memory is the frame's: `capacity` × 4 bytes per buffer sorted, and about
    52 KB.
- **The shaders** (`sort.slang`) follow FidelityFX Parallel Sort's structure:
  - 128 threads × 4 keys per block, at most 800 count and scatter workgroups;
  - a setup dispatch writes the constants and the indirect dispatch sizes;
  - credited in THIRD_PARTY_NOTICES.md.
- **`ComputeRecorder::dispatch_indirect`** and **`barrier_for_indirect`** (ADR 0052's
  compute passes).
- **Devices must have subgroup arithmetic in compute** (wave sums and prefix sums) besides
  basic, vote and ballot (ADR 0034). Every Vulkan 1.3 desktop GPU has it; both reference
  GPUs do.
- The scans take a workgroup's waves to be consecutive invocations (as FidelityFX does).
  Vulkan does not promise it; NVIDIA and AMD do it, and the test would fail otherwise.

## Consequences

- Particles (queue item 1) sort by depth on the GPU. Others can sort too: transparent
  instances, light and decal bins.
- Small sorts cost their dispatches: 50 µs (RTX) and 163 µs (Radeon) for 16 K particles
  with 16-bit depth keys.
- Not done (measured first when a user needs it):
  - a one-workgroup sort for small counts;
  - 8-bit digits;
  - a register-resident local sort with fewer barriers (the Radeon's scatter);
  - wave-level ballot ranking (check the patents first).
