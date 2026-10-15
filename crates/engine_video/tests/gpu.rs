//! The GPU's AV1 decoding against rav1d's (ADR 0100): every picture of every clip the
//! device's decoder takes, bit for bit, under the validation layer, on the GPU
//! `ENGINE_ADAPTER` picks.
//!
//! Without a GPU that decodes AV1 the tests print why and pass, unless
//! `ENGINE_REQUIRE_GPU=1`; a GPU without the decode queue only says so.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks
)]

use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Device, ExtraQueues, Instance, InstanceDesc,
    MemoryLocation,
};
use engine_video::av1::{Parser, TemporalUnit};
use engine_video::gpu::{GpuDecoder, GpuDecoderDesc, GpuError, GpuPicture};
use engine_video::software::{Picture, Settings, SoftwareDecoder};
use engine_video::webm::{TrackKind, Webm};

const CLIPS: [&str; 11] = [
    "svt_8bit_320x180.webm",
    "aom_10bit_tiles_320x180.webm",
    "svt_grain_tiles_640x360.webm",
    "rav1e_8bit_320x180.webm",
    "aom_tools_320x180.webm",
    "aom_resilient_320x180.webm",
    "aom_lossless_160x90.webm",
    "svt_superres_320x180.webm",
    "svt_resize_320x180.webm",
    "svt_superres_320x176.webm",
    "svt_resize_320x176.webm",
];

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_video tests"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let adapter = instance.select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )?;
        let device = Device::with_extras(
            &instance,
            &adapter,
            &[],
            ExtraQueues {
                video_decode: true,
                ..ExtraQueues::default()
            },
        )?;
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

/// Reads decoded pictures back on the graphics queue.
struct Readback {
    pool: vk::CommandPool,
    commands: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
}

/// A picture's two planes as the GPU wrote them: luma, then interleaved chroma.
struct Planes {
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

const READBACK_BYTES: u64 = 4 << 20;

impl Readback {
    fn new(device: &Device, allocator: &mut Allocator) -> Self {
        let raw = device.raw();
        let pool = unsafe {
            raw.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
        }
        .unwrap();
        let commands = unsafe {
            raw.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .unwrap()[0];
        let fence = unsafe { raw.create_fence(&vk::FenceCreateInfo::default(), None) }.unwrap();
        let (buffer, allocation) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(READBACK_BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        Self {
            pool,
            commands,
            fence,
            buffer,
            allocation: Some(allocation),
        }
    }

    /// Copies `picture`'s planes to the host once the decoder's semaphore reaches it.
    fn read(&mut self, device: &Device, decoder: &GpuDecoder, picture: &GpuPicture) -> Planes {
        let raw = device.raw();
        let wide = picture.format == vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;
        let sample = if wide { 2 } else { 1 };
        let (w, h) = (picture.width, picture.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let luma_bytes = u64::from(w * h) * sample;
        let chroma_offset = luma_bytes.next_multiple_of(16);
        let chroma_bytes = u64::from(cw * ch) * sample * 2;
        assert!(chroma_offset + chroma_bytes <= READBACK_BYTES);
        unsafe {
            raw.reset_command_buffer(self.commands, vk::CommandBufferResetFlags::empty())
                .unwrap();
            raw.begin_command_buffer(self.commands, &vk::CommandBufferBeginInfo::default())
                .unwrap();
            let barrier = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(picture.layout)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(picture.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            raw.cmd_pipeline_barrier2(
                self.commands,
                &vk::DependencyInfo::default().image_memory_barriers(&[barrier]),
            );
            let region = |aspect, offset, width, height| vk::BufferImageCopy {
                buffer_offset: offset,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: aspect,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D::default(),
                image_extent: vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                },
            };
            raw.cmd_copy_image_to_buffer(
                self.commands,
                picture.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &[
                    region(vk::ImageAspectFlags::PLANE_0, 0, w, h),
                    region(vk::ImageAspectFlags::PLANE_1, chroma_offset, cw, ch),
                ],
            );
            // Back in its layout: a hidden frame's picture may be shown again.
            let back = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(picture.layout)
                .image(picture.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            raw.cmd_pipeline_barrier2(
                self.commands,
                &vk::DependencyInfo::default().image_memory_barriers(&[back]),
            );
            raw.end_command_buffer(self.commands).unwrap();
            let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(self.commands)];
            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(decoder.semaphore())
                .value(picture.ready)
                .stage_mask(vk::PipelineStageFlags2::COPY)];
            raw.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default()
                    .command_buffer_infos(&commands)
                    .wait_semaphore_infos(&waits)],
                self.fence,
            )
            .unwrap();
            raw.wait_for_fences(&[self.fence], true, u64::MAX).unwrap();
            raw.reset_fences(&[self.fence]).unwrap();
        }
        let mapped = self
            .allocation
            .as_mut()
            .unwrap()
            .mapped_slice_mut()
            .unwrap();
        Planes {
            luma: mapped[..luma_bytes as usize].to_vec(),
            chroma: mapped[chroma_offset as usize..(chroma_offset + chroma_bytes) as usize]
                .to_vec(),
        }
    }

