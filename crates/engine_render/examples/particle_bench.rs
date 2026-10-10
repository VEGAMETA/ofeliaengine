//! GPU particles' time (ADR 0081) at 1920 × 1080: sparks (small, added) from 16 K to 1 M
//! alive, and smoke (large and blended: its draw is overdraw) from 256 to 16 K. Each case
//! runs until its pool is full of living particles, then times the passes: simulation
//! (with emission), the sort, the draw. Median of the timed frames, µs.
//!
//! `cargo run -p engine_render --release --example particle_bench`
//! (`ENGINE_ADAPTER=radeon` for the integrated GPU). Prints a Markdown table.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Affine3A, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::forward::Camera;
use engine_render::graph::{Access, LoadOp, RenderGraph, StoreOp, TextureDesc};
use engine_render::particles::{ParticleEmitter, ParticleSystem, ParticleTarget};
use engine_render::post::HDR_FORMAT;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const DT: f32 = 1.0 / 60.0;
const TIMED: usize = 30;

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// The sparks of `count` alive (on average): lifetimes of 1 s.
fn sparks(count: u32) -> ParticleEmitter {
    ParticleEmitter {
        rate: count as f32,
        lifetime: [0.8, 1.2],
        radius: 0.2,
        direction: Vec3::Y,
        spread: 0.6,
        speed: [3.0, 6.0],
        acceleration: Vec3::new(0.0, -9.81, 0.0),
        drag: 0.1,
        size: [0.03, 0.01],
        color: [Vec4::new(6.0, 2.0, 0.5, 1.0), Vec4::new(1.0, 0.2, 0.0, 0.0)],
        additive: 1.0,
        seed: 1,
        ..ParticleEmitter::default()
    }
}

/// Smoke of `count` alive: lifetimes of 2 s, large and blended.
fn smoke(count: u32) -> ParticleEmitter {
    ParticleEmitter {
        rate: count as f32 / 2.0,
        lifetime: [1.5, 2.5],
        radius: 1.0,
        direction: Vec3::Y,
        spread: 0.8,
        speed: [0.5, 1.0],
        acceleration: Vec3::new(0.0, 0.3, 0.0),
        drag: 0.3,
        size: [0.5, 2.0],
        color: [Vec4::new(0.4, 0.4, 0.4, 0.3), Vec4::new(0.5, 0.5, 0.5, 0.0)],
        additive: 0.0,
        spin: [-0.5, 0.5],
        soft_distance: 0.5,
        seed: 2,
        ..ParticleEmitter::default()
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("particle_bench"),
        validation: false,
        debug_utils: false,
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
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
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
    let eye = Vec3::new(0.0, 2.0, 8.0);
    let target = Vec3::new(0.0, 2.0, 0.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 200.0)
            * look_at(eye, target, Vec3::Y),
        position: eye,
    };

    println!("### GPU particles, 1920 × 1080");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!();
    println!("| particles | kind | simulate µs | sort µs | draw µs |");
    println!("|---:|---|---:|---:|---:|");
    let cases = [16u32 << 10, 64 << 10, 256 << 10, 1 << 20]
        .map(|n| ("sparks", n, sparks(n)))
        .into_iter()
        .chain([256u32, 1 << 10, 4 << 10, 16 << 10].map(|n| ("smoke", n, smoke(n))));
    {
        for (kind, count, emitter) in cases {
            let mut particles = ParticleSystem::new(
                &device,
                &mut allocator,
                &bindless,
                count + count / 2,
                HDR_FORMAT,
            )
            .unwrap();
            particles.add_emitter(ParticleEmitter {
                transform: Affine3A::IDENTITY,
                ..emitter
            });
            let (mut simulate, mut sort, mut draw) = (Vec::new(), Vec::new(), Vec::new());
            // 3 s to fill the pool with living particles, then the timed frames.
            let frames = 180 + TIMED;
            for frame in 0..frames {
                ring.begin_slot(0);
                let step = particles
                    .prepare(&mut ring, DT, eye, (target - eye).normalize(), 200.0)
                    .unwrap();
                // SAFETY: one frame at a time.
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
                {
                    // SAFETY: the previous frame finished.
                    let mut fg = unsafe { graph.begin_slot(0, cmd) };
                    let color =
                        fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                    let depth = fg.create_texture(
                        "depth",
                        TextureDesc::new_2d(vk::Format::D32_SFLOAT, WIDTH, HEIGHT),
                    );
                    fg.add_pass("clear")
                        .color(color, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                        .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
                        .execute(|_| {});
                    particles.add_passes(
                        &mut fg,
                        &bindless,
                        &step,
                        &ParticleTarget {
                            color,
                            depth,
                            reactive: None,
                            clip_from_world: camera.clip_from_world,
                            camera,
                        },
                    );
                    fg.add_pass("keep")
                        .read(color, Access::TransferRead)
                        .side_effects()
                        .execute(|_| {});
                    fg.execute_with_bindless(&mut allocator, Some(&bindless))
                        .unwrap();
                }
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
                if frame > frames - TIMED {
                    for time in graph.gpu_pass_times() {
                        let us = f64::from(time.gpu_us);
                        match time.name {
                            "particles simulate" => simulate.push(us),
                            "particles sort" => sort.push(us),
                            "particles" => draw.push(us),
                            _ => {}
                        }
                    }
                }
            }
            println!(
                "| {count} | {kind} | {:.1} | {:.1} | {:.1} |",
                median(simulate),
                median(sort),
                median(draw)
            );
            // SAFETY: the device is idle.
            unsafe { particles.destroy(&mut allocator) };
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        ring.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        d.destroy_command_pool(pool, None);
    }
}
