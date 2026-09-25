# Reference hardware

Machines whose numbers go into `history.md` and the performance gates (CLAUDE.md §5).
Data comes from the `Runner info` workflow (`.github/workflows/runner-info.yml`);
re-run it after any hardware or driver change and update this file.

## ref-1: developer desktop (self-hosted runner)

Collected 2026-09-25 by `Runner info` (run #1 on `bootstrap/workspace`).

| Component | Value |
|---|---|
| OS | Windows 11 Pro 24H2, 10.0.26100 |
| CPU | AMD Ryzen 7 7700 (Zen 4), 8 cores / 16 threads, one CCD |
| RAM | 64 GB |
| dGPU | NVIDIA GeForce RTX 4060 Ti (Ada), driver 32.0.16.1692 (≈ 616.92) |
| iGPU | AMD Radeon Graphics (Raphael, RDNA2, 2 CU), driver 32.0.21036.18 |
| RAM speed, disk, display | not collected yet (added to the workflow, next run) |
| Vulkan | not collected yet (the first run failed on the Vulkan step) |

What this machine covers:

- NVIDIA (priority vendor): `VK_NV_low_latency2`, DLSS via user-supplied Streamline DLLs,
  mesh shaders, ray tracing.
- AMD (priority vendor): the iGPU runs the AMD driver, which covers `VK_AMD_anti_lag` and AMD
  driver behavior. It is a very weak GPU, so it is used for correctness, not for performance.
- Multi-GPU: two Vulkan devices on one machine exercise device selection
  (the dGPU must be picked by default).
- AVX-512 (Zen 4): the widest runtime-dispatch path of §5 can be tested here.

What it does **not** cover:

- Intel GPUs (Arc or iGPU): no Intel GPU tests yet.
- Hybrid CPUs (P/E cores, `EfficiencyClass`): Zen 4 desktop has no E-cores. The thread
  placement code of §5 needs an Intel 12th gen+ or Zen 5c machine.
- It is not a dedicated benchmark machine. Background load adds noise, so on this machine
  the 3% regression gate is advisory, not blocking, until a dedicated machine exists.
