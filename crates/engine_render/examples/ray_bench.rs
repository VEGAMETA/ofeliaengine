//! Ray scene benchmark (ADR 0087), headless: a field of cubes in a GPU scene over a ground
//! slab, some of them moving, and a ray through every pixel of a camera above it, traced
//! in the same frames against top levels kept by different policies (`RayScene`s over the
//! same GPU scene): rebuilt in every frame where something moved, and refitted with a
//! rebuild after a number of refits. Shows how a refitted top level's rays slow down as
//! its instances travel away from where it was built, and what each policy costs.
//!
//! Every `--every` frames it prints, per policy, the mean GPU time of the top level's
//! passes (instance records and build or refit) and of the camera rays over those frames,
//! the rays relative to the rebuilt top level's; at the end their medians, and whether
//! every policy's last distances match the rebuilt one's.
//!
//! `cargo run -p engine_render --release --example ray_bench -- [--cubes N] [--moving F]
//! [--speed M/S] [--frames N] [--every N] [--refits A,B,...]` (defaults: 10,000 cubes,
//! 10 % moving at 3 m/s, 600 frames of 1/60 s, every 60, refits 8,32 and never).

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance};
use engine_render::graph::{BufferState, ImportedBuffer, RenderGraph};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::rays::{RAY_MASK_ALL, RayScene, RaySettings, TopLevelUpdate};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
    MemoryLocation,
};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// Seconds of motion per frame.
const DT: f32 = 1.0 / 60.0;

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

/// A moving cube: its position on the ground plane and its velocity.
struct Mover {
    id: InstanceId,
    rotation: Quat,
    at: [f32; 2],
    velocity: [f32; 2],
}

fn cube_at(rotation: Quat, at: [f32; 2]) -> Mat4 {
    Mat4::from_scale_rotation_translation(Vec3::splat(0.8), rotation, Vec3::new(at[0], 0.5, at[1]))
}

