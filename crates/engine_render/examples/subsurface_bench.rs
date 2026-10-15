//! Subsurface scattering's cost (ADR 0059): a skin-profile panel covering 10%, 50% or
//! 100% of a 1920×1080 HDR target in front of an ordinary wall, at every sample budget,
//! with the profile at its real size (skin a metre away) and four times larger. Prints the
//! median GPU time of the passes: the depth prepass (forced by subsurface materials), the
//! forward pass, the forward subsurface pass and the scatter pass; the first rows draw the
//! panel as an ordinary material, with and without the prepass.
//!
//! `cargo run -p engine_render --release --example subsurface_bench` (`ENGINE_ADAPTER=radeon`
//! for the integrated GPU).

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable, Subsurface, Transmission};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::subsurface::{SubsurfaceProfile, SubsurfaceQuality, SubsurfaceSettings};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 20;
const FRAMES: usize = 120;

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("subsurface_bench"),
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
    let mut renderer = ForwardRenderer::new(
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
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
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

    let skin = [0.85, 0.62, 0.52];
    let plain = materials.add(MaterialDesc::solid(skin, 0.0, 0.5));
    let scattering = materials.add(MaterialDesc {
        subsurface: Some(Subsurface {
            transmission: Transmission::Thick,
            thickness: 0.005,
            ..Subsurface::default()
        }),
        ..MaterialDesc::solid(skin, 0.0, 0.5)
    });
    let wall = materials.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.7));
    let eye = Vec3::new(0.0, 0.0, 1.0);
    let fov = 60f32.to_radians();
    let camera = Camera {
        clip_from_world: vulkan_perspective(fov, WIDTH as f32 / HEIGHT as f32, 0.05, 50.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.3, -0.5, -1.0).normalize(),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };
    let half_height = (fov * 0.5).tan();
    let visible_width = 2.0 * half_height * WIDTH as f32 / HEIGHT as f32;
    let pixels_per_metre = HEIGHT as f32 * 0.5 / half_height;

    println!("### Subsurface scattering, {WIDTH}×{HEIGHT} HDR");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!();
    println!(
        "| panel | profile | budget | GPU prepass µs | GPU forward µs | GPU forward subsurface µs | GPU scatter µs | sum µs |"
    );
    println!("|---|---|---|---:|---:|---:|---:|---:|");
    let mut frame_number = 0u64;
    for coverage in [0.1f32, 0.5, 1.0] {
        let draws_with = |material| {
            [
                DrawItem {
                    mesh,
                    material: wall,
                    world_from_object: Mat4::from_scale_rotation_translation(
                        Vec3::new(10.0, 10.0, 0.02),
                        Quat::IDENTITY,
                        Vec3::new(0.0, 0.0, -0.3),
                    ),
                },
                DrawItem {
                    mesh,
                    material,
                    world_from_object: Mat4::from_scale_rotation_translation(
                        Vec3::new(coverage * visible_width, 2.5 * half_height, 0.02),
                        Quat::IDENTITY,
                        Vec3::ZERO,
                    ),
                },
            ]
        };
        // (label, subsurface material, prepass, profile scale, quality)
        let mut configs: Vec<(String, bool, bool, f32, SubsurfaceQuality)> = vec![
            (
                "none (ordinary)".into(),
                false,
                false,
                1.0,
                SubsurfaceQuality::Low,
            ),
            (
                "none, prepass".into(),
                false,
                true,
                1.0,
                SubsurfaceQuality::Low,
            ),
        ];
        for scale in [1.0, 4.0] {
            for quality in [
                SubsurfaceQuality::Low,
                SubsurfaceQuality::Medium,
                SubsurfaceQuality::High,
                SubsurfaceQuality::Ultra,
            ] {
                let profile = SubsurfaceProfile::skin().with_scale(0.001 * scale);
                let reach = profile.sample_radius(0.99) * pixels_per_metre;
                configs.push((
                    format!("skin ×{scale} (99% within {reach:.0} px)"),
                    true,
                    false,
                    scale,
                    quality,
                ));
            }
        }
        for (label, subsurface, prepass, scale, quality) in configs {
            renderer
                .set_subsurface_profiles(
                    &device,
                    &bindless,
                    &[SubsurfaceProfile::skin().with_scale(0.001 * scale)],
                )
                .unwrap();
            renderer.set_subsurface(Some(SubsurfaceSettings { quality }));
            renderer.set_depth_prepass(prepass);
            let draws = draws_with(if subsurface { scattering } else { plain });
            let mut times: [Vec<f64>; 4] = Default::default();
            for frame in 0..WARMUP + FRAMES {
                frame_number += 1;
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
                {
                    // SAFETY: nothing is in flight.
                    let mut fg = unsafe { graph.begin_slot(0, cmd) };
                    let geo = geometry
                        .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                        .unwrap();
                    textures
                        .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                        .unwrap();
                    let mats = materials
                        .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                        .unwrap();
                    let hdr =
                        fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                    let depth = fg
                        .create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
                    renderer
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
                                color: hdr,
                                depth,
                                clear: Some([0.0, 0.0, 0.0, 1.0]),
                                occlusion: None,
                                temporal: None,
                                shadow_cache: None,
                                virtual_shadows: None,
                                lights: &[],
                                decals: &[],
                                environment: None,
                                reflections: Default::default(),
                                gi: None,
                                features: ViewFeatures::ALL,
                            },
                        )
                        .unwrap();
                    fg.add_pass("consume")
                        .read(hdr, Access::TransferRead)
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
                if frame < WARMUP {
                    continue;
                }
                let mut frame_times = [0.0f64; 4];
                for time in graph.gpu_pass_times() {
                    let slot = match time.name {
                        "depth prepass" => 0,
                        "forward" => 1,
                        "forward subsurface" => 2,
                        "subsurface scatter" => 3,
                        _ => continue,
                    };
                    frame_times[slot] += f64::from(time.gpu_us);
                }
                for (list, t) in times.iter_mut().zip(frame_times) {
                    list.push(t);
                }
            }
            let medians: Vec<f64> = times.iter_mut().map(|t| median(t)).collect();
            let budget = if subsurface {
                quality.samples().to_string()
            } else {
                "-".into()
            };
            println!(
                "| {:.0}% | {label} | {budget} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                coverage * 100.0,
                medians[0],
                medians[1],
                medians[2],
                medians[3],
                medians.iter().sum::<f64>(),
            );
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
    drop(renderer);
}
