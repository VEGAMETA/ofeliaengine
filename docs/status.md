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
  `H:\Desktop\ofeliaengine-wt\<name>`, with its own `<name>-target`, and ends with a PR.
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
| Upscalers: TAAU, FSR 3.1, DLSS via Streamline (user-supplied DLLs) | done | ADRs 0060, 0063 |
| Every antialiasing and scaling mode: MSAA 2/4/8× (+ alpha to coverage), FXAA, MLAA, SMAA 1x/S2x/T2x/4x, nearest/bilinear/integer/FSR 1, TAAU/FSR/DLSS; `check()` and the GPU's support list for game menus | done | ADR 0075 |
| Skeletal animation, clip compression, IK, GPU skinning | done | ADR 0068 |
| Physics API on Jolt/Box2D: queries, characters, rollback | done | ADR 0066 |
| Networking: encrypted UDP transport with NAT traversal; replication, prediction, lag compensation, co-op example | done | ADRs 0070, 0074 |
| Assets: glTF, cooking, packs, IoRing streaming; GPU memory budget, texture streaming by GPU feedback, mesh LOD streaming (`engine_streaming`) | done | ADRs 0056, 0057, 0076 |
| Audio, text, 2D, basic UI | solid / basic | §11, ADR 0015 |
| Tests: golden images (FLIP, per vendor), per-pass perf gate, determinism, zero-allocation tests | solid | ADR 0062 |
| Editor, Luau | skeleton only, by plan | §10, §12 |

## 4. In flight

- Faster checks (queue item 0), by the coordinator, after the second core review
  (2026-10-10, `docs/research/core-audit-2.md`; the owner answered its questions the same
  day, see §5). First part: ADR 0077 (cargo-nextest, one build per check, unified
  dependency features, doc tests and formatting for what changed, `-Affected`): a check
  with nothing changed 632 → 168 s.

## 5. Queue (in order)

Merged with the second review's findings (`docs/research/core-audit-2.md`). The owner's
answers of 2026-10-10: faster checks first, then the review's P0 fixes; the player crate
after the rendering queue; custom 3D materials after GI; the discrete AO default stays High.

0. **Faster checks, first after the review** (owner, 2026-10-02, confirmed 2026-10-10). Today
   `check.ps1 -Gpu -Perf` takes 10–25 min, and it grows with every crate and test. Per
   step, from recent runs:

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

   Done (ADR 0077): duplicates removed, one build per check, nextest, `-Affected`, doc tests
   and formatting for what changed (632 → 168 s with nothing changed). Left: fewer
   integration-test binaries per crate (`engine_render`: 36 binaries for 52 tests); the
   linker and `line-tables-only` measurements that close ADR 0001; sccache for agents'
   worktrees; the target directory on an NVMe drive (`C:`, owner 2026-10-10: measure after
   the test changes); the perf step's 3 runs × 200 frames per scene (a variance study first).
0a. **The review's P0 fixes** (§3.1 and §3.3):
   - `engine_rhi::Pipeline` without its copy of the device's function table (1,504 bytes
     per pipeline: the renderers' construction overflows Windows' 1 MiB main thread on
     the Radeon, so `game_loop` crashes on start), the renderers' large state off the
     stack, and a test that builds the frame renderer with every feature on a 1 MiB thread
     on both GPUs; then drop the 16–64 MiB thread workarounds in `perf_gate`, `aa_bench`
     and `flight`;
   - GPU pass times without overlap (exclusive times, or a serialized mode for the gate)
     and the gate's baselines re-checked with them: today a pass after a long one without
     a barrier absorbs its tail (feedback measured 2.4 ms, really 49 µs).
0b. **No visible streaming pop-in on fast camera turns** (owner, 2026-10-02: in UE, models
   and textures failed to load in time when the camera turned quickly). Our ADR 0076 has
   the same risk:
   - texture levels come only from the current view's feedback;
   - mesh levels are asked only by instances in the frustum;
   - request → upload takes about 0.35–0.5 s.

   Plan:
   - feedback over 360° (one face of a small cube around the camera per frame, besides the
     main view); first check it stays clear of the Graphine prediction patents
     (US10748241 family);
   - mesh levels by a sphere around the camera, by distance, not by the frustum;
   - lower latency: newly visible textures first, smaller reads, later GPU decompression.

   Acceptance: a "fast turn" test (180° in 0.2 s) measures the pixels drawn below their
   wanted level and for how long. The target is no visible pop-in at the default budget.
   The test joins the perf gate.
   The review adds: the geometry pool grows on the frame thread when streamed mesh levels
   arrive (4–8 ms on the Radeon at the first fast turn): reserve it from the pack's totals
   and grow ahead off the frame thread; give the upload ring growth and a high-water mark
   (today a full ring drops the view).
0c. Spatial upscales Lanczos-3 (with an anti-ringing clamp) and bicubic Catmull-Rom,
   measured in ADR 0075's table. No Lanczos-2: FSR 1's EASU covers it (owner,
   2026-10-02).
