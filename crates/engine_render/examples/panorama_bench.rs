//! HDR panoramas' cost (ADR 0083).
//!
//! - **CPU:** reading a `.hdr` file, decoding it, finding its sun; cooking it into a BC6H
//!   cube (`engine_assets::panorama`) on the job system; writing and reading its pack;
//!   making the `Panorama` (texels written for the GPU, or a cooked cube's levels kept).
//! - **GPU,** at 1920 × 1080 behind a cube, everything on the graphics queue (an async
//!   compute pass's time would include the work it overlaps), median of the timed frames,
//!   µs: the gradient sky
//!   changing every frame; a panorama turned every frame (the lighting only); a panorama
//!   made from its texels every frame (its background cube and the lighting); a cooked
//!   panorama set every frame (its upload through the ring, over as many frames as it takes,
//!   and the lighting).
//!
//! `cargo run -p engine_render --release --example panorama_bench [-- <file.hdr>]`
//! (`ENGINE_ADAPTER=radeon` for the integrated GPU; the default file is the test panorama of
//! `tools/fetch-test-assets.ps1`). Prints Markdown.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::hdr::{SunSearch, decode_hdr, find_sun};
use engine_assets::panorama::{
    PanoramaCookSettings, cook_panorama, load_panorama_pack, write_panorama_pack,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::panorama::{CookedPanoramaDesc, Panorama, PanoramaDesc, PanoramaSun};
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
/// Passes timed, in the table's order.
const PASSES: [&str; 6] = [
    "panorama background",
    "panorama upload",
    "environment sky",
    "environment prefilter",
    "environment irradiance",
    "sky",
];

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn ms(t: std::time::Duration) -> f64 {
    t.as_secs_f64() * 1e3
}

/// What each timed frame does.
#[derive(Clone, Copy, PartialEq)]
enum Case {
    Gradient,
    Turned,
    Made,
    Cooked,
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
    let jobs = ThreadPool::builder().build().unwrap();
    let start = Instant::now();
    let cooked = cook_panorama(&image, &PanoramaCookSettings::default(), Some(&jobs));
    let cook_time = start.elapsed();
    let pack = std::env::temp_dir().join(format!("panorama_bench_{}.pack", std::process::id()));
    let start = Instant::now();
    let stats = write_panorama_pack(&pack, &cooked, 3).unwrap();
    let pack_written = start.elapsed();
    let start = Instant::now();
    let loaded = load_panorama_pack(&pack, &jobs).unwrap();
    let pack_read = start.elapsed();
    assert_eq!(loaded, cooked);
    let _ = std::fs::remove_file(&pack);

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
    // One queue: an async pass's time would include the graphics work it overlaps.
    graph.set_async_compute(false);
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
    let sun = found.map(|s| PanoramaSun {
        direction: s.direction,
        illuminance: s.illuminance,
        cone: s.cone,
        sky: s.sky,
    });
    let mut made_time = std::time::Duration::ZERO;
    let mut make = |allocator: &mut Allocator| {
        let start = Instant::now();
        let p = Panorama::new(
            &device,
            allocator,
            &bindless,
            &PanoramaDesc {
                width: image.width,
                height: image.height,
                pixels: &image.pixels,
                sun,
                background_size: 0,
            },
        )
        .unwrap();
        made_time = start.elapsed();
        p
    };
    let mut cooked_time = std::time::Duration::ZERO;
    let mut from_cooked = |allocator: &mut Allocator| {
        let start = Instant::now();
        let p = Panorama::from_cooked(
            &device,
            allocator,
            &bindless,
            &CookedPanoramaDesc {
                size: cooked.size,
                levels: &cooked.levels,
                sun,
            },
        )
        .unwrap();
        cooked_time = start.elapsed();
        p
    };
    let first = make(&mut allocator);
    let made_memory = first.memory();
    let made_size = first.background_size();
    let cooked_memory = {
        let mut p = from_cooked(&mut allocator);
        let memory = p.memory();
        // SAFETY: never used by the GPU.
        unsafe { p.destroy(&mut allocator, &bindless) };
        memory
    };
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
        "{} ({} × {}, {:.1} MB): read {:.1} ms, decoded {:.1} ms, sun found in {:.1} ms ({}); \
         cooked into a {}² BC6H cube in {:.0} ms on {} workers ({:.2} MiB; its pack {:.2} MiB, \
         written in {:.1} ms, read in {:.1} ms)",
        path.file_name().unwrap().to_string_lossy(),
        image.width,
        image.height,
        bytes.len() as f64 / 1e6,
        ms(read),
        ms(decoded),
        ms(searched),
        found.map_or(String::from("none"), |s| format!(
            "{:.2}° up, cone {:.2}°",
            s.direction.y.asin().to_degrees(),
            s.cone.to_degrees()
        )),
        cooked.size,
        ms(cook_time),
        jobs.num_workers(),
        cooked.bytes() as f64 / f64::from(1 << 20),
        stats.stored_bytes as f64 / f64::from(1 << 20),
        ms(pack_written),
        ms(pack_read),
    );
    println!();
    println!(
        "| sky | background cube µs | upload µs (frames) | lighting cube µs | prefilter µs | irradiance µs | sky pass µs |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    let mut current = Some(first);
    for case in [Case::Gradient, Case::Turned, Case::Made, Case::Cooked] {
        let mut times: [Vec<f64>; 6] = Default::default();
        let mut upload_frames = Vec::new();
        let mut frame = 0usize;
        let mut sample = 0usize;
        while sample < TIMED + 2 {
            // What this frame changes.
            let mut fresh = false;
            match case {
                Case::Gradient => environment.set_sky(Sky {
                    zenith: Vec3::new(0.1, 0.2, 0.5 + frame as f32 * 1e-3),
                    ..Sky::default()
                }),
                Case::Turned => {
                    if environment.panorama().is_none() {
                        environment.set_panorama(current.take());
                    }
                    environment
                        .panorama()
                        .unwrap()
                        .set_rotation(frame as f32 * 0.1);
                }
                Case::Made | Case::Cooked => {
                    if environment.panorama().is_none_or(Panorama::is_ready) {
                        let next = if case == Case::Made {
                            make(&mut allocator)
                        } else {
                            from_cooked(&mut allocator)
                        };
                        if let Some(mut old) = environment.set_panorama(Some(next)) {
                            // SAFETY: the device is idle between frames here.
                            unsafe { old.destroy(&mut allocator, &bindless) };
                        }
                        fresh = true;
                    }
                }
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
            let number = frame as u64 + 1;
            {
                // SAFETY: the previous frame finished.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, number)
                    .unwrap();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, number)
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
            frame += 1;
            if fresh && case == Case::Cooked {
                upload_frames.push(0);
            }
            let mut upload_us = 0.0;
            for time in graph.gpu_pass_times() {
                let Some(slot) = PASSES.iter().position(|&p| p == time.name) else {
                    continue;
                };
                if slot == 1 {
                    upload_us += f64::from(time.gpu_us);
                } else if sample >= 2 {
                    times[slot].push(f64::from(time.gpu_us));
                }
            }
            if upload_us > 0.0 {
                if let Some(n) = upload_frames.last_mut() {
                    *n += 1;
                }
                if sample >= 2 {
                    times[1].push(upload_us);
                }
            }
            // A cooked panorama's sample is the frames until it is ready, together.
            let ready = environment.panorama().is_none_or(Panorama::is_ready);
            if case != Case::Cooked || ready {
                sample += 1;
            }
        }
        let [background, upload, light, prefilter, irradiance, sky] = times.map(median);
        let cell = |v: f64| {
            if v.is_nan() {
                String::from("–")
            } else {
                format!("{v:.1}")
            }
        };
        let upload = if upload.is_nan() {
            String::from("–")
        } else {
            format!(
                "{upload:.1} per frame ({} frames)",
                median(upload_frames.iter().map(|&n| f64::from(n)).collect())
            )
        };
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            match case {
                Case::Gradient => "gradient, changed",
                Case::Turned => "panorama, turned",
                Case::Made => "panorama, made from texels",
                Case::Cooked => "panorama, cooked and uploaded",
            },
            cell(background),
            upload,
            cell(light),
            cell(prefilter),
            cell(irradiance),
            cell(sky)
        );
    }
    println!();
    println!(
        "`Panorama::new` {:.1} ms ({made_size}² RGBA16F background with its texels: {:.1} MiB); \
         `Panorama::from_cooked` {:.1} ms ({:.1} MiB)",
        ms(made_time),
        made_memory as f64 / f64::from(1 << 20),
        ms(cooked_time),
        cooked_memory as f64 / f64::from(1 << 20),
    );

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
