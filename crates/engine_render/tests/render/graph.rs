//! Render graph on a real device under the validation layer with synchronization
//! validation: any missing or wrong barrier is reported as an error and fails the test.
//! Runs on lavapipe in containers and on NVIDIA and AMD on ref-1.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_render::graph::{
    Access, BufferDesc, BufferState, ImportedBuffer, LoadOp, RenderGraph, StoreOp, TextureDesc,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc, MemoryLocation};
use std::cell::Cell;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render tests"),
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

const SIZE: u32 = 64;
const PIXELS: u64 = (SIZE * SIZE) as u64;

fn copy_region() -> vk::ImageCopy {
    let layer = vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    };
    vk::ImageCopy {
        src_subresource: layer,
        src_offset: vk::Offset3D::default(),
        dst_subresource: layer,
        dst_offset: vk::Offset3D::default(),
        extent: vk::Extent3D {
            width: SIZE,
            height: SIZE,
            depth: 1,
        },
    }
}

#[test]
fn passes_culling_aliasing_and_barriers_are_hazard_free() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut graph = RenderGraph::new(&device, 2);

    // Readback: first the image (as RGBA8), then the filled buffer.
    let readback_size = PIXELS * 4 + 1024;
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();

    // SAFETY: plain object creation on a live device.
    let (pool, cmds) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmds = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(2),
            )
            .unwrap();
        (pool, cmds)
    };

    let rgba = vk::Format::R8G8B8A8_UNORM;
    for frame in 0..6u32 {
        let slot = (frame % 2) as usize;
        let cmd = cmds[slot];
        let culled_ran = Cell::new(false);
        let red = [1.0, 0.0, 0.0, 1.0];
        let green = [0.0, 1.0, 0.0, 1.0];
        // SAFETY: the device is idle between frames in this test.
        unsafe {
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
        }
        {
            // SAFETY: the previous submission of this slot completed (wait_idle below).
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
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
            let a = fg.create_texture("a", TextureDesc::new_2d(rgba, SIZE, SIZE));
            let b = fg.create_texture("b", TextureDesc::new_2d(rgba, SIZE, SIZE));
            let c = fg.create_texture("c", TextureDesc::new_2d(rgba, SIZE, SIZE));
            let unused = fg.create_texture("unused", TextureDesc::new_2d(rgba, SIZE, SIZE));
            let data = fg.create_buffer("data", BufferDesc { size: 1024 });

            // a = red (attachment clear, no callback needed).
            fg.add_pass("clear a")
                .color(a, LoadOp::Clear(red), StoreOp::Store)
                .execute(|_| {});
            // Culled: nobody reads `unused`.
            fg.add_pass("culled")
                .color(unused, LoadOp::Clear(green), StoreOp::Store)
                .execute(|_| culled_ran.set(true));
            // b = a (copy). After this `a` is dead; `c` can reuse its memory.
            fg.add_pass("copy a to b")
                .read(a, Access::TransferRead)
                .write(b, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; images in the layouts the graph arranged.
                    unsafe {
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            ctx.image(a),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.image(b),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[copy_region()],
                        );
                    }
                });
            // c = b with its top-left quarter cleared green by a second render pass that
            // loads it (tests attachment load after a transfer write).
            fg.add_pass("copy b to c")
                .read(b, Access::TransferRead)
                .write(c, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: as above.
                    unsafe {
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            ctx.image(b),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.image(c),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[copy_region()],
                        );
                    }
                });
            fg.add_pass("green corner")
                .color(c, LoadOp::Load, StoreOp::Store)
                .execute(move |ctx| {
                    let clear = vk::ClearAttachment {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        color_attachment: 0,
                        clear_value: vk::ClearValue {
                            color: vk::ClearColorValue { float32: green },
                        },
                    };
                    let rect = vk::ClearRect {
                        rect: vk::Rect2D {
                            offset: vk::Offset2D::default(),
                            extent: vk::Extent2D {
                                width: SIZE / 2,
                                height: SIZE / 2,
                            },
                        },
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    // SAFETY: inside the pass's rendering scope.
                    unsafe { ctx.device.cmd_clear_attachments(ctx.cmd, &[clear], &[rect]) };
                });
            fg.add_pass("fill data")
                .write(data, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; the buffer is at least 1024 bytes.
                    unsafe {
                        ctx.device
                            .cmd_fill_buffer(ctx.cmd, ctx.buffer(data), 0, 1024, 0xABCD_1234);
                    }
                });
            fg.add_pass("read back")
                .read(c, Access::TransferRead)
                .read(data, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        buffer_offset: 0,
                        buffer_row_length: 0,
                        buffer_image_height: 0,
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_offset: vk::Offset3D::default(),
                        image_extent: vk::Extent3D {
                            width: SIZE,
                            height: SIZE,
                            depth: 1,
                        },
                    };
                    // SAFETY: as above.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(c),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(data),
                            ctx.buffer(out),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: PIXELS * 4,
                                size: 1024,
                            }],
                        );
                    }
                });
            fg.execute(&mut allocator).unwrap();
        }
        // SAFETY: one-shot submission, then wait for idle.
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
        let stats = graph.stats();
        println!("frame {frame} slot {slot}: {stats:?}");
        let times = graph.gpu_pass_times();
        if frame >= 2 {
            // The previous frame of this slot: every needed pass, in order.
            let names: Vec<&str> = times.iter().map(|t| t.name).collect();
            assert_eq!(
                names,
                [
                    "clear a",
                    "copy a to b",
                    "copy b to c",
                    "green corner",
                    "fill data",
                    "read back"
                ]
            );
            assert!(times.iter().all(|t| t.gpu_us >= 0.0 && t.gpu_us < 1e6));
            println!("GPU µs: {times:?}");
        }
        assert!(!culled_ran.get(), "the culled pass ran");
        assert_eq!(stats.culled_passes, 1);
        // a, b, c and the buffer; `unused` was never created.
        assert_eq!(stats.transient_resources, 4);
        if frame >= 2 {
            assert_eq!(
                stats.created_objects, 0,
                "slot {slot} should reuse its objects"
            );
        }

        let bytes = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers `readback_size` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), readback_size as usize) };
        let pixel = |x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        assert_eq!(pixel(1, 1), [0, 255, 0, 255], "green corner");
        assert_eq!(pixel(SIZE - 1, SIZE - 1), [255, 0, 0, 255], "red elsewhere");
        let filled = &bytes[(PIXELS * 4) as usize..(PIXELS * 4) as usize + 4];
        assert_eq!(u32::from_le_bytes(filled.try_into().unwrap()), 0xABCD_1234);
    }
    let stats = graph.stats();
    // `a` and `c` do not overlap in time, so they share memory: the images need two
    // images' worth, not three.
    assert!(
        stats.transient_placed_bytes < stats.transient_unaliased_bytes,
        "{stats:?}"
    );

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, None);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}

