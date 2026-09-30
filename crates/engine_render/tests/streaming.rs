//! Streaming textures into the texture pool (ADR 0057), under the validation layer with
//! synchronization validation: levels uploaded from the staging ring in any order, the
//! texture resident from its smallest level up (a new bindless index each time a larger
//! level joins), the data in the image as uploaded, and the ring's memory lent, full, and
//! reused once the frames copying from it have finished.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph};
use engine_render::texture_pool::{
    LEVEL_ALIGN, STAGING_ALIGN, StagingSpan, TextureError, TextureFormat, TextureId, TexturePool,
};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

/// A 32 × 32 RGBA8 texture: 6 levels.
const SIZE: u32 = 32;
const LEVELS: u32 = 6;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render streaming test"),
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

fn level_side(k: u32) -> u32 {
    (SIZE >> k).max(1)
}

/// Texel (x, y) of level k: (k, x, y, 200).
fn level(k: u32) -> Vec<u8> {
    let s = level_side(k);
    (0..s * s)
        .flat_map(|i| [k as u8, (i % s) as u8, (i / s) as u8, 200])
        .collect()
}

/// Levels `first..first + count` as a staging span holds them: each padded to 16 bytes.
fn packed(first: u32, count: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for k in first..first + count {
        out.extend(level(k));
        out.resize(out.len().next_multiple_of(LEVEL_ALIGN as usize), 0);
    }
    out
}

struct Harness {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    pool: TexturePool,
    graph: RenderGraph,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    frame_number: u64,
}

/// Bytes of every level, back to back (for the readback).
fn all_levels_bytes() -> u64 {
    (0..LEVELS)
        .map(|k| u64::from(level_side(k).pow(2) * 4))
        .sum()
}

