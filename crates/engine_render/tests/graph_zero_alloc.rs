//! CLAUDE.md §5: zero heap allocations per frame in steady state, for building,
//! compiling and recording a render graph.
//!
//! A counting global allocator observes the process while the same frame is declared and
//! recorded repeatedly after a warm-up (driver and layer allocations happen in C and are
//! not counted: this checks the engine's own code). One test per file, so nothing else
//! allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{
    Access, BufferDesc, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc,
};
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
            app_name: String::from("engine_render zero-alloc test"),
            validation: false,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let adapter = instance.select_adapter(&AdapterPreference::HighPerformance)?;
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

    // A deferred-shading-like frame: G-buffer, lighting, 8 post passes, a culled debug
    // pass and a compute pass producing a buffer.
    let frame = |graph: &mut RenderGraph, allocator: &mut Allocator, slot: usize| {
        // SAFETY: nothing recorded by this test is ever submitted.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        let counter = 7u64;
        {
            // SAFETY: nothing submitted, so no GPU work uses any slot.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            let desc = |format| TextureDesc::new_2d(format, 320, 180);
            let albedo = fg.create_texture("albedo", desc(vk::Format::R8G8B8A8_UNORM));
            let normal = fg.create_texture("normal", desc(vk::Format::R16G16B16A16_SFLOAT));
            let depth = fg.create_texture("depth", desc(vk::Format::D32_SFLOAT));
            let lights = fg.create_buffer("lights", BufferDesc { size: 4096 });
            let mut hdr = fg.create_texture("hdr", desc(vk::Format::R16G16B16A16_SFLOAT));
            let debug = fg.create_texture("debug", desc(vk::Format::R8G8B8A8_UNORM));
            fg.add_pass("cull lights")
                .write(lights, Access::StorageWrite(ShaderStages::COMPUTE))
                .execute(move |ctx| {
                    std::hint::black_box((ctx.buffer(lights), counter));
                });
            fg.add_pass("gbuffer")
                .color(albedo, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .color(normal, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
                .execute(|_| {});
            fg.add_pass("lighting")
                .read(albedo, Access::Sampled(ShaderStages::FRAGMENT))
                .read(normal, Access::Sampled(ShaderStages::FRAGMENT))
                .read(lights, Access::StorageRead(ShaderStages::FRAGMENT))
                .depth_read_only(depth)
                .color(hdr, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(|_| {});
            fg.add_pass("debug view")
                .color(debug, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(|_| {});
            for _ in 0..8 {
                let next = fg.create_texture("post", desc(vk::Format::R16G16B16A16_SFLOAT));
                fg.add_pass("post")
                    .read(hdr, Access::Sampled(ShaderStages::FRAGMENT))
                    .color(next, LoadOp::DontCare, StoreOp::Store)
                    .execute(|_| {});
                hdr = next;
            }
            fg.add_pass("present stand-in")
                .read(hdr, Access::Sampled(ShaderStages::FRAGMENT))
                .side_effects()
                .execute(|_| {});
            fg.execute(allocator).unwrap();
        }
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for i in 0..20 {
        frame(&mut graph, &mut allocator, i % 2);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 0..100 {
        frame(&mut graph, &mut allocator, i % 2);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let stats = graph.stats();
    println!("100 frames: {allocations} heap allocations; last frame {stats:?}");
    assert_eq!(allocations, 0);
    assert_eq!(stats.culled_passes, 1);
    assert_eq!(stats.created_objects, 0);

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(pool, None);
    }
}
