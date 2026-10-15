//! FSR 3.1 upscaling (ADR 0060) against a supersampled reference and against the engine's
//! own TAAU, under the validation layer with synchronization validation.
//!
//! The scene and the reference are those of `engine_upscale/tests/taa.rs`: slanted cubes
//! on a ground slab, rendered at 4 × 4 samples per pixel and averaged. Compared with it
//! (PSNR of tonemapped colors), FSR must:
//!
//! - antialias at the native size (converged over 24 jittered frames) clearly better than
//!   one sample per pixel;
//! - upscale from half the size clearly better than stretching a half-size frame;
//! - stay better than no antialiasing with the camera panning (it follows the motion
//!   vectors).
//!
//! It also checks the output's size and format, a change of output size (a new SDK
//! context), that the SDK reports no warnings with its debug checking on, and that the
//! engine's jitter sequence is the SDK's.
//!
//! `framegen` tests FSR 3.1's frame interpolation (ADR 0091) on the same scene.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod framegen;

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_fsr::{Fsr, FsrCamera, FsrInput, OUTPUT_FORMAT, QualityMode};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
    ViewFeatures,
};
use engine_render::graph::{
    Access, BufferState, ImportedBuffer, RenderGraph, Texture, TextureDesc,
};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::texture_pool::{TextureFormat, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};
use engine_upscale::{Taau, TaauInput};

const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const SIZE: u32 = 96;
const SUPERSAMPLE: u32 = 4;
const FOV_Y: f32 = 0.7;
const NEAR: f32 = 0.5;
const FAR: f32 = 40.0;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_fsr test"),
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

/// Who resolves a jittered frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resolver {
    Taau,
    Fsr,
}