    fn destroy(mut self, device: &Device, allocator: &mut Allocator) {
        let raw = device.raw();
        unsafe {
            raw.destroy_fence(self.fence, None);
            raw.destroy_command_pool(self.pool, None);
            allocator.destroy_buffer(self.buffer, self.allocation.take().unwrap());
        }
    }
}

/// How the GPU's planes differ from rav1d's picture: the count of samples that differ,
/// the largest difference and the box they are in, per plane; `None` when equal.
fn differs(gpu: &Planes, picture: &Picture, width: u32, height: u32, wide: bool) -> Option<String> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let sample = |bytes: &[u8], i: usize| -> i32 {
        if wide {
            i32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        } else {
            i32::from(bytes[i])
        }
    };
    // P010 keeps its 10 bits in the high bits of each word.
    let shift = if wide { 6 } else { 0 };
    #[derive(Default)]
    struct Stats {
        count: usize,
        largest: i32,
        first: Option<(usize, usize, i32, i32)>,
        min: (usize, usize),
        max: (usize, usize),
    }
    let mut stats = [
        Stats {
            min: (usize::MAX, usize::MAX),
            ..Stats::default()
        },
        Stats {
            min: (usize::MAX, usize::MAX),
            ..Stats::default()
        },
        Stats {
            min: (usize::MAX, usize::MAX),
            ..Stats::default()
        },
    ];
    let mut note = |plane: usize, x: usize, y: usize, ours: i32, theirs: i32| {
        if ours != theirs {
            let s = &mut stats[plane];
            s.count += 1;
            s.largest = s.largest.max((ours - theirs).abs());
            s.first.get_or_insert((x, y, ours, theirs));
            s.min = (s.min.0.min(x), s.min.1.min(y));
            s.max = (s.max.0.max(x), s.max.1.max(y));
        }
    };
    let (luma, stride) = picture.plane(0).unwrap();
    for y in 0..h {
        for x in 0..w {
            note(
                0,
                x,
                y,
                sample(&gpu.luma, y * w + x) >> shift,
                sample(&luma[y * stride..], x),
            );
        }
    }
    let (u, u_stride) = picture.plane(1).unwrap();
    let (v, v_stride) = picture.plane(2).unwrap();
    for y in 0..ch {
        for x in 0..cw {
            note(
                1,
                x,
                y,
                sample(&gpu.chroma, 2 * (y * cw + x)) >> shift,
                sample(&u[y * u_stride..], x),
            );
            note(
                2,
                x,
                y,
                sample(&gpu.chroma, 2 * (y * cw + x) + 1) >> shift,
                sample(&v[y * v_stride..], x),
            );
        }
    }
    let lines: Vec<String> = stats
        .iter()
        .zip(["Y", "U", "V"])
        .filter(|(s, _)| s.count > 0)
        .map(|(s, name)| {
            let (x, y, ours, theirs) = s.first.unwrap_or_default();
            format!(
                "{name}: {} samples differ by up to {}, in {:?}..={:?}; first at {x},{y}: GPU {ours}, rav1d {theirs}",
                s.count, s.largest, s.min, s.max
            )
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

/// Decodes `clip` on the GPU and with rav1d, comparing every shown picture. `None` when
/// the device's decoder does not take the clip (with why).
fn compare_clip(
    device: &Device,
    allocator: &mut Allocator,
    readback: &mut Readback,
    clip: &str,
    film_grain: bool,
) -> Result<usize, String> {
    let path = format!("{}/tests/clips/{clip}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(&path).unwrap();
    let webm = Webm::read(&data).unwrap();
    let video = webm.track(TrackKind::Video).unwrap();
    let blocks: Vec<_> = webm
        .blocks()
        .map(Result::unwrap)
        .filter(|b| b.track == video.number)
        .collect();
    let mut parser = Parser::with_codec_private(&video.private).unwrap();
    let mut unit = TemporalUnit::default();
    let mut software = SoftwareDecoder::new(&Settings {
        threads: 1,
        max_frame_delay: 1,
        apply_grain: film_grain,
        output_invisible_frames: false,
    })
    .unwrap();
    let mut decoder: Option<GpuDecoder> = None;
    let mut pictures = Vec::new();
    let mut compared = 0;
    let mut mismatches = Vec::new();
    let mut outcome = Ok(());
    for block in &blocks {
        parser.parse(block.data(), &mut unit).unwrap();
        let sequence = *parser.sequence().unwrap();
        if decoder.is_none() {
            match GpuDecoder::new(
                device,
                allocator,
                &sequence,
                parser.sequence_serial(),
                &GpuDecoderDesc {
                    outputs: 12,
                    film_grain,
                },
            ) {
                Ok(d) => decoder = Some(d),
                Err(GpuError::Unsupported(why)) => return Err(why.to_owned()),
                Err(e) => panic!("{clip}: {e}"),
            }
        }
        let gpu = decoder.as_mut().unwrap();
        pictures.clear();
        match gpu.decode(
            device,
            allocator,
            block.data(),
            &unit,
            &sequence,
            block.time_ns,
            &mut pictures,
        ) {
            Ok(()) => {}
            Err(GpuError::Unsupported(why)) => {
                outcome = Err(why.to_owned());
                break;
            }
            Err(e) => panic!("{clip} at {} ms: {e}", block.time_ns / 1_000_000),
        }
        let mut theirs = Vec::new();
        software
            .decode(block.data(), block.time_ns, |p| theirs.push(p))
            .unwrap();
        assert_eq!(
            pictures.len(),
            theirs.len(),
            "{clip} at {} ms: pictures",
            block.time_ns / 1_000_000
        );
        for (ours, theirs) in pictures.iter().zip(&theirs) {
            assert_eq!(
                (ours.width, ours.height),
                (theirs.width(), theirs.height()),
                "{clip}: picture size"
            );
            assert_eq!(
                ours.timestamp,
                theirs.timestamp(),
                "{clip}: picture {compared}'s time"
            );
            let planes = readback.read(device, gpu, ours);
            let wide = theirs.bit_depth() > 8;
            if let Some(difference) = differs(&planes, theirs, ours.width, ours.height, wide) {
                mismatches.push(format!(
                    "picture {compared} ({} ms): {difference}",
                    block.time_ns / 1_000_000
                ));
            }
            gpu.release(ours);
            compared += 1;
        }
    }
    if let Some(gpu) = decoder {
        unsafe { gpu.destroy(device, allocator) };
    }
    if !mismatches.is_empty() {
        return Ok(Err(mismatches))
            .map(|e: Result<usize, Vec<String>>| e)
            .map_or_else(Err, |r| {
                r.map_err(|m| {
                    format!(
                        "MISMATCH {} of {compared} pictures:
  {}",
                        m.len(),
                        m.join(
                            "
  "
                        )
                    )
                })
            });
    }
    outcome.map(|()| compared)
}

fn compare_all(film_grain: bool, clips: &[&str]) {
    let Some((instance, device)) = gpu() else {
        return;
    };
    if device.video_decode.is_none() {
        println!(
            "{}: no AV1 decode queue, nothing to compare",
            device.adapter().name()
        );
        return;
    }
    let mut allocator = Allocator::new(&device);
    let mut readback = Readback::new(&device, &mut allocator);
    let mut compared = 0;
    let mut failed = Vec::new();
    let mut refused = Vec::new();
    for clip in clips {
        match compare_clip(&device, &mut allocator, &mut readback, clip, film_grain) {
            Ok(pictures) => {
                println!("{clip}: {pictures} pictures equal rav1d's");
                compared += 1;
            }
            Err(why) if why.starts_with("MISMATCH") => {
                println!("{clip}: {why}");
                failed.push(*clip);
            }
            Err(why) => {
                println!("{clip}: not on {} ({why})", device.adapter().name());
                refused.push(why);
            }
        }
    }
    readback.destroy(&device, &mut allocator);
    unsafe { device.raw().device_wait_idle() }.unwrap();
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
    assert!(
        failed.is_empty(),
        "pictures differ from rav1d's: {failed:?}"
    );
    // A clip may be refused only for its pictures' size (the RTX takes nothing below
    // 128 × 128); both reference GPUs take most clips.
    assert!(
        refused
            .iter()
            .all(|why| why.contains("smaller") || why.contains("frame size")),
        "refused for other reasons: {refused:?}"
    );
    assert!(
        compared * 2 >= clips.len(),
        "only {compared} clips compared"
    );
}

#[test]
fn every_picture_matches_rav1d() {
    compare_all(false, &CLIPS);
}

#[test]
fn film_grain_matches_rav1d() {
    compare_all(
        true,
        &["svt_grain_tiles_640x360.webm", "aom_tools_320x180.webm"],
    );
}
