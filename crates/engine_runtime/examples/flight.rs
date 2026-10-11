//! A game's frame on the shipping path, measured end to end (`docs/research/core-audit-2.md`):
//! what a frame costs on the CPU stage by stage, where its worst frames come from, how
//! long the game takes to show its first frame, and how much memory it holds.
//!
//! The game:
//! - a cooked scene from its pack, its textures and mesh levels streamed by GPU feedback
//!   within the memory budget (ADR 0076);
//! - its instances as ECS entities, mirrored into a persistent GPU scene by
//!   `engine_runtime` (ADR 0064), with `movers` cubes in spinning hierarchies of 10;
//! - `characters` skinned characters (glTF Fox) walking in circles, their poses sampled
//!   on the job system every frame and skinned on the GPU (ADR 0068);
//! - Sponza's seven local lights with cached shadows (ADR 0071), sky light (ADR 0046);
//! - the frame renderer with the device class's defaults: ambient occlusion and the
//!   antialiasing / render size (ADR 0072);
//! - a window, presented without vsync (or with `--vsync`), two frames in flight.
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
//! cargo run -p engine_runtime --release --example flight -- <scene.pack>
//!     [--frames <n>]        frames of flight (default 1800: 30 s of game time)
//!     [--fps <n>]           game time per frame, 1/n s (default 60: the 64 Hz
//!                           simulation ticks in most frames; 240 about one in four)
//!     [--movers <n>]        moving cubes (default 2000)
//!     [--characters <n>]    skinned characters (default 64; needs test-assets/Fox)
//!     [--no-lights]         no local lights
//!     [--no-ao]             no ambient occlusion
//!     [--aa off|taau|fsr]   [--quality native|quality|balanced|performance]
//!     [--vsync]
//!     [--present apart|direct]   the frame into an image of its own copied into the
//!                                swapchain image, or straight into it (default: apart
//!                                only when presenting across adapters)
//!     [--pipeline-cache <dir>]   load and save the pipeline cache there (default: none)
//!     [--no-warm-up]        shading variants made on first use only, not also on the
//!                           device's warm-up thread (ADR 0079)
//!     [--fg]                frame generation (FSR 3.1, ADR 0091): a paced presenter shows
//!                           a generated frame before each real one; implies --ui
//!     [--ui]                a UI layer drawn on every presented frame: a HUD, a panel on
//!                           the nave's wall, a panel on the camera
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
    use engine_assets::{CookSettings, DerivedDataCache, cook_scene, import_gltf};
    use engine_core::alloc::EngineAllocator;
    use engine_core::jobs::ThreadPool;
    use engine_core::math::{Affine3A, Mat4, Quat, Vec2, Vec3, Vec4};
    use engine_ecs::{Entity, GlobalTransform, QueryState, Transform, World, pod_component};
    use engine_frame::{
        Antialiasing, FrameContext, FrameGeneration, FrameRenderer, GeneratedTarget, Placement,
        PrepareContext, Quality, RenderDefaults, SceneDraws, SceneInput, UiCanvas, UiRenderer,
        View, ViewTarget,
    };
    use engine_latency::{PacedFrame, PacedPresenter, PresentImage, Presented};
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::budget::GpuBudget;
    use engine_render::environment::{Environment, Sky};
    use engine_render::feedback::{FeedbackSettings, TextureFeedback};
    use engine_render::forward::{
        DirectionalLight, LocalLight, LocalShadowSettings, ShadowSettings, Spot,
    };
    use engine_render::gpu_scene::{GpuScene, InstanceId};
    use engine_render::graph::{
        ImportedTexture, LoadOp, RenderGraph, Texture, TextureDesc, TextureState,
    };
    use engine_render::material::{MaterialDesc, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, cube};
    use engine_render::present::PresentTarget;
    use engine_render::skinning::{SkinnedId, SkinnedMeshData, Skinning};
    use engine_render::streaming::LodView;
    use engine_render::texture_pool::{TextureId, TextureLevels, TexturePool};
    use engine_render::upload::UploadRing;
    use engine_render2d::{Canvas, Quad};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        OutputColor, PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use engine_runtime::{Camera, CameraView, MeshRenderer, Runtime, look_rotation};
    use engine_streaming::{SceneStreamSettings, SceneStreamer, material_desc, texture_format};
    use std::alloc::{GlobalAlloc, Layout};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const IN_FLIGHT: usize = 2;
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
        entity: Entity,
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

    const COLUMNS: [Column; 16] = [
        ("wait for frame slot", |s| s.wait),
        ("acquire", |s| s.acquire),
        ("tick systems", |s| s.simulate),
        ("transform propagation", |s| s.propagate),
        ("extraction", |s| s.extract),
        ("interpolation", |s| s.interpolate),
        ("animation (job system) + poses", |s| s.animate),
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

    fn us(start: Instant) -> f64 {
        start.elapsed().as_secs_f64() * 1e6
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
            "\nTransient targets: {:.1} MiB reserved in this slot's heaps ({:.1} placed, {:.1} without aliasing), {} resources, {} passes ({} culled). Process: working set {working:.0} MiB, private {private:.0} MiB.\n",
            g.transient_heap_bytes as f64 / MIB,
            g.transient_placed_bytes as f64 / MIB,
            g.transient_unaliased_bytes as f64 / MIB,
            g.transient_resources,
            g.passes,
            g.culled_passes,
        );
        memory_breakdown(engine_mapped);
    }

    /// Sponza's local lights, as the golden test bed places them (with shadows).
    fn sponza_lights() -> Vec<LocalLight> {
        let mut lights: Vec<LocalLight> = (0..5)
            .map(|k| LocalLight {
                position: Vec3::new(
                    -6.0 + 4.0 * k as f32,
                    1.4,
                    if k % 2 == 0 { 2.2 } else { -2.2 },
                ),
                range: 8.0,
                color: Vec3::new(24.0, 15.0, 8.0),
                spot: None,
                shadow: true,
            })
            .collect();
        for x in [-2.0, 6.0] {
            lights.push(LocalLight {
                position: Vec3::new(x, 7.0, 0.0),
                range: 12.0,
                color: Vec3::new(90.0, 100.0, 140.0),
                spot: Some(Spot {
                    direction: Vec3::new(0.15, -1.0, 0.1).normalize(),
                    inner: 0.4,
                    outer: 0.65,
                }),
                shadow: true,
            });
        }
        lights
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
        let feedback_on = !flag("--no-feedback");
        // Where the frame is drawn (`engine_render::present`): `--present apart|direct`,
        // by default apart only when the swapchain presents across adapters.
        let present_apart = option("--present").map(|v| v == "apart");
        let vsync = flag("--vsync");
        let cache_dir = option("--pipeline-cache").map(PathBuf::from);
        let warm_up = !flag("--no-warm-up");
        let frame_generation = flag("--fg");
        let ui_on = frame_generation || flag("--ui");

        // Start-up, phase by phase. What does not depend on each other overlaps (queue
        // item 0g): the Vulkan instance and device (the drivers' own start-up) on a thread
        // beside the window, and the swapchain on a thread beside the renderer, the pack
        // and the scene (on the Radeon iGPU, presenting across adapters, it takes 170 ms).
        // Each phase reports its own time; the first frame's time since `main` is what
        // the overlap shortens.
        let mut startup: Vec<(&str, f64)> = Vec::new();
        let gpu = std::thread::Builder::new()
            .name(String::from("vulkan start-up"))
            .spawn(move || {
                let phase = Instant::now();
                let instance = Instance::new(&InstanceDesc::default()).unwrap();
                let preference =
                    AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
                let adapter = instance.select_adapter(&preference).unwrap();
                let adapter_name = adapter.summary();
                let mut device = Device::new(&instance, &adapter).unwrap();
                device.set_pipeline_warm_up(warm_up);
                let cache_path = cache_dir
                    .as_deref()
                    .map(|dir| dir.join(device.pipeline_cache_file_name()));
                let cache_load = cache_path
                    .as_deref()
                    .map(|path| device.load_pipeline_cache(path));
                (
                    instance,
                    adapter_name,
                    device,
                    cache_path,
                    cache_load,
                    ms(phase),
                )
            })
            .unwrap();
        let phase = Instant::now();
        let mut windows = WindowSystem::start(4096).unwrap();
        let window = windows
            .create_window(&WindowDesc {
                title: String::from("flight"),
                width: WIDTH,
                height: HEIGHT,
                resizable: false,
                visible: true,
            })
            .unwrap();
        startup.push(("window (beside the device)", ms(phase)));
        // Work that needs no device, beside it too: the job pool, and the glTF Fox
        // imported and cooked through the derived data cache.
        let phase = Instant::now();
        let jobs = Arc::new(ThreadPool::builder().build().unwrap());
        let fox = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets/Fox/Fox.gltf")
            .canonicalize()
            .ok()
            .filter(|_| character_count > 0)
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                let dir = path.parent();
                let scene = import_gltf(&bytes, dir).unwrap();
                let cache = DerivedDataCache::new(path.parent().unwrap().join("../.ddc")).unwrap();
                cook_scene(&scene, dir, Some(&cache), &jobs, &CookSettings::default())
                    .unwrap()
                    .0
            });
        startup.push((
            "job pool + Fox cooked from the cache (beside the device)",
            ms(phase),
        ));
        let (instance, adapter_name, device, cache_path, cache_load, device_ms) =
            gpu.join().unwrap();
        startup.push((
            "Vulkan instance + device (+ pipeline cache file)",
            device_ms,
        ));
        let surface =
            Arc::new(Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap());
        let swapchain_desc = SwapchainDesc {
            width: WIDTH,
            height: HEIGHT,
            present_mode: if vsync {
                PresentMode::Fifo
            } else {
                PresentMode::Immediate
            },
            srgb: true,
            color: OutputColor::Sdr,
            low_latency: false,
        };
        // The renderer draws into the format the swapchain will have.
        let swapchain_format = Swapchain::format_for(&device, &surface, &swapchain_desc).unwrap();
        let device = Arc::new(device);
        let pending_swapchain = {
            let (device, surface) = (Arc::clone(&device), Arc::clone(&surface));
            std::thread::Builder::new()
                .name(String::from("swapchain"))
                .spawn(move || {
                    let phase = Instant::now();
                    let swapchain = Swapchain::new(&device, &surface, &swapchain_desc).unwrap();
                    (swapchain, ms(phase))
                })
                .unwrap()
        };

        let phase = Instant::now();
        let pipelines_before = engine_rhi::creation_stats();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let depth_format = vk::Format::D32_SFLOAT;
        let defaults = RenderDefaults::for_device(&device);
        let antialiasing = match option("--aa") {
            Some("off") => Antialiasing::OFF,
            Some("taau") => Antialiasing::TAAU,
            Some("fsr") => Antialiasing::FSR,
            _ => defaults.antialiasing,
        };
        let quality = match option("--quality") {
            Some("native") => Quality::Native,
            Some("quality") => Quality::Quality,
            Some("balanced") => Quality::Balanced,
            Some("performance") => Quality::Performance,
            _ => defaults.quality,
        };
        let ambient_occlusion = if flag("--no-ao") {
            None
        } else {
            defaults.ambient_occlusion
        };
        let mut frame_renderer = FrameRenderer::new(
            &device,
            &bindless,
            swapchain_format,
            depth_format,
            ShadowSettings::default(),
            IN_FLIGHT,
        )
        .unwrap();
        frame_renderer
            .forward_mut()
            .set_ambient_occlusion(&device, &bindless, ambient_occlusion)
            .unwrap();
        if lights_on {
            frame_renderer
                .forward_mut()
                .set_local_shadows(
                    &device,
                    &mut allocator,
                    &bindless,
                    Some(LocalShadowSettings::default()),
                )
                .unwrap();
        }
        let mut skinning = Skinning::new(
            &device,
            &mut allocator,
            &bindless,
            depth_format,
            1 << 16,
            IN_FLIGHT,
        )
        .unwrap();
        let mut feedback =
            TextureFeedback::new(&device, &bindless, FeedbackSettings::default(), IN_FLIGHT)
                .unwrap();
        let mut environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
        let pipelines_after = engine_rhi::creation_stats();
        startup.push(("renderer + features (pipelines)", ms(phase)));

        // The scene from its pack, streamed.
        let phase = Instant::now();
        let mut budget = GpuBudget::new(&device);
        let mut textures = TexturePool::new(&device, 32 << 20, IN_FLIGHT);
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity::default(),
            IN_FLIGHT,
        )
        .unwrap();
        let mut streamer = SceneStreamer::open(
            &pack,
            &jobs,
            &mut textures,
            &mut geometry,
            &mut allocator,
            &mut budget,
            SceneStreamSettings::default(),
        )
        .unwrap();
        let mut materials = MaterialTable::new(IN_FLIGHT);
        let scene_layout = streamer.layout().scene.clone();
        let texture_ids = streamer.textures().to_vec();
        let material_ids: Vec<_> = scene_layout
            .materials
            .iter()
            .map(|m| materials.add(material_desc(m, &texture_ids)))
            .collect();
        let fallback = materials.default_material();
        let mut world = World::new();
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for instance in &scene_layout.instances {
            let world_from_object = Mat4::from_cols_array(&instance.world_from_object);
            let mesh = &scene_layout.meshes[instance.mesh as usize];
            for corner in 0..8 {
                let pick = |bit: usize, axis: usize| {
                    if corner & bit != 0 {
                        mesh.max[axis]
                    } else {
                        mesh.min[axis]
                    }
                };
                let c = world_from_object.transform_point3(Vec3::new(
                    pick(1, 0),
                    pick(2, 1),
                    pick(4, 2),
                ));
                min = min.min(c);
                max = max.max(c);
            }
            world.spawn((
                GlobalTransform(Affine3A::from_mat4(world_from_object)),
                MeshRenderer::new(
                    streamer.meshes()[instance.mesh as usize],
                    instance
                        .material
                        .map_or(fallback, |i| material_ids[i as usize]),
                ),
            ));
        }
        startup.push(("scene opened from its pack (tails resident)", ms(phase)));

        // Moving props: cubes in spinning hierarchies of 10 along the nave.
        let phase = Instant::now();
        let cube_mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let paints: Vec<_> = (0..16)
            .map(|k| {
                let h = k as f32 / 16.0;
                materials.add(MaterialDesc::solid(
                    [0.3 + 0.6 * h, 0.55, 0.9 - 0.6 * h],
                    0.0,
                    0.5,
                ))
            })
            .collect();
        let size = max - min;
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
        let mut characters: Vec<Character> = Vec::new();
        let mut fox_skin = None;
        if let Some(mut cooked) = fox {
            let ids: Vec<TextureId> = std::mem::take(&mut cooked.textures)
                .into_iter()
                .map(|t| {
                    textures
                        .add(
                            &device,
                            &mut allocator,
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
            let fox_material = materials.add(material_desc(&cooked.materials[0], &ids));
            let instance = cooked.instances.iter().find(|i| i.skin.is_some()).unwrap();
            let mesh = &cooked.meshes[instance.mesh as usize];
            let skin = cooked.skins.swap_remove(instance.skin.unwrap() as usize);
            let source = skinning
                .add_skin(&SkinnedMeshData {
                    mesh: MeshData {
                        positions: &mesh.positions,
                        normals: &mesh.normals,
                        tangents: &mesh.tangents,
                        uvs: &mesh.uvs,
                        indices: &mesh.indices,
                    },
                    joints: &mesh.joints,
                    weights: &mesh.weights,
                    joint_count: skin.joints.len() as u16,
                })
                .unwrap();
            for k in 0..character_count {
                let (skinned, drawn) = skinning.add_instance(source, &mut geometry).unwrap();
                let f = (k as f32 + 0.5) / character_count as f32;
                let walk = Walk {
                    x: min.x + size.x * (0.15 + 0.7 * f),
                    z: min.z + size.z * (0.35 + 0.3 * ((k * 5) % 7) as f32 / 7.0),
                    radius: 0.8 + (k % 3) as f32 * 0.4,
                    speed: 0.6 + (k % 4) as f32 * 0.15,
                    angle: k as f32,
                };
                let entity = world.spawn((
                    Transform::from_xyz(walk.x, min.y, walk.z).with_scale(Vec3::splat(0.012)),
                    GlobalTransform::IDENTITY,
                    MeshRenderer::new(drawn, fox_material),
                    walk,
                ));
                let n = skin.skeleton.joint_count();
                characters.push(Character {
                    entity,
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
        startup.push(("movers + characters", ms(phase)));

        // The player's camera.
        let (eye, rotation) = camera_at(0.0, min, max);
        let extent = size.length();
        let camera = world.spawn((
            Transform::from_translation(eye).with_rotation(rotation),
            Camera::perspective(70f32.to_radians(), 0.05, extent * 2.0)
                .with_antialiasing(antialiasing, quality),
        ));
        let mut cameras = CameraView::new(&mut world, defaults.view_settings());
        let entities = world.len() as u32;
        let mut scene = GpuScene::new(&device, &bindless, entities + 16, IN_FLIGHT).unwrap();
        scene.set_jobs(jobs.clone());
        let mut view = View::new(&device, entities + 16);
        let mut runtime = Runtime::<InstanceId>::new(world, 64.0);
        runtime.set_jobs(jobs.clone());
        let mut spins = QueryState::<(&mut Transform, &Spin)>::new(&mut runtime.world);
        let mut walkers = QueryState::<(&mut Transform, &mut Walk)>::new(&mut runtime.world);
        let mut ring = UploadRing::new(&device, &mut allocator, IN_FLIGHT, 32 << 20).unwrap();
        let mut frames = Frames::new(&device, device.graphics.family, IN_FLIGHT).unwrap();
        let mut graph = RenderGraph::new(&device, IN_FLIGHT);
        let lights = if lights_on {
            sponza_lights()
        } else {
            Vec::new()
        };
        let sun = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        };

        // The swapchain, made beside all of the above.
        let wait = Instant::now();
        let (swapchain, swapchain_ms) = pending_swapchain.join().unwrap();
        startup.push((
            "surface + swapchain (beside the renderer and the scene)",
            swapchain_ms,
        ));
        startup.push(("waiting for the swapchain", ms(wait)));
        let present_mode = swapchain.present_mode();
        let across_adapters = swapchain.presents_across_adapters();
        // With frame generation a paced presenter takes the swapchain (ADR 0091).
        let mut swapchain = Some(swapchain);
        let mut paced = None;
        if frame_generation {
            match PacedPresenter::start(&device, &mut allocator, swapchain.take().unwrap(), None) {
                Ok(p) => paced = Some(p),
                Err(failed) => {
                    let (s, e) = *failed;
                    println!("frame generation unavailable: {e}");
                    swapchain = Some(s);
                }
            }
        }
        // The UI layer: a HUD (a crosshair, a bar, a frame for a map), a panel on the nave's
        // wall, a panel on the camera (bottom left, as a helmet's display).
        let ui = ui_on.then(|| {
            UiRenderer::new(&device, &mut allocator, &bindless, swapchain_format).unwrap()
        });
        let mut hud = Canvas::new();
        let (w, h) = (WIDTH as f32, HEIGHT as f32);
        hud.draw(&Quad::solid(Vec2::new(w * 0.5, h * 0.5), Vec2::new(24.0, 2.0)).color(Vec4::ONE));
        hud.draw(&Quad::solid(Vec2::new(w * 0.5, h * 0.5), Vec2::new(2.0, 24.0)).color(Vec4::ONE));
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
        let (wall_from_panel, view_from_visor) = {
            let center = (min + max) * 0.5;
            let size = max - min;
            (
                // 3 × 1.5 m on the wall side of the nave, a centimeter per canvas unit,
                // canvas y down.
                Mat4::from_scale_rotation_translation(
                    Vec3::new(0.01, -0.01, 1.0),
                    Quat::IDENTITY,
                    Vec3::new(
                        center.x - 1.5,
                        min.y + size.y * 0.25,
                        center.z + size.z * 0.3,
                    ),
                ),
                Mat4::from_scale_rotation_translation(
                    Vec3::new(0.0012, -0.0012, 1.0),
                    Quat::IDENTITY,
                    Vec3::new(-0.42, -0.2, -0.6),
                ),
            )
        };
        let layer = [
            UiCanvas {
                canvas: &hud,
                placement: Placement::Screen,
            },
            UiCanvas {
                canvas: &wall_panel,
                placement: Placement::World(wall_from_panel),
            },
            UiCanvas {
                canvas: &visor,
                placement: Placement::Camera(view_from_visor),
            },
        ];
        let mut presented: Vec<Presented> = Vec::with_capacity(2 * frames_to_run + 8);
        let mut generated_shown = 0usize;

        let mut log: Vec<Stages> = Vec::with_capacity(frames_to_run);
        let mut passes: std::collections::BTreeMap<&'static str, Vec<f64>> =
            std::collections::BTreeMap::new();
        let mut first_present = None;
        let mut first_idle = None;
        let mut memory_logged = false;
        let mut drawn = 0usize;
        let mut previous_start: Option<Instant> = None;
        let mut game_time = 0.0f64;
        'main: loop {
            let mut close = false;
            windows.drain_events(|e| {
                if matches!(e.kind, WindowEventKind::CloseRequested) {
                    close = true;
                }
            });
            if close || drawn >= frames_to_run {
                break 'main;
            }
            let mut s = Stages::default();
            let frame_start = Instant::now();
            if let Some(p) = previous_start
                && let Some(last) = log.last_mut()
            {
                last.interval = (frame_start - p).as_secs_f64() * 1e6;
            }
            previous_start = Some(frame_start);
            let pipelines_at_start = engine_rhi::creation_stats().created;
            let frame = frames.begin().unwrap();
            s.wait = us(frame_start);
            let work = Instant::now();
            let t = Instant::now();
            let acquired = match swapchain.as_mut() {
                Some(swapchain) => match swapchain.acquire(frame.image_available, u64::MAX) {
                    Ok(a) => Some(a),
                    Err(RhiError::Vulkan {
                        result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                        ..
                    }) => {
                        frames.wait_idle().unwrap();
                        let e = swapchain.extent();
                        swapchain
                            .recreate(&device, &surface, e.width, e.height)
                            .unwrap();
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                },
                None => None,
            };
            let paced_images = paced.as_ref().map(PacedPresenter::next_images);
            s.acquire = us(t);
            game_time += dt;
            let time = game_time as f32;
            let (eye, rotation) = camera_at(time, min, max);

            // Ticks, the per-frame camera, interpolation.
            let stages = runtime.frame(
                dt,
                &(),
                &mut scene,
                |world, (), dt| {
                    spins.for_each(world, |(t, spin)| {
                        t.rotation *= Quat::from_rotation_y(spin.0 * dt);
                    });
                    walkers.for_each(world, |(t, walk)| {
                        walk.angle += walk.speed * dt;
                        let (sin, cos) = walk.angle.sin_cos();
                        t.translation.x = walk.x + walk.radius * cos;
                        t.translation.z = walk.z + walk.radius * sin;
                        t.rotation = Quat::from_rotation_y(-walk.angle);
                    });
                },
                |world, (), _alpha| {
                    if let Some(t) = world.get_mut::<Transform>(camera) {
                        t.translation = eye;
                        t.rotation = rotation;
                    }
                },
            );
            s.simulate = stages.simulate_us;
            s.propagate = stages.propagate_us;
            s.extract = stages.extract_us;
            s.interpolate = stages.interpolate_us + stages.present_us;

            // Animation: every character's pose on the job system, then its pose for the
            // GPU with the scene instance's transform (bit for bit, for the motion pass).
            let t = Instant::now();
            if let Some((skin, inverse_bind)) = fox_skin.as_ref() {
                let clip = &skin.clips[1 % skin.clips.len()];
                let duration = clip.duration().max(1e-3);
                for_each_parallel(&jobs, &mut characters, 4, |c| {
                    c.time = (c.time + dt as f32) % duration;
                    clip.sample(c.time, skin.skeleton.rest(), &mut c.pose);
                    local_to_model(&skin.skeleton, &c.pose, &Affine3A::IDENTITY, &mut c.model);
                    skinning_matrices(&c.model, &skin.joints, inverse_bind, &mut c.palette);
                });
                for c in &characters {
                    if let Some(id) = runtime.extract().instance(c.entity)
                        && let Some(instance) = scene.get(id)
                    {
                        skinning.set_scene_instance(c.skinned, Some(id));
                        skinning.set_pose(c.skinned, &c.palette, &instance.world_from_object);
                    }
                }
                skinning.update_bounds(&mut geometry, Some(&mut scene));
            }
            s.animate = us(t);

            let main = cameras.resolve(&runtime.world).unwrap();
            let clip_from_world = main.camera.clip_from_world(WIDTH as f32 / HEIGHT as f32);
            let t = Instant::now();
            // Mesh levels by distance in every direction: a fast turn finds them loaded.
            let lod_view = LodView::new(&clip_from_world, eye, HEIGHT, 1.0).around();
            let arrived_before = streamer.stats();
            streamer.update(
                &mut textures,
                &mut geometry,
                &mut allocator,
                &mut budget,
                feedback_on.then_some(&mut feedback),
                Some(&lod_view),
                &materials,
                frame.number,
            );
            budget.update(&allocator);
            s.arrived = streamer
                .stats()
                .arrived
                .saturating_sub(arrived_before.arrived);
            s.streaming = us(t);
            if first_idle.is_none() && drawn > 0 && streamer.idle() {
                first_idle = Some(ms(t0));
            }

            let extent = match (&swapchain, &paced) {
                (Some(swapchain), _) => swapchain.extent(),
                (None, Some(paced)) => paced.extent(),
                (None, None) => unreachable!("one of them presents"),
            };
            let mut settings = main.settings;
            if paced.is_some() {
                settings.frame_generation = FrameGeneration::Fsr;
            }
            let t = Instant::now();
            frame_renderer
                .prepare(
                    &mut view,
                    &settings,
                    (extent.width, extent.height),
                    PrepareContext {
                        device: &device,
                        allocator: &mut allocator,
                        bindless: &bindless,
                        materials: &mut materials,
                        frame_number: frame.number,
                        skinning: Some(&mut skinning),
                    },
                )
                .unwrap();
            s.compose = us(t);
            let lod_bias = materials.texture_lod_bias();
            let shown;
            {
                let t = Instant::now();
                let mut fg = graph.begin(&frame);
                ring.begin(&frame);
                let p = Instant::now();
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame.number)
                    .unwrap();
                s.pool_parts[0] = us(p);
                let p = Instant::now();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame.number)
                    .unwrap();
                s.pool_parts[1] = us(p);
                let p = Instant::now();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame.number)
                    .unwrap();
                s.pool_parts[2] = us(p);
                let skinned_frame = skinning
                    .begin_frame(
                        &mut fg,
                        &mut ring,
                        &device,
                        &mut allocator,
                        &bindless,
                        &geo,
                        frame.number,
                    )
                    .unwrap();
                s.pools = us(t);
                let t = Instant::now();
                let scene_frame = scene
                    .add_passes(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &mut ring,
                        &bindless,
                        &geo,
                        &mats,
                        frame.number,
                    )
                    .unwrap()
                    .unwrap();
                s.upload = us(t);
                let import = |fg: &mut engine_render::graph::FrameGraph<'_>,
                              name: &'static str,
                              image: PresentImage|
                 -> Texture {
                    fg.import_texture(
                        name,
                        ImportedTexture {
                            image: image.image,
                            view: image.view,
                            desc: TextureDesc::new_2d(image.format, extent.width, extent.height),
                            initial: TextureState::undefined(),
                            final_state: Some(TextureState::transfer_source()),
                            sampled_index: None,
                            storage_index: None,
                        },
                    )
                };
                let (present, generated) = match (&swapchain, paced_images) {
                    (Some(swapchain), _) => (
                        Some(PresentTarget::import_with(
                            &mut fg,
                            swapchain,
                            acquired.unwrap().index,
                            present_apart.unwrap_or_else(|| swapchain.presents_across_adapters()),
                        )),
                        None,
                    ),
                    (None, Some(images)) => (
                        None,
                        Some((
                            import(&mut fg, "real frame", images.real),
                            GeneratedTarget {
                                texture: import(&mut fg, "generated frame", images.generated),
                                format: images.generated.raw_format,
                                source_format: images.real.raw_format,
                            },
                        )),
                    ),
                    (None, None) => unreachable!("one of them presents"),
                };
                let target = match (&present, &generated) {
                    (Some(present), _) => present.texture,
                    (None, Some((real, _))) => *real,
                    (None, None) => unreachable!("one of them presents"),
                };
                let t = Instant::now();
                let mut context = FrameContext {
                    device: &device,
                    allocator: &mut allocator,
                    bindless: &bindless,
                    ring: &mut ring,
                    frame_number: frame.number,
                    dt: dt as f32,
                };
                let scene_input = SceneInput {
                    geometry: &geo,
                    materials: &mats,
                    draws: SceneDraws::Scene(&scene_frame),
                    skinning: Some(&skinned_frame),
                    light: sun,
                    lights: &lights,
                    decals: &[],
                    environment: Some(&environment),
                    reflection_probes: None,
                    gi: None,
                };
                let view_target = ViewTarget {
                    texture: target,
                    rect: None,
                    load: LoadOp::DontCare,
                };
                let view_frame = match generated {
                    Some((_, generated_target)) => frame_renderer
                        .add_view_generating(
                            &mut fg,
                            &mut context,
                            &mut view,
                            &scene_input,
                            &main.camera,
                            view_target,
                            generated_target,
                            |_, _| {},
                        )
                        .unwrap(),
                    None => frame_renderer
                        .add_view(
                            &mut fg,
                            &mut context,
                            &mut view,
                            &scene_input,
                            &main.camera,
                            view_target,
                        )
                        .unwrap(),
                };
                if let Some(ui) = &ui {
                    ui.add_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        &layer,
                        target,
                        &main.camera,
                        &view_frame,
                    )
                    .unwrap();
                    if let (Some((_, g)), Some(frame_g)) = (generated, view_frame.generated)
                        && frame_g.shown
                    {
                        ui.add_passes(
                            &mut fg,
                            &mut ring,
                            &bindless,
                            &layer,
                            g.texture,
                            &frame_g.camera,
                            &view_frame,
                        )
                        .unwrap();
                    }
                }
                if let Some(present) = present {
                    present.finish(&mut fg);
                }
                shown = view_frame.generated.is_some_and(|g| g.shown);
                if feedback_on {
                    feedback
                        .add_pass(
                            &mut fg,
                            &device,
                            &mut allocator,
                            &bindless,
                            &geo,
                            &mats,
                            &view_frame.passes,
                            view_frame.render,
                            lod_bias,
                            frame.number,
                        )
                        .unwrap();
                }
                s.compose += us(t);
                let t = Instant::now();
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
                s.execute = us(t);
            }
            let t = Instant::now();
            let number = frame.number;
            match (&swapchain, paced_images) {
                (Some(swapchain), _) => frames
                    .submit(
                        device.graphics,
                        frame,
                        true,
                        Some(swapchain.render_finished(acquired.unwrap().index)),
                    )
                    .unwrap(),
                (None, Some(images)) => frames
                    .submit_waiting(device.graphics, frame, false, Some(images.wait), None, None)
                    .unwrap(),
                (None, None) => unreachable!("one of them presents"),
            }
            s.submit = us(t);
            let t = Instant::now();
            match (&swapchain, &mut paced) {
                (Some(swapchain), _) => {
                    let _ = swapchain.present(device.graphics, acquired.unwrap().index);
                }
                (None, Some(paced)) => {
                    generated_shown += usize::from(shown);
                    paced
                        .present(PacedFrame {
                            done: (frames.timeline(), number),
                            generated: shown,
                            present_id: None,
                            reset: !shown,
                        })
                        .unwrap();
                    paced.drain_presented(|p| presented.push(p));
                }
                (None, None) => unreachable!("one of them presents"),
            }
            s.present = us(t);
            s.cpu = us(work);
            if first_present.is_none() {
                first_present = Some(ms(t0));
            }
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
            s.pipelines = engine_rhi::creation_stats().created - pipelines_at_start;
            log.push(s);
            drawn += 1;
            if drawn == WARMUP && !memory_logged {
                memory_logged = true;
                memory_report(&format!("after {WARMUP} frames"), &allocator, &graph);
            }
        }
        frames.wait_idle().unwrap();
        if let Some(mut p) = paced.take() {
            p.drain_presented(|x| presented.push(x));
            swapchain = Some(p.stop(&mut allocator).unwrap());
        }

        // The report.
        println!(
            "### Flight: {} ({} instances), {movers} movers, {} characters, {} local lights\n",
            pack.display(),
            scene_layout.instances.len(),
            characters.len(),
            lights.len()
        );
        println!(
            "Adapter: {adapter_name}; {WIDTH}×{HEIGHT}, {antialiasing} at {quality:?}, AO {:?}; present mode {:?}, {}; {drawn} frames ({WARMUP} warm-up frames left out)\n",
            ambient_occlusion.map(|a| a.quality),
            present_mode,
            match (
                present_apart.unwrap_or(across_adapters) || frame_generation,
                across_adapters,
            ) {
                (true, true) => "drawn apart and copied (the monitor is on another GPU)",
                (true, false) => "drawn apart and copied",
                (false, true) => "drawn into the swapchain image (the monitor is on another GPU)",
                (false, false) => "drawn into the swapchain image",
            }
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
        println!(
            "\nPipelines at start-up: {} created ({} from the pipeline cache, {} compiled), {:.1} ms in the driver summed over threads; cache file: {}\n",
            pipelines_after.created - pipelines_before.created,
            pipelines_after.cache_hits - pipelines_before.cache_hits,
            pipelines_after.cache_misses - pipelines_before.cache_misses,
            (pipelines_after.time - pipelines_before.time).as_secs_f64() * 1e3,
            match &cache_load {
                Some(load) => format!("{load:?}"),
                None => String::from("not used"),
            }
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
        if frame_generation {
            frame_generation_report(&presented, generated_shown, drawn);
        }
        println!();
        memory_report("at the end", &allocator, &graph);
        let stats = streamer.stats();
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

        if let Some(path) = cache_path.as_deref() {
            let _ = device.save_pipeline_cache(path);
        }
        // SAFETY: the device is idle.
        unsafe {
            if let Some(mut ui) = ui {
                ui.destroy(&mut allocator);
            }
            frame_renderer.destroy(&device, &mut allocator, &bindless);
            view.destroy(&mut allocator, &bindless);
            scene.destroy(&mut allocator);
            skinning.destroy(&mut allocator);
            feedback.destroy(&mut allocator);
            environment.destroy(&mut allocator, &bindless);
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            geometry.destroy(&mut allocator);
            materials.destroy(&mut allocator);
            textures.destroy(&mut allocator, &bindless);
        }
        drop(streamer);
        drop(frames);
        drop(frame_renderer);
        drop(bindless);
        drop(allocator);
        drop(swapchain);
        drop(surface);
        windows.destroy_window(window.id).unwrap();
    }
}
