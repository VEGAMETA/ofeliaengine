//! The tone map pass (`engine_render::post`) writes what its CPU description says, for SDR
//! and HDR output (ADR 0038): known scene values go through the pass into a 32-bit float
//! target and come back within float rounding of a CPU reference of the curve, of scRGB
//! scaling and of PQ encoding.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{Access, BufferState, ImportedBuffer, LoadOp, RenderGraph, TextureDesc};
use engine_render::post::{Composite, OutputEncoding, Tonemap};
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

/// PBR Neutral rolled off towards `peak` (as `tonemap.slang`).
fn pbr_neutral([mut r, mut g, mut b]: [f32; 3], peak: f32) -> [f32; 3] {
    let start_compression = 0.8 - 0.04;
    let desaturation = 0.15;
    let x = r.min(g).min(b);
    let offset = if x < 0.08 { x - 6.25 * x * x } else { 0.04 };
    (r, g, b) = (r - offset, g - offset, b - offset);
    let brightest = r.max(g).max(b);
    let start = start_compression * peak;
    if brightest < start {
        return [r, g, b];
    }
    let d = peak - start;
    let new_peak = peak - d * d / (brightest + d - start);
    let k = new_peak / brightest;
    (r, g, b) = (r * k, g * k, b * k);
    let t = 1.0 - 1.0 / (desaturation * (brightest - new_peak) / peak + 1.0);
    let mix = |c: f32| c + (new_peak - c) * t;
    [mix(r), mix(g), mix(b)]
}

#[test]
fn tone_map_and_encodings_match_the_reference() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let tonemap = Tonemap::new(&device, &bindless, FORMAT).unwrap();
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

    let cases: [(&str, bool, f32, OutputEncoding); 4] = [
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
    for (name, tone_map, peak, encoding) in cases {
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
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
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
                    peak,
                    encoding,
                    load: LoadOp::DontCare,
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
            let [r, g, b] = if tone_map {
                pbr_neutral([r, g, b], peak)
            } else {
                [r, g, b]
            };
            let expected = encoding.encode([r, g, b, a]);
            for c in 0..3 {
                let tolerance = 2e-4 * expected[c].abs().max(1.0);
                assert!(
                    (got[c] - expected[c]).abs() <= tolerance,
                    "{name}: {input:?} gave {got:?}, {expected:?} expected"
                );
            }
            // The highlight roll-off never passes the peak.
            if tone_map && let OutputEncoding::Linear { white } = encoding {
                assert!(got[..3].iter().all(|&c| c <= peak * white * 1.0001));
            }
        }
        println!("{name}: {} values match", INPUT.len());
    }

    // SAFETY: the device is idle.
    unsafe {
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
