//! Async compute in the render graph (ADR 0065) on a real device, under the validation
//! layer with synchronization validation (which checks hazards across queues at submit):
//! a frame whose transfer passes run on the async compute queue between graphics passes,
//! with transients and imported resources crossing queues, aliasing, two frame slots and
//! both ways of sharing resources between queue families. Every configuration must read
//! back the same bytes as the single-queue frame. On devices without an async compute
//! queue, the graph falls back to one queue and the test checks that.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_render::graph::{
    Access, AsyncSharing, BufferDesc, BufferState, ImportedBuffer, ImportedTexture, LoadOp,
    QueueKind, RenderGraph, StoreOp, TextureDesc, TextureState,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc, MemoryLocation};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render async compute test"),
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

const SIZE: u32 = 32;
const PIXELS: u64 = (SIZE * SIZE) as u64;
const RGBA: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Readback layout: three images, then two buffers' first 256 bytes.
const READBACK: u64 = PIXELS * 4 * 3 + 512;

fn layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn whole() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn image_copy() -> vk::ImageCopy {
    vk::ImageCopy {
        src_subresource: layers(),
        src_offset: vk::Offset3D::default(),
        dst_subresource: layers(),
        dst_offset: vk::Offset3D::default(),
        extent: vk::Extent3D {
            width: SIZE,
            height: SIZE,
            depth: 1,
        },
    }
}

fn to_buffer(offset: u64) -> vk::BufferImageCopy {
    vk::BufferImageCopy {
        buffer_offset: offset,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: layers(),
        image_offset: vk::Offset3D::default(),
        image_extent: vk::Extent3D {
            width: SIZE,
            height: SIZE,
            depth: 1,
        },
    }
}

/// Renders 6 frames on 2 slots and returns each frame's readback.
#[allow(clippy::too_many_arguments)]
fn run(
    device: &Device,
    allocator: &mut Allocator,
    async_compute: bool,
    sharing: AsyncSharing,
    history: vk::Image,
    history_view: vk::ImageView,
    counter: vk::Buffer,
    readback: vk::Buffer,
    readback_bytes: *const u8,
) -> Vec<Vec<u8>> {
    let d = device.raw();
    let mut graph = RenderGraph::new(device, 2);
    graph.set_async_compute(async_compute);
    graph.set_async_sharing(sharing);
    let on = graph.async_compute();
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
    // The persistent texture's state between frames, as its owner tracks it.
    let mut history_state = TextureState::undefined();
    let mut frames = Vec::new();
    for frame in 0..6u32 {
        let slot = (frame % 2) as usize;
        let cmd = cmds[slot];
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
                    size: READBACK,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let history = fg.import_texture(
                "history",
                ImportedTexture {
                    image: history,
                    view: history_view,
                    desc: TextureDesc::new_2d(RGBA, SIZE, SIZE),
                    initial: history_state,
                    final_state: Some(TextureState::transfer_source()),
                    sampled_index: None,
                    storage_index: None,
                },
            );
            history_state = TextureState::transfer_source();
            let counter = fg.import_buffer(
                "counter",
                ImportedBuffer {
                    buffer: counter,
                    size: 256,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: None,
                },
            );
            let a = fg.create_texture("a", TextureDesc::new_2d(RGBA, SIZE, SIZE));
            let b = fg.create_texture("b", TextureDesc::new_2d(RGBA, SIZE, SIZE));
            let c = fg.create_texture("c", TextureDesc::new_2d(RGBA, SIZE, SIZE));
            let e = fg.create_texture("e", TextureDesc::new_2d(RGBA, SIZE, SIZE));
            let data = fg.create_buffer("data", BufferDesc { size: 256 });
            let shade = (frame * 40) as f32 / 255.0;

            // Graphics: a = red.
            fg.add_pass("clear a")
                .color(a, LoadOp::Clear([1.0, 0.0, 0.0, 1.0]), StoreOp::Store)
                .execute(|_| {});
            // Graphics, declared before the async work it overlaps: c = green.
            fg.add_pass("clear c")
                .color(c, LoadOp::Clear([0.0, 1.0, shade, 1.0]), StoreOp::Store)
                .execute(|_| {});
            // Async: b = a (a transient crossing queues both ways).
            fg.add_pass("copy a to b")
                .read(a, Access::TransferRead)
                .write(b, Access::TransferWrite)
                .async_compute()
                .execute(move |ctx| {
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            ctx.image(a),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.image(b),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[image_copy()],
                        );
                    }
                });
            // Async: a transient buffer and the imported ones.
            fg.add_pass("fill data")
                .write(data, Access::TransferWrite)
                .write(counter, Access::TransferWrite)
                .async_compute()
                .execute(move |ctx| {
                    // SAFETY: recording; both buffers hold 256 bytes.
                    unsafe {
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(data),
                            0,
                            256,
                            0xA5A5_0000 + frame,
                        );
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(counter),
                            0,
                            256,
                            0xC0DE_0000 + frame,
                        );
                    }
                });
            fg.add_pass("clear history")
                .write(history, Access::TransferWrite)
                .async_compute()
                .execute(move |ctx| {
                    // SAFETY: recording; the image is in TRANSFER_DST_OPTIMAL.
                    unsafe {
                        ctx.device.cmd_clear_color_image(
                            ctx.cmd,
                            ctx.image(history),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &vk::ClearColorValue {
                                float32: [shade, 0.0, 1.0, 1.0],
                            },
                            &[whole()],
                        );
                    }
                });
            // Graphics after the join: e = b, then everything read back.
            fg.add_pass("copy b to e")
                .read(b, Access::TransferRead)
                .write(e, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: as above.
                    unsafe {
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            ctx.image(b),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.image(e),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[image_copy()],
                        );
                    }
                });
            fg.add_pass("read back")
                .read(c, Access::TransferRead)
                .read(e, Access::TransferRead)
                .read(history, Access::TransferRead)
                .read(data, Access::TransferRead)
                .read(counter, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    let image_bytes = PIXELS * 4;
                    // SAFETY: as above; the readback holds all of it.
                    unsafe {
                        for (i, image) in [c, e, history].into_iter().enumerate() {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(image),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &[to_buffer(i as u64 * image_bytes)],
                            );
                        }
                        for (i, buffer) in [data, counter].into_iter().enumerate() {
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                ctx.buffer(buffer),
                                ctx.buffer(out),
                                &[vk::BufferCopy {
                                    src_offset: 0,
                                    dst_offset: 3 * image_bytes + i as u64 * 256,
                                    size: 256,
                                }],
                            );
                        }
                    }
                });
            fg.execute(allocator).unwrap();
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
        if on {
            assert_eq!(stats.async_passes, 3, "{stats:?}");
            assert!(stats.submits >= 2, "{stats:?}");
            if sharing == AsyncSharing::Exclusive {
                assert!(stats.ownership_transfers > 0, "{stats:?}");
            }
        } else {
            assert_eq!((stats.async_passes, stats.submits), (0, 0), "{stats:?}");
        }
        if frame >= 2 {
            assert_eq!(stats.created_objects, 0, "slot {slot}: {stats:?}");
            let times = graph.gpu_pass_times();
            assert_eq!(times.len(), 7, "{times:?}");
            let compute = times
                .iter()
                .filter(|t| t.queue == QueueKind::AsyncCompute)
                .count();
            assert_eq!(compute, if on { 3 } else { 0 }, "{times:?}");
            assert!(graph.gpu_frame_time().is_some_and(|us| us > 0.0));
        }
        // SAFETY: the GPU is idle; the mapping covers the readback.
        let bytes = unsafe { core::slice::from_raw_parts(readback_bytes, READBACK as usize) };
        frames.push(bytes.to_vec());
    }
    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(allocator, None);
        d.destroy_command_pool(pool, None);
    }
    frames
}