impl Harness {
    fn new(instance: Instance, device: Device, staging: u64) -> Self {
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let mut pool = TexturePool::new(&device, 1 << 20, 1);
        pool.set_staging_size(staging);
        let graph = RenderGraph::new(&device, 1);
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(all_levels_bytes())
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        let d = device.raw();
        // SAFETY: plain object creation.
        let (command_pool, cmd) = unsafe {
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
        Self {
            instance,
            device,
            allocator,
            bindless,
            pool,
            graph,
            command_pool,
            cmd,
            readback,
            readback_memory: Some(readback_memory),
            frame_number: 0,
        }
    }

    fn span(&mut self, bytes: u64) -> Option<StagingSpan> {
        self.pool.staging(&mut self.allocator, bytes).unwrap()
    }

    /// Fills a span with `data`.
    fn filled(&mut self, data: &[u8]) -> StagingSpan {
        let mut span = self.span(data.len() as u64).unwrap();
        span.bytes_mut()[..data.len()].copy_from_slice(data);
        span
    }

    /// One frame: the pool's uploads, and with `read`, every level of `read` copied back.
    fn frame(&mut self, read: Option<TextureId>) -> Option<Vec<u8>> {
        let Self {
            device,
            allocator,
            bindless,
            pool,
            graph,
            cmd,
            readback,
            readback_memory,
            frame_number,
            ..
        } = self;
        let (d, cmd) = (device.raw(), *cmd);
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
        *frame_number += 1;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            pool.begin_frame(&mut fg, allocator, bindless, *frame_number)
                .unwrap();
            if let Some(id) = read {
                let image = pool.image(id).unwrap();
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: *readback,
                        size: all_levels_bytes(),
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: None,
                    },
                );
                fg.add_pass("read back")
                    .write(out, Access::TransferWrite)
                    .execute(move |ctx| {
                        let range = vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: LEVELS,
                            base_array_layer: 0,
                            layer_count: 1,
                        };
                        let barrier = |old, new, src, dst| {
                            vk::ImageMemoryBarrier2::default()
                                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                                .src_access_mask(src)
                                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                                .dst_access_mask(dst)
                                .old_layout(old)
                                .new_layout(new)
                                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                                .image(image)
                                .subresource_range(range)
                        };
                        let mut offset = 0;
                        let regions: Vec<_> = (0..LEVELS)
                            .map(|k| {
                                let region = vk::BufferImageCopy {
                                    buffer_offset: offset,
                                    image_subresource: vk::ImageSubresourceLayers {
                                        aspect_mask: vk::ImageAspectFlags::COLOR,
                                        mip_level: k,
                                        base_array_layer: 0,
                                        layer_count: 1,
                                    },
                                    image_extent: vk::Extent3D {
                                        width: level_side(k),
                                        height: level_side(k),
                                        depth: 1,
                                    },
                                    ..Default::default()
                                };
                                offset += u64::from(level_side(k).pow(2) * 4);
                                region
                            })
                            .collect();
                        // SAFETY: recording outside a rendering scope; every level of the
                        // image is uploaded and read-only between the pool's uploads.
                        unsafe {
                            ctx.device.cmd_pipeline_barrier2(
                                ctx.cmd,
                                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                                    vk::AccessFlags2::TRANSFER_READ,
                                )]),
                            );
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                image,
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &regions,
                            );
                            ctx.device.cmd_pipeline_barrier2(
                                ctx.cmd,
                                &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                                    vk::AccessFlags2::TRANSFER_READ,
                                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                                )]),
                            );
                        }
                    });
            }
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
        read.map(|_| {
            let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers every level.
            unsafe { core::slice::from_raw_parts(ptr.as_ptr(), all_levels_bytes() as usize) }
                .to_vec()
        })
    }

    fn finish(mut self) {
        // SAFETY: the device is idle.
        unsafe {
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.pool.destroy(&mut self.allocator, &self.bindless);
            if let Some(memory) = self.readback_memory.take() {
                self.allocator.destroy_buffer(self.readback, memory);
            }
            self.device
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        let Self {
            instance,
            device,
            allocator,
            bindless,
            ..
        } = self;
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
}

#[test]
fn levels_stream_in_and_the_texture_grows_from_its_tail() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut h = Harness::new(instance, device, 16 * STAGING_ALIGN);
    let id = h
        .pool
        .add_empty(
            &mut h.allocator,
            SIZE,
            SIZE,
            TextureFormat::Rgba8Unorm,
            LEVELS,
        )
        .unwrap();
    assert_eq!((h.pool.index(id), h.pool.resident_level(id)), (None, None));

    // The tail: levels 3-5 (4², 2², 1²).
    let tail = h.filled(&packed(3, 3));
    h.pool.upload_levels(id, 3, 3, tail).unwrap();
    h.frame(None);
    assert_eq!(h.pool.resident_level(id), Some(3));
    let first_index = h.pool.index(id).unwrap();

    // Level 1 before level 2: uploaded, but not resident until level 2 is.
    let one = h.filled(&packed(1, 1));
    h.pool.upload_levels(id, 1, 1, one).unwrap();
    h.frame(None);
    assert_eq!(h.pool.resident_level(id), Some(3));
    assert_eq!(h.pool.index(id), Some(first_index));
    let two = h.filled(&packed(2, 1));
    h.pool.upload_levels(id, 2, 1, two).unwrap();
    h.frame(None);
    assert_eq!(h.pool.resident_level(id), Some(1));
    let second_index = h.pool.index(id).unwrap();
    assert_ne!(second_index, first_index);

    // Level 0 (4 KiB): the whole texture; its data is what was uploaded.
    let zero = h.filled(&packed(0, 1));
    h.pool.upload_levels(id, 0, 1, zero).unwrap();
    h.frame(None);
    assert_eq!(h.pool.resident_level(id), Some(0));
    let read = h.frame(Some(id)).unwrap();
    let expected: Vec<u8> = (0..LEVELS).flat_map(level).collect();
    assert_eq!(read, expected);

    // Levels outside the texture, or a stale texture: refused, the span given back.
    let bad = h.span(64).unwrap();
    assert!(matches!(
        h.pool.upload_levels(id, 5, 2, bad),
        Err(TextureError::BadLevels)
    ));
    h.pool.remove(id, h.frame_number, &h.bindless);
    let stale = h.span(64).unwrap();
    assert!(matches!(
        h.pool.upload_levels(id, 0, 1, stale),
        Err(TextureError::BadLevels)
    ));
    h.frame(None);
    h.finish();
}

#[test]
fn the_staging_ring_is_reused_once_its_copies_are_done() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut h = Harness::new(instance, device, 4 * STAGING_ALIGN);
    assert!(matches!(
        h.pool.staging(&mut h.allocator, 5 * STAGING_ALIGN),
        Err(TextureError::StagingTooSmall)
    ));
    let id = h
        .pool
        .add_empty(
            &mut h.allocator,
            SIZE,
            SIZE,
            TextureFormat::Rgba8Unorm,
            LEVELS,
        )
        .unwrap();
    // Level 0 is exactly one block; four fill the ring.
    let spans: Vec<StagingSpan> = (0..4).map(|_| h.filled(&packed(0, 1))).collect();
    assert!(h.span(1).is_none());
    // A span given back without an upload frees its memory once the older ones are back.
    let mut spans = spans.into_iter();
    let first = spans.next().unwrap();
    h.pool.upload_levels(id, 0, 1, first).unwrap();
    for span in spans {
        h.pool.cancel_staging(span);
    }
    assert!(
        h.span(1).is_none(),
        "the upload's span is still being copied"
    );
    h.frame(None);
    // The frame that copied it has not been collected yet (it is, at the next frame).
    h.frame(None);
    let again = h.span(4 * STAGING_ALIGN);
    assert!(again.is_some());
    h.pool.cancel_staging(again.unwrap());
    h.finish();
}