/// What a frame renders into and how.
enum Mode<'a> {
    /// One sample per pixel at `size`.
    Plain(u32),
    /// Jittered at `render` size, resolved at `output` size.
    Temporal {
        render: u32,
        output: u32,
        temporal: &'a Temporal,
        resolver: Resolver,
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
    fsr: Fsr,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    frame_number: u64,
    /// The jitter of the last temporal frame.
    last_jitter: [f32; 2],
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
            fsr,
            cmd,
            readback,
            readback_memory,
            frame_number,
            last_jitter,
            ..
        } = self;
        let d = device.raw();
        let cmd = *cmd;
        let (render_size, output_size, temporal, resolver) = match mode {
            Mode::Plain(size) => (size, size, None, Resolver::Taau),
            Mode::Temporal {
                render,
                output,
                temporal,
                resolver,
            } => (render, output, Some(temporal), resolver),
        };
        let eye = Vec3::new(camera_x, 2.0, 7.0);
        let camera = Camera {
            clip_from_world: vulkan_perspective(FOV_Y, 1.0, NEAR, FAR)
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
            let result: Texture = match (passes.motion, resolver) {
                (Some(motion), Resolver::Taau) => {
                    *last_jitter = motion.jitter;
                    taau.add_pass(
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
                    .unwrap()
                }
                (Some(motion), Resolver::Fsr) => {
                    *last_jitter = motion.jitter;
                    let out = fsr
                        .add_pass(
                            &mut fg,
                            FsrInput {
                                color,
                                depth,
                                motion,
                                camera: FsrCamera {
                                    near: NEAR,
                                    far: FAR,
                                    fov_y: FOV_Y,
                                    meters_per_unit: 1.0,
                                },
                                frame_time_ms: 1000.0 / 60.0,
                                exposure: None,
                            },
                            (output_size, output_size),
                            *frame_number,
                        )
                        .unwrap();
                    let desc = fg.texture_desc(out);
                    assert_eq!(
                        (desc.width, desc.height, desc.format),
                        (output_size, output_size, OUTPUT_FORMAT)
                    );
                    out
                }
                (None, _) => color,
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

    /// `frames` temporal frames at `render` → `output` with the camera moving `step` a
    /// frame from 0; returns the last image and the camera position.
    fn run(
        &mut self,
        draws: &[DrawItem],
        temporal: &Temporal,
        resolver: Resolver,
        (render, output): (u32, u32),
        frames: usize,
        step: f32,
    ) -> (Vec<[f32; 3]>, f32) {
        temporal.set_jitter_phases(Fsr::jitter_phases((render, render), (output, output)));
        temporal.reset();
        let phases = Fsr::jitter_phases((render, render), (output, output));
        let mut image = Vec::new();
        let mut x = 0.0;
        let mut jitters = Vec::with_capacity(frames);
        for _ in 0..frames {
            x += step;
            image = self.render(
                draws,
                x,
                Mode::Temporal {
                    render,
                    output,
                    temporal,
                    resolver,
                },
            );
            jitters.push(self.last_jitter);
        }
        // The forward renderer's jitter is the SDK's sequence (from wherever the view's
        // frame counter is).
        let sdk = |i: usize| engine_fsr::sdk::jitter_offset(i as u32, phases);
        let close =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6;
        assert!(
            (0..phases as usize).any(|o| jitters
                .iter()
                .enumerate()
                .all(|(i, j)| close(*j, sdk(o + i)))),
            "engine {jitters:?}, SDK {:?}",
            (0..phases as usize).map(sdk).collect::<Vec<_>>()
        );
        (image, x)
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
fn quality_modes_and_jitter_match_the_sdk() {
    for mode in QualityMode::ALL {
        assert_eq!(
            mode.ratio(),
            engine_fsr::sdk::upscale_ratio(mode),
            "{mode:?}"
        );
    }
    assert_eq!(QualityMode::Quality.render_size((1920, 1080)), (1280, 720));
    assert_eq!(
        QualityMode::Performance.render_size((3840, 2160)),
        (1920, 1080)
    );
    for (render, output) in [
        (1920, 1920),
        (1280, 1920),
        (1129, 1920),
        (960, 1920),
        (640, 1920),
    ] {
        assert_eq!(
            Fsr::jitter_phases((render, 1), (output, 1)),
            engine_fsr::sdk::jitter_phase_count(render, output),
            "{render} → {output}"
        );
    }
}

/// Everything a test renders with, and the cube mesh.
fn setup() -> Option<(Instance, Gpu, MeshId, Temporal)> {
    let (instance, device) = gpu()?;
    println!("adapter: {}", device.adapter().summary());
    println!("16-bit shader math: {}", device.shader_float16_enabled());
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
    let textures = TexturePool::new(&device, 4 << 20, 1);
    let materials = MaterialTable::new(1);
    let ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let graph = RenderGraph::new(&device, 1);
    let taau = Taau::new(&device, &bindless, 1).unwrap();
    let mut fsr = Fsr::new(&device, 1);
    fsr.set_debug_checking(true);
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
    let gpu = Gpu {
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
        pool,
        cmd,
        readback,
        readback_memory: Some(readback_memory),
        frame_number: 0,
        last_jitter: [0.0; 2],
    };
    Some((instance, gpu, mesh, temporal))
}

/// Frees everything and checks the validation layer's and the SDK's reports.
fn teardown(instance: Instance, gpu: Gpu, temporal: Temporal) {
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
        mut fsr,
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
        fsr.destroy();
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
    assert_eq!(Fsr::dispatch_failures(), 0);
    assert_eq!(
        Fsr::warnings(),
        0,
        "the SDK's debug checking reported problems"
    );
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}

/// The slab the scenes stand on.
fn ground(mesh: MeshId, material: MaterialId) -> DrawItem {
    DrawItem {
        mesh,
        material,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(20.0, 0.2, 20.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, 0.0),
        ),
    }
}

#[test]
fn fsr_upscales_like_a_supersampled_image() {
    let Some((instance, mut gpu, mesh, temporal)) = setup() else {
        return;
    };
    let bright = gpu
        .materials
        .add(MaterialDesc::solid([0.9, 0.8, 0.6], 0.0, 0.3));
    let slab = gpu
        .materials
        .add(MaterialDesc::solid([0.3, 0.35, 0.4], 0.0, 0.8));

    // Cubes turned at odd angles on a ground slab: long slanted edges.
    let mut draws = vec![ground(mesh, slab)];
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
    let half = gpu.render(&draws, 0.0, Mode::Plain(SIZE / 2));
    let stretched = stretch(&half, SIZE / 2);

    let mut scores = vec![
        ("one sample per pixel", psnr(&plain, &truth)),
        ("half size, stretched", psnr(&stretched, &truth)),
    ];
    for resolver in [Resolver::Taau, Resolver::Fsr] {
        let name = |what: &str| -> &'static str {
            Box::leak(format!("{resolver:?} {what}").into_boxed_str())
        };
        let (native, _) = gpu.run(&draws, &temporal, resolver, (SIZE, SIZE), 24, 0.0);
        let (upscaled, _) = gpu.run(&draws, &temporal, resolver, (SIZE / 2, SIZE), 48, 0.0);
        // The camera pans right by 0.02 units a frame (about a third of a pixel).
        let (panned, x) = gpu.run(&draws, &temporal, resolver, (SIZE, SIZE), 24, 0.02);
        let panned_truth = reference(&mut gpu, x);
        let panned_plain = gpu.render(&draws, x, Mode::Plain(SIZE));
        assert!(
            native
                .iter()
                .chain(&upscaled)
                .flatten()
                .all(|v| v.is_finite())
        );
        scores.push((name("native"), psnr(&native, &truth)));
        scores.push((name("from half size"), psnr(&upscaled, &truth)));
        scores.push((name("panning"), psnr(&panned, &panned_truth)));
        scores.push((
            name("panning, one sample"),
            psnr(&panned_plain, &panned_truth),
        ));
    }
    for (name, db) in &scores {
        println!("{name}: {db:.2} dB against 4×4 supersampling");
    }
    let score = |name: &str| scores.iter().find(|(n, _)| *n == name).unwrap().1;
    assert!(
        score("Fsr native") > score("one sample per pixel") + 3.0,
        "FSR antialiases: {scores:?}"
    );
    assert!(
        score("Fsr from half size") > score("half size, stretched") + 2.0,
        "FSR upsamples: {scores:?}"
    );
    assert!(
        score("Fsr panning") > score("Fsr panning, one sample") + 1.0,
        "FSR follows motion: {scores:?}"
    );

    // A new output size makes a new SDK context; sharpening on.
    gpu.fsr.set_sharpness(Some(0.5));
    let (resized, _) = gpu.run(&draws, &temporal, Resolver::Fsr, (40, 80), 4, 0.0);
    assert_eq!(resized.len(), 80 * 80);
    assert!(resized.iter().flatten().all(|v| v.is_finite()));
    println!("SDK context: {} bytes of GPU memory", gpu.fsr.gpu_memory());
    teardown(instance, gpu, temporal);
}

/// A 256 × 256 texture of fine detail (a checker of 2 × 2 texel cells with noisy tints),
/// with its full mip chain, box-filtered.
fn detail_texture() -> TextureLevels {
    let mut size = 256u32;
    let mut level: Vec<u8> = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let cell = (x / 2 + y / 2) % 2;
            let hash = (x / 2).wrapping_mul(73_856_093) ^ (y / 2).wrapping_mul(19_349_663);
            let base: u8 = if cell == 0 { 40 } else { 200 };
            let tint = (hash % 50) as u8;
            level.extend_from_slice(&[base + tint, base, base.saturating_sub(tint), 255]);
        }
    }
    let mut levels = vec![level];
    while size > 1 {
        let half = size / 2;
        let above = levels.last().unwrap();
        let mut next = Vec::with_capacity((half * half * 4) as usize);
        for y in 0..half {
            for x in 0..half {
                for c in 0..4 {
                    let at = |dx: u32, dy: u32| {
                        u32::from(above[(((2 * y + dy) * size + 2 * x + dx) * 4 + c) as usize])
                    };
                    next.push(((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8);
                }
            }
        }
        levels.push(next);
        size = half;
    }
    TextureLevels {
        width: 256,
        height: 256,
        format: TextureFormat::Rgba8Unorm,
        levels,
    }
}

/// The texture LOD bias of the materials (`MaterialTable::set_texture_lod_bias`) against
/// a supersampled reference, on a finely textured ground seen at a grazing angle, upscaled
/// from half the size.
#[test]
fn texture_lod_bias_recovers_texture_detail() {
    let Some((instance, mut gpu, mesh, temporal)) = setup() else {
        return;
    };
    let texture = gpu
        .textures
        .add(&gpu.device, &mut gpu.allocator, detail_texture())
        .unwrap();
    let textured = gpu.materials.add(MaterialDesc {
        base_color: [1.0; 4],
        base_color_texture: Some(TextureRef::new(texture)),
        roughness: 0.9,
        ..MaterialDesc::default()
    });
    let draws = [ground(mesh, textured)];
    let big = gpu.render(&draws, 0.0, Mode::Plain(SIZE * SUPERSAMPLE));
    let truth = downsample(&big, SIZE * SUPERSAMPLE, SUPERSAMPLE);
    let plain = gpu.render(&draws, 0.0, Mode::Plain(SIZE));
    println!("one sample per pixel: {:.2} dB", psnr(&plain, &truth));
    let (render, output) = (SIZE / 2, SIZE);
    let standard = engine_upscale::texture_lod_bias((render, render), (output, output));
    let fsr = Fsr::texture_lod_bias((render, render), (output, output));
    let mut scores = Vec::new();
    for (resolver, bias) in [
        (Resolver::Taau, 0.0),
        (Resolver::Taau, standard),
        (Resolver::Taau, fsr),
        (Resolver::Fsr, 0.0),
        (Resolver::Fsr, standard),
        (Resolver::Fsr, fsr),
    ] {
        gpu.materials
            .set_texture_lod_bias(&gpu.bindless, bias)
            .unwrap();
        let (image, _) = gpu.run(&draws, &temporal, resolver, (render, output), 48, 0.0);
        let db = psnr(&image, &truth);
        println!(
            "{resolver:?} from half size, LOD bias {bias:+.2}: {db:.2} dB against 4×4 supersampling"
        );
        scores.push(db);
    }
    gpu.materials
        .set_texture_lod_bias(&gpu.bindless, 0.0)
        .unwrap();
    assert!(
        scores[1] > scores[0],
        "the LOD bias sharpens TAAU's textures: {scores:?}"
    );
    assert!(
        scores[4] > scores[3],
        "the LOD bias sharpens FSR's textures: {scores:?}"
    );
    teardown(instance, gpu, temporal);
}
