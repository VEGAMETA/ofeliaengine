# Core audit 2: a fresh-eyes review of the engine core (2026-10-10)

The second technical review of the core (`docs/status.md` §7), after the first audit of
2026-10-01 produced the work of ADRs 0054–0076. Done by the coordinator alone, without
agents (owner's instruction of 2026-10-02). It reads the hot paths, measures what had no
number yet on both reference GPUs, looks for structural weak spots and compares the core
with what a shipped engine needs. Findings are ranked by their impact on the core's
quality and speed, each with evidence and a cost estimate; the new queue is at the end.

Machine: ref-1 (`docs/perf/reference-hw.md`): Ryzen 7 7700, RTX 4060 Ti (driver 616.92)
and the Radeon RDNA2 2-CU iGPU (driver 26.8.1), Windows 11. Release builds,
`VK_LOADER_LAYERS_DISABLE=~implicit~`. Main at `ed29fe4`.

## 1. Method

- **Code read:** the frame loop (`engine_runtime`), extraction and the persistent GPU
  scene (`gpu_scene.rs`), the frame renderer (`engine_frame`), the forward renderer's
  scene layout and pipelines (`forward.rs`), the render graph and its GPU timing, the
  upload ring, the geometry pool, skinning, streaming glue, the job system's pool, the Jolt
  backend's threads, the build and lint configuration.
- **A new measuring harness**, `crates/engine_runtime/examples/flight.rs`: the first
  program that runs a game's frame on the shipping path end to end. Sponza from its pack
  (cooked by `ofeliaengine cook`, 72.7 MB), textures and mesh levels streamed by feedback
  (ADR 0076); its 103 instances as ECS entities through `engine_runtime` (ADR 0064);
  2,000 cubes in spinning hierarchies; 64 skinned Fox characters walking, posed on the job
  system (ADR 0068); Sponza's 7 local lights with cached shadows (ADR 0071); sky light; the
  frame renderer with the device class's defaults (ADR 0072: TAAU at native + AO High on
  the RTX, TAAU at 67 % + AO Low on the Radeon); a 1920×1080 window without vsync, two
  frames in flight. The camera flies along the nave and back over 30 s of game time
  (1/60 s per frame, the same path on both GPUs) and turns 180° in 0.2 s every 5 s. It
  reports start-up phases, CPU per frame by stage, GPU frame and passes, the frames over
  twice the median, pipelines created during the flight and memory (device heaps, OS
  budget, transient targets, the process).
  ```text
  ofeliaengine cook test-assets/Sponza/Sponza.gltf -o sponza.pack
  cargo run -p engine_runtime --release --example flight -- sponza.pack --pipeline-cache <dir>
      [--frames N] [--movers N] [--characters N] [--no-lights] [--no-ao] [--no-feedback]
      [--offscreen] [--aa off|taau|fsr] [--quality ...] [--vsync]
  ```
- `scene_bench --pipeline-stats` now also prints the shading with motion vectors and
  with ambient occlusion (`ForwardRenderer::motion_statistics`): the variants the default
  settings actually draw with.

## 2. The numbers that had none

### A game frame (flight, defaults, 1,680 frames after 120 of warm-up)

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| GPU frame, median / p99 | 1.72 / 2.18 ms | 26.9 / 34.5 ms |
| CPU frame without the wait for the GPU, median / p99 | 604 / 771 µs | 920 / 1,488 µs |
| — graph compile + record | 193 µs | 190 µs |
| — present (`vkQueuePresentKHR`) | 104 µs | 303 µs |
| — scene upload (CPU) | 60 µs | 72 µs |
| — animation (64 characters on the job system) + poses | 60 µs | 83 µs |
| — interpolation / extraction / propagation | 31 / 27 / 24 µs | 41 / 38 / 34 µs |
| — streaming update + budget | 23 µs | 29 µs |
| — frame renderer: prepare + declare | 34 µs | 40 µs |
| Largest GPU passes (mean per frame) | AO 481, forward 473, local shadows 220, cascades 211, TAAU 145 µs | forward 10.2, TAAU 4.2, tonemap 2.6, cascades 2.4, local shadows 2.2, AO 2.2, bloom 1.6 ms |
| Frames over twice the median interval | 0–1 (one 11 ms GPU frame, not reproduced in two reruns) | 0 (GPU-bound) |
| Worst CPU frame | 0.9 ms | **5–9 ms** at the first fast turn (§3.6) |
| Pipelines created during the flight | 0 | 0 |

