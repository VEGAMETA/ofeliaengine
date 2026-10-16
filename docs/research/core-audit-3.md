# Core audit 3: a fresh-eyes review of the engine core (2026-10-16)

The third technical review of the core (`docs/status.md` §7), after items 0 to 8 of the
second audit's queue: the player crate, custom materials, meshlets, cached cascades and
virtual shadow pages, GI, frame generation, post effects, video, colliders and localization
(355 commits since `ed29fe4`, about 109,000 lines). Done by the coordinator alone, without
agents. Findings are ranked by impact, each with evidence and a cost estimate; the new queue
and the questions for the owner are at the end.

Machine: ref-1 (`docs/perf/reference-hw.md`): Ryzen 7 7700, RTX 4060 Ti (NVIDIA 616.92) and
the Radeon RDNA2 2-CU iGPU (AMD 26.8.1), Windows 11. Release builds,
`VK_LOADER_LAYERS_DISABLE=~implicit~`. Main at `b899ae0`.

## 1. Method

- **The flight** (`engine_app/examples/flight.rs`, the harness of audit 2, now a game on the
  player):
  - Sponza from its pack, its 103 instances as entities, textures and mesh levels streamed
    by feedback;
  - 2,000 cubes in spinning hierarchies, 64 skinned Fox characters walking, Sponza's 7
    shadowed local lights, sky light;
  - the device class's defaults: TAAU at native and AO High on the RTX, TAAU at Quality and
    AO Low on the Radeon, meshlets per class;
  - Reflex or Anti-Lag 2 on;
  - 1,800 frames of 1/60 s, 120 of warm-up left out.

  The window is a 1920 × 1080 one fit into the work area (`--window windowed`, 1797 × 1011):
  the borderless default would take the screen of whoever works at ref-1. The pipeline
  cache was missing, then warm. Variants: movers 0 to 20,000, characters 0 or 64, without
  local lights, AO, meshlet culling or antialiasing, with probes, GI and SSR, latency
  control off.
- `scene_bench --sky --pipeline-stats` on the Radeon for the forward shading's registers.
- Code read where the numbers pointed: the frame renderer's prepare and declare, the local
  lights' shadow cache (`local_shadows.rs`), the cascade cache, the GPU scene's change
  tracking, the player's frame, the flight's memory report.

## 2. The numbers, against audit 2

### A game frame (defaults, warm cache)

| | RTX 4060 Ti | | Radeon iGPU | |
|---|---:|---:|---:|---:|
| | audit 2 | now | audit 2 | now |
| GPU frame, median | 1.72 ms | **1.56 ms** | 26.9 ms | **21.8 ms** |
| CPU frame without the wait, median | 604 µs | **788 µs** | 920 µs | **1,308 µs** |
| — frame renderer: prepare + declare | 34 µs | **409 µs** | 40 µs | **551 µs** |
| — graph compile + record | 193 µs | 156 µs | 190 µs | 191 µs |
| — interpolation | 31 µs | 0.5 µs | 41 µs | — |
| — scene upload (CPU) | 60 µs | 33 µs | 72 µs | — |
| — animation (64 characters) + poses | 60 µs | 60 µs | 83 µs | — |
| Frames over twice the median interval | 0–1 | 1–2 | 0 | 0 |
| Pipelines created during the flight | 0 | 0 (25 with the cache missing) | 0 | 0 |

The Radeon's GPU passes now (mean per frame, ms):

- forward 9.0 (10.2 in audit 2);
- TAAU 3.0 (4.2);
- local shadows 2.2 (2.2);
- AO 1.8 (2.2);
- cascades 1.75 (2.4);
- tone mapping 0.66 (2.6: the present path's fix);
- bloom 1.0 (1.6);
- the copy into the swapchain 0.5 (the monitor is on the RTX: across adapters).

The RTX's: AO 539 µs (34 % of the frame), forward 385, local shadows 233, cascades 213,
TAAU 120.

### CPU per moving instance (RTX, 600 frames)

| movers, characters | prepare + declare | CPU frame |
|---|---:|---:|
| 0, 0 | 20 µs | 200 µs |
| 0, 64 | 98 µs | 442 µs |
| 2,000, 0 | 321 µs | 582 µs |
| 20,000, 64 | **3,265 µs** | **3,963 µs (CPU-bound; 1.82 ms in audit 2)** |
| 20,000, 64, no local lights | 32 µs | **662 µs** |

Without local lights the movers' path is 2.7 times cheaper than in audit 2 (GPU
interpolation, parallel stages): propagation 61, extraction 99, upload 194 µs at 20,000.
With them it is twice as expensive (§3.1).

### Start-up (time from `main` to the first frame presented)

| | RTX, cache missing | RTX, warm | Radeon, missing | Radeon, warm |
|---|---:|---:|---:|---:|
| audit 2 | 1,357 | 476–588 | 1,238 | 450–490 |
| **now** | **634** | **416** | **663** | **377** |
| — Vulkan instance + device | 234 | 240 | — | 172 |
| — renderer + features (pipelines) | 252 (46 compiled) | 6 | — | 14 |
| — surface + swapchain (beside the renderer) | 206 | 150 | — | 169 |

