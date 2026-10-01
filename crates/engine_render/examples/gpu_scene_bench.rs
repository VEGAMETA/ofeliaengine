//! Persistent GPU scene benchmark (ADR 0064), headless: the same cube field drawn from a
//! draw list rebuilt every frame (`ForwardRenderer::add_passes`) and from a `GpuScene`
//! (`GpuScene::add_passes` + `ForwardRenderer::add_scene_passes`), with nothing moving,
//! 1 % moving, everything moving, and 1 % removed and added again every frame.
//!
//! Per frame (medians): the application's CPU time to hand the frame its instances (the
//! draw list built from stored transforms, or the scene's `set_transform` / `insert` /
//! `remove` calls), the renderer's CPU time (`add_passes`), graph recording, bytes
//! written into the upload ring, and the GPU times of the scene upload, the culling
//! passes, the shadow pass, the forward pass and (draw list with motion vectors) the copy
//! of last frame's instances. 1920×1080 HDR, frustum culling, motion vectors on (a
//! `Temporal` view; `--no-motion` turns them off). Prints Markdown.
//!
//! `cargo run -p engine_render --release --example gpu_scene_bench [max-instances]
//! [--no-motion]`

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::too_many_lines
)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
};
use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 10;
const FRAMES: usize = 60;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Motion {
    Static,
    OnePercent,
    All,
    Churn,
}

impl Motion {
    fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::OnePercent => "1 % moving",
            Self::All => "100 % moving",
            Self::Churn => "1 % removed + added",
        }
    }
}

