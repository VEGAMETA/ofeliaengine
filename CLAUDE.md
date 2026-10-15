# CLAUDE.md

Instructions for Claude working autonomously on this engine.
Read this file in full at the start of every session. If anything here is outdated
or contradicts the actual state of the repository, propose a change via an ADR (see §3).

The engine is called **ofeliaengine** (team decision, 2026-09-30). In code the prefix stays `engine` (crates `engine_*`, `ENGINE_*` variables); the binary is `ofeliaengine.exe` (package `engine_cli`).

---

## 0. Core principles

1. **Blazing fast is priority #1.** Runtime speed, editor responsiveness, input latency,
   developer iteration time. Every decision is checked for its performance impact first.
   No measurement, no decision.
2. **Fully open source.** All engine code is open. No closed static libraries in the build.
   Closed vendor technologies (DLSS, XeSS, etc.) are supported **only** as optional DLLs
   that the end user drops in themselves. We never ship or commit them.
3. **Game developer convenience.** Everything is automated behind "one button":
   shader compilation, asset cooking, builds. Hot reload for everything that can be reloaded.
   The editor must have the best UX, not "the same as everyone else".
4. **Platform: Windows 11 x64 only.** Use Windows 11 platform features freely, with no regard
   for portability. OS abstractions stay thin.
5. **Research first, then write.** For any non-trivial subsystem, study how it is done in open-source
   engines and in academic work (§4), and record your findings.

---

## 1. Stack (fixed)

| Area | Decision |
|---|---|
| Core language | Rust (stable; nightly only behind a feature flag and with an ADR) |
| OS / arch | Windows 11, x86_64-pc-windows-msvc |
| Graphics API | Vulkan 1.3 (`ash`), no intermediate layers such as wgpu |
| Shaders | Slang → SPIR-V |
| Scripting | Luau (via `mlua`, `luau` feature + native codegen) |
| 3D physics | Jolt (vendored, via JoltC). Own Rust implementation deferred (ADR 0028, §8) |
| 2D physics | Box2D v3 (vendored C source behind a C shim; ADR 0028) |
| Audio | Own mixer + DSP graph, WASAPI directly, VST3 hosting (SDK is MIT) |
| Profiling | Tracy (mandatory everywhere) + built-in editor profiler |
| Engine license | MIT as a placeholder (ADR 0020); the humans choose the final license before the first release. No copyleft dependencies |

Decisions change only via an ADR backed by measurements.

---

## 2. Repository layout (target)

```
/crates
  engine_core            math (SIMD), memory (arenas, pools), containers, job system, time, hashing
  engine_platform        Win32 window, raw input, timers, IoRing, CPU sets, MMCSS, DPI
  engine_rhi             Vulkan: device, queues, memory, descriptors (bindless), synchronization
  engine_render          render graph, 3D renderer, materials, post-processing, viewports
  engine_render2d        2D renderer: batching, sprites, tilemaps, vector primitives, 2D lighting
  engine_shader          Slang compilation, permutations, cache, pipeline cache, hot reload
  engine_upscale         TAAU (own) and the `Upscaler` interface shared by every upscaler
  engine_ffx             the FidelityFX SDK built from source as one library: FSR 3.1, Brixelizer, Brixelizer GI (C++ via `cc`; ADR 0090)
  engine_fsr             FSR 3.1 over engine_ffx (ADR 0060)
  engine_brixelizer      Brixelizer GI over engine_ffx: our GI's measured reference (ADR 0090)
  engine_dlss            DLSS SR/DLAA and frame generation through Streamline, user-supplied DLLs (ADRs 0063, 0091)
  engine_frame           the frame renderer: composes a view's frame (scene, TAA/upscaling, post; ADR 0072)
  engine_latency         frame pacing, limiter, VK_NV_low_latency2, VK_AMD_anti_lag
  engine_physics         PhysicsBackend trait, shared types, test scenes
  engine_physics_native  own physics (deferred, ADR 0028)
  engine_physics_jolt    Jolt backend (via the JoltC C wrapper)
  engine_physics2d       2D physics on Box2D v3
  engine_audio           mixer, buses, DSP, streaming, spatial audio
  engine_audio_vst3      VST3 host (scanning, isolation, PDC, state)
  engine_video           video: WebM, AV1 headers, Vulkan Video decoding, rav1d as the fallback (ADR 0100)
  engine_ecs             ECS / scene model
  engine_assets          import, cook, DDC (derived data cache), streaming
  engine_streaming       a cooked scene streamed within the GPU memory budget (ADR 0076)
  engine_script          Luau: VM, bindings, .d.luau generation, hot reload, DAP debugger
  engine_anim            skeletal animation, blending, IK
  engine_nav             navigation
  engine_net_transport   encrypted UDP connections, channels, network simulator, NAT traversal (ADR 0070)
  engine_net             replication, client prediction, lag compensation over the transport (ADR 0074)
  engine_text            shaping, rasterization (MSDF), layout
  engine_ui              in-game UI (runtime)
  engine_profiler        Tracy integration + built-in panels
  engine_log             logging backend of the `log` facade, lock-free ring (ADR 0061)
  engine_crash           crash reports: minidumps, panic reports, log tail (ADR 0061)
  engine_editor          editor
  engine_runtime         game executable (player)
/tools                   CLI: cook, shaders, bench, pack
/bench                   benchmarks (criterion + scene benchmarks)
/tests                   golden-image render tests and the GPU time gate (tests/golden, ADR 0062),
                         physics determinism, integration tests
/docs
  /adr                   architecture decision records (NNNN-title.md)
  /research              research notes (sources, conclusions)
  /research/ux           editor UX research
  /perf                  budgets, reference hardware, measurement history
  /dev-guide             documentation for game developers
/third_party             source only, with licenses; listed in THIRD_PARTY_NOTICES.md
```

