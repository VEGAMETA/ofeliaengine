//! GPU cost of the TAAU resolve (ADR 0043), headless: a field of cubes seen by a camera
//! circling it, drawn jittered with motion vectors at the render resolution and resolved
//! at the output resolution. Reports the median GPU time of the forward pass and of the
//! resolve for plain TAA and for upsampling by 1.5× and 2× per axis, at 1080p and 1440p.
//!
//! `cargo run -p engine_upscale --release --example taau_bench [-- --catmull-rom]`
//! (`ENGINE_ADAPTER=radeon` for the integrated GPU of ref-1; `--catmull-rom` measures the
//! sharper history filter instead of the default bilinear one)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
    ViewFeatures,
};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use engine_upscale::{HistoryFilter, Taau, TaauInput};

const WARMUP: usize = 30;
const FRAMES: usize = 200;
/// Cubes per side of the field.
const FIELD: i32 = 24;

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("taau_bench"),
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
    let mut taau = Taau::new(&device, &bindless, 1).unwrap();
    if std::env::args().any(|a| a == "--catmull-rom") {
        taau.set_history_filter(HistoryFilter::CatmullRom);
    }
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 16 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let palette = [
        materials.add(MaterialDesc::solid([0.9, 0.8, 0.6], 0.0, 0.3)),
        materials.add(MaterialDesc::solid([0.3, 0.5, 0.8], 0.0, 0.6)),
        materials.add(MaterialDesc::solid([0.8, 0.3, 0.2], 1.0, 0.2)),
    ];
    let ground = materials.add(MaterialDesc::solid([0.3, 0.35, 0.4], 0.0, 0.8));
    // SAFETY: plain object creation.
    let (command_pool, cmd) = unsafe {
        let command_pool = d
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
                    .command_pool(command_pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (command_pool, cmd)
    };

    // Cubes of varied heights, turned at odd angles: slanted edges everywhere.
    let mut draws = vec![DrawItem {
        mesh,
        material: ground,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(FIELD as f32 * 2.0, 0.2, FIELD as f32 * 2.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, 0.0),
        ),
    }];
    for z in 0..FIELD {
        for x in 0..FIELD {
            let k = (x * 7 + z * 13) as f32;
            let height = 0.5 + (k * 0.37).sin().abs() * 2.5;
            draws.push(DrawItem {
                mesh,
                material: palette[((x + z) % 3) as usize],
                world_from_object: Mat4::from_scale_rotation_translation(
                    Vec3::new(0.7, height, 0.7),
                    Quat::from_rotation_y(k * 0.41),
                    Vec3::new(
                        (x - FIELD / 2) as f32 * 1.6,
                        height * 0.5,
                        (z - FIELD / 2) as f32 * 1.6,
                    ),
                ),
            });
        }
    }
    let temporal = Temporal::new(&device, &mut allocator, draws.len() as u32).unwrap();
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.2),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };

    println!(
        "### TAAU resolve ({} cubes, camera circling, HDR, shadow map 2048²)",
        draws.len() - 1
    );
    println!();
    println!("Device: {}", device.adapter().summary());
    println!();
    println!("| output | render | jitter phases | GPU forward µs | GPU taau µs |");
    println!("|---|---|---:|---:|---:|");

    let configs: [((u32, u32), (u32, u32)); 6] = [
        ((1920, 1080), (1920, 1080)),
        ((1920, 1080), (1280, 720)),
        ((1920, 1080), (960, 540)),
        ((2560, 1440), (2560, 1440)),
        ((2560, 1440), (1707, 960)),
        ((2560, 1440), (1280, 720)),
    ];
    let mut frame_number = 0u64;
    for (output, render) in configs {
        let phases = Taau::jitter_phases(render, output);
        temporal.set_jitter_phases(phases);
        temporal.reset();
        let (mut forward, mut resolve) = (Vec::new(), Vec::new());
        for frame in 0..WARMUP + FRAMES {
            // A full circle every 600 frames: about a pixel of motion per frame.
            let angle = frame_number as f32 * core::f32::consts::TAU / 600.0;
            let eye = Vec3::new(angle.cos() * 30.0, 12.0, angle.sin() * 30.0);
            let camera = Camera {
                clip_from_world: vulkan_perspective(
                    60f32.to_radians(),
                    output.0 as f32 / output.1 as f32,
                    0.5,
                    200.0,
                ) * look_at(eye, Vec3::ZERO, Vec3::Y),
                position: eye,
            };
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
            frame_number += 1;
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
                let color =
                    fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, render.0, render.1));
                let depth = fg.create_texture(
                    "depth",
                    TextureDesc::new_2d(depth_format, render.0, render.1),
                );
                let passes = renderer
                    .add_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        &geo,
                        &mats,
                        &draws,
                        &ForwardView {
                            camera,
                            light,
                            color,
                            depth,
                            clear: Some([0.02, 0.02, 0.05, 1.0]),
                            occlusion: None,
                            temporal: Some(&temporal),
                            shadow_cache: None,
                            lights: &[],
                            decals: &[],
                            environment: None,
                            reflections: Default::default(),
                            gi: None,
                            features: ViewFeatures::ALL,
                        },
                    )
                    .unwrap();
                // The resolved image is imported, so the pass is kept without a reader.
                taau.add_pass(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &bindless,
                    TaauInput {
                        color,
                        depth,
                        motion: passes.motion.unwrap(),
                    },
                    output,
                    frame_number,
                )
                .unwrap();
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
            if frame > WARMUP {
                for time in graph.gpu_pass_times() {
                    match time.name {
                        "forward" => forward.push(f64::from(time.gpu_us)),
                        "taau" => resolve.push(f64::from(time.gpu_us)),
                        _ => {}
                    }
                }
            }
        }
        println!(
            "| {}×{} | {}×{} | {phases} | {:.0} | {:.1} |",
            output.0,
            output.1,
            render.0,
            render.1,
            median(&mut forward),
            median(&mut resolve),
        );
    }

    let mut temporal = temporal;
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        taau.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(command_pool, None);
    }
}
