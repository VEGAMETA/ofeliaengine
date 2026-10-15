//! FSR 3.1 against the engine's TAAU (ADR 0060), headless, on the scene of `taau_bench`
//! (engine_upscale): a field of cubes seen by a camera circling it, drawn jittered with
//! motion vectors at the render resolution and upscaled to the output resolution.
//!
//! 1. **GPU time** (medians over 200 frames): the forward pass, TAAU's resolve, FSR, and
//!    FSR with its sharpening pass (RCAS, sharpness 0.5), at 1080p and 1440p from the
//!    native size, FSR's Quality size and half the size. Also the SDK context's creation
//!    time and GPU memory.
//! 2. **Image quality** at 1920 × 1080: PSNR of tonemapped colors (c / (1 + c)) against the
//!    same frame rendered at 2 × 2 samples per pixel and averaged, after 64 frames with the
//!    camera still and after 64 frames with it circling (about a pixel a frame).
//!
//! `cargo run -p engine_fsr --release --example upscale_bench [-- --timing | --quality]`
//! (`ENGINE_ADAPTER=radeon` for the integrated GPU of ref-1)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_fsr::{Fsr, FsrCamera, FsrInput, QualityMode};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
    ViewFeatures,
};
use engine_render::graph::{
    Access, BufferState, ImportedBuffer, RenderGraph, ShaderStages, Texture, TextureDesc,
};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};
use engine_upscale::{Taau, TaauInput};
use std::time::Instant;

