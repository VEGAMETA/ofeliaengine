//! DLSS Super Resolution against the engine's TAAU and FSR 3.1 (ADR 0063), all three
//! through the `Upscaler` interface, headless, on the scene of FSR's `upscale_bench`: a
//! field of cubes seen by a camera circling it, drawn jittered with motion vectors at the
//! render resolution and upscaled to the output resolution.
//!
//! 1. **GPU time** (medians over 200 frames): the forward pass, TAAU's resolve, FSR and
//!    DLSS, at 1080p and 1440p from the native size (DLAA), the Quality size and half the
//!    size; the CPU time of recording the frame. Also DLSS's feature creation time
//!    (Streamline, NGX) and its GPU memory.
//! 2. **Image quality** at 1920 × 1080: PSNR of tonemapped colors (c / (1 + c)) against the
//!    same frame rendered at 2 × 2 samples per pixel and averaged, after 64 frames with the
//!    camera still and after 64 frames with it circling (about a pixel a frame).
//!
//! Needs the Streamline DLLs (`ENGINE_STREAMLINE_DIR`) and an NVIDIA RTX GPU for DLSS;
//! without them it measures TAAU and FSR only.
//!
//! `cargo run -p engine_dlss --release --example dlss_bench [-- --timing | --quality]`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_dlss::{Dlss, DlssPreset, Streamline, StreamlineDesc};
use engine_fsr::Fsr;
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
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
use engine_upscale::{Quality, Taau, UpscaleCamera, UpscaleContext, UpscaleInput, Upscaler};
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
enum Which {
    /// One sample per pixel, no temporal state (for the reference and the baseline).
    None,
    Taau,
    Fsr,
    Dlss,
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
    dlss: Option<Dlss>,
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
        which: Which,
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
            dlss,
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
                        temporal: (which != Which::None).then_some(&*temporal),
                        lights: &[],
                        environment: None,
                    },
                )
                .unwrap();
            let result: Texture = match which {
                Which::None => color,
                _ => {
                    let upscaler: &mut dyn Upscaler = match which {
                        Which::Taau => taau,
                        Which::Fsr => fsr,
                        _ => dlss.as_mut().unwrap(),
                    };
                    upscaler
                        .add_pass(
                            &mut fg,
                            UpscaleContext {
                                device,
                                allocator,
                                bindless,
                            },
                            UpscaleInput {
                                color,
                                depth,
                                motion: passes.motion.unwrap(),
                                camera: UpscaleCamera {
                                    near: NEAR,
                                    far: FAR,
                                    fov_y: FOV_Y,
                                },
                                frame_time_ms: 1000.0 / 60.0,
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
                "taau" | "fsr" | "dlss" | "dlss output usage" => {
                    upscale += f64::from(time.gpu_us);
                }
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let timing = !args.iter().any(|a| a == "--quality");
    let quality = !args.iter().any(|a| a == "--timing");
    // Streamline first: it must be initialized before the instance and device exist.
    let load = Instant::now();
    let streamline = Streamline::load(&StreamlineDesc::default())
        .map_err(|e| println!("DLSS unavailable: {e}"))
        .ok();
    if streamline.is_some() {
        println!(
            "Streamline loaded (signature check, slInit) in {:.1} ms",
            load.elapsed().as_secs_f64() * 1e3
        );
    }
    let instance = Instance::with_extensions(
        &InstanceDesc {
            app_name: String::from("dlss_bench"),
            validation: false,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        },
        &streamline
            .as_ref()
            .map(Streamline::instance_extensions)
            .unwrap_or_default(),
    )
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let streamline = streamline.filter(|s| match s.check_adapter(&instance, &adapter) {
        Ok(()) => true,
        Err(e) => {
            println!("DLSS unsupported on {}: {e}", adapter.name());
            false
        }
    });
    let device = Device::with_extensions(
        &instance,
        &adapter,
        &streamline
            .as_ref()
            .map(Streamline::device_extensions)
            .unwrap_or_default(),
    )
    .unwrap();
    let attach = Instant::now();
    if let Some(streamline) = &streamline {
        streamline.attach(&device).unwrap();
        println!(
            "Streamline attached (NGX initialized) in {:.1} ms; DLSS {:?}",
            attach.elapsed().as_secs_f64() * 1e3,
            streamline.versions()
        );
    }
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
    let dlss = streamline.as_ref().map(|s| Dlss::new(s, 1).unwrap());
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
        dlss,
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
    let prepare = |bench: &mut Bench, which: Which, render: (u32, u32), output: (u32, u32)| {
        let phases = match which {
            Which::Fsr => Fsr::jitter_phases(render, output),
            _ => Dlss::jitter_phases(render, output),
        };
        bench.temporal.set_jitter_phases(phases);
        bench.temporal.reset();
        bench.taau.reset();
        bench.fsr.reset();
        if let Some(dlss) = &mut bench.dlss {
            dlss.reset();
        }
    };
    let mut kinds = vec![Which::Taau, Which::Fsr];
    if bench.dlss.is_some() {
        kinds.push(Which::Dlss);
    }

    if timing {
        println!(
            "### GPU time ({} cubes, camera circling, HDR, shadow map 2048²)",
            bench.draws.len() - 1
        );
        println!();
        println!(
            "| output | render | forward µs | TAAU µs | FSR µs | DLSS µs | CPU recording TAAU / FSR / DLSS µs |"
        );
        println!("|---|---|---:|---:|---:|---:|---:|");
        let configs: [((u32, u32), (u32, u32)); 6] = [
            ((1920, 1080), (1920, 1080)),
            ((1920, 1080), Quality::Quality.render_size((1920, 1080))),
            ((1920, 1080), Quality::Performance.render_size((1920, 1080))),
            ((2560, 1440), (2560, 1440)),
            ((2560, 1440), Quality::Quality.render_size((2560, 1440))),
            ((2560, 1440), Quality::Performance.render_size((2560, 1440))),
        ];
        for (output, render) in configs {
            let mut forward = Vec::new();
            let mut row = [f64::NAN; 3];
            let mut cpu = [f64::NAN; 3];
            for (k, &which) in kinds.iter().enumerate() {
                prepare(&mut bench, which, render, output);
                let mut times = Vec::new();
                let mut record = Vec::new();
                for frame in 0..WARMUP + FRAMES {
                    let angle = frame as f32 * step;
                    let (f, u, _) = bench.frame(which, render, output, angle, false);
                    if frame >= WARMUP {
                        forward.push(f);
                        times.push(u);
                        record.push(bench.record_us);
                    }
                }
                row[k] = median(&mut times);
                cpu[k] = median(&mut record);
            }
            println!(
                "| {}×{} | {}×{} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} / {:.0} / {:.0} |",
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
                cpu[2],
            );
        }
        println!();
        if bench.dlss.is_some() {
            // A DLSS feature is created at the first frame and after a change of output
            // size or mode: time that frame (CPU, including the forward pass and a wait).
            let mut creation = Vec::new();
            let mut steady = Vec::new();
            for size in [(1920, 1080), (2560, 1440), (1920, 1080), (2560, 1440)] {
                let render = Quality::Quality.render_size(size);
                prepare(&mut bench, Which::Dlss, render, size);
                let start = Instant::now();
                bench.frame(Which::Dlss, render, size, 0.0, false);
                creation.push(start.elapsed().as_secs_f64() * 1e3);
                let start = Instant::now();
                bench.frame(Which::Dlss, render, size, 0.0, false);
                steady.push(start.elapsed().as_secs_f64() * 1e3);
            }
            println!(
                "DLSS feature creation + first frame: {:.1} ms (median of {}; the next frame {:.1} ms)",
                median(&mut creation.clone()),
                creation.len(),
                median(&mut steady),
            );
            println!();

            // DLSS's network per quality level (NVIDIA's defaults: K for DLAA, Quality and
            // Balanced, M for Performance, L for Ultra Performance).
            println!("### DLSS presets, GPU µs at 1920×1080");
            println!();
            println!("| render | default | J | K | L | M |");
            println!("|---|---:|---:|---:|---:|---:|");
            let output = (1920, 1080);
            for quality in [Quality::Native, Quality::Quality, Quality::Performance] {
                let render = quality.render_size(output);
                let mut row = Vec::new();
                for preset in [
                    DlssPreset::Default,
                    DlssPreset::J,
                    DlssPreset::K,
                    DlssPreset::L,
                    DlssPreset::M,
                ] {
                    bench.dlss.as_mut().unwrap().set_preset(preset);
                    prepare(&mut bench, Which::Dlss, render, output);
                    let mut times = Vec::new();
                    for frame in 0..WARMUP + FRAMES / 2 {
                        let (_, u, _) =
                            bench.frame(Which::Dlss, render, output, frame as f32 * step, false);
                        if frame >= WARMUP {
                            times.push(u);
                        }
                    }
                    row.push(median(&mut times));
                }
                bench.dlss.as_mut().unwrap().set_preset(DlssPreset::Default);
                println!(
                    "| {}×{} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} |",
                    render.0, render.1, row[0], row[1], row[2], row[3], row[4]
                );
            }
            println!();

            // GPU memory: the process's device memory as the OS counts it
            // (`VK_EXT_memory_budget`) before and after DLSS's first frames, with the
            // graph's pools already warm at that size (TAAU frames first) and every DLSS
            // feature freed. It includes DLSS's output image, a graph texture.
            let usage = |bench: &Bench| -> u64 {
                bench
                    .allocator
                    .heap_stats()
                    .iter()
                    .filter_map(|h| h.usage)
                    .sum()
            };
            println!("### DLSS GPU memory");
            println!();
            println!(
                "| output | render | measured MiB | of which the output image MiB | NGX's estimate MiB |"
            );
            println!("|---|---|---:|---:|---:|");
            for (output, quality) in [
                ((1920, 1080), Quality::Native),
                ((1920, 1080), Quality::Quality),
                ((1920, 1080), Quality::Performance),
                ((2560, 1440), Quality::Quality),
                ((3840, 2160), Quality::Performance),
            ] {
                let render = quality.render_size(output);
                prepare(&mut bench, Which::Taau, render, output);
                bench.frame(Which::Taau, render, output, 0.0, false);
                bench.frame(Which::Taau, render, output, 0.0, false);
                // SAFETY: the device is idle after every frame.
                unsafe { bench.dlss.as_mut().unwrap().destroy() };
                let before = usage(&bench);
                prepare(&mut bench, Which::Dlss, render, output);
                bench.frame(Which::Dlss, render, output, 0.0, false);
                bench.frame(Which::Dlss, render, output, 0.0, false);
                let after = usage(&bench);
                let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
                println!(
                    "| {}×{} | {}×{} | {:.1} | {:.1} | {:.1} |",
                    output.0,
                    output.1,
                    render.0,
                    render.1,
                    mib(after.saturating_sub(before)),
                    mib(u64::from(output.0 * output.1 * 8)),
                    mib(bench.dlss.as_ref().unwrap().gpu_memory())
                );
            }
            println!();
        }
    }

    if quality {
        let output = (1920, 1080);
        println!("### Image quality at 1920×1080: PSNR (dB) against 2×2 supersampling");
        println!();
        let reference = |bench: &mut Bench, angle: f32| {
            let (_, _, big) = bench.frame(Which::None, (3840, 2160), (3840, 2160), angle, true);
            downsample(&big.unwrap(), 3840, 2160, 2)
        };
        let still_angle = 0.4;
        let truth = reference(&mut bench, still_angle);
        let (_, _, plain) = bench.frame(Which::None, output, output, still_angle, true);
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
            Quality::Quality.render_size(output),
            Quality::Performance.render_size(output),
        ] {
            for &which in &kinds {
                prepare(&mut bench, which, render, output);
                let mut image = Vec::new();
                for _ in 0..CONVERGE {
                    image = bench
                        .frame(which, render, output, still_angle, true)
                        .2
                        .unwrap();
                }
                let still = psnr(&image, &truth);
                prepare(&mut bench, which, render, output);
                let mut angle = still_angle;
                for _ in 0..CONVERGE {
                    angle += step;
                    image = bench.frame(which, render, output, angle, true).2.unwrap();
                }
                let moving_truth = reference(&mut bench, angle);
                let moving = psnr(&image, &moving_truth);
                println!(
                    "| {}×{} | {} | {still:.2} | {moving:.2} |",
                    render.0,
                    render.1,
                    match which {
                        Which::Taau => "TAAU",
                        Which::Fsr => "FSR",
                        Which::Dlss => "DLSS",
                        Which::None => "none",
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
        mut dlss,
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
        if let Some(dlss) = &mut dlss {
            dlss.destroy();
        }
        if let Some(memory) = readback_memory.take() {
            allocator.destroy_buffer(readback, memory);
        }
        device.raw().destroy_command_pool(pool, None);
    }
    drop(dlss);
    drop(streamline);
    drop(renderer);
}