fn main() {
    let mut cubes = 10_000usize;
    let mut moving = 0.1f32;
    let mut speed = 3.0f32;
    let mut frames = 600u64;
    let mut every = 60u64;
    let mut refits: Vec<u32> = vec![8, 32, u32::MAX];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap();
        match arg.as_str() {
            "--cubes" => cubes = value().parse().unwrap(),
            "--moving" => moving = value().parse().unwrap(),
            "--speed" => speed = value().parse().unwrap(),
            "--frames" => frames = value().parse().unwrap(),
            "--every" => every = value().parse().unwrap(),
            "--refits" => {
                refits = value()
                    .split(',')
                    .map(|r| {
                        if r == "never" {
                            u32::MAX
                        } else {
                            r.parse().unwrap()
                        }
                    })
                    .collect();
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let instance = VkInstance::new(&InstanceDesc {
        app_name: String::from("ray_bench"),
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
    if device.ray_query().is_none() {
        println!("{}: no ray queries", device.adapter().summary());
        return;
    }
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let material = materials.add(MaterialDesc::solid([0.7, 0.7, 0.7], 0.0, 0.5));
    let capacity = cubes as u32 + 64;
    let mut ring = UploadRing::new(
        &device,
        &mut allocator,
        1,
        u64::from(capacity) * 256 + (8 << 20),
    )
    .unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut scene = GpuScene::new(&device, &bindless, capacity, 1).unwrap();

    // The field: a grid 2 m apart over a ground slab; every `stride`-th cube moves in a
    // direction of its own, bouncing off the field's edges.
    let per_row = (cubes as f32).sqrt().ceil() as usize;
    let half = per_row as f32;
    scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(half * 2.4, 0.2, half * 2.4),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, 0.0),
        ),
    });
    let stride = if moving > 0.0 {
        (1.0 / moving).round().max(1.0) as usize
    } else {
        usize::MAX
    };
    let mut movers = Vec::new();
    for i in 0..cubes {
        let h = (i as f32 * 0.618_034).fract();
        let rotation = Quat::from_rotation_y(h * core::f32::consts::TAU);
        let at = [
            (i % per_row) as f32 * 2.0 - half + 1.0,
            (i / per_row) as f32 * 2.0 - half + 1.0,
        ];
        let id = scene.insert(SceneInstance {
            mesh,
            material,
            world_from_object: cube_at(rotation, at),
        });
        if i % stride == 0 {
            let angle = (i as f32 * 0.754_877).fract() * core::f32::consts::TAU;
            movers.push(Mover {
                id,
                rotation,
                at,
                velocity: [angle.cos() * speed, angle.sin() * speed],
            });
        }
    }

    // The policies: rebuilt, then refitted with each rebuild interval.
    let mut policies = vec![("rebuild".to_string(), TopLevelUpdate::Rebuild)];
    for &r in &refits {
        let name = if r == u32::MAX {
            "refit, never rebuilt".to_string()
        } else {
            format!("refit, rebuilt after {r}")
        };
        policies.push((name, TopLevelUpdate::Refit { rebuild_after: r }));
    }
    let mut ray_scenes: Vec<RayScene> = policies
        .iter()
        .map(|&(_, top_level)| {
            RayScene::new(
                &device,
                &bindless,
                RaySettings {
                    top_level,
                    ..RaySettings::default()
                },
                1,
            )
            .unwrap()
        })
        .collect();
    // Each policy's distances, kept (the passes that write them are not culled), and a
    // readback of each for the comparison at the end.
    let bytes = u64::from(WIDTH * HEIGHT) * 4;
    let outputs: Vec<_> = policies
        .iter()
        .map(|_| {
            let (buffer, memory) = allocator
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(bytes).usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                            | vk::BufferUsageFlags::TRANSFER_SRC,
                    ),
                    MemoryLocation::Device,
                )
                .unwrap();
            // SAFETY: created with the device address usage.
            let address = unsafe {
                d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
            };
            let (readback, readback_memory) = allocator
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    MemoryLocation::Readback,
                )
                .unwrap();
            (buffer, memory, address, readback, readback_memory)
        })
        .collect();
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
    let side = half * 2.0;
    let eye = Vec3::new(0.0, side * 0.45, side * 0.75);
    let clip_from_world = vulkan_perspective(
        60f32.to_radians(),
        WIDTH as f32 / HEIGHT as f32,
        0.1,
        side * 4.0,
    ) * look_at(eye, Vec3::new(0.0, 0.0, side * 0.1), Vec3::Y);
    let world_from_clip = clip_from_world.inverse().to_cols_array();

    println!(
        "### Top-level policies under motion ({cubes} cubes, {} moving at {speed} m/s, a ray per pixel at {WIDTH}×{HEIGHT})\n",
        movers.len()
    );
    println!("Device: {}\n", device.adapter().summary());
    print!("| frames |");
    for (name, _) in &policies {
        print!(" {name}: top level µs / rays µs |");
    }
    println!();
    print!("|---|");
    for _ in &policies {
        print!("---:|");
    }
    println!();

    // Per policy: the top level's and the rays' GPU times of every frame measured.
    let mut top_times: Vec<Vec<f64>> = vec![Vec::new(); policies.len()];
    let mut ray_times: Vec<Vec<f64>> = vec![Vec::new(); policies.len()];
    let mut window_start = 0usize;
    for frame_number in 1..=frames + 1 {
        let last = frame_number == frames + 1;
        // Move.
        if frame_number > 2 && !last {
            let limit = half - 0.5;
            for m in &mut movers {
                for axis in 0..2 {
                    m.at[axis] += m.velocity[axis] * DT;
                    if m.at[axis].abs() > limit {
                        m.at[axis] = m.at[axis].clamp(-limit, limit);
                        m.velocity[axis] = -m.velocity[axis];
                    }
                }
                scene.set_transform(m.id, &cube_at(m.rotation, m.at));
            }
        }
        ring.begin_slot(0);
        // SAFETY: one-shot recording; the previous frame has finished.
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
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            let scene_frame = scene
                .add_passes(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let world_from_clip = ring.push(&[world_from_clip]).unwrap();
            for (k, rays) in ray_scenes.iter_mut().enumerate() {
                let ray_frame = rays
                    .add_passes(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &mut ring,
                        &bindless,
                        &geo,
                        &scene_frame,
                        frame_number,
                    )
                    .unwrap()
                    .unwrap();
                let (buffer, _, address, readback, _) = &outputs[k];
                let out = fg.import_buffer(
                    "camera ray distances",
                    ImportedBuffer {
                        buffer: *buffer,
                        size: bytes,
                        address: *address,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::idle()),
                    },
                );
                ray_frame.add_camera_pass(
                    &mut fg,
                    &bindless,
                    world_from_clip,
                    out,
                    [WIDTH, HEIGHT],
                    RAY_MASK_ALL,
                );
                if last {
                    let to = fg.import_buffer(
                        "camera ray readback",
                        ImportedBuffer {
                            buffer: *readback,
                            size: bytes,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: Some(BufferState::host_read()),
                        },
                    );
                    fg.add_pass("camera ray readback")
                        .read(out, engine_render::graph::Access::TransferRead)
                        .write(to, engine_render::graph::Access::TransferWrite)
                        .execute(move |ctx| {
                            // SAFETY: recording; the graph ordered the copy.
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(out),
                                    ctx.buffer(to),
                                    &[vk::BufferCopy {
                                        src_offset: 0,
                                        dst_offset: 0,
                                        size: bytes,
                                    }],
                                );
                            }
                        });
                }
            }
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
        // The pass times of the frame before (they arrive a frame late), in declaration
        // order: each policy's top-level passes (none when it kept its top level), then
        // its camera rays.
        if frame_number > 3 {
            let mut policy = 0;
            let mut top = 0.0;
            for time in graph.gpu_pass_times() {
                let us = f64::from(time.gpu_us);
                match time.name {
                    "tlas instances" | "tlas build" => top += us,
                    "camera rays" => {
                        top_times[policy].push(top);
                        ray_times[policy].push(us);
                        top = 0.0;
                        policy += 1;
                    }
                    _ => {}
                }
            }
            let measured = ray_times[0].len();
            if measured > 0 && ((measured as u64).is_multiple_of(every) || last) {
                print!("| {}–{} |", window_start + 3, measured + 2);
                for k in 0..policies.len() {
                    let tops = &top_times[k][window_start..];
                    let rays = &ray_times[k][window_start..];
                    let base = mean(&ray_times[0][window_start..]);
                    print!(
                        " {:.0} / {:.0} ({:+.1} %) |",
                        mean(tops),
                        mean(rays),
                        (mean(rays) / base - 1.0) * 100.0
                    );
                }
                println!();
                window_start = measured;
            }
        }
    }

    println!();
    println!(
        "| policy | top level µs, median / mean | rays µs, median / mean | last distances vs rebuilt |"
    );
    println!("|---|---:|---:|---|");
    let distances: Vec<Vec<f32>> = outputs
        .iter()
        .map(|(.., memory)| {
            let ptr = memory.mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping holds a float per pixel.
            unsafe {
                core::slice::from_raw_parts(ptr.as_ptr().cast::<f32>(), (WIDTH * HEIGHT) as usize)
            }
            .to_vec()
        })
        .collect();
    for (k, (name, _)) in policies.iter().enumerate() {
        let worst = distances[k]
            .iter()
            .zip(&distances[0])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let hits = distances[k].iter().filter(|&&t| t >= 0.0).count();
        let top_mean = mean(&top_times[k]);
        let ray_mean = mean(&ray_times[k]);
        println!(
            "| {name} | {:.1} / {:.1} | {:.0} / {:.0} | {hits} hits, largest difference {worst:.2e} |",
            median(&mut top_times[k]),
            top_mean,
            median(&mut ray_times[k]),
            ray_mean,
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        for mut rays in ray_scenes {
            rays.destroy(&device, &mut allocator);
        }
        scene.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        for (buffer, memory, _, readback, readback_memory) in outputs {
            allocator.destroy_buffer(buffer, memory);
            allocator.destroy_buffer(readback, readback_memory);
        }
        d.destroy_command_pool(pool, None);
    }
}