0d. **Motion vectors out of the forward shading** (review §3.2). With motion vectors (every
   default antialiasing) the lit forward shader takes 77–89 VGPRs instead of 64 and the
   vertex stage 78 instead of 49: Sponza's forward +22–27 % on the Radeon, +5–11 % on the
   RTX. Write them in the depth prepass (always drawn with the default AO), or reconstruct
   camera motion from depth and draw object motion for movers only; extend the register
   rule to the default variants (`scene_bench --pipeline-stats` prints them now).
0e. **The present path on the iGPU** (review §3.4): the tone mapper writing the swapchain
   image costs the Radeon 1.1 ms of frame against writing an offscreen image and copying
   it. Offscreen + copy where measured faster, find the cause, a windowed scene in the gate.
0f. **Moving instances** (review §3.5): ~70 ns each per frame on one thread (60,000 make
   the RTX CPU-bound at 5.2 ms). Propagation, extraction, interpolation and upload in
   parallel on the job system; interpolation on the GPU so movers upload per tick, not per
   frame.
0g. Start-up overlap (device creation beside the pack's reading and cooking) and pipeline
   variants created on first use (review §3.8): first frame 450–590 ms warm, 1.2–1.4 s
   with the pipeline cache missing.
1. GPU particles and GPU radix sort.
2. Clustered decals.
3. GI: probe volumes, then DDGI-like. Acceptance check, besides the usual: Sponza's metallic
   medallions on the curtains (roughness 0.28, metallic 0.74) must read gold when seen from
   above in shade. Today they look dark brown there. The material and the shading are
   right (seen from below they are gold), but the shaded floor they reflect has no bounced
   light (2026-10-02).
3b. **Custom 3D materials and shaders** (review §4; after GI: owner, 2026-10-10). ADR
   first: it decides permutations and pipeline management for the whole renderer (§9
   material graph and hand-written Slang).
4. Visibility buffer / meshlets. Write the ADR first, with no persistent per-triangle
   visibility bits (patent, ADR 0035 note).
5. Virtual shadow maps.
6. Remaining post effects: DOF, LUT grading, motion blur only on cinematic views
   (ADR 0067), frame generation. For frame generation, both DLSS FG and FSR 3.1 FG, off by
   default, with latency measured.
6b. The iGPU's expensive passes (review §3.11): TAAU 4.2 ms, bloom 1.6 ms (a single-pass
   downsampler), local shadows redrawn every frame when casters move (a per-frame redraw
   budget; never the patented static/dynamic split).
6c. Memory (review §3.9): one transient heap for both frame slots where no transient
   crosses queues (66 / 41 MiB), the global allocator decision (mimalloc, CLAUDE.md §5,
   never made), CPU memory accounting (0.9–1.1 GiB private bytes unexplained).
6d. Jolt's jobs on `engine_core::jobs` through JoltC's callback job system (review §3.10).
6e. `forward.rs` (5,695 lines) split by feature (review §3.12).
6f. **A player crate** (`engine_app`, review §3.7; after the rendering queue: owner,
   2026-10-10): device, swapchain, latency provider, process setup (power throttling,
   MMCSS, CPU sets), pipeline cache and the frame loop in one place; ECS components for
   lights, skinned meshes and probes; features per view instead of per renderer;
   `RenderDefaults` applied whole. The benches and the test bed move onto it.
7. Then: editor tools, Luau bindings and debugger, other tooling.

## 6. Known gaps and leftovers (candidates for the queue)

- **Rendering:**
  - half-resolution aerial perspective and fog on the iGPU (about 1–2 ms there);
  - froxel volumetric fog;
  - per-pixel local-shadow mask (the shadowed forward shading costs +10–12 % on the iGPU);
  - softer shadow filters;
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
  skinning variants (US9613456B2); motion matching variants; Burley's diffusion profile.
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
  - Ambient occlusion: the discrete default stays High (owner, 2026-10-10), although it
    is 28 % of the RTX's default GPU frame (481 µs; Medium 0.40 ms, FLIP 0.096 vs 0.070).
  - The discrete-GPU default stays TAA. The other candidate is MSAA 4× (+1.2 ms on the
    RTX, sharpest in motion, more flicker); the owner decides.
  - A Streamline `eErrorExceptionHandler` hang was seen once in `aa_bench`.
  - `procedural_contact_shadows` (both vendors) and `procedural_local_shadows` (Radeon)
    differ from their golden images by a few pixels on main too. They pass the FLIP
    tolerance; find why and regenerate them.
- **Runtime:** parallel ECS extraction and upload on the job system (100 % moving objects
  cost 2× the draw list's CPU).
- **Reliability:** two silent process exits (`perf_gate` once, `frame_bench` once) were
  never reproduced or explained. Watch for a recurrence and get a stack.
- **Housekeeping:** ADR 0001 is still "Proposed" (needs measurements on a quiet machine).
  `scene_bench` is wired by hand, outside `engine_frame`. From the second review:
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