const WARMUP: usize = 30;
const FRAMES: usize = 200;
/// Frames accumulated before an image is compared.
const CONVERGE: usize = 64;
/// Cubes per side of the field.
const FIELD: i32 = 24;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const FOV_Y: f32 = core::f32::consts::FRAC_PI_3; // 60°
const NEAR: f32 = 0.5;
const FAR: f32 = 200.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Upscaler {
    /// One sample per pixel, no temporal state (for the reference and the baseline).
    None,
    Taau,
    Fsr,
    FsrSharpened,
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

struct Bench {
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    renderer: ForwardRenderer,
    geometry: GeometryPool,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    taau: Taau,
    fsr: Fsr,
    temporal: Temporal,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    draws: Vec<DrawItem>,
    frame_number: u64,
    record_us: f64,
}

impl Bench {
    /// Renders one frame with the camera at `angle` (radians around the field) and returns
    /// the GPU times of the forward pass and of the upscaler, and, with `read`, the image.
    /// `self.record_us` gets the CPU time of recording the frame's commands.
    fn frame(
        &mut self,
        upscaler: Upscaler,
        render: (u32, u32),
        output: (u32, u32),
        angle: f32,
        read: bool,
    ) -> (f64, f64, Option<Vec<[f32; 3]>>) {
        let Self {
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            textures,
            materials,
            ring,
            graph,
            taau,
            fsr,
            temporal,
            cmd,
            readback,
            readback_memory,
            draws,
            frame_number,
            record_us,
            ..
        } = self;
        let d = device.raw();
        let cmd = *cmd;
        let eye = Vec3::new(angle.cos() * 30.0, 12.0, angle.sin() * 30.0);
        let camera = Camera {
            clip_from_world: vulkan_perspective(
                FOV_Y,
                output.0 as f32 / output.1 as f32,
                NEAR,
                FAR,
            ) * look_at(eye, Vec3::ZERO, Vec3::Y),
            position: eye,
        };
        let light = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
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
        *frame_number += 1;
        let bytes = u64::from(output.0 * output.1 * 8);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, allocator, bindless, *frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, device, allocator, textures, *frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, device, allocator, *frame_number)
                .unwrap();
            let color =
                fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, render.0, render.1));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, render.0, render.1));
            let passes = renderer
                .add_passes(
                    &mut fg,
                    ring,
                    bindless,
                    &geo,
                    &mats,
                    draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.02, 0.02, 0.05, 1.0]),
                        occlusion: None,
                        temporal: (upscaler != Upscaler::None).then_some(&*temporal),
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
            let exposure = engine_exposure(&mut fg);
            let result: Texture = match upscaler {
                Upscaler::None => color,
                Upscaler::Taau => taau
                    .add_pass(
                        &mut fg,
                        device,
                        allocator,
                        bindless,
                        TaauInput {
                            color,
                            depth,
                            motion: passes.motion.unwrap(),
                        },
                        output,
                        *frame_number,
                    )
                    .unwrap(),
                Upscaler::Fsr | Upscaler::FsrSharpened => {
                    fsr.set_sharpness((upscaler == Upscaler::FsrSharpened).then_some(0.5));
                    fsr.add_pass(
                        &mut fg,
                        FsrInput {
                            color,
                            depth,
                            motion: passes.motion.unwrap(),
                            camera: FsrCamera {
                                near: NEAR,
                                far: FAR,
                                fov_y: FOV_Y,
                                meters_per_unit: 1.0,
                            },
                            frame_time_ms: 1000.0 / 60.0,
                            exposure,
                        },
                        output,
                        *frame_number,
                    )
                    .unwrap()
                }
            };
            if read {
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: *readback,
                        size: bytes,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                fg.add_pass("read back")
                    .read(result, Access::TransferRead)
                    .write(out, Access::TransferWrite)
                    .execute(move |ctx| {
                        let region = vk::BufferImageCopy {
                            image_subresource: vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: 0,
                                base_array_layer: 0,
                                layer_count: 1,
                            },
                            image_extent: vk::Extent3D {
                                width: output.0,
                                height: output.1,
                                depth: 1,
                            },
                            ..Default::default()
                        };
                        // SAFETY: recording; layouts arranged by the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(result),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &[region],
                            );
                        }
                    });
            } else {
                // Stands for the tonemapper: keeps the upscaler's pass.
                fg.add_pass("consume")
                    .read(result, Access::Sampled(ShaderStages::FRAGMENT))
                    .side_effects()
                    .execute(|_| {});
            }
            let start = Instant::now();
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
            *record_us = start.elapsed().as_secs_f64() * 1e6;
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
        let (mut forward, mut upscale) = (0.0, 0.0);
        for time in graph.gpu_pass_times() {
            match time.name {
                "forward" => forward += f64::from(time.gpu_us),
                "taau" | "fsr" => upscale += f64::from(time.gpu_us),
                _ => {}
            }
        }
        let image = read.then(|| {
            let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the image.
            let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
            raw.chunks_exact(8)
                .map(|p| {
                    let h = |k: usize| {
                        let v = half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                        v / (1.0 + v)
                    };
                    [h(0), h(1), h(2)]
                })
                .collect()
        });
        (forward, upscale, image)
    }
}

/// Averages `factor × factor` blocks of a `width × height` image.
fn downsample(image: &[[f32; 3]], width: u32, height: u32, factor: u32) -> Vec<[f32; 3]> {
    let (w, h) = (width / factor, height / factor);
    let mut result = vec![[0.0f32; 3]; (w * h) as usize];
    for y in 0..height {
        for x in 0..width {
            let p = image[(y * width + x) as usize];
            let r = &mut result[((y / factor) * w + x / factor) as usize];
            for c in 0..3 {
                r[c] += p[c] / (factor * factor) as f32;
            }
        }
    }
    result
}

fn psnr(a: &[[f32; 3]], b: &[[f32; 3]]) -> f64 {
    let mut sum = 0.0f64;
    for (p, q) in a.iter().zip(b) {
        for c in 0..3 {
            let d = f64::from(p[c] - q[c]);
            sum += d * d;
        }
    }
    let mse = sum / (a.len() * 3) as f64;
    10.0 * (1.0 / mse.max(1e-12)).log10()
}

