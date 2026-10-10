//! HDR panoramas' cost (ADR 0083): on the CPU, reading a `.hdr` file, decoding it, finding
//! its sun and making the `Panorama` (its texels written for the GPU, its cube and
//! pipelines); on the GPU, making the background cube and rebuilding the image-based light
//! from it (each timed frame turns the panorama, so both run every frame), and drawing the
//! background at 1920 × 1080 behind a cube, against the gradient sky's. Median of the timed
//! frames, µs.
//!
//! `cargo run -p engine_render --release --example panorama_bench [-- <file.hdr>]`
//! (`ENGINE_ADAPTER=radeon` for the integrated GPU; the default file is the test panorama of
//! `tools/fetch-test-assets.ps1`). Prints a Markdown table.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::hdr::{SunSearch, decode_hdr, find_sun};
use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::panorama::{Panorama, PanoramaDesc, PanoramaSun};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use std::path::PathBuf;
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const TIMED: usize = 30;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[allow(clippy::too_many_lines)]
fn main() {
    let path = std::env::args_os().nth(1).map_or_else(
        || {
            std::env::var_os("ENGINE_TEST_ASSETS")
                .map_or_else(|| PathBuf::from("test-assets"), PathBuf::from)
                .join("HDRI/kloofendal_48d_partly_cloudy_puresky_2k.hdr")
        },
        PathBuf::from,
    );
    let start = Instant::now();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let read = start.elapsed();
    let start = Instant::now();
    let image = decode_hdr(&bytes).unwrap();
    let decoded = start.elapsed();
    let start = Instant::now();
    let found = find_sun(&image, &SunSearch::default());
    let searched = start.elapsed();

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("panorama_bench"),
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
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        DEPTH,
        ShadowSettings::default(),
    )
    .unwrap();
    device.wait_for_pipeline_warm_up();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let material = materials.add(MaterialDesc::solid([0.8; 3], 0.0, 0.5));
    let draws = [DrawItem {
        mesh,
        material,
        world_from_object: Mat4::IDENTITY,
    }];
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
    let mut environment =
        Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
    let start = Instant::now();
    let panorama = Panorama::new(
        &device,
        &mut allocator,
        &bindless,
        &PanoramaDesc {
            width: image.width,
            height: image.height,
            pixels: &image.pixels,
            sun: found.map(|s| PanoramaSun {
                direction: s.direction,
                illuminance: s.illuminance,
                cone: s.cone,
                sky: s.sky,
            }),
            background_size: 0,
        },
    )
    .unwrap();
    let made = start.elapsed();
    let eye = Vec3::new(0.0, 1.0, 4.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 100.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.2),
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };

    println!("### HDR panoramas");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!();
    println!(
        "{} ({} × {}, {:.1} MB): read {:.1} ms, decoded {:.1} ms, sun found in {:.1} ms ({}), \
         `Panorama::new` {:.1} ms; background {}², {:.1} MiB on the GPU with the texels",
        path.file_name().unwrap().to_string_lossy(),
        image.width,
        image.height,
        bytes.len() as f64 / 1e6,
        read.as_secs_f64() * 1e3,
        decoded.as_secs_f64() * 1e3,
        searched.as_secs_f64() * 1e3,
        found.map_or(String::from("none"), |s| format!(
            "{:.2}° up, cone {:.2}°",
            s.direction.y.asin().to_degrees(),
            s.cone.to_degrees()
        )),
        made.as_secs_f64() * 1e3,
        panorama.background_size(),
        panorama.memory() as f64 / f64::from(1 << 20),
    );
    println!();
    println!(
        "| sky | background cube µs | lighting cube µs | prefilter µs | irradiance µs | sky pass µs |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    let mut panorama = Some(panorama);
    for with_panorama in [false, true] {
        if with_panorama {
            environment.set_panorama(panorama.take());
        }
        let mut times: [Vec<f64>; 5] = Default::default();
        for frame in 0..TIMED + 2 {
            // Every frame rebuilds: the gradient changes, the panorama turns.
            match environment.panorama() {
                Some(p) => p.set_rotation(frame as f32 * 0.1),
                None => environment.set_sky(Sky {
                    zenith: Vec3::new(0.1, 0.2, 0.5 + frame as f32 * 1e-3),
                    ..Sky::default()
                }),
            }
            ring.begin_slot(0);
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
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame as u64 + 1)
                    .unwrap();
                let mats = materials
                    .begin_frame(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &textures,
                        frame as u64 + 1,
                    )
                    .unwrap();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame as u64 + 1)
                    .unwrap();
                let color =
                    fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
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
                            color,
                            depth,
                            clear: Some([0.0; 4]),
                            occlusion: None,
                            temporal: None,
                            lights: &[],
                            environment: Some(&environment),
                            reflections: Default::default(),
                        },
                    )
                    .unwrap();
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
            if frame >= 2 {
                for time in graph.gpu_pass_times() {
                    let slot = match time.name {
                        "panorama background" => 0,
                        "environment sky" => 1,
                        "environment prefilter" => 2,
                        "environment irradiance" => 3,
                        "sky" => 4,
                        _ => continue,
                    };
                    times[slot].push(f64::from(time.gpu_us));
                }
            }
        }
        let [background, light, prefilter, irradiance, sky] = times.map(median);
        let cell = |v: f64| {
            if v.is_nan() {
                String::from("–")
            } else {
                format!("{v:.1}")
            }
        };
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            if with_panorama {
                "panorama"
            } else {
                "gradient"
            },
            cell(background),
            cell(light),
            cell(prefilter),
            cell(irradiance),
            cell(sky)
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        environment.destroy(&mut allocator, &bindless);
        ring.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
}
