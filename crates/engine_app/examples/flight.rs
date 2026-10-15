//! A game's frame on the shipping path, measured end to end (`docs/research/core-audit-2.md`):
//! what a frame costs on the CPU stage by stage, where its worst frames come from, how
//! long the game takes to show its first frame, and how much memory it holds.
//!
//! A game on the player (`engine_app`, ADR 0099): the player starts the process, the
//! window, the device, latency control, the renderer and the streamed scene; the game
//! (`Flight`) moves the props, walks the characters and flies the camera through its
//! hooks.
//!
//! The game:
//! - a cooked scene from its pack, its textures and mesh levels streamed by GPU feedback
//!   within the memory budget (ADR 0076);
//! - its instances as ECS entities, mirrored into a persistent GPU scene by
//!   `engine_runtime` (ADR 0064), with `movers` cubes in spinning hierarchies of 10;
//! - `characters` skinned characters (glTF Fox) walking in circles, their poses sampled
//!   on the job system every frame and skinned on the GPU (ADR 0068);
//! - Sponza's seven local lights with cached shadows (ADR 0071), sky light (ADR 0046);
//! - with `--probes`, `--gi` and `--ssr`: reflection probes along the courtyard and a
//!   volume of GI probes over the scene as entities, and screen-space reflections (ADR
//!   0099 step 4);
//! - the frame renderer with the device class's defaults: ambient occlusion, the
//!   antialiasing / render size (ADR 0072) and meshlet culling (ADR 0085);
//! - a borderless window over the monitor (or `--window`), presented without vsync (or
//!   with `--vsync`), two frames in flight, latency control on (Reflex on NVIDIA, Anti-Lag 2
//!   on AMD, or `--low-latency`).
//!
//! The camera flies through the nave for `frames` frames of 1/60 s of game time each (the
//! same path on every GPU), turning 180° in 0.2 s every 5 s.
//!
//! Reports start-up phases, CPU per frame by stage (median, 99th percentile, worst), GPU
//! frame and passes, the frames that took over twice the median with what they spent it
//! on, pipelines created during the flight, and memory (device heaps, the OS budget,
//! transient targets, the process: its committed memory by kind, the process heap and
//! Rust's share of it, the largest private allocations).
//!
//! ```text
//! cargo run -p engine_app --release --example flight -- <scene.pack>
//!     [--frames <n>]        frames of flight (default 1800: 30 s of game time)
//!     [--fps <n>]           game time per frame, 1/n s (default 60: the 64 Hz
//!                           simulation ticks in most frames; 240 about one in four)
//!     [--movers <n>]        moving cubes (default 2000)
//!     [--characters <n>]    skinned characters (default 64; needs test-assets/Fox)
//!     [--no-lights]         no local lights
//!     [--no-ao]             no ambient occlusion
//!     [--probes]            four reflection probes along the courtyard, as entities
//!                           (ADR 0073), like the golden shot `sponza_day`
//!     [--gi [spacing]]      global illumination from a volume of probes over the scene
//!                           (ADR 0088), as an entity: probes `spacing` apart (default 1)
//!     [--ssr]               screen-space reflections at half resolution (ADR 0073)
//!     [--probe-interval <n>]   frames between captures of a probe that the scene's
//!                           changes ask for (default 30; 0: every frame)
//!     [--meshlet-culling on|off]  drawing by meshlet (ADR 0085; default: the device
//!                           class's)
//!     [--aa off|taau|fsr]   [--quality native|quality|balanced|performance]
//!     [--vsync]
//!     [--window full-screen|exclusive|windowed]   borderless over the monitor (default),
//!                           the same with the swapchain holding the monitor for itself
//!                           (`VK_EXT_full_screen_exclusive`), or a 1920 × 1080 window fit
//!                           into the work area (ADR 0099)
//!     [--low-latency off|on|boost]   latency control (ADR 0023; default on)
//!     [--no-process-setup]  the process as Windows starts it: power throttling, no MMCSS
//!                           for the frame thread
//!     [--cap <fps>]         at most this many real frames per second (a game's limiter;
//!                           frame generation shows twice as many)
//!     [--present apart|direct]   the frame into an image of its own copied into the
//!                                swapchain image, or straight into it (default: apart
//!                                only when presenting across adapters)
//!     [--pipeline-cache <dir>]   load and save the pipeline cache there (default: none)
//!     [--no-feedback]       textures streamed by distance only, not by what the views
//!                           sampled
//!     [--no-warm-up]        shading variants made on first use only, not also on the
//!                           device's warm-up thread (ADR 0079)
//!     [--fg]                frame generation (FSR 3.1, ADR 0091): a paced presenter shows
//!                           a generated frame before each real one; implies --ui
//!     [--fg dlss]           DLSS frame generation instead, through Streamline (the user's
//!                           DLLs, an RTX 40 or newer): Streamline presents, Reflex paces
//!     [--ui]                a UI layer drawn on every presented frame: a HUD, a panel on
//!                           the nave's wall, a panel on the camera
//!     [--record-pso <file>] the material types' entry points drawn, merged into that PSO
//!                           record at the end (ADR 0092; `ofeliaengine cook --pso`)
//! ```
//!
//! Cook the pack with `ofeliaengine cook test-assets/Sponza/Sponza.gltf -o sponza.pack`.
//! `ENGINE_ADAPTER=radeon` picks the integrated GPU.

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

#[cfg(not(windows))]
fn main() {
    println!("flight: Windows only");
}

#[cfg(windows)]
fn main() {
    windowed::run();
}

#[cfg(windows)]
mod windowed {
    use engine_anim::{Pose, for_each_parallel, local_to_model, skinning_matrices};
    use engine_app::{FrameParts, Game, Player, PlayerSettings, Window};
    use engine_assets::{CookSettings, CookedSkin, DerivedDataCache, cook_scene, import_gltf};
    use engine_core::alloc::EngineAllocator;
    use engine_core::jobs::ThreadPool;
    use engine_core::math::{Affine3A, Mat4, Quat, Vec2, Vec3, Vec4};
    use engine_dlss::DlssFrameGeneration;
    use engine_ecs::{Entity, GlobalTransform, QueryState, Transform, World, pod_component};
    use engine_frame::{Antialiasing, FrameGeneration, Placement, Quality};
    use engine_latency::{LowLatency, Presented};
    use engine_render::forward::LocalShadowSettings;
    use engine_render::graph::RenderGraph;
    use engine_render::material::MaterialDesc;
    use engine_render::mesh::{MeshData, cube};
    use engine_render::reflections::{ProbeSettings, SsrSettings};
    use engine_render::skinning::{SkinnedId, SkinnedMeshData};
    use engine_render::texture_pool::{TextureId, TextureLevels};
    use engine_render2d::{Canvas, Quad};
    use engine_rhi::Allocator;
    use engine_runtime::{
        GiVolume, MeshRenderer, PointLight, ReflectionProbe, SkinnedMesh, SpotLight, Sun,
        look_rotation,
    };
    use engine_streaming::{material_desc, texture_format};
    use std::alloc::{GlobalAlloc, Layout};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Frames left out of the statistics at the start (first uploads, first captures).
    const WARMUP: usize = 120;
    const MIB: f64 = (1u64 << 20) as f64;