Both GPUs are GPU-bound with this scene. The local shadows redraw every frame: casters
move inside the lights' ranges, so the cache (ADR 0071) never holds.

### CPU per moving instance

`--movers 20000` / `60000` (all spinning every tick, about one tick per frame) on the RTX:

| movers | CPU frame | propagation | extraction | interpolation | scene upload |
|---:|---:|---:|---:|---:|---:|
| 2,000 | 0.60 ms | 24 µs | 27 µs | 31 µs | 60 µs |
| 20,000 | 1.82 ms | 233 µs | 228 µs | 297 µs | 468 µs |
| 60,000 | **5.22 ms (CPU-bound, GPU 2.5 ms)** | 1,273 µs | 817 µs | 855 µs | 1,461 µs |

About 70 ns per moving instance per frame, all on one thread.

### Start-up (time from `main` to the first frame presented)

| | RTX, our cache file missing | RTX, warm | Radeon, missing | Radeon, warm |
|---|---:|---:|---:|---:|
| window | 17 | 14 | 18 | 14 |
| Vulkan instance + device | 229 | 220 | 154 | 162 |
| surface + swapchain | 11 | 10 | 181 | 181 |
| renderer + features: 95 pipelines | 676 (94 compiled, 5.4 s of driver time on 16 threads) | 90 | 658 | 14 |
| scene opened from its pack (tails resident) | 123 | 74 | 95 | 62 |
| movers + characters (Fox cooked from the cache) | 203 | 31 | 29 | 30 |
| **first frame presented** | **1,357** | **476–588** | **1,238** | **450–490** |
| streaming idle for the first time | 2,757 | 1,381–2,084 | 2,072 | 917–1,175 |

"Missing" means our pipeline cache file was absent; the drivers' own caches were probably
warm, so a first run after a driver update is slower still. Every phase runs one after
another on one thread.

### Memory (end of the flight)

| | RTX | Radeon |
|---|---:|---:|
| Device-local heap, OS usage | 599 MiB (allocator 448 in blocks + 96 dedicated) | 544 MiB |
| Host heap, OS usage | 151 MiB | 288 MiB |
| Transient targets per frame slot (two slots) | 66 MiB (52 placed, 82.5 unaliased) | 41 MiB |
| Process working set / private bytes | 331 / 881 MiB | 883 / 1,095 MiB |

Nothing in the engine accounts for CPU memory, so the gigabyte of private bytes is not
attributed (driver, staging, pack, Jolt, Luau: unknown shares).

### Registers of the shading a default view draws with (Radeon, pixel / vertex VGPRs)

| variant | no motion | with motion vectors | motion, after the prepass with AO (the default) |
|---|---:|---:|---:|
| plain | 48 | 47 | 47 |
| local lights | 64 | 66 | 67 |
| environment (sky light) | 64 | **80** | **77** |
| lights + environment | 80 | 89 | 88 |
| vertex stage | 49 | **78** | 78 |

The register rule (`docs/agent-rules.md`: "48/64/64/80 on main") checks only the first
column; temporal antialiasing, the default on both device classes, draws with the others.

## 3. Findings, by impact

Costs are agent-days of implementation and measurement on both GPUs (S ≤ 2, M 3–7,
L > 7).

### 3.1 The reference game loop crashes on the integrated GPU: stack overflow (P0, S)