/// `ENGINE_UPSCALE_EXPOSURE=1`: give the upscaler the exposure the image is judged at (1, a
/// 1 × 1 texture, ADR 0072) instead of its own automatic exposure.
fn engine_exposure(fg: &mut engine_render::graph::FrameGraph<'_>) -> Option<Texture> {
    if !std::env::var("ENGINE_UPSCALE_EXPOSURE").is_ok_and(|v| v == "1") {
        return None;
    }
    let texture = fg.create_texture(
        "exposure",
        TextureDesc::new_2d(engine_upscale::EXPOSURE_FORMAT, 1, 1),
    );
    fg.add_pass("exposure value")
        .write(texture, Access::TransferWrite)
        .execute(move |pass| {
            // SAFETY: recording outside a rendering scope; the graph arranged the layout.
            unsafe {
                pass.device.cmd_clear_color_image(
                    pass.cmd,
                    pass.image(texture),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue {
                        float32: [1.0, 0.0, 0.0, 0.0],
                    },
                    &[vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }],
                );
            }
        });
    Some(texture)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let timing = !args.iter().any(|a| a == "--quality");
    let quality = !args.iter().any(|a| a == "--timing");
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("upscale_bench"),
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
    let taau = Taau::new(&device, &bindless, 1).unwrap();
    let fsr = Fsr::new(&device, 1);
    let ring = UploadRing::new(&device, &mut allocator, 1, 16 << 20).unwrap();
    let graph = RenderGraph::new(&device, 1);
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let palette = [
        materials.add(MaterialDesc::solid([0.9, 0.8, 0.6], 0.0, 0.3)),
        materials.add(MaterialDesc::solid([0.3, 0.5, 0.8], 0.0, 0.6)),
        materials.add(MaterialDesc::solid([0.8, 0.3, 0.2], 1.0, 0.2)),
    ];
    let ground = materials.add(MaterialDesc::solid([0.3, 0.35, 0.4], 0.0, 0.8));
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(3840 * 2160 * 8)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    let d = device.raw();
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
    let mut bench = Bench {
        device,
        allocator,
        bindless,
        renderer,
        geometry,
        textures,
        materials,
        ring,
        graph,
        taau,
        fsr,
        temporal,
        pool,
        cmd,
        readback,
        readback_memory: Some(readback_memory),
        draws,
        frame_number: 0,
        record_us: 0.0,
    };
    println!("Device: {}", bench.device.adapter().summary());
    println!(
        "16-bit shader math: {}",
        bench.device.shader_float16_enabled()
    );
    println!();

    // A full circle every 600 frames: about a pixel of motion per frame at 1080p.
    let step = core::f32::consts::TAU / 600.0;
    let prepare = |bench: &mut Bench, render: (u32, u32), output: (u32, u32)| {
        bench
            .temporal
            .set_jitter_phases(Fsr::jitter_phases(render, output));
        bench.temporal.reset();
        bench.taau.reset();
        bench.fsr.reset();
    };

    if timing {
        println!(
            "### GPU time ({} cubes, camera circling, HDR, shadow map 2048²)",
            bench.draws.len() - 1
        );
        println!();
        println!(
            "| output | render | jitter phases | forward µs | TAAU µs | FSR µs | FSR + RCAS µs | CPU recording, TAAU → FSR µs |"
        );
        println!("|---|---|---:|---:|---:|---:|---:|---:|");
        let configs: [((u32, u32), (u32, u32)); 6] = [
            ((1920, 1080), (1920, 1080)),
            ((1920, 1080), QualityMode::Quality.render_size((1920, 1080))),
            (
                (1920, 1080),
                QualityMode::Performance.render_size((1920, 1080)),
            ),
            ((2560, 1440), (2560, 1440)),
            ((2560, 1440), QualityMode::Quality.render_size((2560, 1440))),
            (
                (2560, 1440),
                QualityMode::Performance.render_size((2560, 1440)),
            ),
        ];
        for (output, render) in configs {
            let phases = Fsr::jitter_phases(render, output);
            let mut forward = Vec::new();
            let mut row = Vec::new();
            let mut cpu = Vec::new();
            for upscaler in [Upscaler::Taau, Upscaler::Fsr, Upscaler::FsrSharpened] {
                prepare(&mut bench, render, output);
                let mut times = Vec::new();
                let mut record = Vec::new();
                for frame in 0..WARMUP + FRAMES {
                    let angle = frame as f32 * step;
                    let (f, u, _) = bench.frame(upscaler, render, output, angle, false);
                    if frame >= WARMUP {
                        forward.push(f);
                        times.push(u);
                        record.push(bench.record_us);
                    }
                }
                row.push(median(&mut times));
                cpu.push(median(&mut record));
            }
            println!(
                "| {}×{} | {}×{} | {phases} | {:.0} | {:.1} | {:.1} | {:.1} | {:.0} → {:.0} |",
                output.0,
                output.1,
                render.0,
                render.1,
                median(&mut forward),
                row[0],
                row[1],
                row[2],
                cpu[0],
                cpu[1],
            );
        }
        println!();
        // Creating an SDK context: at the first frame and after a change of output size.
        let mut creation = Vec::new();
        for size in [(1920, 1080), (2560, 1440), (1920, 1080), (2560, 1440)] {
            let render = QualityMode::Quality.render_size(size);
            prepare(&mut bench, render, size);
            let start = Instant::now();
            bench.frame(Upscaler::Fsr, render, size, 0.0, false);
            creation.push(start.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "FSR context creation + first frame: {:.1} ms (median of {}, includes the forward pass and a GPU wait)",
            median(&mut creation.clone()),
            creation.len()
        );
        println!(
            "FSR context GPU memory at 2560×1440 from Quality: {:.1} MiB",
            bench.fsr.gpu_memory() as f64 / (1024.0 * 1024.0)
        );
        println!();
    }

    if quality {
        let output = (1920, 1080);
        println!("### Image quality at 1920×1080: PSNR (dB) against 2×2 supersampling");
        println!();
        let reference = |bench: &mut Bench, angle: f32| {
            let (_, _, big) = bench.frame(Upscaler::None, (3840, 2160), (3840, 2160), angle, true);
            downsample(&big.unwrap(), 3840, 2160, 2)
        };
        let still_angle = 0.4;
        let truth = reference(&mut bench, still_angle);
        let (_, _, plain) = bench.frame(Upscaler::None, output, output, still_angle, true);
        let plain = plain.unwrap();
        println!(
            "one sample per pixel, 1920×1080: {:.2} (camera still)",
            psnr(&plain, &truth)
        );
        println!();
        println!("| render | upscaler | camera still | camera circling |");
        println!("|---|---|---:|---:|");
        for render in [
            output,
            QualityMode::Quality.render_size(output),
            QualityMode::Performance.render_size(output),
        ] {
            for upscaler in [Upscaler::Taau, Upscaler::Fsr, Upscaler::FsrSharpened] {
                prepare(&mut bench, render, output);
                let mut image = Vec::new();
                for _ in 0..CONVERGE {
                    image = bench
                        .frame(upscaler, render, output, still_angle, true)
                        .2
                        .unwrap();
                }
                let still = psnr(&image, &truth);
                prepare(&mut bench, render, output);
                let mut angle = still_angle;
                for _ in 0..CONVERGE {
                    angle += step;
                    image = bench
                        .frame(upscaler, render, output, angle, true)
                        .2
                        .unwrap();
                }
                let moving_truth = reference(&mut bench, angle);
                let moving = psnr(&image, &moving_truth);
                println!(
                    "| {}×{} | {} | {still:.2} | {moving:.2} |",
                    render.0,
                    render.1,
                    match upscaler {
                        Upscaler::Taau => "TAAU",
                        Upscaler::Fsr => "FSR",
                        Upscaler::FsrSharpened => "FSR + RCAS 0.5",
                        Upscaler::None => "none",
                    },
                );
            }
        }
    }

    let Bench {
        device,
        mut allocator,
        bindless,
        renderer,
        mut geometry,
        mut textures,
        mut materials,
        mut ring,
        mut graph,
        mut taau,
        mut fsr,
        mut temporal,
        pool,
        readback,
        mut readback_memory,
        ..
    } = bench;
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        taau.destroy(&mut allocator, &bindless);
        fsr.destroy();
        if let Some(memory) = readback_memory.take() {
            allocator.destroy_buffer(readback, memory);
        }
        device.raw().destroy_command_pool(pool, None);
    }
    drop(renderer);
}