If the structure does not exist yet, create it as needed, not all at once.

---

## 3. How to work (loop for every task)

1. **Understand the task.** Read related ADRs and `docs/research/*`. If there is no data, research it (§4).
2. **Architectural decision?** (new subsystem, dependency change, data format, public API,
   threading model) → write an ADR in `docs/adr/NNNN-*.md`: context, options (at least 2),
   measurements/arguments, decision, consequences.
3. **Minimal prototype** → tests → benchmark → profile in Tracy.
4. **Implementation.** Small, self-contained commits.
5. **Report** in the PR/commit description: what was done, before/after numbers, what was not done, risks.

### Parallel agents
Several agents may work at once, each on one task in its own git worktree with its own
`CARGO_TARGET_DIR` (never shared), ending in a pull request that the coordinator merges after
`tools/check.ps1 -Gpu -Perf` on both GPUs. Their rules, including the owner's goals and the
team decisions to respect, are in `docs/agent-rules.md`; every agent reads them first.

### When to stop and ask the humans
- Irreversible architectural decisions (on-disk asset format, ECS model, public scripting API).
- Any licensing uncertainty.
- A dependency whose license is not on the allowed list (§15).
- A decision that trades performance for convenience or vice versa. Show numbers for both options.
- Doubt about what the team wants. Do not guess.

### Do not
- Optimize blindly. Profile first.
- Add a dependency for a single function you could write in an hour.
- Commit third-party binaries (DLSS, Streamline, XeSS, VST plugins, etc.).
- Copy code from GPL/LGPL/AGPL projects (Blender, Ardour, LMMS, etc.). You may read them
  for ideas, but the research note must state explicitly that no code was carried over.
- Leave a `TODO` without a link to an issue or ADR.

---

## 4. Research: where to look and how to record it

Before implementing a subsystem, study **at least 2–3 open-source implementations** and,
where applicable, academic papers. Output: `docs/research/<topic>.md` with sources
(links, versions/commits, licenses), how each one works, pros and cons, what we take, what we
don't, and why.

**Permissively licensed code** (MIT/Apache/BSD/Zlib) may be adapted with attribution in
`THIRD_PARTY_NOTICES.md`. **GPL code**: ideas only.

