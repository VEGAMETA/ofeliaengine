//! Temporal antialiasing and upsampling (ADR 0043) against a supersampled reference,
//! under the validation layer with synchronization validation.
//!
//! The reference renders the scene at 4 × 4 samples per pixel and averages them. Compared
//! with it (PSNR of tonemapped colors):
//!
//! - TAA, converged over 16 jittered frames, must beat one sample per pixel by a clear
//!   margin;
//! - TAAU from half resolution must beat stretching a half-resolution frame;
//! - with the camera panning, TAA must stay better than no antialiasing (the history
//!   follows the motion vectors instead of smearing).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
};
use engine_render::graph::{
    Access, BufferState, ImportedBuffer, RenderGraph, Texture, TextureDesc,
};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};
use engine_upscale::{HistoryFilter, Taau, TaauInput};

const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const SIZE: u32 = 96;
const SUPERSAMPLE: u32 = 4;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_upscale TAA test"),
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

/// What a frame renders into and how.
enum Mode<'a> {
    /// One sample per pixel at `size`.
    Plain(u32),
    /// Jittered at `render` size, resolved by TAA(U) at `output` size.
    Temporal {
        render: u32,
        output: u32,
        temporal: &'a Temporal,
    },
}

struct Gpu {
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
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    frame_number: u64,
}

impl Gpu {
    /// Renders one frame of `draws` seen from `camera_x` and returns its (resolved)
    /// pixels, tonemapped to [0, 1) (c / (1 + c)).
    fn render(&mut self, draws: &[DrawItem], camera_x: f32, mode: Mode<'_>) -> Vec<[f32; 3]> {
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
            cmd,
            readback,
            readback_memory,
            frame_number,
            ..
        } = self;
        let d = device.raw();
        let cmd = *cmd;
        let (render_size, output_size, temporal) = match mode {
            Mode::Plain(size) => (size, size, None),
            Mode::Temporal {
                render,
                output,
                temporal,
            } => (render, output, Some(temporal)),
        };
        let eye = Vec3::new(camera_x, 2.0, 7.0);
        let camera = Camera {
            clip_from_world: vulkan_perspective(0.7, 1.0, 0.5, 40.0)
                * look_at(eye, Vec3::new(camera_x, 0.5, 0.0), Vec3::Y),
            position: eye,
        };
        let light = DirectionalLight {
            direction: Vec3::new(-0.4, -1.0, -0.3),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.1),
        };
        // SAFETY: one-shot recording and submission, then wait.
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
        let bytes = u64::from(output_size * output_size * 8);
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
                fg.create_texture("color", TextureDesc::new_2d(HDR, render_size, render_size));
            let depth = fg.create_texture(
                "depth",
                TextureDesc::new_2d(DEPTH, render_size, render_size),
            );
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
                        clear: Some([0.02, 0.02, 0.03, 1.0]),
                        occlusion: None,
                        temporal,
                        lights: &[],
                        environment: None,
                        reflections: Default::default(),
                    },
                )
                .unwrap();
            let result: Texture = match passes.motion {
                Some(motion) => taau
                    .add_pass(
                        &mut fg,
                        device,
                        allocator,
                        bindless,
                        TaauInput {
                            color,
                            depth,
                            motion,
                        },
                        (output_size, output_size),
                        *frame_number,
                    )
                    .unwrap(),
                None => color,
            };
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
                            width: output_size,
                            height: output_size,
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
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
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
    }
}

/// Averages `factor × factor` blocks of a `size × size` image.
fn downsample(image: &[[f32; 3]], size: u32, factor: u32) -> Vec<[f32; 3]> {
    let out = size / factor;
    let mut result = vec![[0.0f32; 3]; (out * out) as usize];
    for y in 0..size {
        for x in 0..size {
            let p = image[(y * size + x) as usize];
            let r = &mut result[((y / factor) * out + x / factor) as usize];
            for c in 0..3 {
                r[c] += p[c] / (factor * factor) as f32;
            }
        }
    }
    result
}

/// Doubles a `size × size` image with bilinear filtering (texel centers aligned).
fn stretch(image: &[[f32; 3]], size: u32) -> Vec<[f32; 3]> {
    let out = size * 2;
    let at = |x: i32, y: i32| {
        image[(y.clamp(0, size as i32 - 1) * size as i32 + x.clamp(0, size as i32 - 1)) as usize]
    };
    (0..out * out)
        .map(|i| {
            let (x, y) = ((i % out) as f32, (i / out) as f32);
            let (sx, sy) = ((x + 0.5) / 2.0 - 0.5, (y + 0.5) / 2.0 - 0.5);
            let (x0, y0) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - x0, sy - y0);
            let (x0, y0) = (x0 as i32, y0 as i32);
            core::array::from_fn(|c| {
                let top = at(x0, y0)[c] * (1.0 - fx) + at(x0 + 1, y0)[c] * fx;
                let bottom = at(x0, y0 + 1)[c] * (1.0 - fx) + at(x0 + 1, y0 + 1)[c] * fx;
                top * (1.0 - fy) + bottom * fy
            })
        })
        .collect()
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

