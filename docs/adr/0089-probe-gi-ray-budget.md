# 0089. Probe GI, phase 2: a ray budget per frame, around the camera first

- Status: Accepted
- Date: 2026-10-11
- Research: [docs/research/global-illumination.md](../research/global-illumination.md)
- Related: [ADR 0084](0084-global-illumination.md) (the two phases),
  [ADR 0088](0088-probe-gi-volume.md) (phase 1: the volume converged at load),
  [ADR 0087](0087-acceleration-structures.md) (the top level refitted as instances move),
  queue item 3

## Context

Phase 1 converges the volume at load and then freezes it: a sun that moves, lights
switched on or off, a door that opens leave the light as it was until `refresh()`
converges the whole volume again, which takes 11 updates of every probe (15 ms on the
RTX 4060 Ti, 1.1 s on the two-CU Radeon for Sponza's 8,680 probes). Phase 2 keeps the
light current at a cost per frame the game can afford on both GPUs.

The update passes already blend a probe's rays into its maps with a share (`alpha`).
What changes is which probes are traced in a frame and how much of the old light each
update keeps.

## Options

1. **Which probes a frame traces.**
   a. Every probe every frame with few rays (DDGI's original: 256 rays per probe per
      frame with a hysteresis of 0.97). Sponza: 1.1 M rays a frame, about 1.4 ms on the
      RTX and 64 ms on the iGPU. Out of reach for the iGPU.
   b. **A window of probes, in turn**: as many as a ray budget pays for, after the last
      window. The whole volume refreshes at a rate set by the budget.
   c. **Part of the budget on the probes around the camera**: they light what the player
      sees, so they follow a change first; the rest goes round the volume.
   d. Rays where the light changes (Wicked Engine's allocation by each probe's
      inconsistency, a tenth outside the view): the best use of a budget, but it needs a
      variance per texel, its readback or a GPU-built list. Left for later.
2. **How much an update keeps.** A fixed hysteresis (`GiSettings::hysteresis`, the share
   of a probe's old light each update keeps), or one lowered where the light changed by a
   lot (DDGI's change detection). The second reacts faster but turns noise into flicker
   where a probe's 128 rays disagree; it waits for 1d's variance.
3. **The camera box.** A fixed box of probes (8 × 4 × 8) centred on the camera's cell,
   clamped to the volume, traced in turn: the camera moving shifts the box, its cursor
   goes on. Or a box sized by the budget.

## Measurements

ref-1, release, 2026-10-11. Sponza at 1920 × 1080 (`scene_bench --gi`), 8,680 probes,
128 rays a probe, GPU time of the frame's GI updates after convergence (median):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Budget (probes × rays) | 1,024 × 128 | 64 × 128 |
| One window, in turn (1b) | 186 µs | 758 µs |
| Two windows: half around the camera (1c) | 205 µs | 1.0 ms |
| Of which trace / irradiance / moments | 110 / 20 / 75 µs | 697 / 91 / 212 µs |
| Each probe of the volume traced every | 11 frames (1b: 8.5) | 271 frames (1b: 136) |
| Each probe of the camera box traced every | frame (256 probes) | 8 frames |

**Responses** (`tests/render/gi.rs`, both GPUs alike):

| Test | Expected | Measured |
|---|---:|---:|
| Sunlit floor (405 probes, every probe traced every frame), 7 frames after the sun is turned off | 0.9^7 = 0.478 | 0.478 |
| The same, 40 frames after | 0.9^40 = 1.48 % | 1.47 % |
| 8,405 probes over a 20 m floor, 64 probes a frame, half in the camera box: light left by the camera after 64 frames | about 0.9^8 | 39 % |
| The same, at the far corner | untouched | 100 % |

The moments' blend is a third of the cost: each of a probe's 196 texels weighs all 128
rays under a sharp lobe.

On the iGPU the second window adds ~0.2 ms. That is the fixed cost of three more small
dispatches on two compute units.

## Decision

Options **1b with 1c, a fixed hysteresis, a fixed camera box**:

- **The budget.** After convergence, every frame traces `GiSettings::rays_per_frame / rays`
  probes. `for_device` gives 131,072 rays on discrete GPUs and 8,192 on integrated ones.
  `0` freezes the light, as in phase 1.
- **The split.** A share of the budget, `local_share` (0.5), goes round the box of
  `LOCAL_BOX` (8 × 4 × 8) probes around the camera, each at most once a frame. The rest
  goes round the whole volume from where the last window ended.
- **Two updates, one after the other.** The two windows are two updates in the frame's
  graph, which orders them: no probe is written by two passes at once, so the result is
  deterministic.
- **The blend.** Each update keeps `hysteresis` (0.9) of a probe's light. Rays from
  converged updates always add the volume's light at their hits (another bounce).
- **How a window is described.** `TraceFrame::first_probe` and `window` give the slots;
  `box_origin` and `box_size` give the box's probes, or the whole volume when
  `box_origin.w` is 0. The ray results are stored by slot, and the transient buffer is
  sized for the larger window.
- **Relocation and classification** stay in the convergence (ADR 0088). A probe that
  geometry moves into stays on until the next `refresh()`.
- **The camera** comes from the view (`GiFrame::add_update_passes`'s `eye`).

## Consequences

- **Following changes.** The light follows the sun, the sky, emission and moving geometry
  without `refresh()`, around the camera first.
  - On the RTX, Sponza's whole volume refreshes every 11 frames: 90 % of a change after
    22 refreshes, about 1.7 s at 144 fps, and within 22 frames in the camera box.
  - On the iGPU the camera box takes 8 frames a refresh (about 7 s for 90 % at 25 fps).
    The rest of the volume is far slower: `refresh()` after a big change (a level's lights
    switched) converges it at once.
- **Cost.** About 0.2 ms a frame on the RTX and 1.0 ms on the iGPU, beside the resolve
  and the shading of ADR 0088.
- **Zero allocations.** `zero_alloc/gpu_scene.rs` gives 0 heap allocations in 100 frames
  of converged updates on both GPUs. The frame that ends the convergence changes the
  frame's layout: its smaller ray buffer is a new graph object.
- **Golden shots.**
  - Temporal shots taken after convergence now include updates: `sponza_day` changed on
    the RTX by 0.16 levels on average, and was rewritten.
  - The golden bed converges GI an update a frame. Eleven updates in one submission took
    about a second on the iGPU. Beside other tests run as threads of one process, that
    led to GPU resets. The same resets hit the MSAA shots on main when two of them run as
    threads; the gate runs tests as processes (nextest) and is unaffected.
- **Next:**
  - Brixelizer GI built as the reference (ADR 0084's bar).
  - Point and spot lights at the rays' hits, so the night scenes get bounced light.
  - The moments' blend at a lower rate than the irradiance's.
  - Rays allocated by the light's change (1d) with change-aware hysteresis (2).
