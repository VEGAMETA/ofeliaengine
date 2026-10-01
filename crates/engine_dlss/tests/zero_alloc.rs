//! CLAUDE.md §5: zero heap allocations per frame in steady state, for declaring and
//! recording the DLSS pass (ADR 0063).
//!
//! A counting global allocator observes the process while the same frame is declared and
//! recorded, and run, repeatedly after a warm-up. Streamline's and NGX's own allocations happen in
//! C++ (the C runtime's heap) and are not counted: this checks the engine's side. One test
//! per file, so nothing else allocates concurrently. Needs the Streamline DLLs and an
//! NVIDIA RTX GPU; skips otherwise.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::Mat4;
use engine_dlss::{Dlss, Streamline, StreamlineDesc};
use engine_render::forward::{MOTION_FORMAT, MotionOutput};
use engine_render::graph::{Access, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};
use engine_upscale::{UpscaleCamera, UpscaleInput};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn steady_state_frames_do_not_allocate() {
    let streamline = match Streamline::load(&StreamlineDesc::default()) {
        Ok(s) => s,
        Err(e) => {
            println!("skipping: DLSS unavailable: {e}");
            return;
        }
    };
    let setup = || -> Result<Option<(Instance, Device)>, engine_rhi::RhiError> {
        let instance = Instance::with_extensions(
            &InstanceDesc {
                app_name: String::from("engine_dlss zero-alloc test"),
                validation: false,
                debug_utils: false,
                surface: false,
                headless_surface: false,
                sync_validation: false,
            },
            &streamline.instance_extensions(),
        )?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        if let Err(e) = streamline.check_adapter(&instance, &adapter) {
            println!("skipping: {}: {e}", adapter.name());
            return Ok(None);
        }
        let device = Device::with_extensions(&instance, &adapter, &streamline.device_extensions())?;
        Ok(Some((instance, device)))
    };
    let Some((_instance, device)) = setup().unwrap() else {
        return;
    };
    streamline.attach(&device).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut graph = RenderGraph::new(&device, 2);
    let mut dlss = Dlss::new(&streamline, 2).unwrap();
    // SAFETY: plain object creation.
    let (pool, cmd) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (pool, cmd)
    };

    // A stand-in scene pass writing color, depth and motion vectors at 64 × 64, upscaled to
    // 128 × 128.
    let frame = |graph: &mut RenderGraph,
                 allocator: &mut Allocator,
                 dlss: &mut Dlss,
                 slot: usize,
                 number: u64| {
        // SAFETY: the previous frame's submission finished (waited for below).
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        {
            // SAFETY: no GPU work is in flight.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            let desc = |format| TextureDesc::new_2d(format, 64, 64);
            let color = fg.create_texture("color", desc(vk::Format::R16G16B16A16_SFLOAT));
            let depth = fg.create_texture("depth", desc(vk::Format::D32_SFLOAT));
            let velocity = fg.create_texture("velocity", desc(MOTION_FORMAT));
            fg.add_pass("scene")
                .color(color, LoadOp::Clear([0.5; 4]), StoreOp::Store)
                .color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
                .execute(|_| {});
            let upscaled = dlss
                .add_pass(
                    &mut fg,
                    UpscaleInput {
                        color,
                        depth,
                        motion: MotionOutput {
                            velocity,
                            jitter: [0.25, -0.25],
                            reset: false,
                            reactive: None,
                            clip_from_world: Mat4::IDENTITY,
                            previous_clip_from_world: Mat4::IDENTITY,
                        },
                        camera: UpscaleCamera {
                            near: 0.1,
                            far: 100.0,
                            fov_y: 1.0,
                        },
                        frame_time_ms: 16.7,
                        exposure: None,
                    },
                    (128, 128),
                    number,
                )
                .unwrap();
            fg.add_pass("present stand-in")
                .read(upscaled, Access::Sampled(ShaderStages::FRAGMENT))
                .side_effects()
                .execute(|_| {});
            fg.execute(allocator).unwrap();
        }
        // SAFETY: a recorded command buffer, submitted and waited for: DLSS runs on the
        // GPU as in a game (Streamline and NGX assume their work completes).
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
        }
        device.wait_idle().unwrap();
    };

    // Past the 60 evaluations after which Streamline logs, once, that it saw no present
    // (the log keeps the text of its first errors).
    for i in 0..80u64 {
        frame(
            &mut graph,
            &mut allocator,
            &mut dlss,
            (i % 2) as usize,
            i + 1,
        );
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 80..180u64 {
        frame(
            &mut graph,
            &mut allocator,
            &mut dlss,
            (i % 2) as usize,
            i + 1,
        );
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("100 frames: {allocations} heap allocations");
    let (failures, last) = Dlss::evaluate_failures();
    println!(
        "Streamline: {:?}; refused evaluations: {failures} (last {})",
        Streamline::log(),
        engine_dlss::result_name(last)
    );
    assert_eq!(allocations, 0);
    assert_eq!(Dlss::evaluate_failures().0, 0);

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        dlss.destroy();
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(pool, None);
    }
    drop(dlss);
    drop(streamline);
}
