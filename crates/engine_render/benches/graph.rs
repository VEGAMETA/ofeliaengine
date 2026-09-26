//! CPU cost of a render graph frame: declaring the passes, compiling (culling, lifetimes,
//! placement, barriers) and recording into a command buffer, in steady state (transient
//! objects cached). Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_render --bench graph
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use engine_render::graph::{
    Access, BufferDesc, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};
use std::time::{Duration, Instant};

/// `passes` passes: a compute pass, a G-buffer pass, then a chain of post passes, each
/// reading the previous result and one of the G-buffer targets, plus one culled pass.
fn declare_and_record(
    graph: &mut RenderGraph,
    allocator: &mut Allocator,
    device: &Device,
    cmd: vk::CommandBuffer,
    slot: usize,
    passes: usize,
) {
    let d = device.raw();
    // SAFETY: nothing recorded here is submitted.
    unsafe {
        d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
            .unwrap();
        d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
            .unwrap();
    }
    {
        // SAFETY: nothing is submitted, so no GPU work uses any slot.
        let mut fg = unsafe { graph.begin_slot(slot, cmd) };
        let desc = |format| TextureDesc::new_2d(format, 256, 144);
        let albedo = fg.create_texture("albedo", desc(vk::Format::R8G8B8A8_UNORM));
        let normal = fg.create_texture("normal", desc(vk::Format::R16G16B16A16_SFLOAT));
        let depth = fg.create_texture("depth", desc(vk::Format::D32_SFLOAT));
        let lights = fg.create_buffer("lights", BufferDesc { size: 65536 });
        let unused = fg.create_texture("unused", desc(vk::Format::R8G8B8A8_UNORM));
        fg.add_pass("cull lights")
            .write(lights, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(|_| {});
        fg.add_pass("gbuffer")
            .color(albedo, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .color(normal, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
            .execute(|_| {});
        fg.add_pass("culled")
            .color(unused, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .execute(|_| {});
        let mut previous = albedo;
        for i in 0..passes.saturating_sub(3) {
            let next = fg.create_texture("post", desc(vk::Format::R16G16B16A16_SFLOAT));
            let side = if i % 2 == 0 { normal } else { depth };
            fg.add_pass("post")
                .read(previous, Access::Sampled(ShaderStages::FRAGMENT))
                .read(side, Access::Sampled(ShaderStages::FRAGMENT))
                .read(lights, Access::StorageRead(ShaderStages::FRAGMENT))
                .color(next, LoadOp::DontCare, StoreOp::Store)
                .execute(|_| {});
            previous = next;
        }
        fg.add_pass("output")
            .read(previous, Access::Sampled(ShaderStages::FRAGMENT))
            .side_effects()
            .execute(|_| {});
        fg.execute(allocator).unwrap();
    }
    // SAFETY: recording command buffer.
    unsafe { d.end_command_buffer(cmd).unwrap() };
}

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("engine_render graph bench"),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .expect("Vulkan instance");
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).expect("adapter");
    let device = Device::new(&instance, &adapter).expect("device");
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

    println!("### Render graph CPU cost\n");
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "Declare + compile + record one frame in steady state (objects cached), release, \
         median and p90 of 200 frames. Recording includes the driver's cost for barriers and \
         `vkCmdBeginRendering`; pass callbacks are empty.\n"
    );
    println!(
        "| passes | median µs / frame | p90 µs | µs per pass | image barriers | transient memory (aliased / unaliased) |"
    );
    println!("|---|---|---|---|---|---|");
    for passes in [16usize, 64, 256] {
        for i in 0..20 {
            declare_and_record(&mut graph, &mut allocator, &device, cmd, i % 2, passes);
        }
        let mut samples: Vec<Duration> = (0..200)
            .map(|i| {
                let start = Instant::now();
                declare_and_record(&mut graph, &mut allocator, &device, cmd, i % 2, passes);
                start.elapsed()
            })
            .collect();
        samples.sort();
        let median = samples[samples.len() / 2].as_secs_f64() * 1e6;
        let p90 = samples[samples.len() * 9 / 10].as_secs_f64() * 1e6;
        let stats = graph.stats();
        println!(
            "| {passes} | {median:.1} | {p90:.1} | {:.2} | {} | {:.1} / {:.1} MiB |",
            median / passes as f64,
            stats.image_barriers,
            stats.transient_placed_bytes as f64 / (1 << 20) as f64,
            stats.transient_unaliased_bytes as f64 / (1 << 20) as f64
        );
    }

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        graph.destroy(&mut allocator);
        d.destroy_command_pool(pool, None);
    }
}
