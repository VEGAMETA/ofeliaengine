# Reference hardware

Machines whose numbers go into `history.md` and the performance gates (CLAUDE.md §5).
Data comes from the `Runner info` workflow (`.github/workflows/runner-info.yml`);
re-run it after any hardware or driver change and update this file.

## ref-1: developer desktop (self-hosted runner)

Collected 2026-09-25 by `Runner info` (commit a103582, run 36193238273).

| Component | Value |
|---|---|
| OS | Windows 11 Pro 24H2, 10.0.26100 |
| CPU | AMD Ryzen 7 7700 (Zen 4), 8 cores / 16 threads, one CCD |
| RAM | 64 GB, 6000 MT/s |
| Storage | 2x Apacer AS2280Q4 1 TB NVMe SSD, WD10EZEX 1 TB SATA HDD |
| GPU0 (discrete) | NVIDIA GeForce RTX 4060 Ti (Ada, `10de:2805`), driver 616.92, Vulkan 1.4.351, conformance 1.4.3.3 |
| GPU1 (integrated) | AMD Radeon Graphics (Raphael, RDNA2, 2 CU, `1002:164e`), driver 25.10.36, Vulkan 1.4.315 |
| Displays | 1920x1080 @ 239 Hz on the NVIDIA GPU; 1920x1080 @ 60 Hz on the iGPU |
| Vulkan SDK | 1.4.350.0 (`D:\SDKs\Vulkan1.4.350.0`) |

Implicit Vulkan layers installed on this machine (they inject into every Vulkan process):
`VK_LAYER_OBS_HOOK`, `VK_LAYER_RTSS` (RivaTuner), `VK_LAYER_VALVE_steam_overlay`,
`VK_LAYER_VALVE_steam_fossilize`, `VK_LAYER_NV_optimus`, `VK_LAYER_NV_present`,
`VK_LAYER_AMD_switchable_graphics`. Overlays and capture hooks change frame timing and
latency, so **benchmark and latency runs set `VK_LOADER_LAYERS_DISABLE=~implicit~`**
(Vulkan loader 1.3.234+) and record that in `history.md`. Tests that must see what players
see (overlay compatibility) run with the layers enabled.

What this machine covers:

- NVIDIA (priority vendor): `VK_NV_low_latency2`, DLSS via user-supplied Streamline DLLs,
  mesh shaders, ray tracing. The 240 Hz display on it suits frame pacing, VRR and
  input-to-photon measurements.
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