    /// Turns its entity about Y, radians per second.
    #[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Spin(f32);
    pod_component!(Spin);

    /// Walks its entity around a circle: center x, z, radius, angular speed.
    #[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Walk {
        x: f32,
        z: f32,
        radius: f32,
        speed: f32,
        angle: f32,
    }
    pod_component!(Walk);

    /// A character's animation state, sampled on the job system.
    struct Character {
        skinned: SkinnedId,
        time: f32,
        pose: Pose,
        model: Vec<Affine3A>,
        palette: Vec<[f32; 12]>,
    }

    /// Where one frame's CPU time went, in microseconds.
    #[derive(Debug, Clone, Copy, Default)]
    struct Stages {
        /// `Frames::begin`: waiting for the frame slot (the GPU two frames back).
        wait: f64,
        acquire: f64,
        simulate: f64,
        propagate: f64,
        extract: f64,
        interpolate: f64,
        animate: f64,
        /// The probe entities and the GI volume mirrored.
        probes: f64,
        streaming: f64,
        /// The pools' `begin_frame`: textures, materials, geometry, skinning.
        pools: f64,
        /// Of which textures, materials, geometry.
        pool_parts: [f64; 3],
        upload: f64,
        compose: f64,
        execute: f64,
        submit: f64,
        present: f64,
        /// From the end of `wait` to the end of `present`: the frame's own CPU work.
        cpu: f64,
        /// From this frame's start to the next one's: what the player sees.
        interval: f64,
        gpu: f64,
        /// The longest pass of the GPU frame `gpu` measures.
        top: (&'static str, f64),
        pipelines: u64,
        arrived: u64,
    }

    /// A column of the CPU table: its name and what it reads from a frame.
    type Column = (&'static str, fn(&Stages) -> f64);

    const COLUMNS: [Column; 17] = [
        ("wait for frame slot", |s| s.wait),
        ("acquire", |s| s.acquire),
        ("tick systems", |s| s.simulate),
        ("transform propagation", |s| s.propagate),
        ("extraction", |s| s.extract),
        ("interpolation", |s| s.interpolate),
        ("animation (job system) + poses", |s| s.animate),
        ("probes + GI volume (entities)", |s| s.probes),
        ("streaming update + budget", |s| s.streaming),
        ("pools: textures, materials, geometry, skinning", |s| {
            s.pools
        }),
        ("scene upload (CPU)", |s| s.upload),
        ("frame renderer: prepare + declare", |s| s.compose),
        ("graph compile + record", |s| s.execute),
        ("submit", |s| s.submit),
        ("present", |s| s.present),
        ("**CPU frame (without the wait)**", |s| s.cpu),
        ("**frame interval**", |s| s.interval),
    ];

    fn percentile(values: &[f64], p: f64) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let mut v = values.to_vec();
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p).round() as usize]
    }

    fn ms(start: Instant) -> f64 {
        start.elapsed().as_secs_f64() * 1e3
    }

    /// `PROCESS_MEMORY_COUNTERS_EX` (psapi.h).
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    /// The engine's allocator (mimalloc, ADR 0086), counting the bytes Rust holds in every
    /// build (its own counting is for debug builds): the process heap serves the C runtime
    /// of every other module (the graphics drivers' included).
    struct Counting;

    const ENGINE: EngineAllocator = EngineAllocator::new();

    static RUST_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);

    // SAFETY: every call is forwarded to the engine's allocator unchanged; the counter is
    // only arithmetic on the sizes.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout, forwarded.
            let p = unsafe { ENGINE.alloc(layout) };
            if !p.is_null() {
                RUST_HEAP_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            }
            p
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as above.
            let p = unsafe { ENGINE.alloc_zeroed(layout) };
            if !p.is_null() {
                RUST_HEAP_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            }
            p
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            RUST_HEAP_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            // SAFETY: the caller's block and layout, forwarded.
            unsafe { ENGINE.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as above.
            let p = unsafe { ENGINE.realloc(ptr, layout, new_size) };
            if !p.is_null() {
                RUST_HEAP_BYTES.fetch_add(new_size, Ordering::Relaxed);
                RUST_HEAP_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            }
            p
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    /// `MEMORY_BASIC_INFORMATION` (winnt.h, 64-bit).
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct MemoryBasicInformation {
        base_address: usize,
        allocation_base: usize,
        allocation_protect: u32,
        partition_id: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }

    /// `HEAP_SUMMARY` (heapapi.h).
    #[repr(C)]
    #[derive(Default)]
    struct HeapSummaryInfo {
        cb: u32,
        allocated: usize,
        committed: usize,
        reserved: usize,
        max_reserve: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualQuery(
            address: *const core::ffi::c_void,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
        fn GetProcessHeap() -> *mut core::ffi::c_void;
        fn HeapSummary(
            heap: *mut core::ffi::c_void,
            flags: u32,
            summary: *mut HeapSummaryInfo,
        ) -> i32;
    }

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_PRIVATE: u32 = 0x2_0000;
    const MEM_MAPPED: u32 = 0x4_0000;
    const MEM_IMAGE: u32 = 0x100_0000;
    const PAGE_WRITECOMBINE: u32 = 0x400;

    /// The process's committed memory by kind (private, mapped, module images): of the
    /// private bytes, the write-combined pages (memory the GPU's driver mapped into the
    /// process; `engine_mapped`: the engine's mapped Vulkan memory), the process heap and
    /// Rust's share of it, and the largest private allocations (review 2 §3.9).
    fn memory_breakdown(engine_mapped: u64) {
        let (mut private, mut mapped, mut image) = (0usize, 0usize, 0usize);
        let mut write_combined = 0usize;
        // Committed private bytes per allocation (its regions are contiguous).
        let mut allocations: Vec<(usize, usize)> = Vec::new();
        let mut address = 0usize;
        loop {
            let mut info = MemoryBasicInformation::default();
            // SAFETY: any address may be queried; `info` is a MEMORY_BASIC_INFORMATION of
            // the size given.
            let written = unsafe {
                VirtualQuery(
                    address as *const core::ffi::c_void,
                    &raw mut info,
                    size_of::<MemoryBasicInformation>(),
                )
            };
            if written == 0 || info.region_size == 0 {
                break;
            }
            if info.state == MEM_COMMIT {
                match info.kind {
                    MEM_PRIVATE => {
                        private += info.region_size;
                        if info.protect & PAGE_WRITECOMBINE != 0 {
                            write_combined += info.region_size;
                        }
                        match allocations.last_mut() {
                            Some(last) if last.0 == info.allocation_base => {
                                last.1 += info.region_size;
                            }
                            _ => allocations.push((info.allocation_base, info.region_size)),
                        }
                    }
                    MEM_MAPPED => mapped += info.region_size,
                    MEM_IMAGE => image += info.region_size,
                    _ => {}
                }
            }
            let Some(next) = info.base_address.checked_add(info.region_size) else {
                break;
            };
            address = next;
        }
        let mut heap = HeapSummaryInfo {
            cb: size_of::<HeapSummaryInfo>() as u32,
            ..HeapSummaryInfo::default()
        };
        // SAFETY: the process heap's handle needs no closing; `heap` has its size in `cb`.
        let summarized = unsafe { HeapSummary(GetProcessHeap(), 0, &raw mut heap) } != 0;
        let rust = RUST_HEAP_BYTES.load(Ordering::Relaxed);
        allocations.sort_by(|a, b| b.1.cmp(&a.1));
        let largest: Vec<String> = allocations
            .iter()
            .take(8)
            .map(|&(_, bytes)| format!("{:.1}", bytes as f64 / MIB))
            .collect();
        if summarized {
            println!(
                "Committed memory: private {:.0} MiB (write-combined pages the GPU's driver mapped {:.0} MiB, the engine's mapped Vulkan memory {:.0} MiB of it; Rust's allocations {:.0} MiB in mimalloc's own memory; the process heap {:.0} MiB committed, {:.0} allocated, for the other modules' C runtime; private memory outside the process heap {:.0} MiB in {} allocations, largest {} MiB), mapped {:.0} MiB, module images {:.0} MiB.\n",
                private as f64 / MIB,
                write_combined as f64 / MIB,
                engine_mapped as f64 / MIB,
                rust as f64 / MIB,
                heap.committed as f64 / MIB,
                heap.allocated as f64 / MIB,
                private.saturating_sub(heap.committed) as f64 / MIB,
                allocations.len(),
                largest.join(", "),
                mapped as f64 / MIB,
                image as f64 / MIB,
            );
        }
    }

    /// The process's working set and private bytes, MiB.
    fn process_memory() -> (f64, f64) {
        let mut c = ProcessMemoryCounters {
            cb: size_of::<ProcessMemoryCounters>() as u32,
            ..ProcessMemoryCounters::default()
        };
        // SAFETY: `c` is a valid PROCESS_MEMORY_COUNTERS_EX with its size in `cb`; the
        // pseudo handle of the current process needs no closing.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut c, c.cb) };
        if ok == 0 {
            return (0.0, 0.0);
        }
        (
            c.working_set_size as f64 / MIB,
            c.private_usage as f64 / MIB,
        )
    }

    fn memory_report(label: &str, allocator: &Allocator, graph: &RenderGraph) {
        let (working, private) = process_memory();
        println!("**Memory, {label}:**\n");
        println!(
            "| heap | device-local | allocator blocks MiB | allocated MiB | dedicated MiB | mapped MiB | OS usage MiB | OS budget MiB |"
        );
        println!("|---|---|---:|---:|---:|---:|---:|---:|");
        let budget = allocator.budget();
        let heap_stats = allocator.heap_stats();
        for (k, (stats, heap)) in heap_stats.iter().zip(budget.heaps()).enumerate() {
            if stats.block_bytes == 0 && stats.dedicated_bytes == 0 && heap.usage == 0 {
                continue;
            }
            println!(
                "| {k} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} |",
                heap.device_local,
                stats.block_bytes as f64 / MIB,
                stats.allocated_bytes as f64 / MIB,
                stats.dedicated_bytes as f64 / MIB,
                stats.mapped_bytes as f64 / MIB,
                heap.usage as f64 / MIB,
                heap.budget as f64 / MIB,
            );
        }
        let engine_mapped: u64 = heap_stats.iter().map(|s| s.mapped_bytes).sum();
        let g = graph.stats();
        println!(
            "\nTransient targets: {:.1} MiB reserved in the heaps this frame used, {:.1} of them shared by the frame slots ({:.1} placed, {:.1} without aliasing), {} resources, {} passes ({} culled). Process: working set {working:.0} MiB, private {private:.0} MiB.\n",
            g.transient_heap_bytes as f64 / MIB,
            g.transient_shared_bytes as f64 / MIB,
            g.transient_placed_bytes as f64 / MIB,
            g.transient_unaliased_bytes as f64 / MIB,
            g.transient_resources,
            g.passes,
            g.culled_passes,
        );
        memory_breakdown(engine_mapped);
    }

    /// Sponza's local lights, as the golden test bed places them (with shadows).
    /// Sponza's seven shadowed lights as entities: five warm points along the nave, two
    /// cool spots from above. Returns how many.
    fn spawn_sponza_lights(world: &mut World) -> usize {
        for k in 0..5 {
            let position = Vec3::new(
                -6.0 + 4.0 * k as f32,
                1.4,
                if k % 2 == 0 { 2.2 } else { -2.2 },
            );
            world.spawn((
                GlobalTransform(Affine3A::from_translation(position)),
                PointLight::new(Vec3::new(24.0, 15.0, 8.0), 8.0).with_shadow(),
            ));
        }
        for x in [-2.0, 6.0] {
            // The spot shines down its entity's −Z axis.
            let down = Quat::from_rotation_arc(Vec3::NEG_Z, Vec3::new(0.15, -1.0, 0.1).normalize());
            world.spawn((
                GlobalTransform(Affine3A::from_rotation_translation(
                    down,
                    Vec3::new(x, 7.0, 0.0),
                )),
                SpotLight::new(Vec3::new(90.0, 100.0, 140.0), 12.0, 0.4, 0.65).with_shadow(),
            ));
        }
        7
    }

    /// The camera at game time `t`: along the nave and back every 30 s, swaying, turning
    /// 180° in 0.2 s every 5 s.
    fn camera_at(t: f32, min: Vec3, max: Vec3) -> (Vec3, Quat) {
        let size = max - min;
        let center = (min + max) * 0.5;
        let phase = (t / 30.0).fract();
        let leg = if phase < 0.5 {
            phase * 2.0
        } else {
            2.0 - phase * 2.0
        };
        let eye = Vec3::new(
            min.x + size.x * (0.15 + 0.7 * leg),
            min.y + size.y * 0.14 + 0.6 * (t * 0.25).sin(),
            center.z + size.z * 0.12 * (t * 0.4).sin(),
        );
        let forward = if phase < 0.5 {
            0.0
        } else {
            core::f32::consts::PI
        };
        // Completed fast turns, smoothed over their 0.2 s.
        let turns = (t / 5.0).floor() + {
            let s = ((t % 5.0) / 0.2).min(1.0);
            s * s * (3.0 - 2.0 * s)
        } - 1.0;
        let yaw = forward + 0.9 * (t * 0.6).sin() + core::f32::consts::PI * turns.max(0.0);
        let pitch = -0.08 + 0.15 * (t * 0.3).sin();
        let dir = Vec3::new(
            yaw.cos() * pitch.cos(),
            pitch.sin(),
            yaw.sin() * pitch.cos(),
        );
        (eye, look_rotation(eye, eye + dir, Vec3::Y))
    }

    /// What DLSS frame generation presented (ADR 0091 §7): Streamline counts the frames it
    /// presented per real frame (`slDLSSGGetState`).
    #[derive(Default)]
    struct DlssReport {
        /// After the warm-up: real frames, frames presented, and when the first and last
        /// were presented.
        real: usize,
        presented: u64,
        first: Option<Instant>,
        last: Option<Instant>,
        /// The last status that was not "ok", and how often.
        failed: usize,
        status: &'static str,
        max_generated: u32,
    }

    impl DlssReport {
        fn record(&mut self, generation: &DlssFrameGeneration, drawn: usize) {
            let Ok(state) = generation.state() else {
                self.failed += 1;
                return;
            };
            self.max_generated = state.max_generated;
            if state.status != 0 {
                self.failed += 1;
                self.status = state.status_text();
            }
            if drawn < WARMUP {
                return;
            }
            let now = Instant::now();
            self.first.get_or_insert(now);
            self.last = Some(now);
            self.real += 1;
            self.presented += u64::from(state.presented);
        }

        fn report(&self, drawn: usize) {
            let seconds = match (self.first, self.last) {
                (Some(first), Some(last)) => (last - first).as_secs_f64().max(1e-9),
                _ => {
                    println!("**Frame generation** (DLSS): nothing presented.\n");
                    return;
                }
            };
            println!(
                "**Frame generation** (DLSS through Streamline, ADR 0091): {drawn} real frames drawn; after the warm-up {} real frames and {} presented in {seconds:.2} s: {:.1} presented per second, {:.1} real ({:.2} presented per real frame; up to {} generated per real frame here). Status: {}.\n",
                self.real,
                self.presented,
                self.presented as f64 / seconds,
                self.real as f64 / seconds,
                self.presented as f64 / self.real.max(1) as f64,
                self.max_generated,
                if self.failed == 0 {
                    String::from("ok")
                } else {
                    format!("{} frames not ok, last: {}", self.failed, self.status)
                }
            );
        }
    }

    /// What the paced presenter presented (ADR 0091): rates, and how evenly the generated
    /// and real frames were spaced.
    fn frame_generation_report(presented: &[Presented], generated_shown: usize, drawn: usize) {
        let timed: Vec<&Presented> = presented
            .iter()
            .filter(|p| p.frame > WARMUP as u64)
            .collect();
        let (Some(first), Some(last)) = (timed.first(), timed.last()) else {
            println!("**Frame generation:** nothing presented.\n");
            return;
        };
        let seconds = (last.presented - first.presented).as_secs_f64().max(1e-9);
        let generated = timed.iter().filter(|p| p.generated).count();
        let real = timed.len() - generated;
        let mut gaps = Vec::new();
        let mut errors = Vec::new();
        let mut spacings = Vec::new();
        for pair in timed.windows(2) {
            if pair[0].generated && !pair[1].generated && pair[0].frame == pair[1].frame {
                let gap = (pair[1].presented - pair[0].presented).as_secs_f64() * 1e3;
                let spacing = pair[1].spacing.as_secs_f64() * 1e3;
                gaps.push(gap);
                spacings.push(spacing);
                errors.push((gap - spacing).abs());
            }
        }
        // What frame generation adds to a real frame's latency: from its GPU work seen done
        // to its present (half a real interval, the generated frame shown first).
        let held: Vec<f64> = timed
            .iter()
            .filter(|p| !p.generated)
            .map(|p| (p.presented - p.ready).as_secs_f64() * 1e3)
            .collect();
        // Every present to the next: the intervals the display gets.
        let intervals: Vec<f64> = timed
            .windows(2)
            .map(|p| (p[1].presented - p[0].presented).as_secs_f64() * 1e3)
            .collect();
        println!(
            "**Frame generation** (FSR 3.1, ADR 0091): {drawn} real frames drawn, {generated_shown} generated frames to show; after the warm-up {real} real and {generated} generated presented in {seconds:.2} s: {:.1} presented per second, {:.1} real.\n",
            timed.len() as f64 / seconds,
            real as f64 / seconds
        );
        println!("| ms | median | p1 | p99 |\n|---|---:|---:|---:|");
        for (name, values) in [
            ("generated → real present", &gaps),
            (
                "spacing (half the real interval, less the margins)",
                &spacings,
            ),
            ("abs(gap − spacing)", &errors),
            ("real frame done → presented (latency added)", &held),
            ("present → present", &intervals),
        ] {
            if values.is_empty() {
                continue;
            }
            println!(
                "| {name} | {:.3} | {:.3} | {:.3} |",
                percentile(values, 0.5),
                percentile(values, 0.01),
                percentile(values, 0.99)
            );
        }
        println!();
    }

    /// The flight's game: movers spinning, characters walking in circles, the camera on its
    /// path through the nave.
    struct Flight {
        spins: QueryState<(&'static mut Transform, &'static Spin)>,
        walkers: QueryState<(&'static mut Transform, &'static mut Walk)>,
        camera: Entity,
        min: Vec3,
        max: Vec3,
        /// Game time: the same path on every GPU, whatever the frame rate.
        game_time: f64,
        frame_time: f64,
        characters: Vec<Character>,
        fox: Option<(CookedSkin, Vec<Affine3A>)>,
    }

    impl Game for Flight {
        fn tick(&mut self, world: &mut World, dt: f32) {
            self.spins.for_each(world, |(t, spin)| {
                t.rotation *= Quat::from_rotation_y(spin.0 * dt);
            });
            self.walkers.for_each(world, |(t, walk)| {
                walk.angle += walk.speed * dt;
                let (sin, cos) = walk.angle.sin_cos();
                t.translation.x = walk.x + walk.radius * cos;
                t.translation.z = walk.z + walk.radius * sin;
                t.rotation = Quat::from_rotation_y(-walk.angle);
            });
        }

        fn present(&mut self, world: &mut World, _alpha: f32) {
            self.game_time += self.frame_time;
            let (eye, rotation) = camera_at(self.game_time as f32, self.min, self.max);
            if let Some(t) = world.get_mut::<Transform>(self.camera) {
                t.translation = eye;
                t.rotation = rotation;
            }
        }

        // Animation: every character's pose on the job system, then its pose for the GPU
        // with the scene instance's transform (bit for bit, for the motion pass).
        fn animate(&mut self, parts: &mut FrameParts<'_>, dt: f32) {
            let Some((skin, inverse_bind)) = self.fox.as_ref() else {
                return;
            };
            let clip = &skin.clips[1 % skin.clips.len()];
            let duration = clip.duration().max(1e-3);
            for_each_parallel(parts.jobs, &mut self.characters, 4, |c| {
                c.time = (c.time + dt) % duration;
                clip.sample(c.time, skin.skeleton.rest(), &mut c.pose);
                local_to_model(&skin.skeleton, &c.pose, &Affine3A::IDENTITY, &mut c.model);
                skinning_matrices(&c.model, &skin.joints, inverse_bind, &mut c.palette);
            });
            // The player places them at their scene instances (`SkinnedMesh`).
            for c in &self.characters {
                parts.skinning.set_palette(c.skinned, &c.palette);
            }
        }
    }

    pub(crate) fn run() {
        let t0 = Instant::now();
        let args: Vec<String> = std::env::args().skip(1).collect();
        let flag = |name: &str| args.iter().any(|a| a == name);
        let option = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
        };
        let number = |name: &str, default: usize| {
            option(name).and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        let Some(pack) = args
            .first()
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from)
        else {
            println!("usage: flight <scene.pack> [options] (see the example's documentation)");
            return;
        };
        let frames_to_run = number("--frames", 1800);
        // Game time per frame (the same flight on every GPU, whatever the frame rate):
        // 1/60 s ticks the 64 Hz simulation in most frames; 1/240 s about one frame in
        // four, as a game at 240 FPS.
        let dt = 1.0 / number("--fps", 60).max(1) as f64;
        let movers = number("--movers", 2000);
        let character_count = number("--characters", 64);
        let lights_on = !flag("--no-lights");
        let probes_on = flag("--probes");
        // `--gi [spacing]`: a volume of GI probes over the scene, `spacing` apart.
        let gi_spacing = flag("--gi").then(|| {
            option("--gi")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(1.0)
        });
        // `--fg`: FSR 3.1's, shown by a paced presenter; `--fg dlss`: DLSS's, through
        // Streamline (ADR 0091).
        let frame_generation = if flag("--fg") {
            match option("--fg") {
                Some("dlss") => FrameGeneration::Dlss,
                _ => FrameGeneration::Fsr,
            }
        } else {
            FrameGeneration::Off
        };
        let ui_on = frame_generation != FrameGeneration::Off || flag("--ui");
        // `--record-pso <file>`: the material types' entry points the flight drew, merged
        // into that PSO record at the end (ADR 0092): a playtest's record for the cook.
        let record_pso = option("--record-pso").map(PathBuf::from);

        // The player's settings from the options.
        let settings = PlayerSettings {
            title: String::from("flight"),
            pack: Some(pack.clone()),
            window: match option("--window") {
                Some("exclusive") => Window::Exclusive,
                Some("windowed") => Window::Windowed {
                    width: 1920,
                    height: 1080,
                },
                _ => Window::FullScreen,
            },
            vsync: flag("--vsync"),
            low_latency: match option("--low-latency") {
                Some("off") => LowLatency::Off,
                Some("boost") => LowLatency::Boost,
                _ => LowLatency::On,
            },
            // `--cap`: real frames at most this often, as a game's limiter (ADR 0091: it
            // caps real frames; frame generation shows twice as many).
            fps_limit: option("--cap")
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|fps| *fps > 0.0),
            frame_generation,
            ambient_occlusion: flag("--no-ao").then_some(None),
            meshlet_culling: match option("--meshlet-culling") {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            },
            antialiasing: match option("--aa") {
                Some("off") => Some(Antialiasing::OFF),
                Some("taau") => Some(Antialiasing::TAAU),
                Some("fsr") => Some(Antialiasing::FSR),
                _ => None,
            },
            quality: match option("--quality") {
                Some("native") => Some(Quality::Native),
                Some("quality") => Some(Quality::Quality),
                Some("balanced") => Some(Quality::Balanced),
                Some("performance") => Some(Quality::Performance),
                _ => None,
            },
            local_shadows: lights_on.then(LocalShadowSettings::default),
            screen_space_reflections: flag("--ssr").then(SsrSettings::default),
            reflection_probes: Some(ProbeSettings {
                scene_interval: number("--probe-interval", 30) as u32,
                ..ProbeSettings::default()
            }),
            texture_feedback: !flag("--no-feedback"),
            // Where the frame is drawn (`engine_render::present`): `--present apart|direct`,
            // by default apart only when the swapchain presents across adapters.
            present_apart: option("--present").map(|v| v == "apart"),
            // No cache unless asked: start-up compiles, the same on every run.
            pipeline_cache: Some(option("--pipeline-cache").map(PathBuf::from)),
            warm_up: !flag("--no-warm-up"),
            process_setup: !flag("--no-process-setup"),
            ..PlayerSettings::default()
        };

        // The glTF Fox imported and cooked through the derived data cache, beside the
        // player's start-up.
        let fox = std::thread::Builder::new()
            .name(String::from("fox"))
            .spawn(move || {
                let phase = Instant::now();
                let jobs = ThreadPool::builder().build().unwrap();
                let cooked = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../test-assets/Fox/Fox.gltf")
                    .canonicalize()
                    .ok()
                    .filter(|_| character_count > 0)
                    .map(|path| {
                        let bytes = std::fs::read(&path).unwrap();
                        let dir = path.parent();
                        let scene = import_gltf(&bytes, dir).unwrap();
                        let cache =
                            DerivedDataCache::new(path.parent().unwrap().join("../.ddc")).unwrap();
                        cook_scene(&scene, dir, Some(&cache), &jobs, &CookSettings::default())
                            .unwrap()
                            .0
                    });
                (cooked, ms(phase))
            })
            .unwrap();
        let mut player = Player::new(&settings).unwrap_or_else(|e| panic!("{e}"));
        player.set_frame_time(Some(dt));
        let (fox, fox_ms) = fox.join().unwrap();
        let mut startup = player.startup().phases.clone();
        startup.push(("Fox cooked from the cache (beside the player)", fox_ms));
        for note in &player.startup().notes {
            println!("{note}");
        }
        let defaults = *player.defaults();
        let (antialiasing, quality) = (player.antialiasing(), player.quality());
        let presentation = player.presentation();
        let adapter_name = player.device().adapter().summary();
        let extent = player.extent();
        let (width, height) = (extent.width, extent.height);

        // Moving props: cubes in spinning hierarchies of 10 along the nave.
        let phase = Instant::now();
        let instances = player.scene_info().meshes.len();
        let (min, max) = player.scene_info().bounds;
        let size = max - min;
        let mut characters: Vec<Character> = Vec::new();
        let mut fox_skin = None;
        {
            let setup = player.setup();
            let cube_mesh = setup.geometry.add_mesh(&cube().as_data()).unwrap();
            let paints: Vec<_> = (0..16)
                .map(|k| {
                    let h = k as f32 / 16.0;
                    setup.materials.add(MaterialDesc::solid(
                        [0.3 + 0.6 * h, 0.55, 0.9 - 0.6 * h],
                        0.0,
                        0.5,
                    ))
                })
                .collect();
            let world = setup.world;
            let hierarchies = movers.div_ceil(10);
            for s in 0..hierarchies {
                let f = (s as f32 + 0.5) / hierarchies as f32;
                let root = world.spawn((
                    Transform::from_xyz(
                        min.x + size.x * (0.1 + 0.8 * f),
                        min.y + size.y * (0.1 + 0.25 * ((s * 7) % 10) as f32 / 10.0),
                        min.z + size.z * (0.3 + 0.4 * ((s * 3) % 10) as f32 / 10.0),
                    )
                    .with_scale(Vec3::splat(0.25)),
                    GlobalTransform::IDENTITY,
                    MeshRenderer::new(cube_mesh, paints[s % paints.len()]),
                    Spin(0.5 + (s % 5) as f32 * 0.3),
                ));
                for c in 0..9 {
                    let a = c as f32 * 0.7;
                    let child = world.spawn((
                        Transform::from_xyz(a.cos() * 2.0, 0.6 * c as f32, a.sin() * 2.0)
                            .with_scale(Vec3::splat(0.6)),
                        GlobalTransform::IDENTITY,
                        MeshRenderer::new(cube_mesh, paints[(s + c) % paints.len()]),
                    ));
                    world.set_parent(child, Some(root)).unwrap();
                }
            }

            // Skinned characters: the Fox cooked above.
            if let Some(mut cooked) = fox {
                let ids: Vec<TextureId> = std::mem::take(&mut cooked.textures)
                    .into_iter()
                    .map(|t| {
                        setup
                            .textures
                            .add(
                                setup.device,
                                &mut *setup.allocator,
                                TextureLevels {
                                    width: t.width,
                                    height: t.height,
                                    format: texture_format(t.format),
                                    levels: t.levels,
                                },
                            )
                            .unwrap()
                    })
                    .collect();
                let fox_material = setup
                    .materials
                    .add(material_desc(&cooked.materials[0], &ids));
                let instance = cooked.instances.iter().find(|i| i.skin.is_some()).unwrap();
                let mesh = &cooked.meshes[instance.mesh as usize];
                let skin = cooked.skins.swap_remove(instance.skin.unwrap() as usize);
                let source = setup
                    .skinning
                    .add_skin(&SkinnedMeshData {
                        mesh: MeshData {
                            positions: &mesh.positions,
                            normals: &mesh.normals,
                            tangents: &mesh.tangents,
                            uvs: &mesh.uvs,
                            indices: &mesh.indices,
                            meshlets: &[],
                        },
                        joints: &mesh.joints,
                        weights: &mesh.weights,
                        joint_count: skin.joints.len() as u16,
                    })
                    .unwrap();
                for k in 0..character_count {
                    let (skinned, drawn) = setup
                        .skinning
                        .add_instance(source, &mut *setup.geometry)
                        .unwrap();
                    let f = (k as f32 + 0.5) / character_count as f32;
                    let walk = Walk {
                        x: min.x + size.x * (0.15 + 0.7 * f),
                        z: min.z + size.z * (0.35 + 0.3 * ((k * 5) % 7) as f32 / 7.0),
                        radius: 0.8 + (k % 3) as f32 * 0.4,
                        speed: 0.6 + (k % 4) as f32 * 0.15,
                        angle: k as f32,
                    };
                    world.spawn((
                        Transform::from_xyz(walk.x, min.y, walk.z).with_scale(Vec3::splat(0.012)),
                        GlobalTransform::IDENTITY,
                        MeshRenderer::new(drawn, fox_material),
                        SkinnedMesh { id: skinned },
                        walk,
                    ));
                    let n = skin.skeleton.joint_count();
                    characters.push(Character {
                        skinned,
                        time: k as f32 * 0.37,
                        pose: skin.skeleton.rest().clone(),
                        model: vec![Affine3A::IDENTITY; n],
                        palette: vec![[0.0; 12]; skin.joints.len()],
                    });
                }
                let inverse_bind: Vec<Affine3A> = skin
                    .inverse_bind
                    .iter()
                    .map(|m| Affine3A::from_mat4(Mat4::from_cols_array(m)))
                    .collect();
                fox_skin = Some((skin, inverse_bind));
            }
        }
        startup.push(("movers + characters", ms(phase)));

        // The player's camera.
        let (eye, rotation) = camera_at(0.0, min, max);
        let extent_len = size.length();
        let camera_component = player.camera(70f32.to_radians(), 0.05, extent_len * 2.0);
        let camera = player.world_mut().spawn((
            Transform::from_translation(eye).with_rotation(rotation),
            camera_component,
        ));
        // The lights as entities (ADR 0099 step 3): Sponza's seven, and the sun.
        let world = player.world_mut();
        let light_count = if lights_on {
            spawn_sponza_lights(world)
        } else {
            0
        };
        world.spawn((
            GlobalTransform(Affine3A::from_rotation_translation(
                Quat::from_rotation_arc(Vec3::NEG_Z, Vec3::new(-0.3, -1.0, -0.2).normalize()),
                Vec3::ZERO,
            )),
            Sun {
                color: Vec3::splat(3.0),
                ambient: Vec3::splat(0.08),
            },
        ));
        // Reflection probes and the GI volume as entities (ADR 0099 step 4): four probes
        // along the courtyard, each across its whole width (the golden shot `sponza_day`'s),
        // and probes over the whole scene.
        if probes_on {
            let cell = Vec3::new(size.x / 4.0, size.y, size.z);
            for k in 0..4 {
                let center = min + cell * Vec3::new(k as f32 + 0.5, 0.5, 0.5);
                world.spawn((
                    GlobalTransform(Affine3A::from_translation(center)),
                    ReflectionProbe::new(cell * 0.5),
                ));
            }
        }
        if let Some(spacing) = gi_spacing {
            world.spawn((
                GlobalTransform(Affine3A::from_translation((min + max) * 0.5)),
                GiVolume::new(size * 0.5, spacing),
            ));
        }

        // The UI layer: a HUD (a crosshair, a bar, a frame for a map), a panel on the nave's
        // wall, a panel on the camera (bottom left, as a helmet's display).
        if ui_on {
            let (w, h) = (width as f32, height as f32);
            let mut hud = Canvas::new();
            hud.draw(
                &Quad::solid(Vec2::new(w * 0.5, h * 0.5), Vec2::new(24.0, 2.0)).color(Vec4::ONE),
            );
            hud.draw(
                &Quad::solid(Vec2::new(w * 0.5, h * 0.5), Vec2::new(2.0, 24.0)).color(Vec4::ONE),
            );
            hud.draw(
                &Quad::rounded_rect(Vec2::new(200.0, h - 60.0), Vec2::new(300.0, 24.0), 6.0)
                    .color(Vec4::new(0.1, 0.1, 0.1, 0.6)),
            );
            hud.draw(
                &Quad::rounded_rect(Vec2::new(170.0, h - 60.0), Vec2::new(232.0, 16.0), 4.0)
                    .color(Vec4::new(0.85, 0.15, 0.1, 1.0)),
            );
            hud.draw(
                &Quad::rounded_rect(Vec2::new(w - 140.0, 140.0), Vec2::new(220.0, 220.0), 10.0)
                    .outline(4.0)
                    .color(Vec4::new(0.9, 0.9, 0.9, 0.8)),
            );
            hud.prepare();
            let mut wall_panel = Canvas::new();
            wall_panel.draw(
                &Quad::rounded_rect(Vec2::new(150.0, 75.0), Vec2::new(300.0, 150.0), 12.0)
                    .color(Vec4::new(0.05, 0.3, 0.6, 0.85)),
            );
            for row in 0..4 {
                wall_panel.draw(
                    &Quad::solid(
                        Vec2::new(150.0, 30.0 + 30.0 * row as f32),
                        Vec2::new(240.0 - 40.0 * row as f32, 10.0),
                    )
                    .color(Vec4::new(0.9, 0.95, 1.0, 1.0)),
                );
            }
            wall_panel.prepare();
            let mut visor = Canvas::new();
            visor.draw(
                &Quad::rounded_rect(Vec2::new(50.0, 25.0), Vec2::new(100.0, 50.0), 6.0)
                    .color(Vec4::new(0.1, 0.8, 0.3, 0.7)),
            );
            visor.prepare();
            let center = (min + max) * 0.5;
            // 3 × 1.5 m on the wall side of the nave, a centimeter per canvas unit, canvas
            // y down.
            let wall_from_panel = Mat4::from_scale_rotation_translation(
                Vec3::new(0.01, -0.01, 1.0),
                Quat::IDENTITY,
                Vec3::new(
                    center.x - 1.5,
                    min.y + size.y * 0.25,
                    center.z + size.z * 0.3,
                ),
            );
            let view_from_visor = Mat4::from_scale_rotation_translation(
                Vec3::new(0.0012, -0.0012, 1.0),
                Quat::IDENTITY,
                Vec3::new(-0.42, -0.2, -0.6),
            );
            let ui = player.ui_mut().unwrap();
            ui.canvases.push((hud, Placement::Screen));
            ui.canvases
                .push((wall_panel, Placement::World(wall_from_panel)));
            ui.canvases
                .push((visor, Placement::Camera(view_from_visor)));
        }

        let mut game = {
            let world = player.world_mut();
            Flight {
                spins: QueryState::new(world),
                walkers: QueryState::new(world),
                camera,
                min,
                max,
                game_time: 0.0,
                frame_time: dt,
                characters,
                fox: fox_skin,
            }
        };

        let mut log: Vec<Stages> = Vec::with_capacity(frames_to_run);
        let mut passes: std::collections::BTreeMap<&'static str, Vec<f64>> =
            std::collections::BTreeMap::new();
        let mut presented: Vec<Presented> = Vec::with_capacity(2 * frames_to_run + 8);
        let mut generated_shown = 0usize;
        let mut dlss = DlssReport::default();
        let mut first_present = None;
        let mut first_idle = None;
        let mut gi_converged = None;
        let mut memory_logged = false;
        let mut drawn = 0usize;
        let mut previous_start: Option<Instant> = None;
        while drawn < frames_to_run && !player.closing() {
            let frame_start = Instant::now();
            let Some(report) = player.frame(&mut game).unwrap_or_else(|e| panic!("{e}")) else {
                continue;
            };
            if let Some(p) = previous_start
                && let Some(last) = log.last_mut()
            {
                last.interval = (frame_start - p).as_secs_f64() * 1e6;
            }
            previous_start = Some(frame_start);
            let mut s = Stages {
                wait: report.wait + report.latency_wait,
                acquire: report.acquire,
                simulate: report.simulate,
                propagate: report.propagate,
                extract: report.extract,
                interpolate: report.interpolate,
                animate: report.animate,
                probes: report.probes,
                streaming: report.streaming,
                pools: report.pools,
                pool_parts: report.pool_parts,
                upload: report.upload,
                compose: report.compose,
                execute: report.execute,
                submit: report.submit,
                present: report.present,
                cpu: report.cpu,
                pipelines: report.pipelines,
                arrived: report.arrived,
                ..Stages::default()
            };
            generated_shown += usize::from(report.generated_shown);
            if let Some(paced) = player.paced() {
                paced.drain_presented(|p| presented.push(p));
            }
            if let Some(generation) = player.dlss() {
                dlss.record(generation, drawn);
            }
            if first_present.is_none() {
                first_present = Some(ms(t0));
            }
            if first_idle.is_none() && drawn > 0 && player.streamer().is_some_and(|s| s.idle()) {
                first_idle = Some(ms(t0));
            }
            if gi_converged.is_none()
                && player
                    .gi_volume()
                    .is_some_and(engine_render::gi::ProbeGi::is_converged)
            {
                gi_converged = Some((drawn, ms(t0)));
            }
            let graph = player.graph();
            s.gpu = f64::from(graph.gpu_frame_time().unwrap_or(0.0));
            for p in graph.gpu_pass_times() {
                if f64::from(p.gpu_us) > s.top.1 {
                    s.top = (p.name, f64::from(p.gpu_us));
                }
            }
            if drawn >= WARMUP {
                for p in graph.gpu_pass_times() {
                    passes.entry(p.name).or_default().push(f64::from(p.gpu_us));
                }
            }
            log.push(s);
            drawn += 1;
            if drawn == WARMUP && !memory_logged {
                memory_logged = true;
                memory_report(
                    &format!("after {WARMUP} frames"),
                    player.allocator(),
                    player.graph(),
                );
            }
        }
        if let Some(paced) = player.paced() {
            paced.drain_presented(|x| presented.push(x));
        }
        if let Some(path) = &record_pso {
            let mut record = engine_assets::pso_record::PsoRecord::new();
            for (desc, entries) in player.renderer().forward().material_type_entries_drawn() {
                for entry in entries {
                    record.add(&desc.module, &desc.type_name, &entry);
                }
            }
            match record.save_merged(path) {
                Ok(merged) => println!(
                    "PSO record {}: {} entry points drawn, {} in the record",
                    path.display(),
                    record.len(),
                    merged.len()
                ),
                Err(e) => println!("PSO record: {e}"),
            }
        }

        // The report.
        println!(
            "### Flight: {} ({instances} meshes), {movers} movers, {} characters, {light_count} local lights\n",
            pack.display(),
            game.characters.len(),
        );
        println!(
            "Adapter: {adapter_name}; {width}×{height} ({}), {antialiasing} at {quality:?}, AO {:?}; present mode {:?}, {}; latency control {}; {drawn} frames ({WARMUP} warm-up frames left out)\n",
            match (presentation.window, presentation.exclusive) {
                (Window::Windowed { .. }, _) => String::from("a window"),
                (_, true) => String::from("full screen, the monitor held by the swapchain"),
                (Window::Exclusive, false) => format!(
                    "full screen, borderless: the driver refused the monitor ({:?})",
                    presentation.exclusive_refused
                ),
                (Window::FullScreen, false) => String::from("full screen, borderless"),
            },
            defaults.ambient_occlusion.map(|a| a.quality),
            presentation.present_mode,
            match (presentation.drawn_apart, presentation.across_adapters) {
                (true, true) => "drawn apart and copied (the monitor is on another GPU)",
                (true, false) => "drawn apart and copied",
                (false, true) => "drawn into the swapchain image (the monitor is on another GPU)",
                (false, false) => "drawn into the swapchain image",
            },
            match player.latency() {
                Some(latency) => format!("{:?}, {:?}", latency.backend(), settings.low_latency),
                None => String::from("none"),
            },
        );
        println!("**Start-up** (ms):\n");
        println!("| phase | ms |\n|---|---:|");
        for (name, value) in &startup {
            println!("| {name} | {value:.1} |");
        }
        println!(
            "| **first frame presented (since `main`)** | **{:.1}** |",
            first_present.unwrap_or(0.0)
        );
        println!(
            "| streaming idle for the first time (since `main`) | {:.1} |",
            first_idle.unwrap_or(f64::NAN)
        );
        let pipelines = player.startup().pipelines;
        println!(
            "\nPipelines at start-up: {} created ({} from the pipeline cache, {} compiled), {:.1} ms in the driver summed over threads; cache file: {}\n",
            pipelines.created,
            pipelines.from_cache,
            pipelines.compiled,
            pipelines.driver_ms,
            player
                .startup()
                .cache
                .clone()
                .unwrap_or_else(|| String::from("not used"))
        );

        let timed: Vec<Stages> = log.iter().skip(WARMUP).copied().collect();
        println!("**CPU per frame** (µs):\n");
        println!("| stage | median | p99 | worst |\n|---|---:|---:|---:|");
        for (name, f) in COLUMNS {
            let v: Vec<f64> = timed.iter().map(f).collect();
            println!(
                "| {name} | {:.1} | {:.1} | {:.1} |",
                percentile(&v, 0.5),
                percentile(&v, 0.99),
                percentile(&v, 1.0)
            );
        }
        let gpu: Vec<f64> = timed.iter().map(|s| s.gpu).filter(|g| *g > 0.0).collect();
        println!(
            "| **GPU frame** | {:.1} | {:.1} | {:.1} |\n",
            percentile(&gpu, 0.5),
            percentile(&gpu, 0.99),
            percentile(&gpu, 1.0)
        );
        println!(
            "**GPU passes** (µs in the frames that ran them, median / p99; how many of the {} frames; mean over every frame), by their cost over the flight:\n",
            timed.len()
        );
        let mut ranked: Vec<(&str, f64, f64, usize, f64)> = passes
            .iter()
            .map(|(name, v)| {
                (
                    *name,
                    percentile(v, 0.5),
                    percentile(v, 0.99),
                    v.len(),
                    v.iter().sum::<f64>() / timed.len().max(1) as f64,
                )
            })
            .collect();
        ranked.sort_by(|a, b| b.4.total_cmp(&a.4));
        println!("| pass | median | p99 | frames | mean per frame |\n|---|---:|---:|---:|---:|");
        for (name, median, p99, count, mean) in ranked.iter().take(20) {
            println!("| {name} | {median:.1} | {p99:.1} | {count} | {mean:.1} |");
        }

        // Hitches: frames whose interval is over twice the median.
        let intervals: Vec<f64> = timed.iter().map(|s| s.interval).collect();
        let median_interval = percentile(&intervals, 0.5);
        let hitches: Vec<(usize, &Stages)> = log
            .iter()
            .enumerate()
            .skip(WARMUP)
            .filter(|(_, s)| s.interval > 2.0 * median_interval)
            .collect();
        let created: u64 = timed.iter().map(|s| s.pipelines).sum();
        println!(
            "\n**Hitches:** {} of {} frames over twice the median interval ({median_interval:.0} µs); pipelines created during the flight: {created}\n",
            hitches.len(),
            timed.len()
        );
        if !hitches.is_empty() {
            println!(
                "| frame | interval µs | CPU µs | wait | streaming | compose | execute | present | GPU µs | longest pass µs | pipelines | levels arrived |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|"
            );
            let mut worst = hitches.clone();
            worst.sort_by(|a, b| b.1.interval.total_cmp(&a.1.interval));
            for (k, s) in worst.iter().take(12) {
                println!(
                    "| {k} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {} {:.0} | {} | {} |",
                    s.interval,
                    s.cpu,
                    s.wait,
                    s.streaming,
                    s.compose,
                    s.execute,
                    s.present,
                    s.gpu,
                    s.top.0,
                    s.top.1,
                    s.pipelines,
                    s.arrived
                );
            }
        }
        let mut by_cpu: Vec<(usize, &Stages)> = log.iter().enumerate().skip(WARMUP).collect();
        by_cpu.sort_by(|a, b| b.1.cpu.total_cmp(&a.1.cpu));
        println!("\n**The 5 longest CPU frames** (µs):\n");
        println!(
            "| frame | CPU | animation | streaming | pools (textures / materials / geometry) | upload | compose | execute | submit | present | pipelines |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for (k, s) in by_cpu.iter().take(5) {
            println!(
                "| {k} | {:.0} | {:.0} | {:.0} | {:.0} ({:.0} / {:.0} / {:.0}) | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {} |",
                s.cpu,
                s.animate,
                s.streaming,
                s.pools,
                s.pool_parts[0],
                s.pool_parts[1],
                s.pool_parts[2],
                s.upload,
                s.compose,
                s.execute,
                s.submit,
                s.present,
                s.pipelines
            );
        }
        if let Some(probes) = player.reflection_probes() {
            let stats = probes.stats();
            println!(
                "\n**Reflection probes:** {} of {} captured, {} visible in the last frame, {} faces drawn in it.",
                stats.captured, stats.probes, stats.visible, stats.faces
            );
        }
        if let Some(volume) = player.gi_volume() {
            let grid = volume.grid();
            let rays = player.ray_scene().map(|r| r.stats()).unwrap_or_default();
            println!(
                "\n**GI volume:** {:?} probes ({}) {:.2} m apart, {:.1} MiB; converged at frame {} ({:.0} ms since `main`); ray scene: {} bottom levels ({:.1} MiB, {} triangles), top level {:.1} MiB.",
                grid.counts,
                grid.probe_count(),
                grid.spacing.min_element(),
                volume.memory_bytes() as f64 / MIB,
                gi_converged.map_or(-1, |(frame, _)| frame as i64),
                gi_converged.map_or(f64::NAN, |(_, ms)| ms),
                rays.blas_count,
                rays.blas_bytes as f64 / MIB,
                rays.triangles,
                rays.tlas_bytes as f64 / MIB,
            );
        }
        if player.paced().is_some() {
            frame_generation_report(&presented, generated_shown, drawn);
        }
        if player.dlss().is_some() {
            dlss.report(drawn);
        }
        println!();
        memory_report("at the end", player.allocator(), player.graph());
        let stats = player.streamer().map(|s| s.stats()).unwrap_or_default();
        println!(
            "Streaming: {} texture levels arrived ({:.1} MiB read), {} mesh levels; request to arrival {:.0} ms on average, {:.0} ms at worst ({} frames); textures planned {:.1} MiB of {:.1} MiB required, grant {:.1} MiB.\n",
            stats.arrived,
            stats.bytes as f64 / MIB,
            stats.mesh_levels,
            stats.latency_ms_sum / stats.arrived.max(1) as f64,
            stats.worst_latency_ms,
            stats.worst_latency_frames,
            stats.textures.planned as f64 / MIB,
            stats.textures.required as f64 / MIB,
            stats.textures.grant as f64 / MIB,
        );

        // The player's drop waits for the GPU, saves the pipeline cache and tears down.
        drop(player);
    }
}