Device creation is now more than half of a warm start.

### Memory (RTX, end of the flight)

| | audit 2 | now |
|---|---:|---:|
| Device-local heap, OS usage | 599 MiB | **787 MiB** |
| — the allocator's blocks / allocated in them | 448 / — | **576 / 248 MiB** |
| Transient targets per frame slot | 66 MiB ×2 | 54 MiB, one heap for both |
| Process private bytes | 881 MiB | 1,276 MiB (789 of it the driver's write-combined mappings, Rust's own 38 MiB) |

CPU memory is now accounted for by the flight's report (audit 2's §3.9).

### Registers of the forward shading (Radeon, pixel VGPRs)

The default path draws motion in the depth prepass (8 VGPRs), so the forward shading uses
the variants without motion: plain 47, local lights 64, environment 64, lights and
environment 80, the rule's line. Without a prepass, motion adds 0 to 9 to them (the vertex
stage 78 against 49). Above 80: transparent surfaces under the atmosphere with lights 91,
decals with lights and the environment 90.

## 3. Findings, by impact

Costs are agent-days of implementation and measurement on both GPUs (S ≤ 2, M 3–7,
L > 7).

### 3.1 Local lights' shadows cost 150 ns per moving caster per frame (P0, S)

Every moved instance's old and new boxes are tested against every shadowed light's range,
and each box inside one is added to the light's `Stale` set
(`local_shadows.rs::invalidate`). Past `DIRTY_BOXES` the set merges each new box into the
one whose union grows least, a search over the set for every box. With 2,064 casters
moving every frame and 7 lights:

- 300 µs of the frame renderer's 321;
- at 20,000 movers, 3.2 ms, which makes the RTX CPU-bound at 3.96 ms (0.66 without local
  lights).

The cost came with "local lights' shadows redraw only where casters moved" (6b, after audit
2). Fix: past a few times `DIRTY_BOXES` boxes for a light, mark its shadow whole and stop
testing. The pieces would have covered its views anyway, which is what the cascade cache
does past 512 moves (`MAX_MOVES`). Expected: under 20 µs at 2,000 movers, the per-mover
cost back to the GPU scene's.

### 3.2 CPU costs have no regression gate (high, S–M)

§3.1 passed every gate since it landed: the perf gate (ADR 0062) times GPU passes, and no
test times the CPU stages of a frame. The flight measures them, by hand. Fix:
- an offscreen mode for the flight (the player has `Window::Offscreen`);
- a short run in `tools/check.ps1 -Perf` comparing each CPU stage's median with a baseline
  (the GPU gate's rule: 3 % and more than an absolute floor), on the scene with movers,
  characters and lights.

### 3.3 Device memory the allocator keeps (medium, S)

On the RTX the allocator's device-local blocks grew from 320 to 576 MiB during the flight
while the bytes allocated in them grew from 230 to 248. At the end 328 MiB of blocks held
nothing, and the OS counts them (787 MiB used). The growth came with the texture pool's
growth (733 µs at frame 426); the Radeon's blocks did not grow. The GPU memory budget (ADR
0076) sees less room than there is. Fix: free empty blocks beyond one per memory type
(after the frames in flight), and report blocks against bytes in the budget.

### 3.4 Reflex's frame pacing in a window (medium, S)

With latency control on, the RTX's median frame interval is 1.92–1.95 ms against 1.78 off
(GPU frame 1.65 against 1.77: the GPU idles about 15 %). The 99th percentile is 3.27–3.36
against 2.27 ms. A slower median is Reflex's trade (latency for throughput), but a 45 %
wider tail is not. One run of three had a 50 ms frame, waiting for the frame slot. Measured
windowed with immediate presentation; check borderless and exclusive full screen (the
defaults), then the pacer's sleep (ADR 0023) against `VK_NV_low_latency2`'s
recommendations.

### 3.5 Recording is the second CPU stage (medium, M)

Graph compile and record take 156–246 µs, as in audit 2: one thread records every pass.
The graph keeps its compiled shapes (4b-3a); recording passes into secondary command
buffers on the job system (or fewer, larger passes) would take most of it off the frame
thread.

### 3.6 The Radeon's GPU frame (medium, M each)

- The forward pass is 41 % of the frame: 9.0 ms at 67 % of 1080p.
- TAAU 3.0 ms.
- Local shadows (2.2 ms) and cascades (1.75 ms) are redrawn every frame. This scene moves
  2,064 casters every frame: past 512 moves the cascade cache draws everything, and the
  local lights' ranges hold movers.

A scene with fewer movers keeps them cached; a redraw budget per frame (ADR 0071's
leftover) would bound the worst case.

### 3.7 A test that fails under load (low-medium, S)

`engine_app`'s `zero_alloc_probes` ends its warm-up after a number of frames that, under
the gate's parallel load, comes too early. 125 frames against about 980 alone, and a late
allocation lands in the measured frames. It failed twice on 2026-10-16 (6 allocations,
Radeon pass), 6/6 alone. Fix: end the warm-up when the probes, the GI volume and the
streaming report settled, not by frame count.

