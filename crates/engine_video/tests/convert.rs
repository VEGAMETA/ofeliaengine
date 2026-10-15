//! The conversion pass (ADR 0100) against its CPU reference, under the validation layer
//! with synchronization validation, on the GPU `ENGINE_ADAPTER` picks:
//!
//! - synthetic planes of each chroma layout, bit depth, range, matrix and siting,
//!   uploaded as the software decoder's pictures are, at the texture's size and scaled;
//! - the GPU decoder's pictures, read where they were decoded, against rav1d's planes
//!   (bit-exact with them, `tests/gpu.rs`) converted on the CPU.
//!
//! Every texel may differ from the reference by one in a channel at most (the GPU's
//! arithmetic order); without a GPU the tests print why and pass, unless
//! `ENGINE_REQUIRE_GPU=1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks
)]

use engine_render::graph::RenderGraph;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, ExtraQueues, Instance,
    InstanceDesc, MemoryLocation,
};
use engine_video::av1::sequence::ColorConfig;
use engine_video::av1::{Parser, TemporalUnit};
use engine_video::convert::{
    Conversion, Converter, DecoderPlanes, PlaneSource, PlaneUpload, Planes, VideoTexture,
    wait_for_decode,
};
use engine_video::gpu::{GpuDecoder, GpuDecoderDesc, GpuError};
use engine_video::software::{Settings, SoftwareDecoder};
use engine_video::webm::{TrackKind, Webm};

fn gpu(video_decode: bool) -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_video conversion tests"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
        })?;
        let adapter = instance.select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )?;
        let device = Device::with_extras(
            &instance,
            &adapter,
            &[],
            ExtraQueues {
                video_decode,
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

const READBACK_BYTES: u64 = 4 << 20;

/// Runs conversions through a frame graph and reads the texture back.
struct Bed {
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    graph: RenderGraph,
    converter: Converter,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
}

impl Bed {
    fn new(device: Device) -> Self {
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let converter = Converter::new(&device, &bindless).unwrap();
        let d = device.raw();
        let pool = unsafe {
            d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
        }
        .unwrap();
        let cmd = unsafe {
            d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
        }
        .unwrap()[0];
        let (readback, memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(READBACK_BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        Self {
            device,
            allocator,
            bindless,
            graph,
            converter,
            pool,
            cmd,
            readback,
            readback_memory: Some(memory),
        }
    }

    /// Converts `source` into `texture` (after `semaphore` reaches its value, for a GPU
    /// decoder's picture) and returns the texture's RGBA bytes.
    fn convert(
        &mut self,
        texture: &mut VideoTexture,
        source: &PlaneSource,
        conversion: &Conversion,
        wait: Option<(vk::Semaphore, u64)>,
    ) -> Vec<[u8; 4]> {
        let Self {
            device,
            allocator,
            bindless,
            graph,
            converter,
            cmd,
            readback,
            readback_memory,
            ..
        } = self;
        let (d, cmd, buffer) = (device.raw(), *cmd, *readback);
        if let Some((semaphore, value)) = wait {
            wait_for_decode(device, semaphore, value).unwrap();
        }
        let [w, h] = texture.size();
        let bytes = u64::from(w * h * 4);
        assert!(bytes <= READBACK_BYTES);
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
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            converter.add_pass(&mut fg, bindless, texture, source, conversion);
            let image = texture.image();
            fg.add_pass("read back")
                .side_effects()
                .execute(move |ctx| unsafe {
                    let range = vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    let to_copy = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                        .old_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .image(image)
                        .subresource_range(range);
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_copy]),
                    );
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        buffer,
                        &[vk::BufferImageCopy {
                            image_subresource: vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: 0,
                                base_array_layer: 0,
                                layer_count: 1,
                            },
                            image_extent: vk::Extent3D {
                                width: w,
                                height: h,
                                depth: 1,
                            },
                            ..Default::default()
                        }],
                    );
                    let back = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::COPY)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                        .image(image)
                        .subresource_range(range);
                    let host = vk::MemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::COPY)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                        .dst_access_mask(vk::AccessFlags2::HOST_READ);
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default()
                            .memory_barriers(&[host])
                            .image_memory_barriers(&[back]),
                    );
                });
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
        }
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
        let mapped = readback_memory
            .as_mut()
            .unwrap()
            .mapped_slice_mut()
            .unwrap();
        mapped[..bytes as usize]
            .chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect()
    }

    /// Frees everything; returns the instance's validation errors.
    fn finish(mut self, instance: &Instance) -> u32 {
        unsafe {
            self.device.wait_idle().unwrap();
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.allocator
                .destroy_buffer(self.readback, self.readback_memory.take().unwrap());
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        drop(self.converter);
        drop(self.bindless);
        drop(self.allocator);
        drop(self.device);
        let report = instance.validation_report();
        println!(
            "validation: enabled={} errors={} warnings={}",
            report.enabled, report.errors, report.warnings
        );
        if report.errors > 0 {
            println!("{:#?}", report.first_errors);
        }
        report.errors
    }
}

