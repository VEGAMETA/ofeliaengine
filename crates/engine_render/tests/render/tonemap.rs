//! The tone map pass (`engine_render::post`) writes what its CPU description says, for SDR
//! and HDR output (ADR 0038): known scene values go through the pass into a 32-bit float
//! target and come back within float rounding of a CPU reference of the curve, of scRGB
//! scaling and of PQ encoding. Its resampling kernels (Catmull-Rom, Lanczos-3; ADR 0075)
//! match a CPU reference, give texels back at 1:1, keep flat areas flat and never ring.
//! Graded (ADR 0094), the grading LUT gives the CPU reference of the grading and the
//! curve within a step of 8-bit output; with a color vision transform after the curve (ADR
//! 0097; before PBR Neutral's) too.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::color_vision::{ColorVision, ColorVisionMode, Deficiency};
use engine_render::grading::{ColorGrading, GradingLut};
use engine_render::graph::{Access, BufferState, ImportedBuffer, LoadOp, RenderGraph, TextureDesc};
use engine_render::post::{Composite, OutputEncoding, Resample, ToneCurve, Tonemap};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
    SamplerKind,
};

const FORMAT: vk::Format = vk::Format::R32G32B32A32_SFLOAT;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render tonemap test"),
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

/// Scene values: darks (the toe), mid-tones, reference white, highlights far above it,
/// and saturated colors.
const INPUT: [[f32; 4]; 12] = [
    [0.0, 0.0, 0.0, 1.0],
    [0.02, 0.05, 0.07, 1.0],
    [0.18, 0.18, 0.18, 1.0],
    [0.5, 0.25, 0.1, 1.0],
    [1.0, 1.0, 1.0, 1.0],
    [2.0, 1.5, 1.0, 1.0],
    [4.0, 4.0, 4.0, 1.0],
    [6.0, 1.0, 0.5, 1.0],
    [20.0, 20.0, 20.0, 1.0],
    [100.0, 50.0, 10.0, 1.0],
    [0.0, 3.0, 0.0, 1.0],
    [0.3, 0.3, 12.0, 1.0],
];

#[test]
fn tone_map_and_encodings_match_the_reference() {
    check_tone_map(false);
}

#[test]
fn graded_tone_map_matches_the_reference() {
    check_tone_map(true);
}

#[test]
fn color_vision_tone_map_matches_the_reference() {
    check_tone_map_with(true, &color_visions());
}

/// The color vision transforms `color_vision_tone_map_matches_the_reference` runs.
fn color_visions() -> Vec<(&'static str, ColorVision)> {
    let vision = |deficiency, severity, mode| ColorVision {
        deficiency: Some(deficiency),
        severity,
        mode,
    };
    vec![
        (
            "protanomaly corrected",
            vision(Deficiency::Protan, 0.6, ColorVisionMode::Correct),
        ),
        (
            "deuteranopia simulated",
            vision(Deficiency::Deutan, 1.0, ColorVisionMode::Simulate),
        ),
        (
            "tritanopia simulated",
            vision(Deficiency::Tritan, 1.0, ColorVisionMode::Simulate),
        ),
        (
            "tritanomaly corrected",
            vision(Deficiency::Tritan, 0.8, ColorVisionMode::Correct),
        ),
    ]
}

