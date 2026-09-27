//! Depth pyramids (`engine_render::hiz`) are exactly the level-by-level reduction: every
//! texel of level `k` is the farthest of the 2 × 2 texels below it, reads clamped to the
//! edge. Pseudo-random depths are uploaded, reduced on the GPU and compared bit for bit
//! with a CPU reference, for sizes that end in partial tiles and need 1 to 3 dispatches.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{
    Access, BufferDesc, BufferState, ImportedBuffer, RenderGraph, TextureDesc,
};
use engine_render::hiz::{DepthPyramid, PyramidLayout, level_size};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render hiz test"),
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

/// A depth in [0, 1) from a hash of the pixel: no structure a reduction could get lucky
/// with.
fn depth_at(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// The pyramid, level by level, as the layout stores it (the levels it does not store are
/// computed and dropped).
fn reference(depth: &[f32], width: u32, height: u32) -> Vec<f32> {
    let layout = PyramidLayout::new(width, height);
    let mut out = vec![0.0; layout.floats as usize];
    let (mut below, mut bw, mut bh) = (depth.to_vec(), width, height);
    for k in 1..=layout.levels {
        let (w, h) = level_size(width, height, k);
        let mut level = vec![0.0f32; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let (x0, y0) = (2 * x, 2 * y);
                let (x1, y1) = ((2 * x + 1).min(bw - 1), (2 * y + 1).min(bh - 1));
                let at = |x: u32, y: u32| below[(y * bw + x) as usize];
                level[(y * w + x) as usize] =
                    at(x0, y0).max(at(x1, y0)).max(at(x0, y1)).max(at(x1, y1));
            }
        }
        if k >= layout.finest {
            let offset = layout.offsets[k as usize - 1] as usize;
            out[offset..offset + level.len()].copy_from_slice(&level);
        }
        (below, bw, bh) = (level, w, h);
    }
    out
}

#[test]
fn pyramid_matches_the_reference() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let pyramid = DepthPyramid::new(&device, &bindless).unwrap();
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

    // 1080p (2 dispatches), partial tiles on one or both axes, 1 × 1, and a size whose 13
    // levels need 3 dispatches.
    let sizes = [
        (1920, 1080),
        (1001, 7),
        (7, 1001),
        (130, 257),
        (3, 3),
        (1, 1),
        (8192, 3),
    ];
    for (width, height) in sizes {
        let layout = PyramidLayout::new(width, height);
        let depth: Vec<f32> = (0..height)
            .flat_map(|y| (0..width).map(move |x| depth_at(x, y)))
            .collect();
        let depth_bytes = (depth.len() * 4) as u64;
        let (upload, upload_mem) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(depth_bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                MemoryLocation::Upload,
            )
            .unwrap();
        let ptr = upload_mem.mapped_ptr().unwrap();
        // SAFETY: the mapping covers the buffer; nothing else uses it yet.
        unsafe {
            core::ptr::copy_nonoverlapping(
                depth.as_ptr().cast::<u8>(),
                ptr.as_ptr(),
                depth_bytes as usize,
            );
        }
        let (readback, readback_mem) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(layout.bytes())
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();

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
            let target = fg.create_texture(
                "depth",
                TextureDesc::new_2d(vk::Format::D32_SFLOAT, width, height),
            );
            let values = fg.import_buffer(
                "depth values",
                ImportedBuffer {
                    buffer: upload,
                    size: depth_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: None,
                },
            );
            fg.add_pass("upload depth")
                .read(values, Access::TransferRead)
                .write(target, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
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
                        ctx.device.cmd_copy_buffer_to_image(
                            ctx.cmd,
                            ctx.buffer(values),
                            ctx.image(target),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                });
            let levels = fg.create_buffer(
                "pyramid",
                BufferDesc {
                    size: layout.bytes(),
                },
            );
            pyramid.add_pass(&mut fg, &bindless, target, levels);
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: layout.bytes(),
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let bytes = layout.bytes();
            fg.add_pass("read back")
                .read(levels, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; the graph orders the copy after the pyramid.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(levels),
                            ctx.buffer(out),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                size: bytes,
                            }],
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
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let gpu: &[f32] = unsafe {
            core::slice::from_raw_parts(ptr.as_ptr().cast::<f32>(), layout.floats as usize)
        };
        let expected = reference(&depth, width, height);
        for k in layout.finest..=layout.levels {
            let (w, h) = level_size(width, height, k);
            let offset = layout.offsets[k as usize - 1] as usize;
            let len = (w * h) as usize;
            let mismatch =
                (0..len).find(|&i| gpu[offset + i].to_bits() != expected[offset + i].to_bits());
            assert!(
                mismatch.is_none(),
                "{width}×{height}, level {k} ({w}×{h}): texel {:?} is {} on the GPU, {} expected",
                mismatch.map(|i| (i as u32 % w, i as u32 / w)),
                mismatch.map_or(0.0, |i| gpu[offset + i]),
                mismatch.map_or(0.0, |i| expected[offset + i]),
            );
        }
        println!("{width}×{height}: {} levels match", layout.levels);
        // SAFETY: the GPU is idle; the graph only borrowed them for this frame.
        unsafe {
            allocator.destroy_buffer(upload, upload_mem);
            allocator.destroy_buffer(readback, readback_mem);
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        d.destroy_command_pool(pool, None);
    }
    drop(pyramid);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "validation errors");
}