`game_loop` (the runtime's own example) and any program that builds the frame renderer on
Windows' 1 MiB main thread overflow the stack on the Radeon (reproduced 2026-10-10:
`thread 'main' has overflowed its stack`). Cause: `engine_rhi::Pipeline` stores a copy of
`ash::Device`, its whole function table: **1,504 bytes per pipeline**. `ForwardRenderer`
holds about 120 pipelines inline (181 KB), `FrameRenderer` 186 KB, and constructing them
moves those values through stack frames of **372 KB** (`FrameRenderer::new`), 270 KB
(`set_local_shadows`), 256 KB (`set_samples`), 156 KB (`ForwardRenderer::new`), 119 KB
(`MsaaPipelines::ensure`). Earlier work hit the overflow and worked around it in the perf
gate and `aa_bench` with 16–64 MiB threads (`70c694f`) instead of fixing the cause; a game
would crash on start.

Fix: `Pipeline { raw, bind_point }` with the device passed to destruction (or a shared
`Arc` of the table: 8 bytes); box the large renderer state; a test that builds the frame
renderer with every feature on a 1 MiB thread on both GPUs. Expected: `Pipeline` 16–24
bytes, `ForwardRenderer` a few KB, renderer construction frames in the low KB.

### 3.2 The default view's forward shading is over the register line (high, M)

With motion vectors (TAA, TAAU, FSR, DLSS, SMAA T2x: every default) the forward pixel
shader of a lit scene with sky light takes 77–89 VGPRs instead of 64, and the vertex stage
78 instead of 49 (table above). The perf gate shows the cost on the Radeon: Sponza's
forward pass +22 % with TAA (14.9 vs 12.3 ms) and +27 % with TAAU 67 % against FSR 1 at
the same size (8.0 vs 6.3 ms); on the RTX +5–11 %. That is ~1.7–2.7 ms of the iGPU's
default frame, the largest single lever there.

Options:
1. **Motion vectors from the depth prepass.** The default path always draws a prepass
   (ambient occlusion needs it, ADR 0058); its fragment shader is nearly empty, so the
   previous-position math and the second color output move there and the forward shading
   returns to the no-motion variants (64 / 49).
2. Camera motion reconstructed from depth for static instances (one full-screen pass,
   like `motion background`), object motion drawn only for instances that moved (as the
   skinned motion pass already does for skinned meshes, ADR 0068).

Either way, extend the register rule and `--pipeline-stats` to the default-path variants
(done for reporting in this audit).

### 3.3 GPU pass times overlap: the perf gate can blame the wrong pass (high, S–M)