fn place(i: usize, count: usize, t: f32) -> Mat4 {
    let per_row = (count as f32).sqrt().ceil() as usize;
    let half = per_row as f32;
    let (x, z) = ((i % per_row) as f32, (i / per_row) as f32);
    let h = (i as f32 * 0.618_034).fract();
    Mat4::from_scale_rotation_translation(
        Vec3::splat(0.8),
        Quat::from_rotation_y(t + h * core::f32::consts::TAU),
        Vec3::new(x * 2.0 - half + 1.0, 0.6, z * 2.0 - half + 1.0),
    )
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let mut max_instances = 100_000usize;
    let mut motion_vectors = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-motion" => motion_vectors = false,
            n => max_instances = n.parse().unwrap(),
        }
    }
    let instance = VkInstance::new(&InstanceDesc {
        app_name: String::from("gpu_scene_bench"),
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
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 64 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let palette: Vec<MaterialId> = (0..64)
        .map(|k| {
            let h = k as f32 / 64.0;
            materials.add(MaterialDesc::solid(
                [0.3 + 0.6 * h, 0.5, 0.9 - 0.6 * h],
                0.0,
                0.5,
            ))
        })
        .collect();
    let capacity = max_instances as u64 + 1024;
    let mut ring = UploadRing::new(&device, &mut allocator, 1, capacity * 256).unwrap();
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
    let light = DirectionalLight {
        direction: Vec3::new(-0.4, -1.0, -0.3),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };

    println!(
        "### Persistent GPU scene against a per-frame draw list ({WIDTH}×{HEIGHT} HDR, frustum culling, motion vectors {})\n",
        if motion_vectors { "on" } else { "off" }
    );
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "| instances | motion | path | app µs | renderer µs | graph µs | upload KiB | GPU upload µs | GPU cull µs | GPU shadow µs | GPU forward µs | GPU remember µs |"
    );
    println!("|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");

    let mut frame_number = 0u64;
    for count in [10_000usize, 100_000]
        .into_iter()
        .filter(|&c| c <= max_instances)
    {
        let side = (count as f32).sqrt().ceil() * 2.0;
        let eye = Vec3::new(0.0, side * 0.9, side * 0.9);
        let camera = Camera {
            clip_from_world: vulkan_perspective(
                60f32.to_radians(),
                WIDTH as f32 / HEIGHT as f32,
                0.1,
                side * 3.0,
            ) * look_at(eye, Vec3::ZERO, Vec3::Y),
            position: eye,
        };
        for motion in [
            Motion::Static,
            Motion::OnePercent,
            Motion::All,
            Motion::Churn,
        ] {
            for scene_path in [false, true] {
                let temporal = Temporal::new(&device, &mut allocator, capacity as u32).unwrap();
                let mut scene = GpuScene::new(&device, &bindless, capacity as u32, 1).unwrap();
                let material = |i: usize| palette[(i * 7) % palette.len()];
                let mut transforms: Vec<Mat4> = (0..count).map(|i| place(i, count, 0.0)).collect();
                let mut ids: Vec<InstanceId> = if scene_path {
                    (0..count)
                        .map(|i| {
                            scene.insert(SceneInstance {
                                mesh,
                                material: material(i),
                                world_from_object: transforms[i],
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let mut draws: Vec<DrawItem> = Vec::with_capacity(count);
                let (mut app, mut cpu, mut record, mut bytes) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                let (mut upload, mut cull, mut shadow, mut forward, mut remember) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for frame in 0..WARMUP + FRAMES {
                    let t = frame as f32 * 0.02;
                    // The simulation's new transforms (not timed: the same for both paths).
                    match motion {
                        Motion::Static => {}
                        Motion::OnePercent | Motion::Churn => {
                            for i in (frame % 100..count).step_by(100) {
                                transforms[i] = place(i, count, t);
                            }
                        }
                        Motion::All => {
                            for (i, m) in transforms.iter_mut().enumerate() {
                                *m = place(i, count, t);
                            }
                        }
                    }
                    // The application hands the frame its instances.
                    let start = Instant::now();
                    if scene_path {
                        match motion {
                            Motion::Static => {}
                            Motion::OnePercent => {
                                for i in (frame % 100..count).step_by(100) {
                                    scene.set_transform(ids[i], &transforms[i]);
                                }
                            }
                            Motion::All => {
                                for (id, m) in ids.iter().zip(&transforms) {
                                    scene.set_transform(*id, m);
                                }
                            }
                            Motion::Churn => {
                                for i in (frame % 100..count).step_by(100) {
                                    scene.remove(ids[i]);
                                    ids[i] = scene.insert(SceneInstance {
                                        mesh,
                                        material: material(i),
                                        world_from_object: transforms[i],
                                    });
                                }
                            }
                        }
                    } else {
                        draws.clear();
                        draws.extend(transforms.iter().enumerate().map(|(i, m)| DrawItem {
                            mesh,
                            material: material(i),
                            world_from_object: *m,
                        }));
                    }
                    let handed = start.elapsed();
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
                    let (prepared, executed, used);
                    {
                        // SAFETY: nothing is in flight.
                        let mut fg = unsafe { graph.begin_slot(0, cmd) };
                        frame_number += 1;
                        let geo = geometry
                            .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                            .unwrap();
                        textures
                            .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                            .unwrap();
                        let mats = materials
                            .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                            .unwrap();
                        let hdr = fg
                            .create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                        let depth = fg.create_texture(
                            "depth",
                            TextureDesc::new_2d(depth_format, WIDTH, HEIGHT),
                        );
                        let ring_before = ring.used();
                        let view = ForwardView {
                            camera,
                            light,
                            color: hdr,
                            depth,
                            clear: Some([0.02, 0.02, 0.05, 1.0]),
                            occlusion: None,
                            temporal: motion_vectors.then_some(&temporal),
                            lights: &[],
                            environment: None,
                        };
                        let t0 = Instant::now();
                        let passes = if scene_path {
                            let frame = scene
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
                            renderer
                                .add_scene_passes(
                                    &mut fg, &mut ring, &bindless, &geo, &mats, &frame, &view,
                                )
                                .unwrap()
                        } else {
                            renderer
                                .add_passes(
                                    &mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view,
                                )
                                .unwrap()
                        };
                        prepared = t0.elapsed();
                        used = ring.used() - ring_before;
                        let mut consume = fg.add_pass("consume").read(hdr, Access::TransferRead);
                        if let Some(m) = passes.motion {
                            consume = consume.read(m.velocity, Access::TransferRead);
                        }
                        consume.side_effects().execute(|_| {});
                        let t1 = Instant::now();
                        fg.execute_with_bindless(&mut allocator, Some(&bindless))
                            .unwrap();
                        executed = t1.elapsed();
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
                    if frame >= WARMUP {
                        app.push(handed.as_secs_f64() * 1e6);
                        cpu.push(prepared.as_secs_f64() * 1e6);
                        record.push(executed.as_secs_f64() * 1e6);
                        bytes.push(used as f64 / 1024.0);
                    }
                    if frame > WARMUP {
                        let (mut u, mut c, mut s, mut f, mut r) = (0.0, 0.0, 0.0, 0.0, 0.0);
                        if std::env::var("BENCH_PASSES").is_ok() && frame == WARMUP + FRAMES - 1 {
                            for time in graph.gpu_pass_times() {
                                println!("    {} {:.1}", time.name, time.gpu_us);
                            }
                        }
                        for time in graph.gpu_pass_times() {
                            let us = f64::from(time.gpu_us);
                            match time.name {
                                "scene upload" | "scene upload (full)" => u += us,
                                "cull setup" | "cull" => c += us,
                                "shadow cull" | "shadow" => s += us,
                                "forward" | "motion background" => f += us,
                                "remember instances" => r += us,
                                _ => {}
                            }
                        }
                        upload.push(u);
                        cull.push(c);
                        shadow.push(s);
                        forward.push(f);
                        remember.push(r);
                    }
                }
                println!(
                    "| {count} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.0} | {:.0} | {:.1} |",
                    motion.name(),
                    if scene_path { "GPU scene" } else { "draw list" },
                    median(&mut app),
                    median(&mut cpu),
                    median(&mut record),
                    median(&mut bytes),
                    median(&mut upload),
                    median(&mut cull),
                    median(&mut shadow),
                    median(&mut forward),
                    median(&mut remember),
                );
                device.wait_idle().unwrap();
                let mut temporal = temporal;
                // SAFETY: the device is idle.
                unsafe {
                    scene.destroy(&mut allocator);
                    temporal.destroy(&mut allocator);
                }
            }
        }
    }
    println!();

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
}
