# Engine status and hand-off

Where the engine stands and how work on it is run. A new session (coordinator or agent)
reads this after `CLAUDE.md` and `docs/agent-rules.md`. Keep it current: whoever merges a
PR that changes the picture updates this file in the same PR.

Last updated: 2026-10-10.

## 1. What matters most

The owner's goal is a **technically ideal engine core**. That means well-designed
internals and speed proven by measurement on both reference GPUs: the RTX 4060 Ti and the
weak 2-CU Radeon iGPU (`ENGINE_ADAPTER=radeon`). The editor, Luau and tooling come after
the core. The detailed rules are in `docs/agent-rules.md` ("What matters most" and the team
decisions). In short:

- **Decide by evidence.** Research first (papers, talks, shipped engines, open code with
  licenses and patents checked), then options in an ADR, then a measured choice.
- **Never make existing things worse.** Golden images, the per-pass perf gate and the
  register rule exist for that. A feature a view does not use costs it nothing, not even
  shader registers.
- **Responsiveness is first-class** (§7): no added latency without a measured reason.
- **Standing team decisions:**
  - no patented algorithms;
  - NVIDIA DLLs are never shipped;
  - no camera motion blur in gameplay;
  - singleplayer has no loopback client/server;
  - every antialiasing and scaling mode exists, and the game decides what players may
    pick.

## 2. How the work is run

- **Roles.** One coordinator session plans, writes agent briefs, gates and merges, and
  reports to the owner in Russian.
- **Agents.** None until the owner allows them again (see §7). After that, at most **2**
  agents at a time, all on Opus. Each works on one task in its own worktree under
  `H:\Desktop\ofeliaengine-wt\<name>`, building in its own `C:\cargo-target\<name>` (the NVMe
  drive, ADR 0077), and ends with a PR. The main checkout builds in `C:\cargo-target\ofeliaengine`
  (`.cargo/config.toml`).
- **Gate.**
  - The coordinator merges after its own `tools/check.ps1 -Gpu -Perf`, plus `-Deny` when
    dependencies change.
  - The checks take a machine-wide GPU lock and wait for a quiet CPU before measuring.
  - Before a merge, the branch has origin/main merged in.
- **After a merge.**
  - Remove the worktree (look for links first: never junction the main checkout's
    folders, give scenes via `ENGINE_TEST_ASSETS`) and its target dir.
  - `tools/clean.ps1` frees stale build outputs.
- **Backups.** Everything is pushed: agents push after every commit. When the usage limit
  stops agents, the coordinator snapshots their uncommitted work to `backup/wip*`
  branches, then resumes them by message.
- **Machine.** ref-1 needs VDDCR_SOC at 1.25 V, or the Radeon flips LDS bits under CPU load
  (`docs/research/radeon-igpu-lds-faults.md`). `check.ps1 -Gpu` runs that self-check first.

## 3. What is done (merged to main)

Estimate: the technical core is about **72 %** done, and the whole engine (editor, Luau and
tooling included) about **50 %**.