Pass timestamps are written at `TOP_OF_PIPE` before and `BOTTOM_OF_PIPE` after each pass
(`graph/timing.rs`). Consecutive passes without a barrier between them run concurrently,
so a pass's interval includes the tail of the one before. In the flight on the Radeon,
"texture feedback" measured **2.43 ms**; drawn after an offscreen tone mapper instead of
the swapchain one it measured **49 µs** (ADR 0076's number). Turning feedback off saved
2.45 ms of GPU frame, but 2.2 ms of that was the forward pass sampling coarser textures
(feedback is what brings the finer levels in), not the pass. "light tiles clear" shows
100 µs for a buffer fill. The pass times add up to 10–15 % more than the frame.

The perf gate compares exactly these times per pass (ADR 0062), so a regression can land
on an innocent neighbour, and optimization effort follows wrong numbers. Fix: report
exclusive times (`end_N − max(start_N, end_{N−1})`, which sum to the frame) and/or a
serialized profiling mode (a full barrier before each pass) for the gate's runs; check the
existing baselines with it.

### 3.4 The present path costs the iGPU 1.1 ms that no test sees (high, S)

On the Radeon the tone mapper writing straight into the swapchain image takes **2.62 ms**;
the same pass into an offscreen image of the same format takes **0.86 ms**, plus 0.71 ms
to copy it into the swapchain: the GPU frame drops from 23.58 to **22.48 ms** (static
flight, 900 frames). The RTX shows no difference (27.6 µs either way). The perf gate and
golden images render offscreen, so they never measure what a windowed game pays. Fix:
offscreen + copy where measured faster (per vendor), find the cause (presentable images'
compression or tiling, memory type: `VK_EXT_image_compression_control_swapchain`,
different formats), and add a windowed present scene to the gate.

### 3.5 CPU cost per moving instance is serial (high, M–L)

About 70 ns per moving instance per frame across four single-threaded stages
(propagation, extraction, interpolation, scene upload): 60,000 movers make the RTX
CPU-bound at 5.2 ms. Two independent fixes:
1. Run the four stages chunk-parallel on the job system (the queue's existing "parallel
   ECS extraction" item, now measured in a real frame).
2. **Interpolate on the GPU:** upload each mover's transform once per tick (its previous
   and current tick transforms) and blend them in the culling pass with the frame's
   alpha. Per-frame mover CPU then follows the tick rate (64 Hz), not the frame rate: at
   240 FPS a 3.75× cut before any threading. Motion vectors already need both transforms.

### 3.6 Frame-thread stalls when pools grow (medium-high, S–M)

At the first fast turn (frame 310, deterministic) `GeometryPool::begin_frame` takes
**4.1–8.0 ms** on the Radeon (267 µs on the RTX): mesh levels arriving by streaming grow
the pool, and the growth allocates device memory and copies on the frame thread. Hidden
here by a 27 ms GPU frame, it is a hitch on any CPU-bound frame. The upload ring is fixed
(`UploadRing::new` capacities range from 1 to 64 MiB by guess), keeps no high-water mark,
and when full the view is not drawn (`FrameError::RingFull`; every harness `unwrap`s it).
Fix: reserve the pool from the pack's totals at open (the pack knows every level's size),
grow ahead of need off the frame thread, give the ring growth (retire the old buffer after
the frames in flight) and telemetry.

### 3.7 There is no player: every program wires the engine by hand (medium-high, L)

No crate owns "a game": device, swapchain, latency, the frame loop, process setup and the
pipeline cache. Each program wires them itself: the golden test bed (a 700-line
`render`), `game_loop`, `flight`, `scene_bench` (outside `engine_frame`), `stream_bench`
(the forward renderer with a draw list), `tools/stress` (draw lists, pre-0064), the frame
renderer's tests and the editor. Consequences seen in this audit:
- `RenderDefaults::ambient_occlusion` is applied by nothing (`game_loop` runs without AO
  while "the defaults" say AO on);
- `engine_latency` (Reflex, Anti-Lag 2, limiter, present_wait) and the CLAUDE.md §5 game
  process setup (power throttling off, MMCSS "Games", P-cores) run in the editor only;
- the runtime extracts meshes and the camera only: lights, skinned meshes, probes and the
  environment are passed by hand; a skinned instance must get its scene instance's
  interpolated transform bit for bit (`Skinning::set_pose`), which the runtime does not
  expose (the harness reads it back with `GpuScene::get`);
- ambient occlusion, local and contact shadows, reflections, subsurface and the prepass
  are renderer-wide switches while antialiasing and SSR are per view: two views (split
  screen, editor viewports) cannot differ;
- conversions are duplicated (`material_descs`/`stored_format` in the test bed and
  `engine_streaming`).

Fix: an `engine_app` (player) crate that owns the wiring, with ECS components for lights,
skinned meshes and probes, the latency provider and process setup; the benches and the
test bed built on it. This also gives start-up (§3.8) and memory (§3.9) one place.

### 3.8 Start-up is serial (medium, M)

