//! Text through the 2D renderer under the validation layer with synchronization
//! validation: the glyph atlas is uploaded whole, then only the rows that changed, and the
//! rendered pixels must match the atlas coverage composited on the CPU.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::upload::UploadRing;
use engine_render2d::{Canvas, GlyphTexture, Order, QuadMode, Renderer2d, View2d, draw_text};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};
use engine_text::{Fonts, GlyphAtlas, GlyphKey, TextLayout, TextStyle};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render2d text test"),
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

const W: u32 = 160;
const H: u32 = 48;
const SIZE: f32 = 18.0;

/// What the GPU should produce: white glyphs composited over black, in draw order.
fn expected(layout: &TextLayout, fonts: &Fonts, atlas: &mut GlyphAtlas, origin: Vec2) -> Vec<f32> {
    let mut img = vec![0.0f32; (W * H) as usize];
    let (aw, _) = atlas.size();
    for g in layout.glyphs() {
        let (key, x) = GlyphKey::at(g.font, g.glyph, SIZE, origin.x + g.x);
        let Some(a) = atlas.get(fonts, key) else {
            continue;
        };
        let left = x + i32::from(a.left);
        let top = (origin.y + g.y).round() as i32 - i32::from(a.top);
        for row in 0..i32::from(a.height) {
            for col in 0..i32::from(a.width) {
                let (px, py) = (left + col, top + row);
                if px < 0 || py < 0 || px >= W as i32 || py >= H as i32 {
                    continue;
                }
                let cov = f32::from(
                    atlas.pixels()[((u32::from(a.y) + row as u32) * aw
                        + u32::from(a.x)
                        + col as u32) as usize],
                ) / 255.0;
                let d = &mut img[(py as u32 * W + px as u32) as usize];
                *d = cov + *d * (1.0 - cov);
            }
        }
    }
    img
}

#[test]
fn text_matches_atlas_coverage() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut renderer = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        color_format,
        QuadMode::for_device(&device),
    )
    .unwrap();

    let mut fonts = Fonts::new();
    let go = fonts.load(engine_text::fonts::GO_REGULAR, 0).unwrap();
    let mut atlas = GlyphAtlas::new(256, 256);
    let mut glyphs = GlyphTexture::new(&device, &mut allocator, &bindless, &atlas).unwrap();
    let mut layout = TextLayout::new();
    let style = TextStyle {
        size: SIZE,
        ..TextStyle::default()
    };

    let readback_size = u64::from(W * H * 4);
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
    // Frame 0 uploads the whole atlas; frame 1 adds Cyrillic glyphs (a row-band upload).
    for (frame, text) in ["Hi, text 0.5!", "Привет, мир 42"].into_iter().enumerate() {
        let origin = Vec2::new(4.3, 6.0);
        layout.layout(&mut fonts, &[go], text, &style);
        canvas.clear();
        let n = draw_text(
            &mut canvas,
            &layout,
            &fonts,
            &mut atlas,
            &glyphs,
            origin,
            SIZE,
            Vec4::ONE,
            Order::layer(0),
        );
        assert!(n >= text.chars().filter(|c| !c.is_whitespace()).count());
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
            glyphs.sync(&mut fg, &mut ring, &mut atlas).unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, W, H));
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
                    &View2d::pixels(W, H),
                    color,
                    Some([0.0, 0.0, 0.0, 1.0]),
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
                            width: W,
                            height: H,
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
        }
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let want = expected(&layout, &fonts, &mut atlas, origin);
        let mut worst = 0i32;
        let mut ink = 0u32;
        for (i, &w) in want.iter().enumerate() {
            let got = i32::from(bytes[i * 4]);
            ink += got as u32;
            worst = worst.max((got - (w * 255.0).round() as i32).abs());
        }
        println!("frame {frame} {text:?}: {n} glyph quads, ink {ink}, worst pixel error {worst}");
        assert!(ink > 1000, "text is visible");
        assert!(
            worst <= 2,
            "GPU pixels match the atlas coverage (worst error {worst})"
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        renderer.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        glyphs
            .texture_mut()
            .destroy(&device, &mut allocator, &bindless, 0);
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