#[test]
fn taa_converges_to_the_supersampled_image() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR,
        DEPTH,
        ShadowSettings {
            resolution: 1024,
            ..ShadowSettings::default()
        },
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let bright = materials.add(MaterialDesc::solid([0.9, 0.8, 0.6], 0.0, 0.3));
    let ground = materials.add(MaterialDesc::solid([0.3, 0.35, 0.4], 0.0, 0.8));
    let ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let graph = RenderGraph::new(&device, 1);
    let taau = Taau::new(&device, &bindless, 1).unwrap();
    let max_bytes = u64::from(SIZE * SUPERSAMPLE * SIZE * SUPERSAMPLE * 8);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(max_bytes)
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
    let temporal = Temporal::new(&device, &mut allocator, 64).unwrap();
    let mut gpu = Gpu {
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
        pool,
        cmd,
        readback,
        readback_memory: Some(readback_memory),
        frame_number: 0,
    };

    // Cubes turned at odd angles on a ground slab: long slanted edges.
    let mut draws = vec![DrawItem {
        mesh,
        material: ground,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(20.0, 0.2, 20.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, 0.0),
        ),
    }];
    for i in 0..7 {
        let a = i as f32;
        draws.push(DrawItem {
            mesh,
            material: bright,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(0.8, 1.6 + 0.3 * (a % 3.0), 0.8),
                Quat::from_rotation_y(0.3 + a * 0.47) * Quat::from_rotation_z(0.05 * a),
                Vec3::new(-3.0 + a, 0.8, -1.0 - (a % 2.0) * 1.5),
            ),
        });
    }

    let reference = |gpu: &mut Gpu, camera_x: f32| {
        let big = gpu.render(&draws, camera_x, Mode::Plain(SIZE * SUPERSAMPLE));
        downsample(&big, SIZE * SUPERSAMPLE, SUPERSAMPLE)
    };
    let truth = reference(&mut gpu, 0.0);
    let plain = gpu.render(&draws, 0.0, Mode::Plain(SIZE));

    // TAA at full resolution, converged.
    temporal.set_jitter_phases(Taau::jitter_phases((SIZE, SIZE), (SIZE, SIZE)));
    temporal.reset();
    let mut taa = Vec::new();
    for _ in 0..24 {
        taa = gpu.render(
            &draws,
            0.0,
            Mode::Temporal {
                render: SIZE,
                output: SIZE,
                temporal: &temporal,
            },
        );
    }
    // TAAU from half resolution.
    temporal.set_jitter_phases(Taau::jitter_phases((SIZE / 2, SIZE / 2), (SIZE, SIZE)));
    temporal.reset();
    let mut taau = Vec::new();
    for _ in 0..48 {
        taau = gpu.render(
            &draws,
            0.0,
            Mode::Temporal {
                render: SIZE / 2,
                output: SIZE,
                temporal: &temporal,
            },
        );
    }
    let half = gpu.render(&draws, 0.0, Mode::Plain(SIZE / 2));
    let stretched = stretch(&half, SIZE / 2);

    // The camera pans right by 0.02 units a frame (about a third of a pixel).
    temporal.set_jitter_phases(8);
    temporal.reset();
    let mut panned = Vec::new();
    let mut x = 0.0;
    for _ in 0..24 {
        x += 0.02;
        panned = gpu.render(
            &draws,
            x,
            Mode::Temporal {
                render: SIZE,
                output: SIZE,
                temporal: &temporal,
            },
        );
    }
    let panned_truth = reference(&mut gpu, x);
    let panned_plain = gpu.render(&draws, x, Mode::Plain(SIZE));

    // The same pan with the Catmull-Rom history.
    gpu.taau.set_history_filter(HistoryFilter::CatmullRom);
    temporal.reset();
    let mut sharp = Vec::new();
    let mut x = 0.0;
    for _ in 0..24 {
        x += 0.02;
        sharp = gpu.render(
            &draws,
            x,
            Mode::Temporal {
                render: SIZE,
                output: SIZE,
                temporal: &temporal,
            },
        );
    }
    gpu.taau.set_history_filter(HistoryFilter::default());

    let scores = [
        ("one sample per pixel", psnr(&plain, &truth)),
        ("TAA", psnr(&taa, &truth)),
        ("half resolution, stretched", psnr(&stretched, &truth)),
        ("TAAU from half resolution", psnr(&taau, &truth)),
        ("panning, one sample", psnr(&panned_plain, &panned_truth)),
        ("panning, TAA", psnr(&panned, &panned_truth)),
        (
            "panning, TAA, Catmull-Rom history",
            psnr(&sharp, &panned_truth),
        ),
    ];
    for (name, db) in scores {
        println!("{name}: {db:.2} dB against 4×4 supersampling");
    }
    assert!(taa.iter().flatten().all(|v| v.is_finite()));
    assert!(
        scores[1].1 > scores[0].1 + 3.0,
        "TAA antialiases: {scores:?}"
    );
    assert!(
        scores[3].1 > scores[2].1 + 2.0,
        "TAAU upsamples: {scores:?}"
    );
    assert!(
        scores[5].1 > scores[4].1 + 1.0,
        "TAA follows motion: {scores:?}"
    );
    assert!(
        scores[6].1 > scores[5].1,
        "the Catmull-Rom history is sharper in motion: {scores:?}"
    );

    let Gpu {
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
        pool,
        readback,
        mut readback_memory,
        ..
    } = gpu;
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
        if let Some(memory) = readback_memory.take() {
            allocator.destroy_buffer(readback, memory);
        }
        device.raw().destroy_command_pool(pool, None);
    }
    drop(renderer);
    drop(taau);
    drop(bindless);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