/// sRGB's encoding, for comparing linear outputs as an 8-bit display would show them.
fn srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// The gradings `check_tone_map` runs: none (the LUT holds the curve alone), and a strong
/// one touching every control.
fn gradings() -> [(&'static str, ColorGrading); 2] {
    [
        ("neutral grading", ColorGrading::default()),
        (
            "strong grading",
            ColorGrading {
                white_balance: [0.4, -0.2],
                channel_mixer: [[0.9, 0.1, 0.0], [0.05, 0.9, 0.05], [0.0, 0.1, 0.9]],
                shadows: [1.1, 1.0, 1.3],
                midtones: [1.0, 1.05, 0.95],
                highlights: [1.2, 1.1, 0.9],
                slope: [1.05, 1.0, 0.98],
                offset: [0.01, 0.0, -0.01],
                power: [0.95, 1.0, 1.05],
                contrast: 1.3,
                vibrance: 1.4,
                saturation: 0.8,
                ..ColorGrading::default()
            },
        ),
    ]
}

/// The tone mapper's composites over `INPUT` against `ToneCurve::apply` (and, `graded`,
/// `ColorGrading::apply` through a grading LUT) on every output and curve.
fn check_tone_map(graded: bool) {
    check_tone_map_with(graded, &[("", ColorVision::default())]);
}

/// `check_tone_map` with each of `visions` after the curve (`ColorVision::apply`); with a
/// color vision transform, only the neutral grading.
fn check_tone_map_with(graded: bool, visions: &[(&'static str, ColorVision)]) {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let tonemap = Tonemap::new(&device, &bindless, FORMAT).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut lut = graded.then(|| GradingLut::new(&device, &mut allocator, &bindless).unwrap());
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
    let width = INPUT.len() as u32;
    let bytes = (INPUT.len() * 16) as u64;
    let (upload, upload_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )
        .unwrap();
    // SAFETY: the mapping covers the buffer; nothing else uses it yet.
    unsafe {
        core::ptr::copy_nonoverlapping(
            INPUT.as_ptr().cast::<u8>(),
            upload_mem.mapped_ptr().unwrap().as_ptr(),
            bytes as usize,
        );
    }
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();

    let outputs: [(&str, bool, f32, OutputEncoding); 4] = [
        ("SDR", true, 1.0, OutputEncoding::LINEAR),
        (
            "scRGB, 200-nit white, 1000-nit peak",
            true,
            5.0,
            OutputEncoding::Linear { white: 2.5 },
        ),
        (
            "HDR10 of a tone-mapped HDR image",
            false,
            1.0,
            OutputEncoding::Pq { white_nits: 200.0 },
        ),
        (
            "HDR10, tone mapped in the same pass",
            true,
            3.0,
            OutputEncoding::Pq { white_nits: 250.0 },
        ),
    ];
    let with_vision = visions.iter().any(|(_, v)| !v.is_off());
    let gradings = if graded && !with_vision {
        gradings().to_vec()
    } else {
        vec![("", ColorGrading::default())]
    };
    let cases = outputs
        .iter()
        // A grading needs the curve: not on an image tone-mapped already.
        .filter(|c| c.1 || !graded)
        .flat_map(|&c| {
            [
                ToneCurve::Gt,
                ToneCurve::PbrNeutral,
                ToneCurve::Agx,
                ToneCurve::Aces,
            ]
            .map(|curve| (c, curve))
        })
        .flat_map(|c| gradings.iter().map(move |&g| (c, g)))
        .flat_map(|c| visions.iter().map(move |&v| (c, v)));
    let mut worst = 0.0f32;
    for (
        (((name, tone_map, peak, encoding), curve), (grading_name, grading)),
        (vision_name, vision),
    ) in cases
    {
        let name = format!("{name}, {curve:?} {grading_name} {vision_name}");
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
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            let lut_texture = lut.as_ref().map(|l| {
                l.add_passes(
                    &mut fg, &mut ring, &bindless, &grading, curve, peak, &vision,
                )
                .unwrap()
            });
            let source = fg.create_texture("scene", TextureDesc::new_2d(FORMAT, width, 1));
            let values = fg.import_buffer(
                "scene values",
                ImportedBuffer {
                    buffer: upload,
                    size: bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: None,
                },
            );
            fg.add_pass("upload")
                .read(values, Access::TransferRead)
                .write(source, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width,
                            height: 1,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_buffer_to_image(
                            ctx.cmd,
                            ctx.buffer(values),
                            ctx.image(source),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                });
            let output = fg.create_texture("output", TextureDesc::new_2d(FORMAT, width, 1));
            tonemap.add_composite_pass(
                &mut fg,
                &bindless,
                source,
                output,
                Composite {
                    sampler: SamplerKind::NearestClamp,
                    tonemap: tone_map,
                    curve,
                    peak,
                    encoding,
                    load: LoadOp::DontCare,
                    grading: lut_texture,
                    ..Composite::default()
                },
            );
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("read back")
                .read(output, Access::TransferRead)
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
                            width,
                            height: 1,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; the graph orders the copy after the tone map.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(output),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                    }
                });
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
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let gpu: &[[f32; 4]] = unsafe {
            core::slice::from_raw_parts(
                readback_mem
                    .mapped_ptr()
                    .unwrap()
                    .as_ptr()
                    .cast::<[f32; 4]>(),
                INPUT.len(),
            )
        };
        for (input, got) in INPUT.iter().zip(gpu) {
            let [r, g, b, a] = *input;
            let [r, g, b] = if graded {
                grading.apply([r, g, b])
            } else {
                [r, g, b]
            };
            // The color vision transform after the curve (before PBR Neutral's).
            let [r, g, b] = if tone_map && curve == ToneCurve::PbrNeutral {
                curve.apply(vision.apply([r, g, b]), peak)
            } else if tone_map {
                vision.apply(curve.apply([r, g, b], peak))
            } else {
                [r, g, b]
            };
            let expected = encoding.encode([r, g, b, a]);
            // Brettel's two half-planes for tritans meet along a plane through the grey
            // axis, which the LUT interpolates across (the half-planes blended over
            // `SEPARATION_BAND`, ADR 0097): greys within 1.55 / 255 on the Radeon.
            let steps = if vision.deficiency == Some(Deficiency::Tritan) && !vision.is_off() {
                2.0
            } else {
                1.0
            };
            for c in 0..3 {
                if graded {
                    // Through the LUT: within a step of 8-bit output, compared as a
                    // display shows it (PQ is perceptual already; linear outputs as sRGB
                    // over their brightest value).
                    let shown = |v: f32| match encoding {
                        OutputEncoding::Pq { .. } => v,
                        OutputEncoding::Linear { white } => srgb(v / (peak * white)),
                    };
                    let error = (shown(got[c]) - shown(expected[c])).abs();
                    worst = worst.max(error / steps);
                    assert!(
                        error <= steps / 255.0,
                        "{name}: {input:?} gave {got:?}, {expected:?} expected ({:.2} / 255)",
                        error * 255.0
                    );
                    continue;
                }
                let tolerance = 2e-4 * expected[c].abs().max(1.0);
                assert!(
                    (got[c] - expected[c]).abs() <= tolerance,
                    "{name}: {input:?} gave {got:?}, {expected:?} expected"
                );
            }
            // The highlight roll-off never passes the peak (a color vision correction
            // may: it adds to what the deficiency loses).
            if tone_map
                && vision.is_off()
                && let OutputEncoding::Linear { white } = encoding
            {
                assert!(got[..3].iter().all(|&c| c <= peak * white * 1.0001));
            }
        }
        println!("{name}: {} values match", INPUT.len());
    }
    if graded {
        println!("graded: worst {:.2} / 255", worst * 255.0);
    }

    // SAFETY: the device is idle.
    unsafe {
        if let Some(lut) = lut.as_mut() {
            lut.destroy(&mut allocator, &bindless, 0);
        }
        ring.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(upload, upload_mem);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(tonemap);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "validation errors");
}

