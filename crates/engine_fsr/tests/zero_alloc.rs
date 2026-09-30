//! CLAUDE.md §5: zero heap allocations per frame in steady state, for declaring and
//! recording the FSR pass (ADR 0060).
//!
//! A counting global allocator observes the process while the same frame is declared and
//! recorded repeatedly after a warm-up. The FidelityFX SDK's own allocations happen in C++
//! (the C runtime's heap) and are not counted: this checks the engine's side. One test per
//! file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_fsr::{Fsr, FsrCamera, FsrInput};
use engine_render::forward::{MOTION_FORMAT, MotionOutput};
use engine_render::graph::{Access, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};
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
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_fsr zero-alloc test"),
            validation: false,
            debug_utils: false,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    let (_instance, device) = match setup() {
        Ok(pair) => pair,
        Err(e) if !required => {
            println!("skipping: {e}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut graph = RenderGraph::new(&device, 2);
    let mut fsr = Fsr::new(&device, 2);
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
    // 96 × 96.
    let frame = |graph: &mut RenderGraph,
                 allocator: &mut Allocator,
                 fsr: &mut Fsr,
                 slot: usize,
                 number: u64| {
        // SAFETY: nothing recorded by this test is ever submitted.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        {
            // SAFETY: nothing submitted, so no GPU work uses any slot.
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
            let upscaled = fsr
                .add_pass(
                    &mut fg,
                    FsrInput {
                        color,
                        depth,
                        motion: MotionOutput {
                            velocity,
                            jitter: [0.25, -0.25],
                            reset: false,
                            reactive: None,
                        },
                        camera: FsrCamera {
                            near: 0.1,
                            far: 100.0,
                            fov_y: 1.0,
                            meters_per_unit: 1.0,
                        },
                        frame_time_ms: 16.7,
                    },
                    (96, 96),
                    number,
                )
                .unwrap();
            fg.add_pass("present stand-in")
                .read(upscaled, Access::Sampled(ShaderStages::FRAGMENT))
                .side_effects()
                .execute(|_| {});
            fg.execute(allocator).unwrap();
        }
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for i in 0..20u64 {
        frame(
            &mut graph,
            &mut allocator,
            &mut fsr,
            (i % 2) as usize,
            i + 1,
        );
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 20..120u64 {
        frame(
            &mut graph,
            &mut allocator,
            &mut fsr,
            (i % 2) as usize,
            i + 1,
        );
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("100 frames: {allocations} heap allocations");
    assert_eq!(allocations, 0);
    assert_eq!(Fsr::dispatch_failures(), 0);

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        fsr.destroy();
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(pool, None);
    }
}