/// A pass that follows a long one with no barrier between them (they share nothing) has
/// its opening timestamp written while the long one still runs: its time must be its own,
/// not the long pass's tail (core review 2: a 49 µs pass read 2.4 ms). The passes' times
/// add up to no more than the frame.
#[test]
fn a_pass_after_a_long_one_is_timed_alone() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut graph = RenderGraph::new(&device, 1);
    const LONG: u64 = 256 << 20;
    const SHORT: u64 = 256;
    let buffer = |allocator: &mut Allocator, size| {
        allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Device,
            )
            .unwrap()
    };
    // Imported, so that the graph neither culls the passes nor lets the buffers share
    // memory (which would put a barrier between them).
    let (long, long_mem) = buffer(&mut allocator, LONG);
    let (short, short_mem) = buffer(&mut allocator, SHORT);
    // SAFETY: plain object creation on a live device.
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
    for frame in 0..3u32 {
        // SAFETY: the device is idle between frames in this test.
        unsafe {
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
        }
        {
            // SAFETY: the previous submission completed (wait_idle below).
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            let import = |fg: &mut engine_render::graph::FrameGraph<'_>, name, buffer, size| {
                fg.import_buffer(
                    name,
                    ImportedBuffer {
                        buffer,
                        size,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: None,
                    },
                )
            };
            let long = import(&mut fg, "long", long, LONG);
            let short = import(&mut fg, "short", short, SHORT);
            fg.add_pass("long fill")
                .write(long, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; the buffer holds `LONG` bytes.
                    unsafe {
                        ctx.device
                            .cmd_fill_buffer(ctx.cmd, ctx.buffer(long), 0, LONG, 1);
                    }
                });
            fg.add_pass("short fill")
                .write(short, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; the buffer holds `SHORT` bytes.
                    unsafe {
                        ctx.device
                            .cmd_fill_buffer(ctx.cmd, ctx.buffer(short), 0, SHORT, 2);
                    }
                });
            fg.execute(&mut allocator).unwrap();
        }
        // SAFETY: one-shot submission, then wait for idle.
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
        if frame == 0 {
            continue;
        }
        // The previous frame's times.
        let times = graph.gpu_pass_times();
        println!("GPU µs: {times:?}, frame {:?}", graph.gpu_frame_time());
        let names: Vec<&str> = times.iter().map(|t| t.name).collect();
        assert_eq!(names, ["long fill", "short fill"]);
        let (long, short) = (times[0], times[1]);
        assert!(times.iter().all(|t| t.gpu_us <= t.span_us + 0.01));
        assert!(
            short.gpu_us < long.gpu_us / 4.0,
            "the short fill read {} µs (span {}) after a {} µs fill",
            short.gpu_us,
            short.span_us,
            long.gpu_us
        );
        if let Some(frame_us) = graph.gpu_frame_time() {
            let sum: f32 = times.iter().map(|t| t.gpu_us).sum();
            assert!(
                sum <= frame_us + 1.0,
                "passes {sum} µs, frame {frame_us} µs"
            );
        }
    }
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, None);
        allocator.destroy_buffer(long, long_mem);
        allocator.destroy_buffer(short, short_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