/// Records and runs one composite of `source` (`size`) into an `output`-sized target
/// through `tonemap`, without tone mapping (exposure 1, linear output), and reads it back.
#[allow(clippy::too_many_arguments)]
fn composite(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    graph: &mut RenderGraph,
    cmd: vk::CommandBuffer,
    tonemap: &Tonemap,
    source: &[[f32; 4]],
    size: (u32, u32),
    output: (u32, u32),
) -> Vec<[f32; 4]> {
    let d = device.raw();
    let in_bytes = (source.len() * 16) as u64;
    let out_len = (output.0 * output.1) as usize;
    let out_bytes = (out_len * 16) as u64;
    let (upload, upload_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(in_bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )
        .unwrap();
    // SAFETY: the mapping covers the buffer; nothing else uses it yet.
    unsafe {
        core::ptr::copy_nonoverlapping(
            source.as_ptr().cast::<u8>(),
            upload_mem.mapped_ptr().unwrap().as_ptr(),
            in_bytes as usize,
        );
    }
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(out_bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    // SAFETY: one-shot recording; nothing is in flight.
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
    let layers = vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    };
    {
        // SAFETY: nothing is in flight.
        let mut fg = unsafe { graph.begin_slot(0, cmd) };
        let scene = fg.create_texture("scene", TextureDesc::new_2d(FORMAT, size.0, size.1));
        let values = fg.import_buffer(
            "scene values",
            ImportedBuffer {
                buffer: upload,
                size: in_bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        fg.add_pass("upload")
            .read(values, Access::TransferRead)
            .write(scene, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: layers,
                    image_extent: vk::Extent3D {
                        width: size.0,
                        height: size.1,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording; layouts arranged by the graph.
                unsafe {
                    ctx.device.cmd_copy_buffer_to_image(
                        ctx.cmd,
                        ctx.buffer(values),
                        ctx.image(scene),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                }
            });
        let target = fg.create_texture("output", TextureDesc::new_2d(FORMAT, output.0, output.1));
        tonemap.add_composite_pass(
            &mut fg,
            bindless,
            scene,
            target,
            Composite {
                sampler: SamplerKind::LinearClamp,
                tonemap: false,
                load: LoadOp::DontCare,
                ..Composite::default()
            },
        );
        let out = fg.import_buffer(
            "readback",
            ImportedBuffer {
                buffer: readback,
                size: out_bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("read back")
            .read(target, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: layers,
                    image_extent: vk::Extent3D {
                        width: output.0,
                        height: output.1,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording; the graph orders the copy after the composite.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(target),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[region],
                    );
                }
            });
        fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
    }
    // SAFETY: as above; the GPU is idle when the mapping is read.
    let result = unsafe {
        d.end_command_buffer(cmd).unwrap();
        let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        d.queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
            vk::Fence::null(),
        )
        .unwrap();
        device.wait_idle().unwrap();
        core::slice::from_raw_parts(
            readback_mem
                .mapped_ptr()
                .unwrap()
                .as_ptr()
                .cast::<[f32; 4]>(),
            out_len,
        )
        .to_vec()
    };
    // SAFETY: the GPU is idle.
    unsafe {
        allocator.destroy_buffer(upload, upload_mem);
        allocator.destroy_buffer(readback, readback_mem);
    }
    result
}

/// Catmull-Rom's weights (Keys, a = -0.5) of the four texels around fraction `f`.
fn catmull_rom_weights(f: f64) -> Vec<f64> {
    let (f2, f3) = (f * f, f * f * f);
    vec![
        -0.5 * f + f2 - 0.5 * f3,
        1.0 - 2.5 * f2 + 1.5 * f3,
        0.5 * f + 2.0 * f2 - 1.5 * f3,
        -0.5 * f2 + 0.5 * f3,
    ]
}

/// Lanczos-3's normalized weights of the six texels around fraction `f`.
fn lanczos3_weights(f: f64) -> Vec<f64> {
    let w: Vec<f64> = (0..6)
        .map(|i| {
            let x = core::f64::consts::PI * (f - (f64::from(i) - 2.0));
            if x.abs() < 1e-9 {
                1.0
            } else {
                3.0 * x.sin() * (x / 3.0).sin() / (x * x)
            }
        })
        .collect();
    let total: f64 = w.iter().sum();
    w.iter().map(|v| v / total).collect()
}

/// What a kernel gives at an output pixel: the clamped value, the unclamped one, the
/// range of the four texels around the sample, and the largest magnitude under the kernel.
struct Expected {
    value: [f64; 3],
    unclamped: [f64; 3],
    range: ([f64; 3], [f64; 3]),
    scale: f64,
}

/// For each output pixel, what the kernel may give: a sample on a texel center may land a
/// hair before it on the GPU, where the clamp takes the texels around it from the other
/// side (both are right), so such samples have two candidates per axis.
fn expected(
    source: &[[f32; 4]],
    size: (u32, u32),
    output: (u32, u32),
    kernel: Resample,
) -> Vec<Vec<Expected>> {
    let (taps, weights): (i64, fn(f64) -> Vec<f64>) = match kernel {
        Resample::CatmullRom => (4, catmull_rom_weights),
        Resample::Lanczos3 => (6, lanczos3_weights),
        Resample::Sampler => unreachable!("a kernel"),
    };
    let texel = |x: i64, y: i64| {
        let x = x.clamp(0, i64::from(size.0) - 1) as usize;
        let y = y.clamp(0, i64::from(size.1) - 1) as usize;
        source[y * size.0 as usize + x].map(f64::from)
    };
    let at = |px: f64, py: f64| {
        let (bx, by) = (px.floor(), py.floor());
        let (wx, wy) = (weights(px - bx), weights(py - by));
        let origin = (bx as i64 - (taps / 2 - 1), by as i64 - (taps / 2 - 1));
        let mut sum = [0.0; 3];
        let mut scale = 0.0f64;
        for (j, wy) in wy.iter().enumerate() {
            for (i, wx) in wx.iter().enumerate() {
                let c = texel(origin.0 + i as i64, origin.1 + j as i64);
                for k in 0..3 {
                    sum[k] += c[k] * wx * wy;
                    scale = scale.max(c[k].abs());
                }
            }
        }
        let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let c = texel(bx as i64 + dx, by as i64 + dy);
            for k in 0..3 {
                lo[k] = lo[k].min(c[k]);
                hi[k] = hi[k].max(c[k]);
            }
        }
        let value = match kernel {
            // Two passes: each row across, clamped to its two texels nearest the sample
            // (kept in half floats: the tolerance covers them), then the rows down,
            // clamped to the two nearest.
            Resample::Lanczos3 => {
                let between = |v: f64, a: f64, b: f64| v.clamp(a.min(b), a.max(b));
                let row = |y: i64| -> [f64; 3] {
                    let mut sum = [0.0; 3];
                    for (i, w) in wx.iter().enumerate() {
                        let c = texel(origin.0 + i as i64, y);
                        for k in 0..3 {
                            sum[k] += c[k] * w;
                        }
                    }
                    let (a, b) = (texel(bx as i64, y), texel(bx as i64 + 1, y));
                    core::array::from_fn(|k| between(sum[k], a[k], b[k]))
                };
                let rows: Vec<[f64; 3]> = (0..taps).map(|j| row(origin.1 + j)).collect();
                let mut down = [0.0; 3];
                for (j, w) in wy.iter().enumerate() {
                    for k in 0..3 {
                        down[k] += rows[j][k] * w;
                    }
                }
                core::array::from_fn(|k| between(down[k], rows[2][k], rows[3][k]))
            }
            _ => core::array::from_fn(|k| sum[k].clamp(lo[k], hi[k])),
        };
        Expected {
            value,
            unclamped: sum,
            range: (lo, hi),
            scale,
        }
    };
    // A position on a texel center, and a hair before it.
    let candidates = |p: f64| {
        if (p - p.round()).abs() < 1e-4 {
            vec![p.round(), p.round() - 1e-9]
        } else {
            vec![p]
        }
    };
    let mut all = Vec::new();
    for oy in 0..output.1 {
        for ox in 0..output.0 {
            let px = (f64::from(ox) + 0.5) / f64::from(output.0) * f64::from(size.0) - 0.5;
            let py = (f64::from(oy) + 0.5) / f64::from(output.1) * f64::from(size.1) - 0.5;
            let mut options = Vec::new();
            for &x in &candidates(px) {
                for &y in &candidates(py) {
                    options.push(at(x, y));
                }
            }
            all.push(options);
        }
    }
    all
}

#[test]
fn resampling_kernels_match_the_reference_and_do_not_ring() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
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

    // A 12 x 10 scene: noise in [0, 2) above, in it a highlight of 1000 (a sun next to
    // sky: what negative lobes ring black around), and a flat band in the six rows below.
    const FLAT: [f32; 4] = [0.5, 0.25, 0.125, 1.0];
    let size = (12u32, 10u32);
    let mut seed = 0x2545_f491u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0
    };
    let source: Vec<[f32; 4]> = (0..size.0 * size.1)
        .map(|i| match (i % size.0, i / size.0) {
            (4, 2) => [1000.0, 500.0, 100.0, 1.0],
            (_, y) if y >= 4 => FLAT,
            _ => [noise(), noise(), noise(), 1.0],
        })
        .collect();

    for kernel in [Resample::CatmullRom, Resample::Lanczos3] {
        let tonemap = Tonemap::with_resample(&device, &bindless, Some(FORMAT), kernel).unwrap();
        // The first row whose kernel lies in the flat band.
        let flat_from = match kernel {
            Resample::Lanczos3 => 6.0,
            _ => 5.0,
        };
        let mut rings = false;
        // 2.5 times the size; 1.42 times across and 1:1 down; 1:1.
        for output in [(30, 25), (17, 10), size] {
            let got = composite(
                &device,
                &mut allocator,
                &bindless,
                &mut graph,
                cmd,
                &tonemap,
                &source,
                size,
                output,
            );
            let want = expected(&source, size, output, kernel);
            for (i, (got, options)) in got.iter().zip(&want).enumerate() {
                let (x, y) = (i as u32 % output.0, i as u32 / output.0);
                // Within float rounding of one candidate, and within the four nearest
                // texels' range (no ringing).
                let matches = |want: &Expected| {
                    let tolerance = 1e-3 * want.scale + 1e-4;
                    (0..3).all(|k| {
                        let g = f64::from(got[k]);
                        (g - want.value[k]).abs() <= tolerance
                            && g >= want.range.0[k] - tolerance
                            && g <= want.range.1[k] + tolerance
                    })
                };
                assert!(
                    options.iter().any(matches),
                    "{kernel:?} to {output:?} at ({x}, {y}): {got:?}, {:?} expected",
                    options.iter().map(|o| o.value).collect::<Vec<_>>()
                );
                rings |= (0..3).any(|k| options[0].unclamped[k] < options[0].range.0[k] - 1.0);
                // Flat areas stay flat: the weights sum to 1.
                let py = (f64::from(y) + 0.5) / f64::from(output.1) * f64::from(size.1) - 0.5;
                if py.floor() >= flat_from {
                    for k in 0..3 {
                        assert!(
                            (got[k] - FLAT[k]).abs() <= 1e-5,
                            "{kernel:?} to {output:?} at ({x}, {y}): {got:?} in the flat band"
                        );
                    }
                }
            }
            // 1:1 gives the texels back (within the interpolated coordinates' rounding:
            // a sample a hair off the center takes a hair of its neighbours; and Lanczos-3's
            // half-float rows).
            if output == size {
                let relative = match kernel {
                    Resample::Lanczos3 => 1e-3,
                    _ => 1e-4,
                };
                for ((got, want), options) in got.iter().zip(&source).zip(&want) {
                    let tolerance = relative * options[0].scale + 1e-5;
                    for k in 0..3 {
                        assert!(
                            f64::from((got[k] - want[k]).abs()) <= tolerance,
                            "{kernel:?} at 1:1: {got:?}, {want:?} expected"
                        );
                    }
                }
            }
            println!("{kernel:?} to {output:?}: {} pixels match", got.len());
        }
        // Without the clamp the highlight would have rung far below its neighbours.
        assert!(rings, "{kernel:?}: the test did not exercise the clamp");
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        d.destroy_command_pool(pool, None);
    }
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "validation errors");
}
