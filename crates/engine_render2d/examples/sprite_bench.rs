//! 2D renderer benchmark, headless: many small textured sprites (y-sorted, four textures)
//! and SDF discs into a 1920×1080 target. For each quad mode (ADR 0015) and count it
//! reports the CPU time of recording the canvas, of preparing it (sort, batch, write into
//! the upload ring) and of recording the graph, and the GPU time of the "2d" pass
//! (median of the measured frames, from the graph's timestamps). Prints Markdown.
//!
//! `cargo run -p engine_render2d --release --example sprite_bench [max-quads]`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::texture::GpuTexture;
use engine_render::upload::UploadRing;
use engine_render2d::{Canvas, Order, Quad, QuadMode, Renderer2d, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 5;
const FRAMES: usize = 30;

#[derive(Clone, Copy, PartialEq)]
enum Scene {
    /// 8×8-pixel textured sprites, y-sorted.
    Sprites,
    /// Discs of radius 6 pixels with antialiased SDF edges.
    Discs,
}

fn fill(canvas: &mut Canvas, scene: Scene, count: usize, textures: &[u32], frame: usize) {
    canvas.clear();
    // A cheap deterministic scatter (Weyl sequence), moving a little every frame.
    let t = frame as f32 * 0.37;
    for i in 0..count {
        let fx = ((i as f32 * 0.618_034 + t * 0.01).fract()) * WIDTH as f32;
        let fy = ((i as f32 * 0.414_214 + t * 0.013).fract()) * HEIGHT as f32;
        let p = Vec2::new(fx, fy);
        match scene {
            Scene::Sprites => canvas.draw(
                &Quad::sprite(textures[i % textures.len()], p, Vec2::splat(8.0))
                    .order(Order::y_sorted(0, -fy)),
            ),
            Scene::Discs => canvas.draw(&Quad::circle(p, 6.0).color(Vec4::new(0.9, 0.5, 0.2, 0.8))),
        }
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let max_quads: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("sprite_bench"),
        validation: false,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let format = vk::Format::R8G8B8A8_UNORM;
    let mut ring =
        UploadRing::new(&device, &mut allocator, 1, (max_quads as u64 + 1024) * 72).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
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

    // Four 32×32 textures: soft discs of different colors (alpha falls off to the edge).
    let mut textures: Vec<GpuTexture> = (0..4)
        .map(|_| GpuTexture::new(&device, &mut allocator, &bindless, format, 32, 32).unwrap())
        .collect();
    let pixels: Vec<Vec<u8>> = (0..4u8)
        .map(|k| {
            let mut data = Vec::with_capacity(32 * 32 * 4);
            for y in 0..32 {
                for x in 0..32 {
                    let r = Vec2::new(x as f32 - 15.5, y as f32 - 15.5).length() / 16.0;
                    let a = ((1.0 - r) * 2.0).clamp(0.0, 1.0);
                    data.extend_from_slice(&[60 * k + 60, 200 - 40 * k, 120, (a * 255.0) as u8]);
                }
            }
            data
        })
        .collect();
    let indices: Vec<u32> = textures.iter().map(GpuTexture::sampled_index).collect();
    let mut uploaded = false;

    println!("### 2D renderer ({WIDTH}×{HEIGHT}, R8G8B8A8)\n");
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "| scene | quads | mode | record canvas µs | prepare + upload µs | graph µs | GPU 2d pass µs | GPU ns/quad | batches |"
    );
    println!("|---|---:|---|---:|---:|---:|---:|---:|---:|");

    let mut canvas = Canvas::new();
    let counts = [10_000usize, 100_000, 1_000_000];
    for scene in [Scene::Sprites, Scene::Discs] {
        for &count in counts.iter().filter(|&&c| c <= max_quads) {
            let modes: &[QuadMode] = if scene == Scene::Sprites {
                &[QuadMode::Indexed, QuadMode::List, QuadMode::Instanced]
            } else {
                &[QuadMode::Indexed]
            };
            for &mode in modes {
                let mut renderer =
                    Renderer2d::new(&device, &mut allocator, &bindless, format, mode).unwrap();
                let (mut record, mut prepare, mut graph_cpu, mut gpu) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for frame in 0..WARMUP + FRAMES {
                    let start = Instant::now();
                    fill(&mut canvas, scene, count, &indices, frame);
                    let recorded = start.elapsed();
                    // SAFETY: the previous frame finished (waited below).
                    unsafe {
                        d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                            .unwrap();
                        d.begin_command_buffer(
                            cmd,
                            &vk::CommandBufferBeginInfo::default()
                                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                        )
                        .unwrap();
                    }
                    ring.begin_slot(0);
                    let (prepared, executed);
                    {
                        // SAFETY: nothing is in flight.
                        let mut fg = unsafe { graph.begin_slot(0, cmd) };
                        if !uploaded {
                            for (texture, data) in textures.iter().zip(&pixels) {
                                texture.upload(&mut fg, &mut ring, data).unwrap();
                            }
                        }
                        let target =
                            fg.create_texture("target", TextureDesc::new_2d(format, WIDTH, HEIGHT));
                        let t = Instant::now();
                        renderer
                            .add_pass(
                                &mut fg,
                                &mut ring,
                                &bindless,
                                &mut canvas,
                                &View2d::pixels(WIDTH, HEIGHT),
                                target,
                                Some([0.0, 0.0, 0.0, 1.0]),
                            )
                            .unwrap();
                        prepared = t.elapsed();
                        fg.add_pass("consume")
                            .read(target, Access::TransferRead)
                            .side_effects()
                            .execute(|_| {});
                        let t = Instant::now();
                        fg.execute_with_bindless(&mut allocator, Some(&bindless))
                            .unwrap();
                        executed = t.elapsed();
                    }
                    uploaded = true;
                    // SAFETY: as above.
                    unsafe {
                        d.end_command_buffer(cmd).unwrap();
                        let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                        d.queue_submit2(
                            device.graphics.raw,
                            &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                            vk::Fence::null(),
                        )
                        .unwrap();
                        device.wait_idle().unwrap();
                    }
                    if frame >= WARMUP {
                        record.push(recorded.as_secs_f64() * 1e6);
                        prepare.push(prepared.as_secs_f64() * 1e6);
                        graph_cpu.push(executed.as_secs_f64() * 1e6);
                    }
                    // Times of the frame before this one (read when its slot is reused).
                    if frame > WARMUP
                        && let Some(time) = graph.gpu_pass_times().iter().find(|t| t.name == "2d")
                    {
                        gpu.push(f64::from(time.gpu_us));
                    }
                }
                let gpu_us = median(&mut gpu);
                println!(
                    "| {} | {count} | {mode:?} | {:.0} | {:.0} | {:.0} | {:.0} | {:.2} | {} |",
                    match scene {
                        Scene::Sprites => "sprites 8 px, textured, y-sorted",
                        Scene::Discs => "SDF discs r = 6 px",
                    },
                    median(&mut record),
                    median(&mut prepare),
                    median(&mut graph_cpu),
                    gpu_us,
                    gpu_us * 1000.0 / count as f64,
                    canvas.stats().batches,
                );
                // SAFETY: the device is idle.
                unsafe { renderer.destroy(&mut allocator) };
            }
        }
    }
    println!();

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        for texture in &mut textures {
            texture.destroy(&device, &mut allocator, &bindless, 0);
        }
        d.destroy_command_pool(pool, None);
    }
}
