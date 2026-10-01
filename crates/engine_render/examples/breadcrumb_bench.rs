//! Cost of GPU breadcrumbs (ADR 0061): a frame of render graph passes, recorded and run
//! on two devices of the same GPU, one with the markers and one without, frames
//! alternating between them:
//!
//! ```text
//! cargo run -p engine_render --release --example breadcrumb_bench
//! ```
//!
//! Two kinds of frames: a chain where every pass samples the previous one's output (a
//! barrier between each), and independent passes that each clear their own target (no
//! barriers: the GPU may overlap them, unless the markers stop it). Every target is
//! 512×512 RGBA16F. Reported: the CPU time to declare, compile and record the frame, and
//! the GPU time between timestamps at the frame's start and end, medians of 300 frames, and
//! the difference per pass (two markers).

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_render::graph::{Access, LoadOp, RenderGraph, ShaderStages, StoreOp, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};
use std::time::Instant;

const FRAMES: usize = 300;
const WARMUP: usize = 30;
const SIZE: u32 = 512;

fn declare(
    graph: &mut RenderGraph,
    slot: usize,
    cmd: vk::CommandBuffer,
    allocator: &mut Allocator,
    passes: usize,
    chain: bool,
) {
    // SAFETY: the bench waits for each frame before recording the next into any slot.
    let mut fg = unsafe { graph.begin_slot(slot, cmd) };
    let desc = TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, SIZE, SIZE);
    if chain {
        let mut previous = fg.create_texture("first", desc);
        fg.add_pass("first")
            .color(previous, LoadOp::Clear([0.1; 4]), StoreOp::Store)
            .execute(|_| {});
        for _ in 1..passes {
            let next = fg.create_texture("link", desc);
            fg.add_pass("link")
                .read(previous, Access::Sampled(ShaderStages::FRAGMENT))
                .color(next, LoadOp::Clear([0.2; 4]), StoreOp::Store)
                .execute(|_| {});
            previous = next;
        }
        fg.add_pass("output")
            .read(previous, Access::Sampled(ShaderStages::FRAGMENT))
            .side_effects()
            .execute(|_| {});
    } else {
        for _ in 0..passes {
            let target = fg.create_texture("target", desc);
            fg.add_pass("clear")
                .color(target, LoadOp::Clear([0.3; 4]), StoreOp::Store)
                .side_effects()
                .execute(|_| {});
        }
    }
    fg.execute(allocator).unwrap();
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// One device with its graph and frame objects.
struct Rig {
    allocator: Allocator,
    graph: RenderGraph,
    pool: vk::CommandPool,
    cmds: Vec<vk::CommandBuffer>,
    queries: vk::QueryPool,
    fence: vk::Fence,
    period_ns: f64,
    frame: usize,
    // Last: dropped after everything created from it.
    device: Device,
}

impl Rig {
    fn new(instance: &Instance, breadcrumbs: bool) -> Self {
        // SAFETY: single-threaded here; `Device::new` below reads the variable.
        unsafe {
            std::env::set_var(
                "ENGINE_GPU_BREADCRUMBS",
                if breadcrumbs { "1" } else { "0" },
            );
        }
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference).unwrap();
        let device = Device::new(instance, &adapter).unwrap();
        assert_eq!(
            device.breadcrumbs().is_some(),
            breadcrumbs,
            "no VK_AMD_buffer_marker"
        );
        let d = device.raw();
        // SAFETY: plain object creation.
        let (pool, cmds, queries, fence) = unsafe {
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
            let queries = d
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(2),
                    None,
                )
                .unwrap();
            let fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .unwrap();
            (pool, cmds, queries, fence)
        };
        Self {
            allocator: Allocator::new(&device),
            graph: RenderGraph::new(&device, 2),
            pool,
            cmds,
            queries,
            fence,
            period_ns: f64::from(device.adapter().limits.timestamp_period),
            frame: 0,
            device,
        }
    }

    /// Records, runs and times one frame: (CPU µs, GPU µs).
    fn frame(&mut self, passes: usize, chain: bool) -> (f64, f64) {
        let d = self.device.raw();
        let slot = self.frame % 2;
        self.frame += 1;
        let cmd = self.cmds[slot];
        // SAFETY: the previous frame finished (fence wait below) and its queries were
        // read.
        let recorded = unsafe {
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
            d.cmd_reset_query_pool(cmd, self.queries, 0, 2);
            d.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::TOP_OF_PIPE, self.queries, 0);
            let start = Instant::now();
            declare(
                &mut self.graph,
                slot,
                cmd,
                &mut self.allocator,
                passes,
                chain,
            );
            let recorded = start.elapsed();
            d.cmd_write_timestamp2(
                cmd,
                vk::PipelineStageFlags2::BOTTOM_OF_PIPE,
                self.queries,
                1,
            );
            d.end_command_buffer(cmd).unwrap();
            let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                self.device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&commands)],
                self.fence,
            )
            .unwrap();
            d.wait_for_fences(&[self.fence], true, u64::MAX).unwrap();
            d.reset_fences(&[self.fence]).unwrap();
            recorded
        };
        let mut stamps = [0u64; 2];
        // SAFETY: the frame finished.
        unsafe {
            d.get_query_pool_results(
                self.queries,
                0,
                &mut stamps,
                vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
            )
            .unwrap();
        }
        (
            recorded.as_secs_f64() * 1e6,
            stamps[1].wrapping_sub(stamps[0]) as f64 * self.period_ns / 1000.0,
        )
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let d = self.device.raw();
        // SAFETY: every frame was waited for.
        unsafe {
            self.graph.destroy(&mut self.allocator, None);
            d.destroy_fence(self.fence, None);
            d.destroy_query_pool(self.queries, None);
            d.destroy_command_pool(self.pool, None);
        }
    }
}

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("breadcrumb bench"),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    // Two devices on the same GPU, one with breadcrumbs; their frames alternate, so
    // clocks and other load affect both alike.
    let mut off = Rig::new(&instance, false);
    let mut on = Rig::new(&instance, true);
    println!("Device: {}", on.device.adapter().summary());
    println!(
        "Breadcrumb memory: {}\n",
        if on.device.breadcrumbs().is_some_and(|b| b.uncached()) {
            "device-coherent, uncached (AMD)"
        } else {
            "host-visible, coherent"
        }
    );
    println!(
        "| frame | passes | CPU µs off / on | CPU ns per pass | GPU µs off / on | GPU ns per pass |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    for chain in [true, false] {
        for passes in [16usize, 64, 256] {
            let mut samples: [Vec<f64>; 4] = Default::default();
            for frame in 0..WARMUP + FRAMES {
                let (cpu_off, gpu_off) = off.frame(passes, chain);
                let (cpu_on, gpu_on) = on.frame(passes, chain);
                if frame >= WARMUP {
                    for (v, s) in [cpu_off, cpu_on, gpu_off, gpu_on]
                        .into_iter()
                        .zip(&mut samples)
                    {
                        s.push(v);
                    }
                }
            }
            let [cpu_off, cpu_on, gpu_off, gpu_on] = samples.map(|mut s| median(&mut s));
            let per_pass = |off: f64, on: f64| (on - off) * 1000.0 / passes as f64;
            println!(
                "| {} | {passes} | {cpu_off:.1} / {cpu_on:.1} | {:.0} | {gpu_off:.1} / {gpu_on:.1} | {:.0} |",
                if chain { "chain" } else { "independent" },
                per_pass(cpu_off, cpu_on),
                per_pass(gpu_off, gpu_on),
            );
        }
    }
}
