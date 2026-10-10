//! GPU particles (`engine_render::particles`, ADR 0081): the pool keeps every particle
//! (alive + dead = capacity), the alive count follows the spawn rate and the lifetime
//! exactly, each particle is where integrating its velocity puts it, and two runs of the
//! same steps make the same particles. Steps of 1/64 s and lifetimes of 0.5 s are exact
//! in binary, so a particle dies at exactly its 32nd step.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Affine3A, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::forward::Camera;
use engine_render::graph::{Access, LoadOp, RenderGraph, StoreOp, TextureDesc};
use engine_render::particles::{ParticleEmitter, ParticleSystem, ParticleTarget};
use engine_render::post::HDR_FORMAT;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render particles test"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

const CAPACITY: u32 = 4096;
const DT: f32 = 1.0 / 64.0;
const ORIGIN: Vec3 = Vec3::new(0.0, 1.0, 0.0);
const V0: f32 = 2.0;
const GRAVITY: f32 = -10.0;

/// A particle read back: its birth number, position and age.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Read {
    serial: u32,
    position: [f32; 3],
    age: f32,
}

/// Runs `frames` steps of one fountain and reads back, after each, the alive and dead
/// counts, and after the last the living particles.
#[allow(clippy::too_many_lines)]
fn run(device: &Device, frames: usize) -> (Vec<(u32, u32)>, Vec<Read>) {
    let d = device.raw();
    let mut allocator = Allocator::new(device);
    let bindless = Bindless::new(device).unwrap();
    let mut ring = UploadRing::new(device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(device, 1);
    let mut particles =
        ParticleSystem::new(device, &mut allocator, &bindless, CAPACITY, HDR_FORMAT).unwrap();
    particles.add_emitter(ParticleEmitter {
        transform: Affine3A::from_translation(ORIGIN),
        // 10 per step.
        rate: 640.0,
        lifetime: [0.5, 0.5],
        radius: 0.0,
        direction: Vec3::Y,
        spread: 0.0,
        speed: [V0, V0],
        acceleration: Vec3::new(0.0, GRAVITY, 0.0),
        drag: 0.0,
        size: [0.1, 0.1],
        color: [Vec4::ONE, Vec4::ONE],
        seed: 7,
        ..ParticleEmitter::default()
    });
    let (width, height) = (64u32, 64u32);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, 1.0, 0.1, 100.0)
            * look_at(Vec3::new(0.0, 1.0, 5.0), ORIGIN, Vec3::Y),
        position: Vec3::new(0.0, 1.0, 5.0),
    };
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(64 + u64::from(CAPACITY) * (48 + 4))
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
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

    let mut counts = Vec::new();
    let mut living = Vec::new();
    for frame in 0..frames {
        ring.begin_slot(0);
        let forward = (ORIGIN - camera.position).normalize();
        let step = particles
            .prepare(&mut ring, DT, camera.position, forward, 100.0)
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
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, width, height));
            let depth = fg.create_texture(
                "depth",
                TextureDesc::new_2d(vk::Format::D32_SFLOAT, width, height),
            );
            // An empty scene: color cleared, depth at the far plane.
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
            // Something reads the color: the draw is kept.
            fg.add_pass("keep")
                .read(color, Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        let [pool_buffer, counters, alive] = particles.debug_buffers();
        // SAFETY: after the frame's work (the barrier), copies into the readback buffer.
        unsafe {
            let barrier = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ);
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().memory_barriers(&[barrier]),
            );
            d.cmd_copy_buffer(
                cmd,
                counters,
                readback,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: 64,
                }],
            );
            d.cmd_copy_buffer(
                cmd,
                pool_buffer,
                readback,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 64,
                    size: u64::from(CAPACITY) * 48,
                }],
            );
            d.cmd_copy_buffer(
                cmd,
                alive,
                readback,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 64 + u64::from(CAPACITY) * 48,
                    size: u64::from(CAPACITY) * 4,
                }],
            );
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
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let words: &[u32] = unsafe {
            core::slice::from_raw_parts(
                readback_mem.mapped_ptr().unwrap().as_ptr().cast::<u32>(),
                (16 + CAPACITY * 13) as usize,
            )
        };
        let (alive_count, dead_count) = (words[0], words[1]);
        counts.push((alive_count, dead_count));
        if frame + 1 == frames {
            let list = &words[(16 + CAPACITY * 12) as usize..];
            for &slot in &list[..alive_count as usize] {
                let p = &words[(16 + slot * 12) as usize..][..12];
                living.push(Read {
                    serial: p[9],
                    position: [
                        f32::from_bits(p[0]),
                        f32::from_bits(p[1]),
                        f32::from_bits(p[2]),
                    ],
                    age: f32::from_bits(p[3]),
                });
            }
        }
    }
    living.sort_by_key(|r| r.serial);

    // SAFETY: the device is idle.
    unsafe {
        particles.destroy(&mut allocator);
        ring.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    (counts, living)
}

#[test]
fn particles_are_conserved_and_move_as_integrated() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    const FRAMES: usize = 48;
    let (counts, living) = run(&device, FRAMES);
    for (k, &(alive, dead)) in counts.iter().enumerate() {
        assert_eq!(
            alive + dead,
            CAPACITY,
            "frame {k}: alive {alive} + dead {dead}"
        );
        // Born 10 per step, each lives 31 steps after its first (it dies at age 0.5).
        let expected = 10 * (k as u32 + 1).min(31);
        assert_eq!(alive, expected, "frame {k}");
    }
    // Semi-implicit Euler from (0, V0, 0) under gravity, `n` steps of DT.
    for p in &living {
        let n = (p.age / DT).round();
        assert!((p.age - n * DT).abs() < 1e-6, "{p:?}");
        let y = ORIGIN.y + n * V0 * DT + GRAVITY * DT * DT * n * (n + 1.0) / 2.0;
        assert!(
            p.position[0].abs() < 1e-5
                && p.position[2].abs() < 1e-5
                && (p.position[1] - y).abs() < 1e-4,
            "{p:?}: y {y} expected"
        );
    }
    assert_eq!(living.len(), 310);
    println!("alive after {FRAMES} steps: {}", living.len());

    // The same steps again: the same particles.
    let (_, again) = run(&device, FRAMES);
    assert_eq!(living, again);

    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