First frame 450–590 ms warm and 1.24–1.36 s with our pipeline cache missing; device
creation (150–230 ms), the Radeon's swapchain (181 ms), opening the pack and creating 95
pipelines run one after another. Overlap device creation with reading the pack and
cooking (independent until upload), create the swapchain while pipelines compile, and
create pipeline variants on first use instead of all at `ForwardRenderer::new` (cold:
5.4 s of driver time for 95 pipelines; most games use a fraction).

### 3.9 Memory nobody accounts for (medium, S–M)

- Transient targets are reserved per frame slot (66 MiB ×2 on the RTX at 1080p, 41 ×2 on
  the Radeon). Frames on one queue execute in order: one heap could serve both slots
  when no transient crosses to the async compute queue (it does for AO on NVIDIA).
- No global allocator was ever chosen (CLAUDE.md §5 says mimalloc; ADR 0003 deferred it
  "until the runtime chooses"), and nothing counts CPU memory: 0.9–1.1 GiB of private
  bytes is unexplained.

### 3.10 Two thread pools (medium, M)

Jolt runs its own thread pool beside `engine_core::jobs` (`engine_physics_jolt`), so a
frame with physics and jobs oversubscribes the CPU, and only our pool is placed on CPU
sets. JoltC's callback job system (`JPH_JobSystemCallback_Create`, used today only for
the inline mode) can dispatch Jolt's jobs into our pool.

### 3.11 Expensive passes on the iGPU worth a second look (medium, M each)

In the default Radeon frame (26.9 ms): TAAU 4.2 ms (16 %), bloom 1.6 ms (11 passes at
full and down-sampled sizes; a single-pass downsampler), local shadows 2.2 ms redrawn every
frame once any caster in range moves (a per-frame redraw budget would bound it; the
static/dynamic split stays out, US 11,908,062), ambient occlusion Low 2.2 + 0.8 ms
(denoise, depths). On the RTX, AO High is 28 % of the GPU frame (481 µs): the defaults'
trade-off (ADR 0072), worth stating to the owner with the FLIP numbers.

### 3.12 Code structure (low-medium, M)

`forward.rs` is 5,695 lines; `ForwardRenderer::declare` alone ~1,600. Every feature adds
branches to the one function that must cost nothing when the feature is off, which is
what makes §3.1 and §3.2 easy to miss. Split by feature (pipelines and passes per module),
with the shared layout in one place.

### 3.13 Housekeeping (low, S)