### 3.8 Docs that drifted (low, S)

- CLAUDE.md §2 lists `engine_nav` and `engine_audio_vst3`, which do not exist
  (`engine_physics_native` is deferred, ADR 0028). It lacks 9 crates that do exist:
  `engine_app`, `engine_loc`, `engine_collision`, `engine_input`, `engine_scene`,
  `engine_reflect` and its derive, `engine_audio_decode`, `engine_workspace_hack`.
  CLAUDE.md is changed through an ADR.
- `docs/dev-guide` has no quick start ("from install to a cube on screen in 5 minutes", the
  first of §16's minimum).

### 3.9 Large files (low-medium, M)

`forward/mod.rs` 3,749 lines and `forward/declare.rs` 2,970: audit 2's 5,695-line file is
split in two, not by feature. Also `cook.rs` 2,833, `player.rs` 2,366, `colliders.rs`
2,213. §3.1 lived in such a file: per-feature modules with their own costs measured (§3.2)
would make the next one visible.

### Checked and sound

- The default path's registers (§2).
- The movers' path without local lights: 2.7 times cheaper than in audit 2.
- Start-up halved cold and 15–20 % faster warm.
- One transient heap for both frame slots.
- No pipeline created during a warm flight.
- Zero allocations per frame (the tests, including Arabic and Hebrew text now).
- `unsafe` lints at deny.
- Audit 2's P0 items: `Pipeline` without the device table, exclusive pass times.
- Its present path: the tone mapper at 0.66 ms on the Radeon.

## 4. What a shipped engine needs and the core lacks

| Need | State |
|---|---|
| Custom materials and shaders, particles, decals, GI, a player with pacing, localization | done since audit 2 |
| Navigation (navmesh, paths, avoidance) | missing: CLAUDE.md lists `engine_nav` |
| Save games (versioned, stable format, §13) | missing: ECS snapshots exist for rollback only |
| Terrain, foliage, water | missing, each a subsystem |
| LOD beyond meshes: impostors, HLOD | missing (open worlds) |
| Audio in the player (a game's sounds, buses, spatial; VST3 hosting, §11) | the mixer exists; the player starts audio only for videos; no VST3 host |
| Networking in the player | `engine_net` exists, measured; a game wires it itself |
| Input actions and rebinding in the player (§13) | `engine_input` exists; check its wiring |
| The crash pipeline for shipped builds (upload, symbols) | partial: minidumps and reports (ADR 0061) |
| A dev-guide quick start (§16) | missing |
| Luau bindings, the editor's tools | item 9 |

## 5. The new queue (proposed)

| # | Item | Why | Cost |
|---|---|---|---|
| 0 | ~~**P0: bound the local shadows' invalidation** (§3.1)~~ done 2026-10-16 (ADR 0071's addendum): 409 → 46 µs at 2,000 movers, 3,265 → 60 at 20,000 | 0.3 ms of every frame with movers; CPU-bound at 20,000 | S |
| 0a | ~~**A CPU stage gate** (§3.2)~~ done 2026-10-16 (`perf_gate --cpu`, ADR 0062's addendum) | the next §3.1 is caught | S–M |
| 1 | ~~VSM's leftover, required (owner, 2026-10-15): transparent surfaces and subsurface transmission from the pages, then the pages' default for discrete GPUs~~ done 2026-10-16 (ADR 0093 step 3d); the default is the owner's question | queued | M–L |
| 2 | Device memory: empty blocks freed (§3.3); Reflex's pacing (§3.4) | 328 MiB; a 45 % wider tail | S each |
| 3 | 8c's rules file and `RayHit`'s surface | the owner's question 1 | S–M |
| 4 | **Core gaps for a game** (§4): navigation; save games; audio, input and networking in the player; a quick start | what a game needs before tools | L in all |
| 5 | Item 9: the editor's tools, Luau bindings and debugger, other tooling | after the core | L |
| 6 | Recording on the job system (§3.5); the Radeon's forward and TAAU, a shadow redraw budget (§3.6) | CPU and iGPU frame | M each |
| 7 | Housekeeping: the probes test's warm-up (§3.7), CLAUDE.md §2 (ADR), the forward renderer split by feature (§3.9) | | S–M |

The parked items of `docs/status.md` §5 (mesh shaders on the RTX, the player's follow-ups,
GI's and frame generation's leftovers, …) and §6 stay as they are.

## 6. Questions for the owner

1. P0 (§3.1) is a regression of my own (6b): fix it now, with the CPU gate (0a), before the
   rest? (Recommended: yes; both are small.)
2. After VSM's leftover, which first:
   - A) the core gaps a game needs (navigation, save games, audio, input and networking in
     the player, a quick start), recommended: CLAUDE.md §17 puts the core before tools;
   - B) item 9 (editor, Luau, tooling), as the queue had it;
   - C) both, alternating by item?
3. Terrain, foliage, water and impostors: plan them now (each its own research and ADR), or
   leave them in §6 until a game asks?

**The owner's answers (2026-10-16):** 1 yes (done the same day, #236); 2 A, the core's
gaps first, keeping to the plan; 3 leave them in §6.
