//! Viewports through the render graph under the validation layer with synchronization
//! validation: a child viewport shown inside its parent (rendered first, published for
//! bindless sampling), a render tick (the child keeps its contents between updates), split
//! screen, pixel-perfect integer scaling with letterboxing, and per-viewport GPU times.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::post::Tonemap;
use engine_render::upload::UploadRing;
use engine_render::viewport::{
    Placement, Scaling, UpdateMode, ViewportDesc, ViewportSize, Viewports,
};
use engine_render2d::{Canvas, Quad, QuadMode, Renderer2d, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
    SamplerKind,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("viewports test"),
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
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

#[test]
fn nested_split_screen_and_pixel_perfect() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut renderer = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        FORMAT,
        QuadMode::for_device(&device),
    )
    .unwrap();
    let tonemap = Tonemap::new(&device, &bindless, FORMAT).unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut viewports = Viewports::new(&device, 1);

    // Left half: the "world", which shows the minimap (its child) as a sprite.
    let ldr = |desc: ViewportDesc| ViewportDesc {
        format: FORMAT,
        tonemap: false,
        ..desc
    };
    let world = viewports.create(ldr(ViewportDesc {
        name: String::from("world"),
        placement: Some(Placement {
            rect: [0.0, 0.0, 0.5, 1.0],
            order: 0,
        }),
        clear: Some([0.0, 0.0, 1.0, 1.0]),
        ..ViewportDesc::default()
    }));
    // The minimap renders on odd frames only.
    let minimap = viewports.create(ldr(ViewportDesc {
        name: String::from("minimap"),
        size: ViewportSize::Fixed {
            width: 32,
            height: 32,
        },
        parent: Some(world),
        placement: None,
        update: UpdateMode::Every { n: 2, offset: 1 },
        clear: Some([0.0, 1.0, 0.0, 1.0]),
        ..ViewportDesc::default()
    }));
    // Right half: 16 × 16 pixel art, integer scaled (4×) and letterboxed.
    let pixel_art = viewports.create(ldr(ViewportDesc {
        name: String::from("pixel art"),
        size: ViewportSize::Fixed {
            width: 16,
            height: 16,
        },
        placement: Some(Placement {
            rect: [0.5, 0.0, 0.5, 1.0],
            order: 1,
        }),
        scaling: Scaling::PixelPerfect,
        clear: Some([1.0, 0.0, 0.0, 1.0]),
        ..ViewportDesc::default()
    }));

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

    let mut world_canvas = Canvas::new();
    let mut minimap_canvas = Canvas::new();
    let mut pixel_canvas = Canvas::new();
    let mut world_times = Vec::new();
    let mut minimap_times = Vec::new();
    let mut images = Vec::new();
    for frame_number in 1..=4u64 {
        // The minimap turns from green to red from frame 3 on.
        let minimap_color = if frame_number >= 3 {
            [1.0, 0.0, 0.0, 1.0]
        } else {
            [0.0, 1.0, 0.0, 1.0]
        };
        viewports.desc_mut(minimap).unwrap().clear = Some(minimap_color);

        // Content: the world shows the minimap at (16..48, 16..48) of its 64 × 128 pixels
        // (from frame 2 on: the minimap has no texture before its first render).
        world_canvas.clear();
        if let Some(index) = viewports.texture_index(minimap) {
            world_canvas.draw(
                &Quad::sprite(index, Vec2::splat(32.0), Vec2::splat(32.0))
                    .sampler(SamplerKind::NearestClamp),
            );
        }
        world_canvas.prepare();
        minimap_canvas.clear();
        minimap_canvas.prepare();
        // One white pixel at the pixel-art viewport's top-left corner.
        pixel_canvas.clear();
        pixel_canvas.draw(&Quad::rect(Vec2::splat(0.5), Vec2::ONE).color(Vec4::ONE));
        pixel_canvas.prepare();

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
            let (world_c, minimap_c, pixel_c) = (&world_canvas, &minimap_canvas, &pixel_canvas);
            let (renderer_ref, bindless_ref) = (&renderer, &bindless);
            let ring_ref = &mut ring;
            viewports
                .render(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &bindless,
                    (SIZE, SIZE),
                    frame_number,
                    |fg, vp| {
                        let canvas = if vp.id == world {
                            world_c
                        } else if vp.id == minimap {
                            minimap_c
                        } else {
                            pixel_c
                        };
                        let clear = match vp.load {
                            engine_render::graph::LoadOp::Clear(c) => Some(c),
                            _ => None,
                        };
                        renderer_ref
                            .add_pass(
                                fg,
                                ring_ref,
                                bindless_ref,
                                canvas,
                                &View2d::pixels(vp.width, vp.height),
                                vp.color,
                                clear,
                            )
                            .unwrap();
                    },
                )
                .unwrap();
            let output = fg.create_texture("output", TextureDesc::new_2d(FORMAT, SIZE, SIZE));
            viewports.composite(
                &mut fg,
                &tonemap,
                &bindless,
                output,
                (SIZE, SIZE),
                frame_number,
                [0.0, 0.0, 0.0, 1.0],
            );
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
        // Times read during this frame's execution belong to the previous frame.
        world_times.push(viewports.gpu_time_us(&graph, world));
        minimap_times.push(viewports.gpu_time_us(&graph, minimap));
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        images.push(bytes.to_vec());
    }

    let px = |frame: usize, x: u32, y: u32| {
        let image = &images[frame];
        let i = ((y * SIZE + x) * 4) as usize;
        [image[i], image[i + 1], image[i + 2], image[i + 3]]
    };
    let (blue, green, red, white, black) = (
        [0, 0, 255, 255],
        [0, 255, 0, 255],
        [255, 0, 0, 255],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
    );
    let checks = [
        // Frame 1: the minimap renders but the world does not show it yet.
        ("frame 1: world background", 0, (8, 8), blue),
        ("frame 1: world, no minimap yet", 0, (32, 32), blue),
        // Frame 2: minimap not updated (render tick), shown with its frame-1 contents.
        ("frame 2: minimap from frame 1", 1, (32, 32), green),
        ("frame 2: world around the minimap", 1, (8, 8), blue),
        // Frame 3: the minimap renders red before the world samples it.
        (
            "frame 3: minimap updated in the same frame",
            2,
            (32, 32),
            red,
        ),
        // Frame 4: not updated again, keeps red.
        ("frame 4: minimap kept", 3, (32, 32), red),
        // Pixel art: 16 × 16 at 4× → (64, 32)..(128, 96); pixel (0, 0) is white.
        ("pixel art: scaled top-left pixel", 3, (64, 32), white),
        (
            "pixel art: scaled top-left pixel, far corner",
            3,
            (67, 35),
            white,
        ),
        ("pixel art: next pixel", 3, (68, 36), red),
        ("pixel art: letterbox above", 3, (96, 10), black),
        ("pixel art: letterbox below", 3, (96, 120), black),
        ("pixel art: inside", 3, (100, 60), red),
    ];
    let mut failures = Vec::new();
    for (what, frame, (x, y), expected) in checks {
        let got = px(frame, x, y);
        println!("{what} at ({x}, {y}): {got:?} (expected {expected:?})");
        if got != expected {
            failures.push(format!("{what}: {got:?}, expected {expected:?}"));
        }
    }
    println!("GPU µs per frame (previous frame): world {world_times:?}, minimap {minimap_times:?}");
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(viewports.extent(pixel_art), (16, 16));
    assert_eq!(
        viewports.extent(world),
        (64, 128),
        "placement-sized: half the output"
    );
    // Timestamps: the world renders every frame; the minimap only on odd frames, so the
    // times read in frames 2 and 4 (of frames 1 and 3) are non-zero and those read in
    // frame 3 (of frame 2) are zero.
    if world_times[1] > 0.0 {
        assert!(
            minimap_times[1] > 0.0 && minimap_times[3] > 0.0,
            "{minimap_times:?}"
        );
        assert_eq!(minimap_times[2], 0.0, "{minimap_times:?}");
    }

    // SAFETY: the device is idle.
    unsafe {
        viewports.destroy_all(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        renderer.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(tonemap);
    drop(renderer);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