### Open-source reference projects
- **Engines:** Godot (MIT), Bevy (MIT/Apache), Wicked Engine (MIT), O3DE (Apache/MIT),
  Fyrox (MIT), The Forge (Apache), Filament (Apache), Granite (MIT, Themaister's blog),
  kajiya (MIT/Apache).
- **Physics:** Jolt (MIT), Box2D v3 (MIT), Rapier/Parry (Apache), PhysX 5 (BSD-3), Bullet (Zlib).
- **Audio:** miniaudio (public domain/MIT-0), Steam Audio (Apache 2.0), VST3 SDK (MIT),
  symphonia (MPL-2.0, ADR required), Ardour (GPL, ideas only).
- **UI/text:** Dear ImGui (MIT), egui (MIT/Apache), Taffy (MIT), Parley/Vello/Xilem (Linebender, Apache/MIT),
  rustybuzz / swash (MIT/Apache), msdfgen (MIT), RmlUi (MIT).
- **Editors:** Blender, Godot, Unreal, Unity, Houdini, Figma, VS Code. Read the first two as code;
  study the rest as products.
- **Tools:** Tracy (BSD), RenderDoc (MIT), RAD Debugger (MIT), PresentMon (MIT), meshoptimizer (MIT).

### Papers and talks (starter list, extend it)
- **Physics:** Catto's GDC talks (Soft Constraints, Understanding Constraints) and the Box2D blog
  (Solver2D, SIMD, determinism); Rouwe, "Architecting Jolt Physics for Horizon Forbidden West" (GDC 2022);
  Macklin et al., "Small Steps in Physics Simulation" (2019); Müller et al., "Detailed Rigid Body
  Simulation with Extended Position Based Dynamics" (2020); Ericson, "Real-Time Collision Detection";
  Gregorius's GDC talks on SAT and contact manifolds.
- **Rendering:** Wihlidal, "Optimizing the Graphics Pipeline with Compute" / GPU-driven rendering;
  Karis, Nanite (SIGGRAPH 2021); Frostbite PBR course notes; Filament's PBR documentation.
- **Threading:** Gyrling, "Parallelizing the Naughty Dog Engine Using Fibers" (GDC 2015).
- **Real-time audio:** Bencina, "Real-time audio programming 101: time waits for nothing".
- **UX:** Nielsen's heuristics, Fitts's law and the Hick–Hyman law, Raskin's "The Humane Interface"
  (on modes), Krug's "Don't Make Me Think", Bret Victor's "Inventing on Principle" (immediate feedback).

Verify currency: SDKs and extensions change. Any fact in a research note about an SDK or driver
version must include the date it was checked.

---

## 5. Performance

### Code rules
- **Zero heap allocations per frame** in steady state. Enforced by a test with a tracking
  allocator in dev builds. Per-frame data lives in arenas reset once per frame.
- Data-oriented design: SoA for hot data, dense arrays, indices instead of pointers.
- SIMD: baseline target `x86-64-v2` (the Windows 11 24H2 minimum). AVX2/AVX-512 via
  runtime dispatch (`is_x86_feature_detected!` / `multiversion`). Do not raise the baseline without an ADR.
- Global allocator: mimalloc; in dev builds, a wrapper that collects statistics.
- No locks on hot paths. No `Mutex` on the render or audio threads.
- No async runtime (tokio etc.) in the core. Allowed in tools.
- No panics on runtime hot paths. Errors go through `Result`; degrade instead of crashing.
- Cache lines: align data shared between threads to avoid false sharing.

### Threads and scheduling
- Own job system (work stealing). Before implementing, compare rayon, bevy_tasks, enkiTS and
  Naughty Dog's fiber approach; record the result in an ADR.
- Hybrid CPUs (P/E cores): place threads via `GetSystemCpuSetInformation` / `EfficiencyClass`.
  Critical threads (main, render, audio) go on P-cores.
- Disable power throttling for the game process (`SetProcessInformation`, `ProcessPowerThrottling`).
  The editor keeps it, so it saves power while its window is in the background (team
  decision, 2026-09-27); measurements pass `--no-throttle`.
- Waiting uses high-resolution waitable timers (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`), not `Sleep`.
- MMCSS: audio thread uses the "Pro Audio" task, render thread uses "Games".

### Budgets (initial; refine via ADR on the reference hardware in `docs/perf/reference-hw.md`)
| Metric | Target |
|---|---|
| Allocations per frame (steady state) | 0 |
| Frame spikes | no frame > 2× median over a 10-minute soak test |
| Editor cold start (empty project) | ≤ 2 s |
| Entering Play from the editor | ≤ 1 s |
| Luau script hot reload | ≤ 100 ms |
| Shader edit → on screen | ≤ 500 ms |
| Hot patch of Rust game code | ≤ 3 s |
| Audio device buffer | ≤ 5 ms (e.g. 240 samples @ 48 kHz) |
| Asset import | never blocks the UI |
| Input-to-photon | minimize; measure at every milestone |

### Measurement
- Tracy zones in every subsystem from day one. A dedicated `profiling` build profile.
- Benchmarks: `criterion` for micro-benchmarks, scene benchmarks in `/bench` with fixed scenes.
- **CI performance regressions:** benchmarks run on a dedicated machine. A regression above the
  threshold (initially 3%) fails the build.
- Latency and frame pacing: PresentMon, low-latency markers, and hardware measurement
  (LDAT/OSLTT) at milestones where possible.
- Measurement history goes in `docs/perf/history.md` (briefly: date, commit, scene, numbers).
- **Per-pass GPU time gate** (ADR 0062): `tools/check.ps1 -Perf` renders fixed scenes on both
  GPUs and compares every pass with the vendor's baseline (`tests/golden/perf`); 3 % and more
  than max(5 µs, 0.5 % of the frame) is a regression; uniform shifts (clocks, other work) are
  reported as such. Advisory on ref-1 until a dedicated machine exists, but every merge checks it.
- **A feature costs nothing where it is not used — not even shader registers.** On the AMD
  driver, dead code under a specialization constant still raised forward shading from 64 to 72
  VGPRs (9–13 % slower). Use separate entry points with literal arguments and lazily created
  pipelines, or keep the parameter out of the shader (the LOD bias lives in the samplers).
  Check with `ENGINE_PIPELINE_STATS=1 ENGINE_ADAPTER=radeon scene_bench --sky --pipeline-stats`
  (ADR 0058, ADR 0060); `tools/check.ps1 -Perf` warns when any pipeline's registers grow
  (`perf_gate --registers`, ADR 0062).

---

## 6. Rendering (Vulkan 1.3)

### Baseline device requirements
Core 1.3: dynamic rendering, synchronization2, timeline semaphores, buffer device address,
descriptor indexing (bindless). Optional, with fallbacks: mesh shaders (`VK_EXT_mesh_shader`),
ray tracing, `VK_EXT_descriptor_buffer`, `VK_EXT_graphics_pipeline_library`,
`VK_KHR_pipeline_binary`, `VK_KHR_present_wait`/`present_id`. The minimum GPU tier is fixed
in an ADR.

### Architecture
- `engine_rhi` is a thin layer over `ash`. It does not try to be a cross-platform RHI.
  Memory: `gpu-allocator` or our own (compare in an ADR).
- **Render graph:** automatic barriers, transient resource aliasing, async compute,
  culling of unused passes.
- **GPU-driven:** GPU culling (frustum + HiZ occlusion), meshlets, indirect draws.
  Evaluate visibility buffer vs. clustered forward+ and decide via ADR.
- HDR output, VRR, user-selectable present mode.
- Debugging: debug utils labels on every pass, RenderDoc API integration (capture via hotkey
  from the editor), `VK_EXT_device_fault` and markers for diagnosing GPU crashes.

### Viewports (one shared system for 2D and 3D)
A viewport is a first-class entity, not a "camera window":
- its own camera, resolution, post-processing, visibility layers and render tick;
- nesting: viewport inside a viewport, viewport as a texture in the 3D world and in UI;
- split screen, picture-in-picture, minimaps, portals;
- pixel-perfect mode with integer scaling;
- the editor uses the same system (one codebase, no "special" viewports);
- each viewport is a render graph node; its cost is visible separately in the profiler.

### 2D renderer
- Batching via bindless textures (avoid atlas juggling where possible), instancing.
- Sprites, 9-slice, tilemaps (chunked, GPU culling), particles.
- Vector primitives via SDF (lines, rounded shapes, outlines) and MSDF text.
- 2D lights and shadows, normal maps for sprites.
- Full custom shader support (the same Slang pipeline as 3D).
- Sorting: layers + y-sort + user key; deterministic.

### Motion blur: never from the player's camera in gameplay (ADR 0067)
Team decision (2026-10-01): blur from the camera's own motion is forbidden while the game is
interactive, by the engine's license (the final license must say so; ADR 0020) and by the API:
camera motion blur exists only on views marked cinematic (cutscenes, sequences); a gameplay view
cannot turn it on. The full effect is implemented and tested; only its use is limited.

### Upscaling and frame generation
`Upscaler` interface: color, depth, motion vectors, exposure, jitter, reactive/transparency masks.
The engine always provides the full set of inputs. Implementations:
1. **Own TAAU**: always available; quality reference and fallback.
2. **FSR 3.1** (FidelityFX SDK 1.1.x, MIT, supports Vulkan): built from source as part of the engine.
   **FSR 4 is currently DX12-only** (FidelityFX/FSR SDK 2.x) and not available on Vulkan.
   Re-check periodically and record the check date in `docs/research/upscalers.md`.
3. **DLSS (SR / RR / FG)** via NVIDIA Streamline (open source). The engine **does not ship**
   NVIDIA binaries. The loader looks for the files in `<game>/plugins/nvidia/` (and/or the game folder):
   `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll`, `nvngx_dlss.dll`, etc., depending on the
   Streamline version. Verify the exact list against the current Streamline documentation.
   - Signature (`WinVerifyTrust`) and version checks. If files are missing or incompatible, the option
     is hidden in settings, with no errors and no crashes.
   - Streamline on Vulkan requires the loader interposer/hooks. Study this and describe it in an ADR
     before implementing.
4. **XeSS**: same user-supplied DLL pattern (`libxess.dll`), if the SDK version supports Vulkan.

Player and developer documentation in `docs/dev-guide/upscalers.md`: which files, where to put them,
how to verify.

---

## 7. Input latency and frame pacing: a first-class subsystem

- **NVIDIA Reflex on Vulkan** is available through the driver extension `VK_NV_low_latency2`.
  **No DLL is required.** This is the primary path. If DLSS Frame Generation runs through Streamline,
  Reflex must also go through Streamline (`sl.reflex` / `sl.pcl`), i.e. through user-supplied DLLs.
  Both paths sit behind a single `LatencyProvider` interface.
- **AMD Anti-Lag 2** on Vulkan: the `VK_AMD_anti_lag` extension, also no DLL.
- **Intel XeLL**: via the user-supplied XeSS SDK DLL, if available.
- Our own measures, which work on any GPU:
  - late input sampling; raw input (`WM_INPUT`) on a dedicated thread with timestamps;
  - configurable frames in flight (1–3);
  - `present_wait` to control queue depth;
  - frame limiter just below the display refresh rate under VRR;
  - pipelining simulation and rendering without extra buffering.
- Markers (simulation, render submit, present) are shared across all providers and Tracy.
- Built-in latency overlay in dev builds.

---

## 8. Physics

### Plan (team decision)
1. **P0: interface.** `PhysicsBackend` trait + shared types (bodies, shapes, constraints, queries,
   contact events). Design the API so Jolt and our own physics are interchangeable.
2. **P1: reference.** Jolt backend via JoltC (C wrapper) + our own Rust bindings.
   This is both the comparison baseline and the fallback.
3. **P2: own physics in Rust. Deferred** (team decision 2026-09-26, ADR 0028): Jolt stays the
   3D physics. P2 is reopened only on a trigger listed in ADR 0028 (for example rollback cost
   that Jolt's state save/restore cannot meet), with measurements.
4. **Decision.** If P2 is reopened: compare on the benchmark suite using the criteria below. If our
   physics does not catch up with Jolt within a reasonable time, we stay on Jolt. The decision is
   recorded in an ADR with numbers.

### Benchmark suite (`/bench/physics`)
Pyramids and stacks (stability), thousands of bodies in a pile, ragdolls, fast bodies (CCD),
scenes with lots of static geometry (mesh colliders), batched raycasts and shape casts,
character controller on complex geometry, joints/chains, island sleep and wake-up.
Reference: the scenes in Jolt's own `PerformanceTest`. Metrics: step time (median, p99),
multi-core scaling, memory, stability (jitter, tunneling), determinism.

Initial acceptance criterion: no more than 10% slower than Jolt on the suite with comparable
stability. Adjusted via ADR.

### Directions for our own physics (if P2 is reopened; to research, not dogma)
- Solver: soft step / TGS with substepping, graph coloring for parallelism,
  wide SIMD solver (AVX2), warm starting. Compare against XPBD.
- Broadphase: dynamic AABB tree / BVH with incremental rebuilds; compare against SAP.
- Narrowphase: GJK/EPA, SAT for boxes and convex hulls, manifold reduction.
- CCD: speculative contacts + conservative advancement for fast bodies.
- Islands, sleeping, multithreaded step pipeline.
- **Determinism** (at minimum on the same build and CPU class): fixed timestep,
  stable processing order, control over FMA and vectorization, our own transcendental functions.
  Determinism test in CI.
- Character controller: study Jolt's `CharacterVirtual`.
- Non-standard gravity (fields, attractor points, arbitrary per-body vectors) is a first-class
  API feature, not a hack.
- Debug visualization: contacts, AABBs, islands, sleeping bodies, solver load. All in the editor.

---

## 9. Shaders and materials: "one button"

Goal: the developer never thinks about shader compilation, and players never see hitches.

- **Language:** Slang. Modules, generics and interfaces for organizing materials and permutations.
- **Materials:** material graph in the editor → generated Slang. Plus hand-written Slang with full
  support: custom shaders, compute, post-processing, 2D, particles.
- **Permutations:** minimize via bindless, specialization constants and uber-shaders.
  Every new permutation axis must be justified with its variant count.
- **"Compile All Shaders" button** in the editor and `ofeliaengine shaders build` in the CLI:
  - finds all materials and shaders in the project and every combination actually used;
  - parallel compilation on all cores, cache keyed by hash (source + includes + flags + compiler version);
  - DDC (derived data cache), shared across the team;
  - report: variant count, time, errors linked to file and line.
- **PSO collection:** the editor and playtests automatically record the set of pipelines actually
  used (state + shaders). At game build time this becomes the warm-up list.
- **On the player's machine:**
  - SPIR-V is precompiled at build time;
  - `VK_EXT_graphics_pipeline_library` for fast linking, with full pipelines optimized in the background;
  - persistent `VkPipelineCache` / `VK_KHR_pipeline_binary` with driver header validation;
  - warm-up on first launch or after a driver update, with progress shown and in the background where possible.
- **Hot reload:** file save → recompile affected variants → swap pipelines without restarting.
  Compile errors are shown in the editor; the previous version keeps running.
- Debugging: debug info in dev builds (Slang source visible in RenderDoc/Nsight),
  shader printf in dev mode.

---

## 10. Scripting (Luau)

- `mlua` with Luau and native codegen. Sandboxed by default.
- **Bindings are generated** from Rust (proc macro) together with `.d.luau` type files for luau-lsp.
  Autocompletion and type checking in the IDE are mandatory.
- **Hot reload** at module level with state preserved: a convention for keeping state outside
  reloadable code; explicit `on_reload` hooks.
- **Debugger:** DAP adapter (breakpoints, stepping, watch, call stack), for VS Code and built into the editor.
- **Performance:** hot paths in scripts are measured. Luau traces are visible in Tracy.
  Heavy logic moves to Rust.
- **Rust game code:** a second tier of game modules in Rust with hot patching (evaluate `subsecond`
  and `hot-lib-reloader`, decide via ADR).
- Script errors do not crash the game in the editor: show the error, pause the object, jump to the line.

---

## 11. Audio

### Core
- Output: WASAPI directly (`IAudioClient3` for small periods in shared mode; exclusive mode optional).
- Audio thread: MMCSS "Pro Audio", high priority, on a P-core.
- **Real-time rules on the audio thread:** no allocations, locks, system calls, logging or I/O.
  Communication via lock-free queues (`rtrb` or our own). In dev builds, `assert_no_alloc`
  on the audio thread.
- Mixer: bus graph (buses, sends, sidechain), graph compiled into a linear schedule,
  32-bit float, SIMD, block size = device period.
- Voices: priorities, virtualization, per-category limits.
- Streaming and decoding (Opus/Vorbis/WAV/FLAC) off the audio thread.
- Spatial audio: evaluate Steam Audio (Apache 2.0) against our own HRTF implementation.
- Built-in DSP effects in Rust: EQ, compressor/limiter, reverb, delay, filters.

### VST3 hosting (VST3 only; SDK is MIT)
- Plugin scanning in a separate process (a crashing plugin does not take down the editor).
- In the editor, optional out-of-process plugin isolation (audio over shared memory).
- Plugin delay compensation (PDC), parameter automation, binding parameters to game variables.
- Plugin state save/restore (`IComponent::getState` / `setState`).
- Plugin UIs (`IPlugView`) in editor windows.
- **VST3 is supported in the game runtime**, but `docs/dev-guide/audio.md` and the editor
  (a warning when building with runtime VST3) explain to the developer why they shouldn't:
  third-party plugin redistribution licenses, unpredictable allocations and latency,
  stability, copy protection. The recommended path is baking (bounce/freeze) the effect
  in the editor or replacing it with built-in DSP.

---

## 12. Editor: UX first

### Principles
- **Everything is undoable.** Every action is a command inside a transaction. Global undo/redo
  with a history panel. No actions without undo, except explicitly marked ones.
- **Nothing blocks the UI.** Import, compilation and builds are background tasks with a task panel.
  No modal progress dialogs.
- **Minimal modes** (Raskin): editing during Play; changes made in Play can be kept explicitly.
- **Command palette** (like VS Code): every action is reachable by search. Everything is searchable:
  assets, scene objects, settings, commands.
- **Keyboard-first:** everything is keyboard-accessible, hotkeys are rebindable, conflicts are highlighted.
- **Immediate feedback** (Bret Victor): changing a parameter is instantly visible in the scene
  (shaders, materials, physics, audio).
- **Multi-selection and multi-editing** in the inspector; drag and drop wherever it feels natural.
- **Errors next to their cause:** clicking an error takes you to the object, asset or line of code.
- Dockable panels, saved layouts, multiple windows and monitors, HiDPI.
- Autosave and crash recovery.
- Accessibility: UI scaling, status colors distinguishable for color-blind users, theme design tokens.
- **Editor performance** is held to the same standard as the game: input response within a frame,
  stable frame rate in heavy scenes, lazy panel rendering.

### Process
- Before building a panel or tool, study how at least 3 editors do it
  (Blender, Unreal, Unity, Godot, Houdini, Figma, VS Code). Output:
  `docs/research/ux/<panel>.md`: what works, what annoys, what we take.
- Design from scenarios: "the developer wants X" → number of clicks/keystrokes → remove the excess.
- Editor UI framework: our own system on top of `engine_render` (the same viewport system):
  immediate-mode building with retained per-widget state and full-tree layout, own Clay-like
  layout (Taffy evaluated, not used for the editor); text: `engine_text` (ADR 0030, ADR 0025).
  egui is acceptable for internal debug overlays.
- Once there are users, keep extending `docs/research/ux/findings.md`.

### Required editor tools
Scene and hierarchy, inspector, asset browser with previews, material editor (graph + code),
animation editor, audio mixer with VST3, console, profiler (§14), script debugger,
physics and navigation visualizers, 2D editor (tilemaps, sprites), viewport manager,
"one button" build.

---

## 13. Assets, scenes, data

- **Source assets are text, VCS- and merge-friendly** (RON/TOML, stable ordering,
  stable GUIDs). Team workflow: minimal conflicts.
- **Cooking** into a binary runtime format with zero-copy loading (evaluate `rkyv`).
- **DDC**: a shared derived data cache (textures, meshes, shaders) for the team.
- Import: glTF 2.0 (primary), textures → KTX2 (BC7/BC6H/BC5 etc.), meshoptimizer
  (vertex cache, overdraw, LODs, meshlets).
- **Streaming:** Windows 11 IoRing for batched asynchronous reads. Research GPU decompression
  (e.g. GDeflate on compute) separately. DirectStorage is D3D12-only and not our path.
- Hot reload for any asset: file watcher → reimport → swap at runtime.
- Serialization with versioning and migrations. Game saves use a separate, stable format.
- ECS / scene model: compare our own archetype ECS, `bevy_ecs` as a library, `hecs`, flecs (C).
  Criteria: query iteration speed, structural changes, editor friendliness
  (reflection, undo), scripting friendliness. Decide via ADR; this is one of the key decisions.

### Other subsystems
- Animation: skeletal, blending, state machines, IK. Reference: ozz-animation (MIT).
- Navigation: navmesh; reference: Recast/Detour (Zlib).
- Text: shaping (rustybuzz/swash), MSDF atlases, bidi, fallback fonts.
- In-game UI: our own, sharing the UI system with the editor where it does not hurt performance.
- Input: raw input, gamepads (XInput / GameInput, evaluate), remapping, in-game rebinding.
- Localization: string tables, plural rules, per-language fonts.
- Networking (ADR 0019 and its addendum; next after the runtime loop): authoritative fixed
  ticks, snapshot replication of components marked for the network through reflection, client
  prediction with the same systems, lag compensation; RPC/events only for one-shot events,
  never for state (player actions travel in the input stream). Singleplayer has no loopback
  client/server; the player's camera follows the latest input every rendered frame. Co-op hosts
  (listen servers) traverse NAT: UPnP IGD → NAT-PMP/PCP → STUN hole punching → relay, one UDP
  port mapped per session and removed on exit. The transport: ADR 0070 (`engine_net_transport`;
  no I/O thread, connect tokens, AES-256-GCM through CNG, no default third-party servers).
  Replication (ADR 0074, `engine_net`): `NetSchema::replicate::<T>()` per component, precision
  from reflection; per-entity deltas against acknowledged (and extrapolated) states under a
  priority accumulator and per-client byte budget; prediction re-runs the game's systems on
  the predicted entities only; lag compensation queries a ring of hit capsules.

---

## 14. Debugging and profiling

- **Tracy** everywhere: CPU zones, GPU zones (Vulkan timestamps), allocations, locks, messages, frames.
- **Built-in editor profiler:** frame timeline, GPU passes per viewport, physics, audio DSP
  (per-bus and per-plugin load), Luau. An "Open in Tracy" button.
- **Debuggers:** RAD Debugger (MIT, Windows x64) and Visual Studio; generate convenient visualizers
  (`.natvis`) for engine types. Luau via DAP.
- **GPU:** RenderDoc (capture from the editor via hotkey), Nsight Graphics, Radeon GPU Profiler,
  Radeon GPU Detective. NVIDIA Aftermath only as an optional user-supplied DLL.
- **Frame pacing and latency:** PresentMon + built-in overlay.
- **Crashes:** minidumps (evaluate Embark's `minidumper` / `crash-handler` crates), symbols on
  a symbol server, reports with context (scene, recent logs, GPU markers).
- **Logging:** structured, allocation-free on hot paths, per-subsystem filters in the editor.
- **Visualizers:** debug draw API (lines, shapes, world-space text) with channels toggled in the editor.

---

## 15. Build, dependencies, quality

### Cargo profiles
- `dev`: our crates at `opt-level = 1`, dependencies at `opt-level = 3`, fast linker (`lld-link`).
- `profiling`: release + debug info + Tracy.
- `release`: full optimization, Tracy disabled.
- `shipping`: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, PGO (evaluate and enable
  if it improves scene benchmarks).

### Dependencies
- `cargo-deny` in CI. Allowed: MIT, Apache-2.0, BSD-2/3, Zlib, ISC, Unicode, MIT-0, public domain.
  MPL-2.0 only via ADR. GPL/LGPL/AGPL forbidden.
- Fonts and icon sets (assets only, never code): OFL-1.1 and CC BY 4.0 are also allowed,
  shipped unmodified and credited in the "Fonts and icons" section of `THIRD_PARTY_NOTICES.md`
  (author, upstream, version, license) in the same commit (team decision, ADR 0036).
- Every new dependency: why, size, compile time, license, maintenance status. One line in the
  commit description.
- `THIRD_PARTY_NOTICES.md` is updated with every addition.

### Code
- `unsafe` is allowed in the core, with a `// SAFETY:` comment on every block;
  `#![deny(unsafe_op_in_unsafe_fn)]`.
- `clippy` in CI (rule set in `clippy.toml`, agreed with the team). `rustfmt` is mandatory.
- Errors: `thiserror` in libraries, `anyhow` only in tools.
- Public APIs are documented (`///`) with examples. The scripting API gets separate documentation
  in `docs/dev-guide`.

### Tests
- Unit tests in every crate.
- Golden-image render tests (ADR 0062, `tests/golden`): fixed shots on both GPUs compared
  through FLIP with per-vendor images; a change that alters the picture updates them (after
  looking at the renders), a new rendering feature adds its shot.
- Physics determinism (identical state hash after N steps).
- "Zero allocations per frame" test.
- Fuzzing of asset parsers (`cargo-fuzz`).
- Soak test (10+ minutes) for frame spikes and leaks.

### CI
Build all profiles, tests, clippy, fmt, cargo-deny, performance benchmarks with thresholds,
documentation build. Everything on Windows x64.

Every change is checked before merging on the reference machine with `tools/check.ps1`,
with the switches the change calls for (`-Gpu`, `-Deny`, `-Loom`, `-Profiles`; `-Full` for
all); `-Affected` runs only what a branch touches, between commits. Tests run under
cargo-nextest from one workspace build; every crate depends on `engine_workspace_hack`,
which keeps dependency features the same in every build (ADR 0077). GitHub Actions run
the same workflows on demand only (by hand, or the `ci` / `bench` labels): the self-hosted
runner is the same machine (ADR 0037). Benchmarks run there by hand when a change can move
them, into `docs/perf/history.md`.

---

## 16. Documentation for game developers (`docs/dev-guide`)

Written together with the feature, not after. At minimum:
- quick start (from install to a cube on screen in 5 minutes);
- Luau scripting (API, hot reload, debugging);
- shaders and materials (including "Compile All Shaders" and PSO warm-up);
- viewports (2D, 3D, nested, in UI and in the world);
- physics (including non-standard gravity and determinism);
- audio and VST3 (and why runtime VST3 is a bad idea);
- upscalers and low latency (which DLLs the player provides, where to put them, how to verify);
- performance: how to profile your game, common mistakes.

---

## 17. Work order (initial, adjusted by the team)

**Current priorities (team decision 2026-09-30, kept since).** The engine's technical core
comes first: internals that are thought through and fast, measured on both reference GPUs,
before what makes a game developer's day easier. Editor tools, Luau bindings and debugger
and other tooling come only after the core.

**Where things stand, what is in flight, the queue, the known gaps and how the work is
run: `docs/status.md`.** Read it at the start of every session and keep it current when
merging. A new coordinator session starts with the fresh-eyes technical review described
there (§7) before taking the next queue item.

Done so far, in the order taken: transparency (0054), pipeline cache (0055), assets and
streaming (0056, 0057), GTAO and SSS (0058, 0059), FSR and DLSS (0060, 0063), crash and
GPU-fault reports (0061), golden images and the perf gate (0062), the persistent GPU scene
and runtime loop (0064), async compute (0065), the physics API (0066), skeletal animation
(0068), atmosphere and fog (0069), the network transport (0070), local-light and contact
shadows (0071), the frame renderer (0072), reflections (0073), replication and lag
compensation (0074), the GPU memory budget with texture and mesh streaming (0076).

The initial order below is kept for reference; its remaining items follow these.

1. `engine_core`, `engine_platform` (window, input, timers), Tracy, CI, cargo-deny.
2. `engine_rhi` + triangle → render graph → basic 3D (meshes, PBR, shadows) → 2D batcher → viewports.
3. Slang pipeline with hot reload and cache → "Compile All Shaders".
4. ECS/scene (ADR!) → serialization → editor skeleton (scene, inspector, undo, command palette).
5. Luau with bindings, types, hot reload, DAP.
6. Physics: `PhysicsBackend` → Jolt backend → benchmark suite; 2D on Box2D v3; own physics deferred (ADR 0028).
7. Audio: WASAPI + mixer → DSP → VST3 host.
8. Latency (Reflex/Anti-Lag via Vulkan extensions, our own measures) → upscalers (TAAU, FSR 3.1,
   Streamline/XeSS loaders).
9. Asset pipeline: glTF, KTX2, DDC, IoRing streaming.
10. Animation, navigation, text, in-game UI, "one button" game build.

At every milestone: measurements per §5, update `docs/perf/history.md`, a demo scene.

---

## 18. Work report format

```
## Done
## Numbers (before → after, scene, hardware, build profile)
## Decisions (links to ADRs)
## Not done / known issues
## Next steps
## Questions for the team
```