/// A picture's code values: luma, and Cb, Cr pairs (`None`: luma only).
struct Codes {
    size: [u32; 2],
    luma: Vec<u16>,
    chroma: Option<(Vec<[u16; 2]>, [u32; 2])>,
}

/// The pass's arithmetic on the CPU: the texture `target` converts `codes` into.
fn reference(codes: &Codes, conversion: &Conversion, target: [u32; 2]) -> Vec<[u8; 4]> {
    let [w, h] = codes.size;
    let luma = |x: i32, y: i32| {
        let (x, y) = (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1));
        let v = f32::from(codes.luma[(y as u32 * w + x as u32) as usize]);
        [v, v]
    };
    let (chroma, chroma_size) = match &codes.chroma {
        Some((samples, size)) => (samples.clone(), *size),
        None => (vec![[128, 128]], [1, 1]),
    };
    let chroma = |x: i32, y: i32| {
        let (cw, ch) = (chroma_size[0] as i32, chroma_size[1] as i32);
        let s = chroma[(y.clamp(0, ch - 1) * cw + x.clamp(0, cw - 1)) as usize];
        [f32::from(s[0]), f32::from(s[1])]
    };
    let lerp =
        |a: [f32; 2], b: [f32; 2], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let bilinear = |plane: &dyn Fn(i32, i32) -> [f32; 2], x: [f32; 2]| {
        let corner = [x[0].floor(), x[1].floor()];
        let f = [x[0] - corner[0], x[1] - corner[1]];
        let (px, py) = (corner[0] as i32, corner[1] as i32);
        let top = lerp(plane(px, py), plane(px + 1, py), f[0]);
        let bottom = lerp(plane(px, py + 1), plane(px + 1, py + 1), f[0]);
        lerp(top, bottom, f[1])
    };
    let step = [w as f32 / target[0] as f32, h as f32 / target[1] as f32];
    let mut out = Vec::with_capacity((target[0] * target[1]) as usize);
    for ty in 0..target[1] {
        for tx in 0..target[0] {
            let x = [
                (tx as f32 + 0.5) * step[0] - 0.5,
                (ty as f32 + 0.5) * step[1] - 0.5,
            ];
            let y = if step == [1.0, 1.0] {
                luma(tx as i32, ty as i32)[0]
            } else {
                bilinear(&luma, x)[0]
            };
            let c = bilinear(
                &chroma,
                [
                    x[0] * conversion.chroma_step[0] + conversion.chroma_bias[0],
                    x[1] * conversion.chroma_step[1] + conversion.chroma_bias[1],
                ],
            );
            let [r, g, b] = conversion.rgb([y, c[0], c[1]]);
            out.push([r, g, b, 255]);
        }
    }
    out
}

/// How the GPU's texels differ from the reference: `Err` past one in any channel.
fn compare(ours: &[[u8; 4]], theirs: &[[u8; 4]], width: u32) -> Result<usize, String> {
    assert_eq!(ours.len(), theirs.len());
    let mut off_by_one = 0;
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        let largest = a
            .iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        if largest > 1 {
            let (x, y) = (i as u32 % width, i as u32 / width);
            return Err(format!("texel {x},{y}: GPU {a:?}, CPU {b:?}"));
        }
        off_by_one += usize::from(largest == 1);
    }
    Ok(off_by_one)
}

/// The code values of a picture's planes in memory.
fn codes_of(planes: &Planes<'_>) -> Codes {
    let sample = |(rows, stride): (&[u8], usize), [w, h]: [u32; 2]| -> Vec<u16> {
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                out.push(if planes.bit_depth > 8 {
                    let at = y * stride + 2 * x;
                    u16::from_le_bytes([rows[at], rows[at + 1]])
                } else {
                    u16::from(rows[y * stride + x])
                });
            }
        }
        out
    };
    Codes {
        size: planes.size,
        luma: sample(planes.luma, planes.size),
        chroma: planes.chroma.map(|([cb, cr], size)| {
            let (cb, cr) = (sample(cb, size), sample(cr, size));
            (cb.into_iter().zip(cr).map(|(b, r)| [b, r]).collect(), size)
        }),
    }
}

/// A synthetic picture.
struct Case {
    name: &'static str,
    depth: u8,
    /// Chroma subsampled across and down; `None`: luma only.
    chroma: Option<[bool; 2]>,
    matrix: u8,
    full: bool,
    siting: u8,
    size: [u32; 2],
    target: [u32; 2],
}

