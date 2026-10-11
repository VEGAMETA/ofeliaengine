//! Bloom and automatic exposure (ADR 0047), under the validation layer with synchronization
//! validation.
//!
//! - Bloom's filters are normalized: a uniform image blooms into itself, and a bright block
//!   keeps its energy and spreads symmetrically (Karis' average, on by default, gives up
//!   some of a small source's energy on purpose).
//! - Automatic exposure brings a uniform scene's luminance to middle grey (0.18); 5% of
//!   blinding pixels barely move it (they are above the averaged percentiles); after a
//!   change of scene it adapts by 1 − exp(−dt · speed) of the difference in EV per frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, RenderGraph, StoreOp, Texture,
    TextureDesc,
};
use engine_render::post::{Composite, Tonemap};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings, ExposureSettings};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render post test"),
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

/// An HDR image: `background` everywhere and `value` over `rect` (x, y, width, height).
#[derive(Clone, Copy)]
struct Image {
    background: [f32; 3],
    value: [f32; 3],
    rect: [u32; 4],
}

/// Draws `image` into a new graph texture.
fn draw<'g>(fg: &mut FrameGraph<'g>, image: Image) -> Texture {
    let texture = fg.create_texture("scene", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
    let [r, g, b] = image.background;
    fg.add_pass("draw")
        .color(texture, LoadOp::Clear([r, g, b, 1.0]), StoreOp::Store)
        .execute(move |ctx| {
            let [x, y, w, h] = image.rect;
            if w == 0 || h == 0 {
                return;
            }
            let [r, g, b] = image.value;
            // SAFETY: recording inside the pass's rendering scope; the rectangle is inside
            // the target.
            unsafe {
                ctx.device.cmd_clear_attachments(
                    ctx.cmd,
                    &[vk::ClearAttachment {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        color_attachment: 0,
                        clear_value: vk::ClearValue {
                            color: vk::ClearColorValue {
                                float32: [r, g, b, 1.0],
                            },
                        },
                    }],
                    &[vk::ClearRect {
                        rect: vk::Rect2D {
                            offset: vk::Offset2D {
                                x: x as i32,
                                y: y as i32,
                            },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        base_array_layer: 0,
                        layer_count: 1,
                    }],
                );
            }
        });
    texture
}

#[test]
fn bloom_keeps_energy_and_exposure_finds_middle_grey() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let bloom = Bloom::new(&device, &bindless, BloomSettings::default()).unwrap();
    let exposure = AutoExposure::new(
        &device,
        &mut allocator,
        &bindless,
        ExposureSettings::default(),
    )
    .unwrap();
    let tonemap = Tonemap::new(&device, &bindless, HDR).unwrap();
    let bytes = u64::from(WIDTH * HEIGHT * 8);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
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

    // Runs one frame: `$build` declares passes in `$fg` and returns a texture to read
    // back.
    macro_rules! frame {
        ($fg:ident => $build:block) => {{
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
            let (width, height);
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                let result: Texture = {
                    let $fg = &mut fg;
                    $build
                };
                let desc = fg.texture_desc(result);
                (width, height) = (desc.width, desc.height);
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
                                width,
                                height,
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
            let ptr = readback_memory.mapped_ptr().unwrap();
            let len = (width * height * 8) as usize;
            // SAFETY: the GPU is idle; the mapping covers the image.
            let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), len) };
            let pixels: Vec<[f32; 3]> = raw
                .chunks_exact(8)
                .map(|p| {
                    let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                    [h(0), h(1), h(2)]
                })
                .collect();
            (pixels, width, height)
        }};
    }

    // 1. A uniform image blooms into itself.
    let uniform = Image {
        background: [2.0, 1.0, 0.5],
        value: [0.0; 3],
        rect: [0; 4],
    };
    let (b, _, _) = frame!(fg => {
        let scene = draw(fg, uniform);
        bloom.add_passes(fg, &bindless, scene).texture
    });
    let worst = b
        .iter()
        .flat_map(|p| {
            p.iter()
                .zip(uniform.background)
                .map(|(a, e)| (a - e).abs() / e)
        })
        .fold(0.0f32, f32::max);
    println!("uniform bloom: worst relative error {worst:.5}");
    assert!(worst < 2e-3, "{worst}");

    // 2. A bright block keeps its energy (without Karis' average) and spreads
    // symmetrically.
    bloom.set_settings(BloomSettings {
        karis_average: false,
        ..BloomSettings::default()
    });
    let block = Image {
        background: [0.0; 3],
        value: [1000.0; 3],
        rect: [WIDTH / 2 - 1, HEIGHT / 2 - 1, 2, 2],
    };
    let (b, w, h) = frame!(fg => {
        let scene = draw(fg, block);
        bloom.add_passes(fg, &bindless, scene).texture
    });
    // Each bloom texel covers 2 × 2 image pixels.
    let energy: f32 = b.iter().map(|p| p[0]).sum::<f32>() * 4.0;
    let expected = 1000.0 * 4.0;
    let asymmetry = (0..h)
        .flat_map(|y| (0..w / 2).map(move |x| (x, y)))
        .map(|(x, y)| (b[(y * w + x) as usize][0] - b[(y * w + (w - 1 - x)) as usize][0]).abs())
        .fold(0.0f32, f32::max);
    let peak = b.iter().map(|p| p[0]).fold(0.0f32, f32::max);
    println!(
        "block bloom: energy {energy} (expected {expected}), peak {peak}, asymmetry {asymmetry}"
    );
    assert!((energy - expected).abs() < expected * 0.02, "{energy}");
    assert!(asymmetry < peak * 0.02, "{asymmetry} of {peak}");
    // With Karis' average the block keeps less, still symmetric.
    bloom.set_settings(BloomSettings::default());
    let (b, w, h) = frame!(fg => {
        let scene = draw(fg, block);
        bloom.add_passes(fg, &bindless, scene).texture
    });
    let karis: f32 = b.iter().map(|p| p[0]).sum::<f32>() * 4.0;
    let asymmetry = (0..h)
        .flat_map(|y| (0..w / 2).map(move |x| (x, y)))
        .map(|(x, y)| (b[(y * w + x) as usize][0] - b[(y * w + (w - 1 - x)) as usize][0]).abs())
        .fold(0.0f32, f32::max);
    println!("block bloom with Karis' average: energy {karis}");
    assert!(
        karis < energy && karis > 0.0 && asymmetry < 1e-2,
        "{karis} {asymmetry}"
    );

    // 2b. The shared-memory passes (levels exactly half, and twice, the size of the one
    // they read: here all but the last) give the plain passes' values, with Karis'
    // average and without.
    let spot = Image {
        background: [0.3, 0.2, 0.1],
        value: [500.0, 200.0, 50.0],
        rect: [WIDTH / 3, HEIGHT / 4, 7, 5],
    };
    for karis_average in [true, false] {
        bloom.set_settings(BloomSettings {
            karis_average,
            ..BloomSettings::default()
        });
        let (shared, _, _) = frame!(fg => {
            let scene = draw(fg, spot);
            bloom.add_passes(fg, &bindless, scene).texture
        });
        bloom.set_shared_memory(false);
        let (plain, _, _) = frame!(fg => {
            let scene = draw(fg, spot);
            bloom.add_passes(fg, &bindless, scene).texture
        });
        bloom.set_shared_memory(true);
        let worst = shared
            .iter()
            .zip(&plain)
            .flat_map(|(a, b)| {
                a.iter()
                    .zip(b)
                    .map(|(a, b)| (a - b).abs() / b.abs().max(1e-3))
            })
            .fold(0.0f32, f32::max);
        println!(
            "shared-memory bloom (Karis' average {karis_average}): worst relative difference {worst}"
        );
        assert!(worst < 2e-3, "{worst}");
    }
    bloom.set_settings(BloomSettings::default());

    // 3. Automatic exposure: a tone-map pass without the curve shows exposure × color.
    let mut expose = |image: Image, dt: f32| -> f32 {
        let (out, _, _) = frame!(fg => {
            let scene = draw(fg, image);
            let buffer = exposure.add_passes(fg, &bindless, scene, dt);
            let output = fg.create_texture("output", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
            tonemap.add_composite_pass(
                fg,
                &bindless,
                scene,
                output,
                Composite {
                    tonemap: false,
                    auto_exposure: Some(buffer),
                    ..Composite::default()
                },
            );
            output
        });
        // A background pixel (grey; the bottom right corner) over its value: the exposure.
        out[out.len() - 1][1] / image.background[1]
    };
    let grey = |l: f32| Image {
        background: [l; 3],
        value: [0.0; 3],
        rect: [0; 4],
    };
    // First frame: straight to the target.
    let e = expose(grey(4.0), 0.016);
    println!(
        "uniform luminance 4: exposure {e} (expected {})",
        0.18 / 4.0
    );
    assert!((e * 4.0 - 0.18).abs() < 0.18 * 0.04, "{e}");
    // 5% of the pixels at 1000: above the 90th percentile, they barely count.
    exposure.reset();
    let bright = Image {
        background: [4.0; 3],
        value: [1000.0; 3],
        rect: [0, 0, WIDTH, HEIGHT / 20],
    };
    let e_bright = expose(bright, 0.016);
    println!("with 5% at 1000: exposure {e_bright}");
    assert!((e_bright - e).abs() < e * 0.05, "{e_bright} vs {e}");
    // A change of scene: 1 EV up, with the rate of `speed_up` over dt.
    exposure.reset();
    let e0 = expose(grey(4.0), 0.016);
    let dt = 0.1;
    let e1 = expose(grey(1.0), dt);
    let (ev0, target) = (e0.log2(), (0.18f32 / 1.0).log2());
    let expected =
        ev0 + (target - ev0) * (1.0 - (-dt * ExposureSettings::default().speed_up).exp());
    println!(
        "adapting: {} EV after {dt} s (expected {expected})",
        e1.log2()
    );
    assert!(
        (e1.log2() - expected).abs() < 0.06,
        "{} vs {expected}",
        e1.log2()
    );

    let mut exposure = exposure;
    // SAFETY: the device is idle.
    unsafe {
        exposure.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(exposure);
    drop(bloom);
    drop(tonemap);
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