#[test]
fn async_compute_frames_match_the_single_queue_frames_without_hazards() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!(
        "adapter: {}; async compute queue: {:?}",
        device.adapter().summary(),
        device.async_compute
    );
    let mut allocator = Allocator::new(&device);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(READBACK)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    let (counter, counter_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(256)
                .usage(vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Device,
        )
        .unwrap();
    let (history, history_mem) = allocator
        .create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(RGBA)
                .extent(vk::Extent3D {
                    width: SIZE,
                    height: SIZE,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC),
            MemoryLocation::Device,
        )
        .unwrap();
    let bytes = readback_mem.mapped_ptr().unwrap().as_ptr().cast_const();
    let mut run_with = |on, sharing| {
        run(
            &device,
            &mut allocator,
            on,
            sharing,
            history,
            vk::ImageView::null(),
            counter,
            readback,
            bytes,
        )
    };
    let single = run_with(false, AsyncSharing::Concurrent);
    let concurrent = run_with(true, AsyncSharing::Concurrent);
    let exclusive = run_with(true, AsyncSharing::Exclusive);

    // The single-queue frames are right ...
    for (frame, bytes) in single.iter().enumerate() {
        let shade = (frame * 40) as u8;
        let pixel = |image: u64, i: u64| {
            let at = (image * PIXELS * 4 + i * 4) as usize;
            [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
        };
        assert_eq!(pixel(0, 5), [0, 255, shade, 255], "c, frame {frame}");
        assert_eq!(pixel(1, PIXELS - 1), [255, 0, 0, 255], "e = b = a");
        assert_eq!(pixel(2, 7), [shade, 0, 255, 255], "history");
        let word = |at: u64| {
            let at = at as usize;
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        assert_eq!(word(3 * PIXELS * 4), 0xA5A5_0000 + frame as u32);
        assert_eq!(word(3 * PIXELS * 4 + 256), 0xC0DE_0000 + frame as u32);
    }
    // ... and the two queues give the same bytes.
    assert!(single == concurrent, "concurrent sharing differs");
    assert!(single == exclusive, "exclusive sharing differs");

    // SAFETY: the device is idle.
    unsafe {
        allocator.destroy_buffer(readback, readback_mem);
        allocator.destroy_buffer(counter, counter_mem);
        allocator.destroy_image(history, history_mem);
    }
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
