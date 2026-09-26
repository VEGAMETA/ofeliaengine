//! Sprites and SDF shapes through the render graph under the validation layer with
//! synchronization validation: texture upload, orientation, shapes, blending, clipping and
//! draw order, checked pixel by pixel; the three quad modes must produce the same image.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::texture::GpuTexture;
use engine_render::upload::UploadRing;
use engine_render2d::{Blend2d, Canvas, ClipRect, Order, Quad, QuadMode, Renderer2d, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
    SamplerKind,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render2d test"),
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

const SIZE: u32 = 128;
const RED: Vec4 = Vec4::new(1.0, 0.0, 0.0, 1.0);
const GREEN: Vec4 = Vec4::new(0.0, 1.0, 0.0, 1.0);
const BLUE: Vec4 = Vec4::new(0.0, 0.0, 1.0, 1.0);

/// The test scene in pixel space (y down).
fn scene(canvas: &mut Canvas, texture: u32) {
    canvas.clear();
    // 2×2 texture (red green / blue white) stretched over 32×32 pixels at (8, 8).
    canvas.draw(
        &Quad::sprite(texture, Vec2::new(24.0, 24.0), Vec2::splat(32.0))
            .sampler(SamplerKind::NearestClamp),
    );
    // A disc and a hollow rounded rectangle.
    canvas.draw(&Quad::circle(Vec2::new(80.0, 30.0), 16.0).color(RED));
    canvas.draw(
        &Quad::rounded_rect(Vec2::new(80.0, 90.0), Vec2::new(40.0, 30.0), 6.0)
            .color(GREEN)
            .outline(3.0),
    );
    // Additive light over the background, two quads that overlap in the middle.
    let glow = Vec4::new(0.5, 0.0, 0.0, 1.0);
    canvas.draw(
        &Quad::rect(Vec2::new(20.0, 64.0), Vec2::new(16.0, 8.0))
            .color(glow)
            .blend(Blend2d::Additive),
    );
    canvas.draw(
        &Quad::rect(Vec2::new(28.0, 64.0), Vec2::new(16.0, 8.0))
            .color(glow)
            .blend(Blend2d::Additive),
    );
    // Multiply darkens the background.
    canvas.draw(
        &Quad::rect(Vec2::new(56.0, 64.0), Vec2::new(8.0, 8.0))
            .color(Vec4::new(0.5, 0.5, 0.5, 1.0))
            .blend(Blend2d::Multiply),
    );
    // Order: blue on layer 1 is submitted first but drawn over red on layer 0.
    canvas.draw(
        &Quad::rect(Vec2::new(12.0, 100.0), Vec2::splat(8.0))
            .color(BLUE)
            .order(Order::layer(1)),
    );
    canvas.draw(&Quad::rect(Vec2::new(12.0, 100.0), Vec2::splat(8.0)).color(RED));
    // A capsule line.
    canvas.draw(&Quad::line(
        Vec2::new(20.0, 110.0),
        Vec2::new(50.0, 124.0),
        4.0,
    ));
    // A screen-sized quad clipped to a small rectangle.
    canvas.set_clip(Some(ClipRect {
        x: 100,
        y: 100,
        width: 20,
        height: 20,
    }));
    canvas.draw(
        &Quad::rect(Vec2::splat(64.0), Vec2::splat(128.0))
            .color(Vec4::new(1.0, 1.0, 0.0, 1.0))
            .order(Order::layer(5)),
    );
    canvas.set_clip(None);
}

#[test]
fn sprites_and_shapes() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut texture = GpuTexture::new(
        &device,
        &mut allocator,
        &bindless,
        vk::Format::R8G8B8A8_UNORM,
        2,
        2,
    )
    .unwrap();
    let texels: [u8; 16] = [
        255, 0, 0, 255, 0, 255, 0, 255, // top row: red, green
        0, 0, 255, 255, 255, 255, 255, 255, // bottom row: blue, white
    ];

    let readback_size = u64::from(SIZE * SIZE * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
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

    let mut canvas = Canvas::new();
    let mut images: Vec<(QuadMode, Vec<u8>)> = Vec::new();
    // The test scene in each mode, then a scene already in draw order (instances read
    // directly instead of through the order array).
    let runs = [
        (QuadMode::Indexed, false),
        (QuadMode::List, false),
        (QuadMode::Instanced, false),
        (QuadMode::Indexed, true),
    ];
    for (frame, (mode, in_order)) in runs.into_iter().enumerate() {
        let mut renderer =
            Renderer2d::new(&device, &mut allocator, &bindless, color_format, mode).unwrap();
        if in_order {
            canvas.clear();
            canvas.draw(&Quad::rect(Vec2::splat(64.0), Vec2::splat(40.0)).color(RED));
            canvas.draw(&Quad::rect(Vec2::splat(64.0), Vec2::splat(20.0)).color(GREEN));
        } else {
            scene(&mut canvas, texture.sampled_index());
        }
        canvas.prepare();
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
            if frame == 0 {
                texture.upload(&mut fg, &mut ring, &texels).unwrap();
            }
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, SIZE, SIZE));
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: readback_size,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            renderer
                .add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &canvas,
                    &View2d::pixels(SIZE, SIZE),
                    color,
                    Some([0.0, 0.0, 0.25, 1.0]),
                )
                .unwrap();
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
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
                            width: SIZE,
                            height: SIZE,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
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
            renderer.destroy(&mut allocator);
        }
        println!("{mode:?}: {:?}", canvas.stats());
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        assert_eq!(canvas.stats().presorted, in_order);
        images.push((mode, bytes.to_vec()));
    }
    let in_order = images.pop().unwrap().1;
    let at = |x: u32, y: u32| {
        let i = ((y * SIZE + x) * 4) as usize;
        [
            in_order[i],
            in_order[i + 1],
            in_order[i + 2],
            in_order[i + 3],
        ]
    };
    assert_eq!(
        at(64, 64),
        [0, 255, 0, 255],
        "in-order scene: later quad on top"
    );
    assert_eq!(at(50, 64), [255, 0, 0, 255], "in-order scene: earlier quad");

    let (_, image) = &images[0];
    let px = |x: u32, y: u32| {
        let i = ((y * SIZE + x) * 4) as usize;
        [image[i], image[i + 1], image[i + 2], image[i + 3]]
    };
    let background = [0, 0, 64, 255];
    let checks: [(&str, (u32, u32), [u8; 4]); 16] = [
        ("background", (0, 0), background),
        ("texture top-left (upright)", (16, 16), [255, 0, 0, 255]),
        ("texture top-right", (32, 16), [0, 255, 0, 255]),
        ("texture bottom-left", (16, 32), [0, 0, 255, 255]),
        ("texture bottom-right", (32, 32), [255, 255, 255, 255]),
        ("disc center", (80, 30), [255, 0, 0, 255]),
        ("outside the disc, inside its quad", (94, 16), background),
        ("hollow center", (80, 90), background),
        ("outline", (80, 76), [0, 255, 0, 255]),
        ("one additive quad", (14, 64), [128, 0, 64, 255]),
        ("two additive quads", (24, 64), [255, 0, 64, 255]),
        ("multiply", (56, 64), [0, 0, 32, 255]),
        ("higher layer drawn last", (12, 100), [0, 0, 255, 255]),
        ("line", (35, 117), [255, 255, 255, 255]),
        ("inside the clip", (110, 110), [255, 255, 0, 255]),
        ("outside the clip", (96, 110), background),
    ];
    let mut failures = Vec::new();
    for (what, (x, y), expected) in checks {
        let got = px(x, y);
        let close = got
            .iter()
            .zip(expected)
            .all(|(&g, e)| (i32::from(g) - i32::from(e)).abs() <= 2);
        println!("{what} at ({x}, {y}): {got:?} (expected {expected:?})");
        if !close {
            failures.push(format!(
                "{what} at ({x}, {y}): {got:?}, expected {expected:?}"
            ));
        }
    }
    // The SDF edge is antialiased: some pixels of the disc's bounding box are partially
    // covered.
    let partial = (14..46)
        .flat_map(|y| (64..96).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let r = px(x, y)[0];
            r > 20 && r < 235
        })
        .count();
    println!("partially covered pixels on the disc's rim: {partial}");
    assert!(partial > 16, "the disc's edge is not antialiased");
    for (mode, other) in &images[1..] {
        let differing = image
            .chunks_exact(4)
            .zip(other.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        println!("{mode:?} differs from Indexed in {differing} pixels");
        assert_eq!(differing, 0, "{mode:?} differs from the indexed image");
    }
    assert!(failures.is_empty(), "{failures:#?}");

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        texture.destroy(&device, &mut allocator, &bindless, 0);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
