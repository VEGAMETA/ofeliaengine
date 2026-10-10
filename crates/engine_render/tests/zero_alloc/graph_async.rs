//! CLAUDE.md §5: zero heap allocations per frame in steady state, with async compute
//! (ADR 0065): scheduling across queues, segments, ownership transfers and the graph's own
//! submissions included.
//!
//! As `graph_zero_alloc.rs`, but every frame is submitted and waited for: with async
//! compute the graph submits its earlier segments itself during `execute`. Driver and
//! layer allocations happen in C and are not counted. One test per file, so nothing else
//! allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{
    Access, BufferDesc, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};
use std::sync::atomic::Ordering;

use crate::ALLOCATIONS;

#[test]
fn steady_state_async_compute_frames_do_not_allocate() {
    let _alone = crate::alone();
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render async zero-alloc test"),
            validation: false,
            debug_utils: true,
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
    graph.set_async_compute(true);
    if !graph.async_compute() {
        println!("no async compute queue: the single-queue test covers this device");
        return;
    }
    // SAFETY: plain object creation.
    let (pool, cmds) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmds = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(2),
            )
            .unwrap();
        (pool, cmds)
    };

    // A frame with two async passes overlapping the G-buffer: a light list and a
    // reduction of the depth, each read by the lighting.
    let frame = |graph: &mut RenderGraph, allocator: &mut Allocator, slot: usize| {
        let cmd = cmds[slot];
        // SAFETY: the previous frame finished (waited below).
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        {
            // SAFETY: every earlier frame was waited for.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            let desc = |format| TextureDesc::new_2d(format, 320, 180);
            let albedo = fg.create_texture("albedo", desc(vk::Format::R8G8B8A8_UNORM));
            let depth = fg.create_texture("depth", desc(vk::Format::D32_SFLOAT));
            let lights = fg.create_buffer("lights", BufferDesc { size: 4096 });
            let reduced = fg.create_buffer("reduced depth", BufferDesc { size: 4096 });
            let hdr = fg.create_texture("hdr", desc(vk::Format::R16G16B16A16_SFLOAT));
            fg.add_pass("prepass")
                .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
                .execute(|_| {});
            fg.add_pass("cull lights")
                .write(lights, Access::TransferWrite)
                .async_compute()
                .execute(move |ctx| {
                    // SAFETY: recording; the buffer holds 4096 bytes.
                    unsafe {
                        ctx.device
                            .cmd_fill_buffer(ctx.cmd, ctx.buffer(lights), 0, 4096, 1)
                    };
                });
            fg.add_pass("reduce depth")
                .read(depth, Access::Sampled(ShaderStages::COMPUTE))
                .write(reduced, Access::StorageWrite(ShaderStages::COMPUTE))
                .async_compute()
                .execute(|_| {});
            fg.add_pass("gbuffer")
                .color(albedo, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(|_| {});
            fg.add_pass("lighting")
                .read(albedo, Access::Sampled(ShaderStages::FRAGMENT))
                .read(lights, Access::StorageRead(ShaderStages::FRAGMENT))
                .read(reduced, Access::StorageRead(ShaderStages::FRAGMENT))
                .depth_read_only(depth)
                .color(hdr, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                .execute(|_| {});
            fg.add_pass("present stand-in")
                .read(hdr, Access::Sampled(ShaderStages::FRAGMENT))
                .side_effects()
                .execute(|_| {});
            fg.execute(allocator).unwrap();
        }
        // SAFETY: recorded above; submitted after the graph's own segments.
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

    for i in 0..20 {
        frame(&mut graph, &mut allocator, i % 2);
    }
    // Shading variants the frames have not drawn with are made on the device's
    // warm-up thread (ADR 0079), which allocates: done before counting.
    device.wait_for_pipeline_warm_up();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 0..100 {
        frame(&mut graph, &mut allocator, i % 2);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let stats = graph.stats();
    println!("100 frames: {allocations} heap allocations; last frame {stats:?}");
    assert_eq!(allocations, 0);
    assert_eq!(stats.async_passes, 2);
    assert!(stats.submits > 0);
    assert_eq!(stats.created_objects, 0);

    // SAFETY: nothing is in use (waited above).
    unsafe {
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(pool, None);
    }
}