| Area | State | Where |
|---|---|---|
| Platform, core, jobs, memory, timers, input, latency | solid | §5, §7 |
| Vulkan RHI: bindless, render graph, async compute, pipeline cache, crash/GPU-fault reports, Vulkan instances on a process-lifetime loader thread | solid | ADRs 0006, 0055, 0061, 0065 |
| Persistent GPU scene, ECS extraction, runtime frame loop, cameras | done | ADR 0064 |
| **Frame renderer** (`engine_frame`): one object composes a frame: scene → hook → exposure → TAA/TAAU/FSR/DLSS → bloom/tonemap; defaults per device class | done | ADR 0072 |
| Lighting: PBR forward with clustered lights, CSM, IBL, GTAO, SSS, transparency (sorted + OIT) | done | ADRs 0045–0047, 0054, 0058, 0059 |
| Local-light shadows (cached atlas), contact shadows (Bend) | done | ADR 0071 |
| Reflections: parallax-corrected probes, SSR (FidelityFX SSSR-based) | done | ADR 0073 |
| Atmosphere (Hillaire 2020) and height fog | done | ADR 0069 |
| HDRI environments: `.hdr` and OpenEXR panoramas as the sky, their sun found and lit as the directional light, cooked into BC6H cubes (DDC, packs) | done | ADR 0083 |
| GPU particles and GPU radix sort | done | ADRs 0080, 0081 |
| Upscalers: TAAU, FSR 3.1, DLSS via Streamline (user-supplied DLLs) | done | ADRs 0060, 0063 |
| Every antialiasing and scaling mode: MSAA 2/4/8× (+ alpha to coverage), FXAA, MLAA, SMAA 1x/S2x/T2x/4x, nearest/bilinear/Catmull-Rom/Lanczos-3/integer/FSR 1, TAAU/FSR/DLSS; `check()` and the GPU's support list for game menus | done | ADR 0075 |
| Skeletal animation, clip compression, IK, GPU skinning | done | ADR 0068 |
| Physics API on Jolt/Box2D: queries, characters, rollback | done | ADR 0066 |
| Networking: encrypted UDP transport with NAT traversal; replication, prediction, lag compensation, co-op example | done | ADRs 0070, 0074 |
| Assets: glTF, cooking, packs, IoRing streaming; GPU memory budget, texture streaming by GPU feedback, mesh LOD streaming (`engine_streaming`) | done | ADRs 0056, 0057, 0076 |
| Audio, text, 2D, basic UI | solid / basic | §11, ADR 0015 |
| Tests: golden images (FLIP, per vendor), per-pass perf gate, determinism, zero-allocation tests | solid | ADR 0062 |
| Editor, Luau | skeleton only, by plan | §10, §12 |

## 4. In flight