/// Rows of samples with padding past each (a stride longer than the row).
fn rows(values: &[u16], [w, h]: [u32; 2], depth: u8) -> (Vec<u8>, usize) {
    let sample = if depth > 8 { 2 } else { 1 };
    let stride = w as usize * sample + 7;
    let mut bytes = vec![0xA5; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = values[y * w as usize + x];
            if sample == 2 {
                bytes[y * stride + 2 * x..][..2].copy_from_slice(&v.to_le_bytes());
            } else {
                bytes[y * stride + x] = v as u8;
            }
        }
    }
    (bytes, stride)
}

/// Smooth gradients with noise over a plane, every code value of `depth` bits reachable.
fn plane(seed: u64, [w, h]: [u32; 2], depth: u8) -> Vec<u16> {
    let max = (1u32 << depth) - 1;
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..w * h)
        .map(|i| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let (x, y) = (i % w, i / w);
            let smooth = (x * max / w.max(2) + y * max / h.max(2)) / 2;
            let noise = (state >> 40) as u32 % (max / 4 + 1);
            (smooth + noise).min(max) as u16
        })
        .collect()
}

#[test]
fn synthetic_planes_match_the_cpu() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let cases = [
        Case {
            name: "8-bit 4:2:0, BT.709 studio range, chroma left",
            depth: 8,
            chroma: Some([true, true]),
            matrix: 1,
            full: false,
            siting: 0,
            size: [64, 48],
            target: [64, 48],
        },
        Case {
            name: "10-bit 4:2:0, BT.2020 full range, colocated, odd size",
            depth: 10,
            chroma: Some([true, true]),
            matrix: 9,
            full: true,
            siting: 2,
            size: [61, 37],
            target: [61, 37],
        },
        Case {
            name: "8-bit 4:4:4 GBR",
            depth: 8,
            chroma: Some([false, false]),
            matrix: 0,
            full: true,
            siting: 0,
            size: [40, 30],
            target: [40, 30],
        },
        Case {
            name: "8-bit 4:2:2, BT.601 studio range",
            depth: 8,
            chroma: Some([true, false]),
            matrix: 6,
            full: false,
            siting: 0,
            size: [48, 32],
            target: [48, 32],
        },
        Case {
            name: "8-bit luma only",
            depth: 8,
            chroma: None,
            matrix: 1,
            full: false,
            siting: 0,
            size: [32, 24],
            target: [32, 24],
        },
        Case {
            name: "12-bit 4:2:0 scaled down",
            depth: 12,
            chroma: Some([true, true]),
            matrix: 1,
            full: false,
            siting: 1,
            size: [80, 60],
            target: [64, 40],
        },
        Case {
            name: "8-bit 4:2:0 scaled up",
            depth: 8,
            chroma: Some([true, true]),
            matrix: 2,
            full: false,
            siting: 0,
            size: [40, 30],
            target: [100, 70],
        },
    ];
    let mut bed = Bed::new(device);
    for (index, case) in cases.iter().enumerate() {
        let color = ColorConfig {
            bit_depth: case.depth,
            high_bitdepth: case.depth > 8,
            twelve_bit: case.depth == 12,
            mono_chrome: case.chroma.is_none(),
            color_description_present_flag: true,
            color_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: case.matrix,
            color_range: case.full,
            subsampling_x: case.chroma.is_none_or(|c| c[0]),
            subsampling_y: case.chroma.is_none_or(|c| c[1]),
            chroma_sample_position: case.siting,
            separate_uv_delta_q: false,
        };
        let [w, h] = case.size;
        let chroma_size = case.chroma.map(|[x, y]| {
            [
                if x { w.div_ceil(2) } else { w },
                if y { h.div_ceil(2) } else { h },
            ]
        });
        let seed = index as u64 * 3;
        let luma = rows(&plane(seed, case.size, case.depth), case.size, case.depth);
        let chroma = chroma_size.map(|size| {
            (
                rows(&plane(seed + 1, size, case.depth), size, case.depth),
                rows(&plane(seed + 2, size, case.depth), size, case.depth),
                size,
            )
        });
        let planes = Planes {
            size: case.size,
            bit_depth: case.depth,
            luma: (&luma.0, luma.1),
            chroma: chroma
                .as_ref()
                .map(|(cb, cr, size)| ([(&cb.0[..], cb.1), (&cr.0[..], cr.1)], *size)),
        };
        let conversion = Conversion::new(&color, case.target[0], case.target[1]);
        let mut upload = PlaneUpload::new(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            &color,
            case.size,
            1,
        )
        .unwrap();
        let mut texture = VideoTexture::new(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            case.target[0],
            case.target[1],
        )
        .unwrap();
        let source = upload.stage(0, &planes).unwrap();
        let ours = bed.convert(&mut texture, &source, &conversion, None);
        let theirs = reference(&codes_of(&planes), &conversion, case.target);
        match compare(&ours, &theirs, case.target[0]) {
            Ok(off) => println!("{}: {} texels, {off} off by one", case.name, ours.len()),
            Err(e) => panic!("{}: {e}", case.name),
        }
        unsafe {
            texture.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0);
            upload.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0);
        }
    }
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn decoded_pictures_match_the_cpu() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    if device.video_decode.is_none() {
        println!(
            "{}: no AV1 decode queue, nothing to convert",
            device.adapter().name()
        );
        return;
    }
    let mut bed = Bed::new(device);
    let clips = [
        // 8 bits, coded 184 lines high under a 180-line picture: scaled to 180.
        "svt_8bit_320x180.webm",
        // 10 bits: P010's words.
        "aom_10bit_tiles_320x180.webm",
        "svt_grain_tiles_640x360.webm",
    ];
    let mut converted = 0;
    for clip in clips {
        let path = format!("{}/tests/clips/{clip}", env!("CARGO_MANIFEST_DIR"));
        let data = std::fs::read(&path).unwrap();
        let webm = Webm::read(&data).unwrap();
        let video = webm.track(TrackKind::Video).unwrap();
        let mut parser = Parser::with_codec_private(&video.private).unwrap();
        let mut unit = TemporalUnit::default();
        let mut software = SoftwareDecoder::new(&Settings {
            threads: 1,
            max_frame_delay: 1,
            apply_grain: true,
            output_invisible_frames: false,
        })
        .unwrap();
        let mut decoder: Option<(GpuDecoder, DecoderPlanes)> = None;
        let mut texture: Option<VideoTexture> = None;
        let mut pictures = Vec::new();
        let mut count = 0;
        for block in webm.blocks().map(Result::unwrap) {
            if block.track != video.number {
                continue;
            }
            parser.parse(block.data(), &mut unit).unwrap();
            let sequence = *parser.sequence().unwrap();
            if decoder.is_none() {
                let gpu = match GpuDecoder::new(
                    &bed.device,
                    &mut bed.allocator,
                    &sequence,
                    parser.sequence_serial(),
                    &GpuDecoderDesc::default(),
                ) {
                    Ok(d) => d,
                    Err(GpuError::Unsupported(why)) => {
                        println!("{clip}: not on this GPU ({why})");
                        break;
                    }
                    Err(e) => panic!("{clip}: {e}"),
                };
                let planes = DecoderPlanes::new(&gpu, &bed.bindless).unwrap();
                decoder = Some((gpu, planes));
            }
            let (gpu, planes) = decoder.as_mut().unwrap();
            pictures.clear();
            gpu.decode(
                &bed.device,
                &mut bed.allocator,
                block.data(),
                &unit,
                &sequence,
                block.time_ns,
                &mut pictures,
            )
            .unwrap();
            let mut theirs = Vec::new();
            software
                .decode(block.data(), block.time_ns, |p| theirs.push(p))
                .unwrap();
            assert_eq!(pictures.len(), theirs.len(), "{clip}: pictures");
            for (ours, theirs) in pictures.iter().zip(&theirs) {
                let target = [ours.render_width, ours.render_height];
                let texture = texture.get_or_insert_with(|| {
                    VideoTexture::new(
                        &bed.device,
                        &mut bed.allocator,
                        &bed.bindless,
                        target[0],
                        target[1],
                    )
                    .unwrap()
                });
                let conversion = Conversion::new(&sequence.color, target[0], target[1]);
                let source = planes.source(ours).unwrap();
                let semaphore = gpu.semaphore();
                let texels =
                    bed.convert(texture, &source, &conversion, Some((semaphore, ours.ready)));
                let reference = reference(
                    &codes_of(&Planes::from(theirs)),
                    &conversion,
                    texture.size(),
                );
                if let Err(e) = compare(&texels, &reference, texture.size()[0]) {
                    panic!("{clip}, picture {count}: {e}");
                }
                gpu.release(ours);
                count += 1;
            }
        }
        if let Some(texture) = texture {
            unsafe { texture.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0) };
        }
        if let Some((gpu, planes)) = decoder {
            planes.release(&bed.bindless, 0);
            unsafe { gpu.destroy(&bed.device, &mut bed.allocator) };
        }
        if count > 0 {
            println!("{clip}: {count} pictures converted as on the CPU");
            converted += 1;
        }
    }
    assert!(converted > 0, "no clip converted");
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}
