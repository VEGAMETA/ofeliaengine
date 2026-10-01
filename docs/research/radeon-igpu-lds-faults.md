# Radeon iGPU of ref-1: groupshared memory faults under CPU load

Investigation of the flaky `a_scene_draws_what_the_same_draw_list_draws` test
(`crates/engine_render/tests/gpu_scene.rs`) and of other image comparisons that failed
now and then on the Radeon only. Measured on ref-1 (`docs/perf/reference-hw.md`) on
2026-10-01: Ryzen 7 7700 with its Raphael iGPU (RDNA2, 2 CU, `1002:164e`), AMD driver
25.10.36 (Vulkan 1.4.315, "AMD proprietary shader compiler"), Windows 11 24H2, RAM at
6000 MT/s (its SPD speed is 4800: an EXPO profile).

## Conclusion

**The fault is in the GPU (or the platform it runs on), not in the engine.** While the CPU
is busy, reads of groupshared memory (LDS) on the iGPU sometimes return wrong data: almost
always in lane 24 of a 32-wide wave, with one bit flipped (mostly bit 9). A trivially
correct compute shader, checked against the same computation on the CPU, shows it:

| Run (`tests/gpu_selfcheck.rs`, 500 dispatches of 2¹⁸ threads, one LDS exchange each) | Wrong dispatches |
|---|---|
| Radeon, CPU idle | 0 of 500 |
| Radeon, 16 busy CPU threads (in the test process) | 399–1642 of 500–2000 (70–80 %) |
| RTX 4060 Ti, 16 busy CPU threads | 0 of 500 |

Wrong threads per wave lane in one loaded run:
`lane 24: 1509, lane 23: 9, lane 11: 5, lane 18: 1, all others 0`. Typical wrong value:
`fb4488c9` instead of `fb450ac9` — the value read from LDS differs from the one written by
a bit (and the carry it causes in the mixing step).

The engine's own frames behave the same way: 16 busy processes at *idle* priority made
them differ as often (453 of 1500 frames) as 16 at normal priority (411 of 1500), so it is
not thread scheduling. A fault pinned to one lane and one bit, that follows the CPU's load
and that
no API-level synchronization changes, is an electrical or firmware problem of the iGPU on
this machine: an unstable voltage/frequency point of the SoC (memory overclock, SoC voltage,
an undervolt) or a power-management fault. It cannot be fixed or reliably worked around in
the engine.

## Why the engine's images changed

Everything that differed between runs on the Radeon went through LDS:

- **Vertex shaders.** The driver compiles vertex shaders as NGG primitive shaders; most of
  ours get its primitive culling with vertex compaction, which moves positions, indices and
  primitive IDs between threads through LDS (`ds_write_b128` / `ds_read_b128` around
  `s_barrier`). A flipped bit there is a crack, a missing triangle, a spike to a far-away
  point or a missing object.
- **Fragment shaders.** Interpolated inputs are read from LDS (`v_interp_p1/p2_f32`); a flipped
  bit is a wrong color or lighting value on a few pixels. Flat inputs (`v_interp_mov_f32`)
  showed no errors in 10 000 frames, interpolated ones in 25–28 % of frames under load.
- Compute shaders without LDS, buffer copies (48 GB moved under load) and the CPU
  (`memtest`-style check, 16 threads, 10 min) showed no errors.

The flaky test failed in about 3 % of runs with three copies of it on the Radeon (their CPU
use is the load), never with one copy on an idle machine, never on the RTX. The golden
test `async_compute_renders_the_same_bytes_as_one_queue` (`ENGINE_ASYNC_COMPUTE=1`, ambient
occlusion) failed in 33 of 40 runs on the Radeon with 16 busy CPU threads, 5 of 40 while
another agent's build kept the CPU at 98 %, and 0 of 20 on the RTX with 16 busy threads.

## What was ruled out (each with the stress repro below)

- **Render graph barriers.** A full `ALL_COMMANDS` memory barrier before every pass, and
  splitting the frame into separate submissions with `vkDeviceWaitIdle` between them, left
  the failure rate unchanged.
- **Host writes and the upload ring.** The ring's bytes are identical frame to frame (hashed
  after each frame); putting it in uncached, write-combined or cached-coherent memory, or
  sleeping 2 ms before submit, changed nothing. A copy of the ring read back by the GPU
  always matched what the CPU wrote.
- **Readback.** A sentinel fill of the readback buffer never showed through; uncached
  readback memory behaved the same.
- **Transient aliasing.** Disabling aliasing changed nothing (and this frame does not alias).
- **Breadcrumbs, timestamp queries, `VK_EXT_device_fault`, the validation layer,
  multi-draw indirect, culling, shadows:** each turned off in turn, the failures stayed.
- **The engine's shaders.** A raw-Vulkan pipeline with no engine code (a cube placed by
  arithmetic, eight interpolated `float4` outputs, a fragment shader summing them) differs
  between identical frames in 25–28 % of frames under CPU load on the Radeon, 0 % when idle,
  0 % on the RTX.

## Repro and check

```text
# The hardware check (fails on a faulty machine; opt-in):
ENGINE_ADAPTER=radeon ENGINE_REQUIRE_GPU=1 cargo test -p engine_render --test gpu_selfcheck -- --ignored --nocapture
#   ENGINE_SELFCHECK_CPU_LOAD=0       no CPU load (passes on ref-1)
#   ENGINE_SELFCHECK_DISPATCHES=2000  more dispatches
```

The shader is `crates/engine_render/tests/shaders/selfcheck.slang`.

## What to do

- **On ref-1:** check the BIOS settings of the SoC (memory/EXPO profile, SoC voltage,
  Curve Optimizer or other undervolting including the iGPU's, power limits) and the AMD
  driver version, then run the self-check under load until it passes. Until then a
  mismatch in an exact image comparison on the Radeon under CPU load is a hardware fault
  first. The same instability may explain other abnormal exits seen under heavy load on
  this machine (silent exits, an access violation at process exit), though the CPU-side
  check here found no errors.
- **In the engine:** nothing to change. Every vertex and fragment shader goes through LDS
  on this hardware, so there is no zero-cost workaround; a game on a machine like this
  would show rare one-frame glitches, which only the platform fix removes.
- **For AMD:** the self-check is a minimal reproducer (one compute shader, no engine code).