- 118 pairs of `*_got.png` / `*_ref.png` in `H:\` (2026-10-01, from a test that no longer
  exists in the tree): moved to the Recycle Bin (owner, 2026-10-10).
- `game_loop` reports "input → camera 637 ms": a stale window event counted as the frame's
  input.
- `scene_bench --ao` swallows the next argument as the AO quality (`--ao --pipeline-stats`
  printed no statistics).
- ADR 0001 (lld-link) still "Proposed"; CLAUDE.md §5's mimalloc and §2's "engine_runtime:
  game executable" describe things that do not exist.

### Checked and sound

The GPU scene's slot, batch and upload bookkeeping (including remove/insert in one frame,
growth during a full upload); extraction's change detection and interpolation across
several ticks; the render graph's design (ADR 0011 numbers hold); the upload ring's
mapping; zero allocations per frame in steady state (existing tests); `unsafe` blocks are
documented (`clippy::undocumented_unsafe_blocks = "deny"`); no pipeline was created
during the flight (no first-use hitches from shaders).

## 4. What a shipped engine needs and the core lacks

| Need | State | Note |
|---|---|---|
| Custom materials and shaders for 3D (§9 material graph, hand-written Slang) | missing: one fixed PBR model | decides how permutations and PSOs are managed: core architecture, not tooling |
| Particles, decals | queued | |
| GI | queued | |
| LOD beyond meshes: impostors, HLOD | missing | open worlds |
| Terrain, foliage, water | missing | each a subsystem |
| A player with frame pacing (latency, VRR limiter) in the game loop | missing (§3.7) | |
| Save games (versioned, stable format, §13) | missing | ECS snapshots exist for rollback only |
| Localization (string tables, plurals, fonts) | missing | |
| Crash pipeline for shipped builds (upload, symbol server) | partial: minidumps and reports exist (ADR 0061) | |
| UI rendering cost in game | measured for the editor only | |
| Dev guide quick start (§16) | missing | |

## 5. The new queue

As proposed to the owner (P0 items are small and fix a crash or wrong measurements that
later work depends on). The order decided on 2026-10-10 (§6) is in `docs/status.md` §5:
faster checks first, then the P0 items; the player crate after the rendering queue; custom
3D materials after GI.

| # | Item | Why | Cost |
|---|---|---|---|
| 0 | **P0: `Pipeline` without the device table; renderer state off the stack; 1 MiB-thread test** (§3.1) | crash on start on the iGPU | S |
| 0a | **P0: exclusive or serialized GPU pass times; re-check the gate's baselines** (§3.3) | the gate attributes costs | S–M |
| 1 | Faster checks (owner, 2026-10-02: first after the review) | every change pays it | M |
| 2 | Motion vectors out of the forward shading; register rule over the default variants (§3.2) | 1.7–2.7 ms of the iGPU frame | M |
| 3 | Present path: offscreen + copy where faster, cause, a windowed gate scene (§3.4) | 1.1 ms of the iGPU frame | S |
| 4 | No pop-in on fast turns (status 0b) with pool reservation / growth off the frame thread and ring growth (§3.6) | visible pop-in; 4–8 ms stalls | M |
| 5 | Movers: parallel stages + GPU interpolation (§3.5) | CPU-bound at 60k movers | M–L |
| 6 | Player crate (`engine_app`): wiring, latency, process setup, ECS lights/skins/probes, per-view features (§3.7) | drift and missing integration | L |
| 7 | Start-up overlap and lazy pipeline variants (§3.8) | 0.5–1.3 s to first frame | M |
| 8 | Lanczos-3 and bicubic (status 0c) | owner's list | S |
| 9 | GPU particles and radix sort; clustered decals; GI; visibility buffer; virtual shadow maps; remaining post effects (status 1–6) | features | as planned |
| 10 | iGPU passes: TAAU, bloom, local shadow redraw budget (§3.11) | 2–4 ms of the iGPU frame | M each |
| 11 | Memory: one transient heap, the global allocator decision, CPU accounting (§3.9) | 50–130 MiB, unexplained GiB | S–M |
| 12 | Jolt on our job system (§3.10) | oversubscription | M |
| 13 | `forward.rs` split by feature (§3.12) | maintainability | M |
| 14 | Custom 3D materials and shaders (§4) | the biggest missing core piece for real games | L (ADR first) |
| 15 | Editor tools, Luau bindings and debugger, other tooling (status 7) | after the core | |

The leftovers of `docs/status.md` §6 stay as they are.

## 6. Questions for the owner, and the answers (2026-10-10)

1. P0 items 0 and 0a before "faster checks", or after it as decided on 2026-10-02? (They
   are small; one is a crash on start on the iGPU.) **Faster checks first.**
2. Item 6 (a player crate) is architectural: build it before more features (it makes
   every feature cheaper to integrate and measure), or keep it after the rendering queue?
   **After the rendering queue.**
3. Item 14 (custom materials): its ADR decides how shader permutations work in the whole
   renderer. When should it start? **After GI.**
4. On the RTX, AO High is 28 % of the default GPU frame. Keep High as the discrete
   default (ADR 0072), or Medium (0.40 vs 0.52 ms; FLIP 0.096 vs 0.070)? **High stays.**
5. Delete the stray PNGs in `H:\`? **Yes, carefully:** the 236 files (all
   `<digits>_got.png` / `<digits>_ref.png` of 2026-10-01, 128 × 128 debug renders of the
   Radeon LDS investigation, 1.3 MB) went to the Recycle Bin.