Nothing. Done on 2026-10-10, by the coordinator: item 0 (faster checks, but for sccache:
ADR 0077 and its addenda; the gate `-Gpu -Perf` 10–25 min → about 320–400 s), 0a (the
review's P0 fixes: PRs #124, #125; the Radeon's perf limits, PR #126), 0b (fast turns:
PRs #127–#130; left optional: the fast-turn test in the perf gate, one wider feedback
pass if a larger scene shows pop-in), 0c (Catmull-Rom and Lanczos-3, ADR 0075's
addendum), 0d (motion vectors from the depth prepass, ADR 0043's addendum), TAAU's
Catmull-Rom history with point reads (owner, 2026-10-10; ADR 0043's second amendment), 0e
(presenting across adapters, ADR 0007's addendum), 0f (moving instances placed on the
GPU, the tick's stages on the job system: ADR 0064's addendum), 0f2 (16-byte aligned
accesses through buffer addresses in every shader: ADR 0013's addendum), 0g (shading
variants made on first use and by the device's warm-up thread, start-up phases
overlapped: ADR 0079). On 2026-10-11: 1 (GPU particles and GPU radix sort: ADRs 0080,
0081; the reactive mask for effects, ADR 0081's amendment), 2 researched (decals, ADR 0082
Proposed: the owner's choice), 2b phase 1 (HDRI environments, ADR 0083). Next: 2b phase 2,
or 2 once the owner has chosen.

## 5. Queue (in order)

Merged with the second review's findings (`docs/research/core-audit-2.md`). The owner's
answers of 2026-10-10: faster checks first, then the review's P0 fixes; the player crate
after the rendering queue; custom 3D materials after GI; the discrete AO default stays High.

0. **Faster checks, first after the review** (owner, 2026-10-02, confirmed 2026-10-10). Then
   `check.ps1 -Gpu -Perf` took 10–25 min, and it grew with every crate and test. Per
   step, from the runs before 2026-10-10:

   | Step | Minutes |
   |---|---|
   | test | 1–6.5 |
   | tracy | 2–6.5 |
   | gpu | 2–7 |
   | perf | 2.5–4 |
   | clippy (cold) | up to 5 |

   Measure every step before and after each change, then:
   - **remove duplicates:** the tracy step reruns the whole test suite (build with Tracy
     and run only the profiler's tests instead); the test step reruns GPU tests that the
     gpu step runs on both GPUs;
   - **link faster:** lld-link (closes ADR 0001), `line-tables-only` debug info for tests,
     fewer integration-test binaries per crate;
   - **share compiled dependencies across worktrees** with sccache (Apache-2.0);
   - **adopt cargo-nextest;**
   - **add an "affected crates" mode** for agents' intermediate runs. The full run stays
     the gate before every merge.

   Done (ADR 0077 and its addendum): duplicates removed, one build per check, nextest,
   `-Affected`, doc tests, docs and formatting for what changed, one test binary per crate
   (100 → 45), test builds with line tables only, ADR 0001 accepted (rust-lld measured
   ~12 % faster than link.exe), the build directory on the NVMe drive
   (`C:\cargo-target\ofeliaengine`: rebuilding every test after a change in engine_core
   40-58 s → 11-15 s): 632 → 141 s with nothing changed; the perf step (ADR 0062 addendum:
   runs a round at a time, a scene with a slower pass measured again, 100 frames per run,
   baselines from median runs, a 12 s wait for a quiet CPU instead of 2 × 2 min) 749 →
   237 s: the gate `-Gpu -Perf` 901 → 400 s. Left: sccache for agents' worktrees (when
   agents run again), and a study of the Radeon's variance:
   - the shadow pass (and bloom) read 20–27 % slower in a few scenes for minutes at a time,
     in every round, with the CPU quiet, then normal again (2026-10-10). Work on the CPU
     is not it (it costs 10 % while every core spins and nothing seconds later). The slow
     scenes followed scenes with large render targets in the gate's order (FXAA after
     MSAA 4×, reflections after FSR 1 twice, FSR 1 after SMAA T2x), and the same scene
     measured again after itself read normal: the first suspect is where the driver places
     a new device's render targets in the iGPU's memory after the previous device freed
     large ones. Later the same day most scenes read slower for a whole check (frame
     22.1–23.7 ms over the day, memory-bound passes +15–37 %) with the CPU at 0 % and the
     RTX idle, and the same scene measured again after itself stayed slow: more likely the
     SoC's memory or Data Fabric power states. The owner chose (2026-10-10) a 10 %
     threshold for AMD and not counting scenes whose GPU ran slower as a whole (ADR 0062
     addendum) over a BIOS experiment (DF C-states, JEDEC memory speed), which stays
     possible;
   - `sponza_streamed` read +10 to +23 % against its baseline from run to run in the
     morning while `sponza` stayed within 0.5 % (the same with and without PR #120's
     streaming fix; within 1 % in the afternoon's runs): which levels are resident when
     the timed frames start depends on timing. Make the scene settle to a deterministic
     residency before timing.
0a. **The review's P0 fixes** (§3.1 and §3.3):
   - ~~`engine_rhi::Pipeline` without its copy of the device's function table~~ done
     2026-10-10: `Pipeline` 1,504 → 24 bytes (an `Arc` of the device), the renderers
     181/186 KB → 4 KB, no boxing needed; `engine_frame`'s test builds and renders every
     feature and antialiasing mode on a 1 MiB thread on both GPUs (it overflowed before;
     it fits 256 KB now); `game_loop` runs on the Radeon; the 16–64 MiB threads in
     `perf_gate`, `aa_bench` and `flight` are gone;
   - ~~GPU pass times without overlap~~ done 2026-10-10 (ADR 0011 addendum): a pass's
     time starts at the previous pass's end on its queue when that came later; the
     Radeon's texture feedback reads 88 µs instead of 2.4 ms, its culling passes
     0.3–1.6 µs instead of 4–76; both perf baselines re-measured.
0b. **No visible streaming pop-in on fast camera turns** (owner, 2026-10-02: in UE, models
   and textures failed to load in time when the camera turned quickly). Our ADR 0076 has
   the same risk:
   - texture levels come only from the current view's feedback;
   - mesh levels are asked only by instances in the frustum;
   - request → upload takes about 0.35–0.5 s.

   Done (2026-10-10): the Graphine patents read (`docs/research/streaming-fast-turns.md`:
   360° feedback views besides the main view read on US11195250 and are out, as is
   distance prediction combined with feedback); the fast-turn test (`stream_bench --turn`,
   `TextureStreamer::sharpness`): on first sight a few percent of the pixels wait
   0.08–0.15 s, later turns 0 %, tight budgets blurry all the time rather than after turns;
   mip fade (ADR 0078). Left:
   - ~~mesh levels by a sphere around the camera~~ done 2026-10-10: `LodView::around`
     (distance in every direction; after a fast turn on Sponza no mesh level is left to
     load, instead of 41, for 0.4–0.8 MiB more; the reference `flight` uses it);
   - the fast-turn test in the perf gate, and a larger scene than Sponza (its pack fits the
     disk's cache);
   - one feedback pass with a wider field of view instead of the main view's, if a larger
     scene shows pop-in.
   The review's additions, done 2026-10-10: the geometry pool reserved from the pack's
   totals as the scene opens (`GeometryPool::reserve`; the Radeon's worst frame at the
   first fast turn 5.0 → 1.7 ms), the upload ring grows ahead (`UploadRing::grow` from the
   frame renderer, `stats()`; a refused frame still drops its view once). Growth beyond a
   reservation (scenes added while running) still happens on the frame thread.
0c. ~~Spatial upscales Lanczos-3 (with an anti-ringing clamp) and bicubic Catmull-Rom,
   measured in ADR 0075's table. No Lanczos-2: FSR 1's EASU covers it (owner,
   2026-10-02).~~ Done 2026-10-10 (ADR 0075's addendum): both in the tone mapper,
   clamped to the four nearest texels (no dark rings around HDR highlights), each texel
   read on its own (the bilinear-fetch trick is ATI's patent); Lanczos-3 in two passes.
   Catmull-Rom 67 %: +0.04 ms RTX, about +1 ms Radeon, sharper than bilinear and better
   than FSR 1 in every metric; Lanczos-3 the same quality for about +2 ms on the Radeon.
0d. ~~**Motion vectors out of the forward shading** (review §3.2). With motion vectors (every
   default antialiasing) the lit forward shader takes 77–89 VGPRs instead of 64 and the
   vertex stage 78 instead of 49: Sponza's forward +22–27 % on the Radeon, +5–11 % on the
   RTX. Write them in the depth prepass (always drawn with the default AO), or reconstruct
   camera motion from depth and draw object motion for movers only; extend the register
   rule to the default variants (`scene_bench --pipeline-stats` prints them now).~~ Done
   2026-10-10 (ADR 0043's addendum): the prepass writes them when it runs; the default
   path's shading back at a 49-VGPR vertex stage (was 78) and 48/63/72/80 pixel VGPRs;
   Sponza 1080p TAA + AO: Radeon prepass + forward 16.64 → 15.70 ms, RTX 391 → 387 µs;
   golden images byte-identical; 16 entry points and ~40 pipelines fewer; the perf gate
   measures the default settings (`sponza_taa_ao`). Left: still geometry's motion from
   depth (the prepass now pays +0.5 ms on the Radeon for writing it).
0e. ~~**The present path on the iGPU** (review §3.4): the tone mapper writing the swapchain
   image costs the Radeon 1.1 ms of frame against writing an offscreen image and copying
   it. Offscreen + copy where measured faster, find the cause, a windowed scene in the gate.~~
   Done 2026-10-10 (ADR 0007's addendum). The cause: ref-1's monitor is on the RTX, so the
   Radeon presents across adapters (shared, row-major swapchain images). The window's
   monitor adapter (D3DKMT) is compared with Vulkan's `deviceLUID`; across adapters the
   frame is drawn apart and copied (`engine_render::present::PresentTarget`): Radeon GPU
   frame 27.1 → 26.3 ms in `flight`, the RTX unchanged (direct). Left: a windowed present
   scene in the perf gate, with the player crate (8a).
0f. ~~**Moving instances** (review §3.5): ~70 ns each per frame on one thread (60,000 make
   the RTX CPU-bound at 5.2 ms). Propagation, extraction, interpolation and upload in
   parallel on the job system; interpolation on the GPU so movers upload per tick, not per
   frame.~~ Done 2026-10-10 (ADR 0064's addendum). A mover's transform goes up once per tick
   and a compute pass places it every frame; propagation, extraction and the scene's apply
   run on the job system with the same results as on one thread (`Runtime::set_jobs`,
   `GpuScene::set_jobs`). `flight`, 60,000 movers: RTX CPU frame 4.94 / 6.99 ms → 1.53 /
   2.70 (median / p99, 60 fps), worst frame at 240 fps 1.95× the median (main 1.93×);
   Radeon CPU frame 6.43 → 2.83 ms, its GPU-bound frame 1 % longer (placement 0.45–0.53
   ms against the copy's 0.15). Found and fixed on the way: an instance that changed
   batch between ticks kept its old batch word on the GPU, and culling dropped it or
   another batch's instance.
0f2. ~~**Aligned pointer accesses in shaders** (owner, 2026-10-10: a serious topic): loads
   and stores through buffer device addresses are declared 4-byte aligned (Slang's default
   for pointers), so a driver may split 16-byte accesses; every vector load of the forward,
   culling and shadow shaders is (vertexMain 19, depthVertex 7, shadowVertex 9, cullMain 21
   and 5 stores, fragmentMain 33, the scene's scatter). Declare 16 bytes
   (`loadAligned<16>`) only where the data is so aligned (instance rows, batches; not packed
   `float3` vertices), and measure each pass on both GPUs with the register rule; golden
   images must stay byte-identical. The scene's motion passes already do (no change on the
   Radeon; on the RTX it came with their new layout, not measured alone).~~ Done 2026-10-10
   (ADR 0013's addendum): every vector access through a buffer address in the engine's
   shaders declares its alignment. RTX: forward 336 → 321 µs, the atmosphere pass 56 → 44,
   the reflections composite 128 → 118, the SSR history 61 → 57, the GPU frame of Sponza
   with atmosphere, probes and SSR 1,478 → 1,445; Radeon: the SSR depth pyramid 413 → 308,
   registers unchanged. A 4-byte value read by a dynamic index stays scalar (as a 16-byte
   load it made the SSR trace 7.5 % slower).
0g. ~~Start-up overlap (device creation beside the pack's reading and cooking) and pipeline
   variants created on first use (review §3.8): first frame 450–590 ms warm, 1.2–1.4 s
   with the pipeline cache missing.~~ Done 2026-10-10 (ADR 0079): `flight`'s first frame
   with the cache 324 → 261 ms (RTX), 433 → 367 (Radeon); with the drivers cold 796 → 504,
   1,028 → 547. The forward renderer describes its shading variants (`LazyPipeline`) and
   makes them on first use or on the device's warm-up thread; the device is created
   beside the window and the Fox's cook, the swapchain beside the renderers and the
   scene.
1. ~~GPU particles and GPU radix sort.~~ Done 2026-10-11: the sort (ADR 0080: 1 M keys
   with a payload in 0.59 ms on the RTX, 9.9 ms on the Radeon) and the particles (ADR
   0081: a GPU pool with GPU-only counts, deterministic, sorted by depth, drawn soft; 16 K
   sparks in 0.13 / 0.65 ms, a golden shot on both GPUs).
2. Clustered decals. Researched 2026-10-11 (ADR 0082, Proposed): clustered in the shading
   doubles the 32 fragment entry points (best after the forward.rs split, 7c), a decal
   buffer before lighting needs the prepass and skips transparent surfaces. Waiting for
   the owner's choice; 2b meanwhile.
2b. **HDRI environments** (owner, 2026-10-10: before GI, a necessity). ~~Phase 1~~ done
   2026-10-11 (ADR 0083): our own `.hdr` decoder, RGBE kept and decoded on the GPU (an
   unclipped sun exceeds half floats); a background cube of its own made on the GPU (a
   quarter of the panorama's width, at most 1,024 by default: 0.23 ms on the RTX, 6.8 ms on
   the Radeon for 512², once); the sun found in the panorama and taken out into the
   directional light (shadows; the 128² lighting cube without its spike, so prefiltering
   stays clean and the harmonics do not ring); turning; a golden shot with a CC0 Poly Haven
   panorama on both GPUs. ~~Phase 2~~ done 2026-10-11 (ADR 0083's amendment): cooking into
   BC6H cubes with mips (DDC; packs of their own, version 4; the background 8 times
   smaller, 2 frames of upload through the ring), the cube turned when sampled (turning
   rebuilds only the lighting: 0.14 ms for its fill on the Radeon); OpenEXR through the
   `exr` crate (1.74.0: every compression but the lossy DWA; Poly Haven's 1K EXR, PIZ
   and float, decodes to exactly its `.hdr`'s pixels).
   **Left** (§6): the sun's angular size for soft shadows.
3. GI: probe volumes, then DDGI-like. Acceptance check, besides the usual: Sponza's metallic
   medallions on the curtains (roughness 0.28, metallic 0.74) must read gold when seen from
   above in shade. Today they look dark brown there. The material and the shading are
   right (seen from below they are gold), but the shaded floor they reflect has no bounced
   light (2026-10-02). Researched 2026-10-11 (ADR 0084, Proposed): DDGI probes traced with
   ray queries (NVIDIA's patent application on DDGI was abandoned; both reference GPUs have
   ray queries; acceleration structures needed) or AMD's Brixelizer GI (MIT, compute, any
   GPU). Waiting for the owner's choice.
3b. **Custom 3D materials and shaders** (review §4; after GI: owner, 2026-10-10). ADR
   first: it decides permutations and pipeline management for the whole renderer (§9
   material graph and hand-written Slang).
3c. **Directional GTAO and GTSO** (owner, 2026-10-10: after GI): bent normals (XeGTAO's
   directional term, about +25 % of the AO pass) for ambient light along the unoccluded
   direction and cone-based specular occlusion; fixes dielectrics looking too bright next
   to metals in shade indoors (§6). Own entry points, only in views with AO (register rule).
4. Visibility buffer / meshlets. Write the ADR first, with no persistent per-triangle
   visibility bits (patent, ADR 0035 note).
5. Virtual shadow maps.
Items 6–8 were one item 6, with sub-items 6b–6h, until 2026-10-10 (owner: item 6 had
grown, divide and conquer); the old labels stay in parentheses.

6. **Rendering leftovers.**
6a (was 6). Remaining post effects: DOF, LUT grading, motion blur only on cinematic views
   (ADR 0067), frame generation. For frame generation, both DLSS FG and FSR 3.1 FG, off by
   default, with latency measured.
6b. The iGPU's expensive passes (review §3.11): TAAU 4.2 ms, bloom 1.6 ms (a single-pass
   downsampler), local shadows redrawn every frame when casters move (a per-frame redraw
   budget; never the patented static/dynamic split).
7. **The second review's core leftovers.**
7a (was 6c). Memory (review §3.9): one transient heap for both frame slots where no
   transient crosses queues (66 / 41 MiB), the global allocator decision (mimalloc,
   CLAUDE.md §5, never made), CPU memory accounting (0.9–1.1 GiB private bytes
   unexplained).
7b (was 6d). Jolt's jobs on `engine_core::jobs` through JoltC's callback job system
   (review §3.10).
7c (was 6e). `forward.rs` (5,695 lines) split by feature (review §3.12).
8. **The player and content** (owner, 2026-10-10: after the rendering queue).
8a (was 6f). **A player crate** (`engine_app`, review §3.7): device, swapchain, latency
   provider, process setup (power throttling, MMCSS, CPU sets), pipeline cache and the frame
   loop in one place; window modes: windowed (placed on the monitor, not larger than its
   work area), borderless full screen and exclusive full screen
   (`VK_EXT_full_screen_exclusive`), measured for latency with PresentMon (owner,
   2026-10-10: `flight`'s 1920 × 1080 window is larger than the 1080p screen and composed by
   DWM); ECS components for lights, skinned meshes and probes; features per view instead of
   per renderer; `RenderDefaults` applied whole. The benches and the test bed move onto it.
8b (was 6g). **Video playback** (owner, 2026-10-10: after the player crate; the engine plays
   no video today): a video frame as an ordinary texture (UI, a screen in the world, a
   viewport, cutscenes). To research and decide in an ADR: AV1 (royalty-free, unlike H.264 /
   H.265) decoded by the GPU through Vulkan Video (`VK_KHR_video_decode_av1`: check both
   reference GPUs' drivers) with dav1d (BSD-2-Clause) on worker threads where it is missing;
   WebM / Matroska demuxed by our own code; Opus sound through the existing decoder,
   synchronized to the audio clock; cost per frame measured on both GPUs.
8c (was 6h). **Colliders made automatically**: collision shapes from meshes at import and
   cook, into the pack ("one button"): static level geometry as triangle meshes (simplified
   where it pays), props as a fitted box, sphere or capsule or a convex hull, concave ones
   split into convex parts (CoACD, MIT, or V-HACD, BSD-3: compare), and colliders authored
   in the source taken as they are (`UCX_` names, a glTF physics extension). Today shapes
   are only made in code.
9. Then: editor tools, Luau bindings and debugger, other tooling.

## 6. Known gaps and leftovers (candidates for the queue)

- **Rendering:**
  - half-resolution aerial perspective and fog on the iGPU (about 1–2 ms there);
  - froxel volumetric fog;
  - per-pixel local-shadow mask (the shadowed forward shading costs +10–12 % on the iGPU);
  - softer shadow filters; an HDR panorama's sun is a point light today: its angular size
    (ADR 0083's sun search can measure it) for soft shadows;
  - contact shadows of local lights (owner, 2026-10-10: on request): Bend's pass per light,
    for the brightest one or two; today only the sun has them;
  - reflections on transparent surfaces;
  - GTSO;
  - probe shadow-map caching;
  - XeSS;
  - ambient light inconsistent indoors: diffuse ambient comes from the sky's irradiance
    (barely occluded), specular from reflection probes that see the real, darker interior.
    In shade, dielectrics therefore look too bright next to metals. Irradiance from the
    probes (SH of their captures) would make them agree until GI lands;
  - cloth shading (sheen) and anisotropy are missing from the material model;
  - FSR 4: ML, RDNA4-only, shipped as a DLL (DX12 at first). Check Vulkan availability and
    license; it needs an RDNA4 GPU to test.
- **Patents avoided on purpose** (don't build without a new decision): static/dynamic
  split of cached shadows (Warner Bros US 11,908,062 / 12,315,068); dual-quaternion
  skinning variants (US9613456B2); motion matching variants; Burley's diffusion profile;
  higher-order texture filters from combined bilinear fetches (ATI US 8,675,007 B2, in
  force until 2032: Catmull-Rom in 9 or 5 fetches; read texels one by one instead).
- **Animation:** morph targets, state machines and blend trees, retargeting, SIMD clip
  decoding (wins back 2× decode speed).
- **Networking:**
  - transport: congestion control beyond the send budget, connection migration;
  - servers: a reference rendezvous/relay server binary;
  - replication: predicted spawns (projectiles), per-component update rates,
    PVS/portal relevance, built-in visual error smoothing;
  - runtime: a windowed demo (needs a dilated client clock in `engine_runtime`).
- **Streaming (ADR 0076):** memory priorities (`VK_EXT_pageable_device_local_memory`),
  feedback from several views, LOD crossfades and impostors, sparse residency. Screen-size
  prefetch is blocked: the Graphine texture-streaming prediction patents (US10748241 and
  family) claim combining prediction schemes, so feedback is the only source of wanted levels.
- **Antialiasing (ADR 0075):**
  - centroid interpolation under MSAA;
  - per-sample or bilaterally upsampled AO and contact shadows under MSAA;
  - dynamic resolution with MSAA;
  - CMAA is not built (Intel patents).
  - TAAU's optional Catmull-Rom history reads its 12 texels one by one since 2026-10-10
    (owner's decision; it merged bilinear fetches as ATI's US 8,675,007 B2 claims): RTX
    resolve 162 → 206 µs at 1080p, Radeon 4.96 → 5.28 ms; the default bilinear one is
    unchanged.
  - Ambient occlusion: the discrete default stays High (owner, 2026-10-10), although it
    is 28 % of the RTX's default GPU frame (481 µs; Medium 0.40 ms, FLIP 0.096 vs 0.070).
  - The discrete-GPU default stays TAA. The other candidate is MSAA 4× (+1.2 ms on the
    RTX, sharpest in motion, more flicker); the owner decides.
  - A Streamline `eErrorExceptionHandler` hang was seen once in `aa_bench`.
  - `procedural_contact_shadows` renders a few pixels differently from run to run on both
    vendors (4–13 pixels between runs of one build, up to 25 levels: a race?), and on main
    `sponza` (52 pixels on the RTX, 155 on the Radeon, 2 levels at most) and
    `procedural_local_shadows` (Radeon, 97 pixels, 20 levels) differ from their golden
    images. They pass the FLIP tolerance (seen 2026-10-10); find why and regenerate them.
- **Runtime:**
  - transform propagation of non-root candidates (children whose own transforms change
    every tick) stays on one thread, by depth;
  - the GPU placement of moving instances costs the Radeon 0.45–0.53 ms per frame at
    60,000 against the old copy's 0.15: whole 64-byte instance writes (the material and
    batch word kept beside the motion planes by every writer of that word) measured 85 µs
    less.
- **Presentation:** test 0e's choice with a monitor on the iGPU (the owner can connect one;
  later, owner 2026-10-10).
- **Particles (ADR 0081):** lighting and shadows on them, depth-buffer collisions, trails
  and mesh particles, user simulation code, large particles drawn at a lower resolution
  (overdraw on the iGPU: 256 smoke puffs cost 1.6 ms there), a one-workgroup sort for small
  counts (the sort is most of 16 K particles' cost), ECS components, motion vectors for
  particles, orthographic views.
- **Start-up (ADR 0079):**
  - acquiring the swapchain image late would let the Radeon's first frame record beside its
    swapchain (it waits 140 ms for it now);
  - `VK_EXT_shader_module_identifier` lookups: a large shader's lookup takes 1.1 ms on
    NVIDIA;
  - a record of the variants a game draws with, so the warm-up makes them first (with the
    material graph);
  - the other renderers' pipelines (TAA and upscalers, post, AO's and local shadows'
    passes, skinning) are still made at start-up.
- **Reliability:** two silent process exits (`perf_gate` once, `frame_bench` once) were
  never reproduced or explained. Watch for a recurrence and get a stack.
- **Housekeeping:** `scene_bench` is wired by hand, outside `engine_frame`. From the second review:
  - `game_loop`'s "input → camera" counts a stale window event (637 ms);
  - `scene_bench --ao` takes the next argument as the AO quality even when it is a flag;
  - CLAUDE.md §5 (mimalloc) and §2 ("engine_runtime: game executable") describe things
    that do not exist yet.

## 7. First task of a new coordinator session: a fresh-eyes technical review

The second review was done on 2026-10-10: `docs/research/core-audit-2.md`. Its harness
`engine_runtime/examples/flight.rs` measures a game's frame end to end; rerun it in later
reviews to compare.

**Owner's instruction (2026-10-02): do this review yourself, and start no agents** (no
`Agent` tool, no subagents, no background workers) until the owner explicitly says agents
may run again. Read, measure and write on your own; ask the owner if something needs
another pair of hands.

Before taking the next item from the queue, review the core again with fresh eyes. The
first audit (2026-10-01) produced most of the work above, so a second look now pays. Work
from the code and from measurements, not only from the docs:

1. **Read the code of the hot paths** and judge it against the best published practice:
   - frame setup and the render graph;
   - GPU scene upload and extraction;
   - culling, the forward shading and its register use;
   - the frame renderer;
   - the job system;
   - ECS iteration;
   - asset streaming;
   - networking per tick.
2. **Measure what has no number yet:**
   - CPU time per frame broken down by system, with Tracy on Sponza and on a stress scene
     with many moving and skinned objects;
   - worst-frame hitches while flying through a scene;
   - start-up time and time to first frame;
   - shader and pipeline warm-up;
   - GPU and CPU memory;
   - all of it on both GPUs.
3. **Look for structural weak spots:**
   - APIs that disagree with each other;
   - duplicated wiring (for example the test bed vs. the runtime vs. the benches);
   - `unsafe` code without a written invariant;
   - missing tests (what would break silently);
   - docs that drifted from the code;
   - features that cost something when unused.
4. **Compare with what a shipped engine needs and we lack.** Examples: shader permutation
   control, LOD and impostors, terrain, foliage, water, decals, UI rendering performance,
   localization, save games, the crash pipeline in shipped builds, platform pacing.
5. **Write the findings** to `docs/research/core-audit-2.md`, ranked by impact on the core's
   quality and speed, with a cost estimate for each. Merge them with §5 and §6 into a new
   queue, then report to the owner before starting.
